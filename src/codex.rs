use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use axum::http::{header, HeaderMap, HeaderValue};
use serde_json::{json, Value};
use tokio::sync::watch;
use tracing::warn;

use crate::adapters::AdapterError;

pub(crate) const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
// Protocol version used for the Codex model catalog and backend client headers.
pub(crate) const CLIENT_VERSION: &str = "0.162.1";

pub(crate) fn auth_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex/auth.json"))
}

fn parse_auth(body: &[u8]) -> io::Result<Option<HeaderMap>> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Codex auth JSON"))?;
    if value["auth_mode"] != "chatgpt" {
        return Ok(None);
    }
    let Some(token) = value["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let mut headers = HeaderMap::new();
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Codex access token header",
        )
    })?;
    authorization.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, authorization);
    if let Some(account) = value["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        let mut account = HeaderValue::from_str(account).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid Codex account header")
        })?;
        account.set_sensitive(true);
        headers.insert("chatgpt-account-id", account);
    }
    Ok(Some(headers))
}

pub(crate) fn read_auth(path: &Path) -> io::Result<Option<HeaderMap>> {
    match std::fs::read(path) {
        Ok(body) => parse_auth(&body),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn available_auth(result: io::Result<Option<HeaderMap>>) -> Option<HeaderMap> {
    result.unwrap_or_else(|err| {
        warn!(target: "llmapi", error = %err, "Codex auth unavailable; skipping automatic provider");
        None
    })
}

pub(crate) fn available() -> bool {
    auth_path()
        .and_then(|path| available_auth(read_auth(&path)))
        .is_some()
}

pub(crate) fn watch_auth(path: Option<PathBuf>) -> watch::Receiver<Option<HeaderMap>> {
    let initial = path
        .as_deref()
        .and_then(|path| available_auth(read_auth(path)));
    let (sender, receiver) = watch::channel(initial);
    if let Some(path) = path {
        let next_poll = tokio::time::Instant::now() + POLL_INTERVAL;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(next_poll, POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = sender.closed() => return,
                    _ = interval.tick() => {}
                }
                let result = match tokio::fs::read(&path).await {
                    Ok(body) => parse_auth(&body),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err),
                };
                let auth = available_auth(result);
                sender.send_if_modified(|current| {
                    if *current == auth {
                        return false;
                    }
                    *current = auth;
                    true
                });
            }
        });
    }
    receiver
}

pub(crate) fn apply_headers(headers: &mut HeaderMap, auth: &HeaderMap, stream: bool) {
    headers.extend(auth.clone());
    headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    headers.insert("version", HeaderValue::from_static(CLIENT_VERSION));
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("codex_cli_rs/0.162.1 (llmapi)"),
    );
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses=experimental"),
    );
    if stream {
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
    }
}

pub(crate) fn prepare_request(value: &mut Value) -> Result<(), AdapterError> {
    let request = value
        .as_object_mut()
        .ok_or(AdapterError::InvalidField("request"))?;
    if !request.get("model").is_some_and(Value::is_string) {
        return Err(AdapterError::MissingField("model"));
    }
    for field in ["previous_response_id", "conversation"] {
        if request.get(field).is_some_and(|value| !value.is_null()) {
            return Err(AdapterError::Unsupported(format!(
                "codex does not support {field}; send the full conversation in input"
            )));
        }
        request.remove(field);
    }
    for field in [
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "stop",
        "user",
        "safety_identifier",
        "prompt_cache_options",
    ] {
        request.remove(field);
    }
    if request
        .get("service_tier")
        .is_some_and(|tier| tier != "priority" && tier != "ultrafast")
    {
        request.remove("service_tier");
    }
    let input = request.entry("input").or_insert_with(|| json!([]));
    if let Some(text) = input.as_str() {
        *input = json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]);
    }
    let input = input
        .as_array_mut()
        .ok_or(AdapterError::InvalidField("input"))?;
    for item in input {
        let item = item
            .as_object_mut()
            .ok_or(AdapterError::InvalidField("input item"))?;
        if item
            .get("type")
            .is_some_and(|kind| kind == "item_reference")
        {
            return Err(AdapterError::Unsupported(
                "codex does not support stored item references; send the full input item".into(),
            ));
        }
        item.remove("id");
        if matches!(
            item.get("type").and_then(Value::as_str),
            None | Some("message" | "reasoning")
        ) {
            item.remove("status");
        }
        if item.get("role").is_some_and(|role| role == "system") {
            item.insert("role".into(), json!("developer"));
        }
    }
    request.insert("store".into(), json!(false));
    request.insert("stream".into(), json!(true));
    request.entry("instructions").or_insert_with(|| json!(""));
    request
        .entry("tool_choice")
        .or_insert_with(|| json!("auto"));
    if request
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .is_none_or(|effort| effort != "none")
    {
        let include = request
            .entry("include")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(AdapterError::InvalidField("include"))?;
        if !include
            .iter()
            .any(|item| item == "reasoning.encrypted_content")
        {
            include.push(json!("reasoning.encrypted_content"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
