use super::*;

async fn start_codex_proxy(config: Config, auth: watch::Receiver<Option<HeaderMap>>) -> SocketAddr {
    let (_, config) = watch::channel(Arc::new(Runtime::new(config)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app_with_auth(config, auth))
            .await
            .unwrap();
    });
    address
}

fn credentials() -> HeaderMap {
    HeaderMap::from_iter([
        (
            axum::http::header::AUTHORIZATION,
            "Bearer local-token".parse().unwrap(),
        ),
        (
            axum::http::HeaderName::from_static("chatgpt-account-id"),
            "account-local".parse().unwrap(),
        ),
    ])
}

fn codex_config(upstream: String) -> Config {
    let mut config = Config::default().with_codex(true);
    config
        .providers
        .get_mut("codex")
        .unwrap()
        .as_upstream_mut()
        .unwrap()
        .base_url = upstream;
    config
}

fn completed_response() -> Value {
    json!({"id": "resp-codex", "object": "response", "status": "completed", "model": "gpt-5",
        "output": [{"type": "message", "id": "msg-1", "role": "assistant", "content": [{"type": "output_text", "text": "hello", "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}, "custom_field": "preserved"})
}

#[tokio::test]
async fn automatic_provider_follows_auth_and_never_exposes_credentials() {
    let (sender, receiver) = watch::channel(None);
    let address = start_codex_proxy(Config::default(), receiver).await;
    let url = format!("http://{address}/providers");
    let value: Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert_eq!(value["providers"], json!({}));
    sender.send_replace(Some(credentials()));
    let value: Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert_eq!(value["default"], "codex");
    assert_eq!(value["providers"]["codex"]["type"], "codex");
    assert!(!value.to_string().contains("local-token"));
    assert!(!value.to_string().contains("account-local"));
    sender.send_replace(None);
    let response = reqwest::Client::new()
        .post(format!("http://{address}/codex/responses"))
        .json(&json!({"model": "gpt-5", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn codex_aggregates_sse_for_all_nonstreaming_protocols() {
    for (path, body) in [
        (
            "responses",
            json!({"model": "gpt-5", "input": "hi", "prompt_cache_key": "conversation-1"}),
        ),
        (
            "chat/completions",
            json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "messages",
            json!({"model": "gpt-5", "max_tokens": 20, "messages": [{"role": "user", "content": "hi"}]}),
        ),
    ] {
        let event = format!(
            "data: {}\n\n",
            json!({"type": "response.completed", "response": completed_response()})
        );
        let (upstream, captured) = start_stream_upstream(vec![
            event.as_bytes()[..17].to_vec(),
            event.as_bytes()[17..].to_vec(),
        ])
        .await;
        let (_, auth) = watch::channel(Some(credentials()));
        let address = start_codex_proxy(codex_config(upstream), auth).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/codex/{path}"))
            .bearer_auth("client-secret")
            .header("chatgpt-account-id", "client-account")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        let value: Value = response.json().await.unwrap();
        match path {
            "responses" => assert_eq!(value, completed_response()),
            "chat/completions" => {
                assert_eq!(value["choices"][0]["message"]["content"], "hello");
                assert_eq!(value["usage"]["total_tokens"], 4);
            }
            _ => assert_eq!(value["content"][0]["text"], "hello"),
        }
        let request = captured.lock().await.clone().unwrap();
        assert_eq!(request.uri.path(), "/responses");
        assert_eq!(request.headers["authorization"], "Bearer local-token");
        assert_eq!(request.headers["chatgpt-account-id"], "account-local");
        assert_eq!(request.headers["accept"], "text/event-stream");
        if path == "responses" {
            assert_eq!(request.headers["session-id"], "conversation-1");
            assert_eq!(request.headers["thread-id"], "conversation-1");
        }
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["store"], false);
        assert!(request.body.get("max_output_tokens").is_none());
    }
}

#[tokio::test]
async fn codex_nonstreaming_errors_on_failed_or_unfinished_streams() {
    for event in [
        json!({"type": "response.created", "response": {"id": "resp-1"}}),
        json!({"type": "response.failed", "response": {"status": "failed", "error": {"message": "backend failed"}}}),
    ] {
        let (upstream, _) =
            start_stream_upstream(vec![format!("data: {event}\n\n").into_bytes()]).await;
        let (_, auth) = watch::channel(Some(credentials()));
        let address = start_codex_proxy(codex_config(upstream), auth).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/responses"))
            .json(&json!({"model": "gpt-5", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let value: Value = response.json().await.unwrap();
        assert!(value["error"]["message"].is_string());
    }
}

#[tokio::test]
async fn codex_models_use_local_credentials_and_client_version() {
    let (upstream, captured) =
        start_upstream(json!({"models": [{"slug": "gpt-5", "display_name": "GPT-5"}]})).await;
    let (_, auth) = watch::channel(Some(credentials()));
    let address = start_codex_proxy(codex_config(upstream), auth).await;
    let response: Value = reqwest::get(format!("http://{address}/codex/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["data"][0]["id"], "gpt-5");
    let request = captured.lock().await.clone().unwrap();
    assert_eq!(request.headers["authorization"], "Bearer local-token");
    assert!(request.uri.query().unwrap().contains("client_version="));
}

#[tokio::test]
async fn codex_streams_native_events_and_converts_chat_and_messages() {
    let events = [
        json!({"type": "response.created", "response": {"id": "resp-codex", "model": "gpt-5"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "msg-1", "type": "message", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hello"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "msg-1", "type": "message"}}),
        json!({"type": "response.completed", "response": completed_response()}),
    ];
    let stream: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    for (path, request) in [
        (
            "responses",
            json!({"model": "gpt-5", "input": "hi", "stream": true}),
        ),
        (
            "chat/completions",
            json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
        ),
        (
            "messages",
            json!({"model": "gpt-5", "max_tokens": 20, "messages": [{"role": "user", "content": "hi"}], "stream": true}),
        ),
    ] {
        let (upstream, _) = start_stream_upstream(vec![stream.as_bytes().to_vec()]).await;
        let (_, auth) = watch::channel(Some(credentials()));
        let address = start_codex_proxy(codex_config(upstream), auth).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/{path}"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let body = response.text().await.unwrap();
        let events = sse_json_events(&body);
        match path {
            "responses" => assert_eq!(body, stream),
            "chat/completions" => {
                assert!(body.ends_with("data: [DONE]\n\n"));
                assert!(events
                    .iter()
                    .any(|event| event["choices"][0]["delta"]["content"] == "hello"));
            }
            _ => {
                assert_eq!(events.last().unwrap()["type"], "message_stop");
                assert!(events.iter().any(|event| event["delta"]["text"] == "hello"));
            }
        }
    }
}

#[tokio::test]
async fn codex_keeps_tool_calls_and_partial_output_when_aggregating() {
    let response = json!({"id": "resp-tool", "model": "gpt-5", "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{"type": "function_call", "id": "fc-1", "call_id": "call-1", "name": "lookup", "arguments": "{\"q\":\"rust\"}"}],
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}});
    let (upstream, _) = start_stream_upstream(vec![format!(
        "data: {}\n\n",
        json!({"type": "response.incomplete", "response": response})
    )
    .into_bytes()])
    .await;
    let (_, auth) = watch::channel(Some(credentials()));
    let address = start_codex_proxy(codex_config(upstream), auth).await;
    let response: Value = reqwest::Client::new()
        .post(format!("http://{address}/chat/completions"))
        .json(&json!({"model": "gpt-5", "messages": [{"role": "user", "content": "lookup rust"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["choices"][0]["finish_reason"], "length");
    assert_eq!(
        response["choices"][0]["message"]["tool_calls"][0]["id"],
        "call-1"
    );
    assert_eq!(
        response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"q\":\"rust\"}"
    );
}

#[tokio::test]
async fn codex_returns_unauthorized_without_retrying_or_changing_cached_auth() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/{*path}",
                any(move || {
                    requests.fetch_add(1, Ordering::SeqCst);
                    async {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": {"message": "expired token"}})),
                        )
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (_, auth) = watch::channel(Some(credentials()));
    let cached = auth.clone();
    let address = start_codex_proxy(codex_config(upstream), auth).await;
    let response = reqwest::Client::new()
        .post(format!("http://{address}/responses"))
        .json(&json!({"model": "gpt-5", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        cached.borrow().as_ref().unwrap()["authorization"],
        "Bearer local-token"
    );
}
