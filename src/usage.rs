use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use comfy_table::{Cell, CellAlignment, Table, presets::UTF8_FULL_CONDENSED};

use axum::http::{Response, header};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, OnceCell, mpsc};

use crate::{adapters::SseDecoder, proxy::MAX_JSON_BODY_BYTES};

#[derive(Debug, clap::Args)]
pub(crate) struct UsageArgs {
    /// Exact provider name
    #[arg(long)]
    name: Option<String>,
    /// UTC day (YYYY-MM-DD)
    #[arg(long, conflicts_with = "date_range")]
    date: Option<NaiveDate>,
    /// Inclusive UTC date range (YYYY-MM-DD YYYY-MM-DD)
    #[arg(long, num_args = 2, value_names = ["START", "END"])]
    date_range: Vec<NaiveDate>,
}

impl UsageArgs {
    pub(crate) async fn report(&self, path: &Path) -> Result<String> {
        let (start, end) = match self.date_range.as_slice() {
            [start, end] => {
                anyhow::ensure!(
                    start <= end,
                    "date range start must be on or before its end"
                );
                (Some(*start), Some(*end))
            }
            _ => (self.date, self.date),
        };
        let empty = "No matching usage records.";
        if !path.try_exists()? {
            return Ok(empty.into());
        }
        let db =
            turso::Builder::new_local(path.to_str().context("usage database path is not UTF-8")?)
                .experimental_multiprocess_wal(true)
                .read_only(true)
                .build()
                .await
                .with_context(|| format!("open usage database {}", path.display()))?;
        let connection = db.connect()?;
        let mut rows = connection
            .query(
                "SELECT provider, coalesce(model, '-'), count(*),
                    sum(input_tokens), sum(output_tokens), sum(total_tokens),
                    sum(cache_read_tokens), sum(cache_creation_tokens), sum(reasoning_tokens)
             FROM token_usage
             WHERE (?1 IS NULL OR provider = ?1)
               AND (?2 IS NULL OR substr(created_at,1,10) BETWEEN ?2 AND ?3)
             GROUP BY provider, model ORDER BY provider, model",
                turso::params![
                    self.name.as_deref(),
                    start.map(|d| d.to_string()),
                    end.map(|d| d.to_string())
                ],
            )
            .await?;
        let mut groups = Vec::new();
        let mut totals = [None; 7];
        while let Some(row) = rows.next().await? {
            let mut counts = [None; 7];
            for (index, count) in counts.iter_mut().enumerate() {
                *count = row.get::<Option<i64>>(index + 2)?;
                if let Some(value) = *count {
                    *totals[index].get_or_insert(0) += value;
                }
            }
            groups.push((row.get::<String>(0)?, row.get::<String>(1)?, counts));
        }
        if groups.is_empty() {
            return Ok(empty.into());
        }
        let headers = [
            "PROVIDER",
            "MODEL",
            "REQUESTS",
            "INPUT",
            "OUTPUT",
            "TOTAL",
            "CACHE READ",
            "CACHE CREATE",
            "REASONING",
        ];
        // Keep core counters visible; optional counters appear when any value was reported.
        let columns: Vec<_> = (0..7)
            .filter(|&index| index < 4 || totals[index].is_some())
            .collect();
        let mut table = Table::new();
        table.load_style(UTF8_FULL_CONDENSED);
        table.set_header(
            headers[..2]
                .iter()
                .copied()
                .chain(columns.iter().map(|index| headers[index + 2])),
        );
        groups.push(("TOTAL".into(), String::new(), totals));
        for (provider, model, counts) in groups {
            let mut cells = vec![Cell::new(provider), Cell::new(model)];
            cells.extend(columns.iter().map(|&index| {
                Cell::new(counts[index].map_or_else(
                    || "-".into(),
                    |value| {
                        if index == 0 {
                            format_count(value)
                        } else {
                            format_tokens(value)
                        }
                    },
                ))
                .set_alignment(CellAlignment::Right)
            }));
            table.add_row(cells);
        }
        let period = match (start, end) {
            (Some(start), Some(end)) if start == end => start.to_string(),
            (Some(start), Some(end)) => format!("{start} to {end}"),
            _ => "all dates".into(),
        };
        Ok(format!(
            "Usage | UTC: {period} | Provider: {}\n{table}\nTokens: K/M/B/T = powers of 1,000; rounded to 2 decimals.\nSums use reported values only; - = not reported.\nTOTAL uses upstream totals; cache/reasoning are not added again.",
            self.name.as_deref().unwrap_or("all"),
        ))
    }
}

