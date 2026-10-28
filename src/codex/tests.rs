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
