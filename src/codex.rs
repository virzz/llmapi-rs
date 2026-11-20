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
pub(crate) const CLIENT_VERSION: &str = "0.171.0";

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
        HeaderValue::from_static("codex_cli_rs/0.171.0"),
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
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn auth_file(token: &str) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": "ignored-api-key",
            "tokens": {"access_token": token, "account_id": "account-1", "refresh_token": "never-used"}
        })
    }

    #[test]
    fn discovers_only_usable_chatgpt_credentials_without_writing_them() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert!(read_auth(&path).unwrap().is_none());
        for value in [
            json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test"}),
            json!({"auth_mode": "chatgpt", "tokens": {"access_token": ""}}),
        ] {
            std::fs::write(&path, value.to_string()).unwrap();
            assert!(read_auth(&path).unwrap().is_none());
        }
        let original = auth_file("access-1").to_string();
        std::fs::write(&path, &original).unwrap();
        let headers = read_auth(&path).unwrap().unwrap();
        assert_eq!(headers[header::AUTHORIZATION], "Bearer access-1");
        assert!(headers[header::AUTHORIZATION].is_sensitive());
        assert_eq!(headers["chatgpt-account-id"], "account-1");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::write(&path, "{").unwrap();
        assert!(read_auth(&path).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn polls_every_five_minutes_and_tracks_login_rotation_and_logout() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut receiver = watch_auth(Some(path.clone()));
        assert!(receiver.borrow().is_none());
        std::fs::write(&path, auth_file("first").to_string()).unwrap();
        tokio::time::advance(Duration::from_secs(299)).await;
        tokio::task::yield_now().await;
        assert!(!receiver.has_changed().unwrap());
        tokio::time::advance(Duration::from_secs(1)).await;
        receiver.changed().await.unwrap();
        assert_eq!(
            receiver.borrow().as_ref().unwrap()[header::AUTHORIZATION],
            "Bearer first"
        );

        std::fs::write(&path, auth_file("second").to_string()).unwrap();
        tokio::time::advance(Duration::from_secs(299)).await;
        assert_eq!(
            receiver.borrow().as_ref().unwrap()[header::AUTHORIZATION],
            "Bearer first"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        receiver.changed().await.unwrap();
        assert_eq!(
            receiver.borrow().as_ref().unwrap()[header::AUTHORIZATION],
            "Bearer second"
        );

        std::fs::write(&path, json!({"auth_mode": "apikey"}).to_string()).unwrap();
        tokio::time::advance(POLL_INTERVAL).await;
        receiver.changed().await.unwrap();
        assert!(receiver.borrow().is_none());
        std::fs::write(&path, auth_file("third").to_string()).unwrap();
        tokio::time::advance(POLL_INTERVAL).await;
        receiver.changed().await.unwrap();
        assert!(receiver.borrow().is_some());
        std::fs::remove_file(&path).unwrap();
        tokio::time::advance(POLL_INTERVAL).await;
        receiver.changed().await.unwrap();
        assert!(receiver.borrow().is_none());
    }

    #[test]
    fn normalizes_codex_requests_without_losing_tools_or_reasoning() {
        let mut request = json!({
            "model": "gpt-5", "stream": false, "store": true,
            "instructions": "Follow the user's instructions",
            "input": [
                {"type": "message", "role": "system", "content": "rules", "id": "msg-old", "status": "completed"},
                {"type": "reasoning", "id": "rs-old", "encrypted_content": "opaque"},
                {"type": "function_call", "id": "fc-old", "call_id": "call-1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call-1", "output": "found"}
            ],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "tool_choice": "required", "max_output_tokens": 32, "temperature": 0.2, "top_p": 0.8,
            "include": ["web_search_call.action.sources"]
        });
        prepare_request(&mut request).unwrap();
        assert_eq!(request["stream"], true);
        assert_eq!(request["store"], false);
        assert_eq!(request["instructions"], "Follow the user's instructions");
        assert_eq!(request["input"][0]["role"], "developer");
        assert!(request["input"][0].get("id").is_none());
        assert!(request["input"][0].get("status").is_none());
        assert_eq!(request["input"][1]["encrypted_content"], "opaque");
        assert_eq!(
            request["input"][2]["call_id"],
            request["input"][3]["call_id"]
        );
        assert_eq!(request["tools"][0]["name"], "lookup");
        assert_eq!(request["tool_choice"], "required");
        for field in ["max_output_tokens", "temperature", "top_p"] {
            assert!(request.get(field).is_none());
        }
        assert!(request["include"]
            .as_array()
            .unwrap()
            .contains(&json!("reasoning.encrypted_content")));
        assert!(request["include"]
            .as_array()
            .unwrap()
            .contains(&json!("web_search_call.action.sources")));
        let mut simple = json!({"model": "gpt-5", "input": "hello"});
        prepare_request(&mut simple).unwrap();
        assert_eq!(simple["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn rejects_stored_conversations_instead_of_dropping_context() {
        for extra in [
            json!({"previous_response_id": "resp-old"}),
            json!({"conversation": "conv-old"}),
            json!({"input": [{"type": "item_reference", "id": "msg-old"}]}),
        ] {
            let mut request = json!({"model": "gpt-5", "input": "hello"});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(matches!(
                prepare_request(&mut request),
                Err(AdapterError::Unsupported(_))
            ));
        }
    }
}