fn format_tokens(value: i64) -> String {
    if value.unsigned_abs() < 1_000 {
        return value.to_string();
    }
    let units = ["", "K", "M", "B", "T"];
    let mut amount = value as f64;
    let mut unit = 0;
    // Promote values that would otherwise round to 1000 of the smaller unit.
    while amount.abs() >= 999.995 && unit < units.len() - 1 {
        amount /= 1_000.0;
        unit += 1;
    }
    let number = format!("{amount:.2}");
    format!(
        "{}{}",
        number.trim_end_matches('0').trim_end_matches('.'),
        units[unit]
    )
}

fn format_count(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut formatted = String::new();
    if value < 0 {
        formatted.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

pub(crate) struct Store {
    database: Arc<Database>,
    writer: OnceLock<mpsc::Sender<Write>>,
}

struct Database {
    path: PathBuf,
    connection: OnceCell<Mutex<turso::Connection>>,
}

enum Write {
    Usage(Snapshot),
    #[cfg(test)]
    Flush(tokio::sync::oneshot::Sender<()>),
}

#[derive(Clone)]
struct Snapshot {
    provider: String,
    model: Option<String>,
    id: String,
    counts: [Option<i64>; 6],
}

impl Store {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            database: Arc::new(Database {
                path,
                connection: OnceCell::new(),
            }),
            writer: OnceLock::new(),
        }
    }

    fn record(self: &Arc<Self>, provider: &str, model: Option<String>) -> Record {
        Record {
            store: self.clone(),
            snapshot: Snapshot {
                provider: provider.into(),
                model,
                id: uuid::Uuid::new_v4().to_string(),
                counts: [None; 6],
            },
            interaction: false,
        }
    }

    fn enqueue(&self, snapshot: Snapshot) {
        let writer = self.writer.get_or_init(|| {
            let (sender, mut receiver) = mpsc::channel(1024);
            let database = self.database.clone();
            tokio::spawn(async move {
                while let Some(write) = receiver.recv().await {
                    match write {
                        Write::Usage(snapshot) => {
                            if let Err(error) = database.write(snapshot).await {
                                tracing::warn!(%error, "could not record token usage");
                            }
                        }
                        #[cfg(test)]
                        Write::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
            sender
        });
        if let Err(error) = writer.try_send(Write::Usage(snapshot)) {
            tracing::warn!(%error, "token usage queue unavailable; dropping snapshot");
        }
    }

    #[cfg(test)]
    pub(crate) async fn flush(&self) {
        if let Some(writer) = self.writer.get() {
            let (done, received) = tokio::sync::oneshot::channel();
            writer.send(Write::Flush(done)).await.unwrap();
            received.await.unwrap();
        }
    }
}

impl Database {
    async fn write(&self, snapshot: Snapshot) -> turso::Result<()> {
        let conn = self.connection().await?.lock().await;
        let [input, output, total, cached, created, reasoning] = snapshot.counts;
        conn.execute("INSERT INTO token_usage
                (id, provider, model, input_tokens, output_tokens, total_tokens, cache_read_tokens, cache_creation_tokens, reasoning_tokens)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(id) DO UPDATE SET
                model=coalesce(excluded.model, model),
                input_tokens=CASE WHEN input_tokens IS NULL THEN excluded.input_tokens WHEN excluded.input_tokens IS NULL THEN input_tokens ELSE max(input_tokens,excluded.input_tokens) END,
                output_tokens=CASE WHEN output_tokens IS NULL THEN excluded.output_tokens WHEN excluded.output_tokens IS NULL THEN output_tokens ELSE max(output_tokens,excluded.output_tokens) END,
                total_tokens=CASE WHEN total_tokens IS NULL THEN excluded.total_tokens WHEN excluded.total_tokens IS NULL THEN total_tokens ELSE max(total_tokens,excluded.total_tokens) END,
                cache_read_tokens=CASE WHEN cache_read_tokens IS NULL THEN excluded.cache_read_tokens WHEN excluded.cache_read_tokens IS NULL THEN cache_read_tokens ELSE max(cache_read_tokens,excluded.cache_read_tokens) END,
                cache_creation_tokens=CASE WHEN cache_creation_tokens IS NULL THEN excluded.cache_creation_tokens WHEN excluded.cache_creation_tokens IS NULL THEN cache_creation_tokens ELSE max(cache_creation_tokens,excluded.cache_creation_tokens) END,
                reasoning_tokens=CASE WHEN reasoning_tokens IS NULL THEN excluded.reasoning_tokens WHEN excluded.reasoning_tokens IS NULL THEN reasoning_tokens ELSE max(reasoning_tokens,excluded.reasoning_tokens) END",
                turso::params![snapshot.id.as_str(), snapshot.provider.as_str(), snapshot.model.as_deref(), input, output, total, cached, created, reasoning]).await?;
        Ok(())
    }

    async fn connection(&self) -> turso::Result<&Mutex<turso::Connection>> {
        self.connection
            .get_or_try_init(|| async {
                if let Some(parent) = self.path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| turso::Error::Error(e.to_string()))?;
                }
                let path = self.path.to_str().ok_or_else(|| {
                    turso::Error::Error("usage database path is not UTF-8".into())
                })?;
                let db = turso::Builder::new_local(path)
                    .experimental_multiprocess_wal(true)
                    .build()
                    .await?;
                let conn = db.connect()?;
                conn.busy_timeout(std::time::Duration::from_secs(5))?;
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS token_usage (
                id TEXT PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                provider TEXT NOT NULL, model TEXT, input_tokens INTEGER, output_tokens INTEGER,
                total_tokens INTEGER, cache_read_tokens INTEGER, cache_creation_tokens INTEGER,
                reasoning_tokens INTEGER)",
                    (),
                )
                .await?;
                Ok(Mutex::new(conn))
            })
            .await
    }
}

struct Record {
    store: Arc<Store>,
    snapshot: Snapshot,
    interaction: bool,
}

impl Record {
    fn observe(&mut self, frame: &Value) {
        let payload = match frame["type"].as_str().or(frame["event_type"].as_str()) {
            Some("message_start") => &frame["message"],
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                &frame["response"]
            }
            Some("interaction.created" | "interaction.completed" | "interaction.status_update") => {
                &frame["interaction"]
            }
            _ => frame,
        };
        if let Some(model) = payload["model"]
            .as_str()
            .or(payload["modelVersion"].as_str())
        {
            self.snapshot.model = Some(model.to_owned());
        }
        if self.interaction {
            if let Some(id) = payload["id"].as_str() {
                self.snapshot.id = format!(
                    "interaction:{}",
                    serde_json::json!([self.snapshot.provider, id])
                );
            } else if !self.snapshot.id.starts_with("interaction:") {
                return;
            }
        }
        let Some(usage) = payload
            .get("usage")
            .or_else(|| payload.get("usageMetadata"))
        else {
            return;
        };
        let count = |keys: &[&str]| keys.iter().find_map(|key| usage[key].as_i64());
        let input = count(&[
            "input_tokens",
            "prompt_tokens",
            "promptTokenCount",
            "total_input_tokens",
        ]);
        let output = count(&[
            "output_tokens",
            "completion_tokens",
            "candidatesTokenCount",
            "total_output_tokens",
        ]);
        let total = count(&["total_tokens", "totalTokenCount"]);
        let cached = count(&[
            "cache_read_input_tokens",
            "cachedContentTokenCount",
            "total_cached_tokens",
        ])
        .or_else(|| {
            usage
                .pointer("/input_tokens_details/cached_tokens")?
                .as_i64()
        })
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")?
                .as_i64()
        });
        let created = count(&["cache_creation_input_tokens"]);
        let reasoning = count(&["thoughtsTokenCount", "total_thought_tokens"])
            .or_else(|| {
                usage
                    .pointer("/output_tokens_details/reasoning_tokens")?
                    .as_i64()
            })
            .or_else(|| {
                usage
                    .pointer("/completion_tokens_details/reasoning_tokens")?
                    .as_i64()
            });
        if [input, output, total, cached, created, reasoning]
            .iter()
            .all(Option::is_none)
        {
            return;
        }
        for (saved, incoming) in self
            .snapshot
            .counts
            .iter_mut()
            .zip([input, output, total, cached, created, reasoning])
        {
            *saved = (*saved).max(incoming);
        }
        self.store.enqueue(self.snapshot.clone());
    }

    fn parse(&mut self, bytes: &[u8]) {
        if let Ok(value) = serde_json::from_slice(bytes) {
            self.observe(&value);
        }
    }
}

pub(crate) fn websocket(store: &Option<Arc<Store>>, provider: &str, bytes: &[u8]) {
    let Some(store) = store else { return };
    if let Ok(value) = serde_json::from_slice::<Value>(bytes)
        && matches!(
            value["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed")
        )
    {
        store.record(provider, None).observe(&value);
    }
}

pub(crate) fn observe(
    store: &Option<Arc<Store>>,
    provider: &str,
    request: &[u8],
    upstream: reqwest::Response,
) -> reqwest::Response {
    observe_response(store, provider, request, upstream, false)
}

pub(crate) fn observe_interaction(
    store: &Option<Arc<Store>>,
    provider: &str,
    request: &[u8],
    upstream: reqwest::Response,
) -> reqwest::Response {
    observe_response(store, provider, request, upstream, true)
}

fn observe_response(
    store: &Option<Arc<Store>>,
    provider: &str,
    request: &[u8],
    upstream: reqwest::Response,
    interaction: bool,
) -> reqwest::Response {
    let Some(store) = store else { return upstream };
    if !upstream.status().is_success() {
        return upstream;
    }
    #[derive(Deserialize)]
    struct RequestedModel {
        model: String,
    }
    let model = serde_json::from_slice::<RequestedModel>(request)
        .ok()
        .map(|r| r.model);
    let mut record = store.record(provider, model);
    record.interaction = interaction;
    let sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let length = upstream.content_length();
    let response: Response<reqwest::Body> = upstream.into();
    let (parts, body) = response.into_parts();
    let mut source = axum::body::Body::new(body).into_data_stream();
    let stream = async_stream::stream! {
        let mut decoder = sse.then(SseDecoder::default);
        let mut json = (!sse && length.is_none_or(|n| n <= MAX_JSON_BODY_BYTES as u64)).then(Vec::new);
        while let Some(chunk) = source.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => { yield Err(error); return; }
            };
            if let Some(parser) = &mut decoder {
                match parser.push(&chunk) {
                    Ok(events) => {
                        for event in events {
                            record.parse(event.data.as_bytes());
                        }
                    }
                    Err(_) => decoder = None,
                }
            } else if let Some(buf) = &mut json {
                if chunk.len() > MAX_JSON_BODY_BYTES - buf.len() {
                    json = None;
                } else {
                    buf.extend_from_slice(&chunk);
                    if length == Some(buf.len() as u64) {
                        record.parse(buf);
                        json = None;
                    }
                }
            }
            yield Ok(chunk);
        }
        if let Some(mut parser) = decoder && let Ok(events) = parser.finish() {
            for event in events {
                record.parse(event.data.as_bytes());
            }
        }
        if let Some(buf) = json {
            record.parse(&buf);
        }
    };
    reqwest::Response::from(Response::from_parts(
        parts,
        reqwest::Body::wrap_stream(stream),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn interactions_merge_polls_and_resumed_streams_without_regression() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().join("usage.db")));
        for (provider, tokens) in [("gemini", 12), ("gemini", 12), ("gemini", 3), ("other", 4)] {
            let body =
                json!({"id":"interaction_1","model":"gemini","usage":{"total_tokens":tokens}});
            let upstream = Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .unwrap()
                .into();
            observe_interaction(&Some(store.clone()), provider, b"{}", upstream)
                .text()
                .await
                .unwrap();
        }
        let event = json!({"event_type":"interaction.completed","interaction":{"id":"interaction_1","usage":{"total_tokens":15}}});
        let upstream = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(format!("data: {event}\n\n"))
            .unwrap()
            .into();
        observe_interaction(&Some(store.clone()), "gemini", b"{}", upstream)
            .text()
            .await
            .unwrap();
        store.flush().await;
        let connection = store.database.connection().await.unwrap().lock().await;
        let mut rows = connection
            .query(
                "SELECT provider,total_tokens FROM token_usage ORDER BY provider",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "gemini");
        assert_eq!(row.get::<i64>(1).unwrap(), 15);
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(1).unwrap(),
            4
        );
        assert!(rows.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn locked_writer_does_not_delay_forwarding() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().join("usage.db")));
        let guard = store.database.connection().await.unwrap().lock().await;
        let body = r#"{"usage":{"prompt_tokens":8}}"#;
        let upstream = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
            .into();
        let reply = observe(&Some(store.clone()), "test", b"{}", upstream);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(200), reply.text())
                .await
                .unwrap()
                .unwrap(),
            body
        );
        drop(guard);
        store.flush().await;
        let connection = store.database.connection().await.unwrap().lock().await;
        let mut rows = connection
            .query("SELECT input_tokens FROM token_usage", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            8
        );
    }

    #[tokio::test]
    async fn full_queue_does_not_block_sse_forwarding() {
        let store = Arc::new(Store::new(PathBuf::from("unused.db")));
        let (sender, mut receiver) = mpsc::channel(1);
        assert!(store.writer.set(sender).is_ok());
        let body = "data: {\"usage\":{\"prompt_tokens\":8}}\n\ndata: {\"usage\":{\"completion_tokens\":3}}\n\n";
        let upstream = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(body)
            .unwrap()
            .into();
        let reply = observe(&Some(store.clone()), "test", b"{}", upstream);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(200), reply.text())
                .await
                .unwrap()
                .unwrap(),
            body
        );
        assert!(matches!(receiver.try_recv().unwrap(), Write::Usage(_)));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn report_summarizes_reported_counts_without_inventing_missing_usage() {
        use clap::Parser;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let store = Store::new(path.clone());
        store.database.connection().await.unwrap().lock().await.execute(
            "INSERT INTO token_usage (id,provider,model,input_tokens,output_tokens,total_tokens,cache_read_tokens) VALUES
             ('1','first','alpha',12000,345,12345,0),
             ('2','second','beta',3000,NULL,NULL,NULL)", (),
        ).await.unwrap();
        let crate::Command::Usage(args) = crate::Cmd::parse_from(["llmapi", "usage"]).command
        else {
            unreachable!()
        };
        let report = args.report(&path).await.unwrap();
        assert!(report.contains("UTC: all dates"));
        assert!(report.contains("CACHE READ"));
        assert!(!report.contains("CACHE CREATE") && !report.contains("REASONING"));
        let total = report
            .lines()
            .find(|line| {
                line.split(['│', '┆'])
                    .nth(1)
                    .is_some_and(|cell| cell.trim() == "TOTAL")
            })
            .unwrap();
        let cells: Vec<_> = total
            .split(['│', '┆'])
            .skip(1)
            .take(7)
            .map(str::trim)
            .collect();
        assert_eq!(cells, ["TOTAL", "", "2", "15K", "345", "12.35K", "0"]);
        let beta = report.lines().find(|line| line.contains("beta")).unwrap();
        assert!(beta.contains("3K"));
        assert_eq!(beta.matches('-').count(), 3);
        assert!(report.contains("reported values only"));
    }

    #[test]
    fn token_display_uses_decimal_units_and_promotes_rounded_boundaries() {
        for (tokens, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (12_345, "12.35K"),
            (999_999, "1M"),
            (1_200_000, "1.2M"),
            (2_500_000_000, "2.5B"),
            (1_000_000_000_000, "1T"),
        ] {
            assert_eq!(format_tokens(tokens), expected);
        }
        assert_eq!(format_count(12_345), "12,345");
    }

    #[tokio::test]
    async fn report_filters_provider_and_inclusive_utc_dates() {
        use clap::Parser;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let store = Store::new(path.clone());
        store
            .database
            .connection()
            .await
            .unwrap()
            .lock()
            .await
            .execute(
                "INSERT INTO token_usage (id,created_at,provider,model,input_tokens) VALUES
             ('1','2026-01-01 00:00:00','first','alpha',4),
             ('2','2026-01-02 23:59:59','first','alpha',6),
             ('3','2026-01-03 00:00:00','first','later',1),
             ('4','2026-01-02 12:00:00','second','beta',3)",
                (),
            )
            .await
            .unwrap();
        for (flags, expected) in [
            (
                vec![
                    "--name",
                    "first",
                    "--date-range",
                    "2026-01-01",
                    "2026-01-02",
                ],
                "10",
            ),
            (vec!["--date", "2026-01-02", "--name", "first"], "6"),
        ] {
            let cmd = crate::Cmd::parse_from(["llmapi", "usage"].into_iter().chain(flags));
            let crate::Command::Usage(args) = cmd.command else {
                unreachable!()
            };
            let report = args.report(&path).await.unwrap();
            assert!(report.contains("alpha") && report.contains(expected));
            assert!(!report.contains("later") && !report.contains("beta"));
        }
    }

    #[tokio::test]
    async fn streamed_snapshots_merge_into_one_persistent_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let store = Arc::new(Store::new(path.clone()));
        let start = json!({"type":"message_start","message":{"model":"sonnet","usage":{"input_tokens":3,"cache_read_input_tokens":7,"output_tokens":1}}});
        let delta = json!({"type":"message_delta","usage":{"output_tokens":4}});
        let body = format!("data: {start}\n\ndata: {delta}\n\ndata: {delta}\n\n");
        let chunks = body
            .as_bytes()
            .chunks(11)
            .map(bytes::Bytes::copy_from_slice)
            .map(Ok::<_, std::io::Error>)
            .collect::<Vec<_>>();
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
                chunks,
            )))
            .unwrap()
            .into();
        let response = observe(&Some(store.clone()), "claude", b"{}", response);
        assert_eq!(response.text().await.unwrap(), body);
        store.flush().await;
        drop(store);
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query("SELECT provider, model, input_tokens, output_tokens, cache_read_tokens FROM token_usage", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "claude");
        assert_eq!(row.get::<String>(1).unwrap(), "sonnet");
        assert_eq!(row.get::<i64>(2).unwrap(), 3);
        assert_eq!(row.get::<i64>(3).unwrap(), 4);
        assert_eq!(row.get::<i64>(4).unwrap(), 7);
        assert!(rows.next().await.unwrap().is_none());
    }
}
