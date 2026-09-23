use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, Request, Response, Uri},
    response::IntoResponse,
    routing::{any, get},
    Json, Router,
};
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{config::Config, proxy, redact};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
}

#[derive(Clone)]
struct ServerState {
    config: watch::Receiver<Arc<Config>>,
    client: reqwest::Client,
}

pub fn app(config: Config) -> Router {
    let (_, config) = watch::channel(Arc::new(config));
    app_with_config(config)
}

fn app_with_config(config: watch::Receiver<Arc<Config>>) -> Router {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|err| {
            warn!(target: "llmapi", error = %err, "failed to configure upstream client");
            reqwest::Client::new()
        });
    let state = ServerState { config, client };

    Router::new()
        .route("/providers", get(providers))
        .route("/{*path}", any(handler))
        .with_state(state)
}

async fn providers(State(state): State<ServerState>) -> impl IntoResponse {
    let mut config = state.config.borrow().as_ref().clone();
    for provider in config.providers.values_mut() {
        if provider.api_key.is_some() {
            provider.api_key = Some("***".into());
        }
    }
    Json(json!({"default": config.default, "providers": config.providers}))
}

pub async fn serve(addr: SocketAddr, config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(target: "llmapi", address = %listener.local_addr()?, "server listening");
    axum::serve(listener, app(config)).await?;
    Ok(())
}

pub(crate) async fn serve_reloading(
    addr: SocketAddr,
    config: Config,
    path: PathBuf,
    server_override: bool,
    default_override: Option<String>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(target: "llmapi", address = %listener.local_addr()?, "server listening");
    let listen_address = config.server.clone();
    let (sender, receiver) = watch::channel(Arc::new(config));
    let watcher = tokio::spawn(watch_config(
        path,
        listen_address,
        server_override,
        default_override,
        sender,
    ));
    let result = axum::serve(listener, app_with_config(receiver)).await;
    watcher.abort();
    result?;
    Ok(())
}

async fn watch_config(
    path: PathBuf,
    listen_address: String,
    server_override: bool,
    default_override: Option<String>,
    sender: watch::Sender<Arc<Config>>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_body = None;
    let mut last_error: Option<String> = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = sender.closed() => return,
        }
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(body) => body,
            Err(err) => {
                let message = err.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    warn!(target: "llmapi", path = %path.display(), error = %err, "config reload failed; keeping previous config");
                    last_error = Some(message);
                }
                last_body = None;
                continue;
            }
        };
        if last_body.as_ref() == Some(&body) {
            continue;
        }
        let config = Config::parse(&path, &body).and_then(|mut config| {
                if let Some(default) = &default_override {
                    config.set_default(default)?;
                }
                if config.server != listen_address {
                    if !server_override {
                        warn!(target: "llmapi", path = %path.display(), "listen address change requires restart");
                    }
                    config.server = listen_address.clone();
                }
                Ok(config)
            });
        last_body = Some(body);
        match config {
            Ok(config) => {
                last_error = None;
                if **sender.borrow() != config {
                    sender.send_replace(Arc::new(config));
                    info!(target: "llmapi", path = %path.display(), "config reloaded");
                }
            }
            Err(err) => {
                let message = err.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    warn!(target: "llmapi", path = %path.display(), error = %err, "config reload failed; keeping previous config");
                    last_error = Some(message);
                }
            }
        }
    }
}

fn handler(
    State(state): State<ServerState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> impl std::future::Future<Output = Response<Body>> + Send {
    info!(
        target: "llmapi",
        request = %redact::request(method.as_str(), &uri),
        "request received"
    );
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .expect("request builder with existing uri");

    let snapshot = AppState {
        config: state.config.borrow().clone(),
        client: state.client.clone(),
    };
    proxy::handle(snapshot, headers, request)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, convert::Infallible, fs, sync::Arc};

    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, Method, StatusCode},
        response::IntoResponse,
        routing::any,
        Json, Router,
    };
    use serde_json::{json, Value};
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::config::{Config, Provider, ProviderConfig};

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Value,
    }

    #[derive(Clone)]
    struct RecordedRequest {
        request: Arc<Mutex<Option<CapturedRequest>>>,
        response: Value,
    }

    #[derive(Clone)]
    struct RecordedStream {
        request: Arc<Mutex<Option<CapturedRequest>>>,
        chunks: Vec<bytes::Bytes>,
    }

    #[derive(Clone)]
    struct RecordedError {
        request: Arc<Mutex<Option<CapturedRequest>>>,
        status: StatusCode,
        response: Value,
    }

    async fn record_request(
        State(recorded): State<RecordedRequest>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        *recorded.request.lock().await = Some(CapturedRequest {
            method,
            uri,
            headers,
            body,
        });
        Json(recorded.response)
    }

    async fn start_upstream(response: Value) -> (String, Arc<Mutex<Option<CapturedRequest>>>) {
        let request = Arc::new(Mutex::new(None));
        let recorded = RecordedRequest {
            request: request.clone(),
            response,
        };
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                upstream_listener,
                Router::new()
                    .route("/{*path}", any(record_request))
                    .with_state(recorded),
            )
            .await
            .unwrap();
        });
        (format!("http://{upstream_addr}"), request)
    }

    async fn record_stream(
        State(recorded): State<RecordedStream>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response<Body> {
        *recorded.request.lock().await = Some(CapturedRequest {
            method,
            uri,
            headers,
            body,
        });
        let chunks = recorded.chunks.into_iter().map(Ok::<_, Infallible>);
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap()
    }

    async fn start_stream_upstream(
        chunks: Vec<Vec<u8>>,
    ) -> (String, Arc<Mutex<Option<CapturedRequest>>>) {
        let request = Arc::new(Mutex::new(None));
        let recorded = RecordedStream {
            request: request.clone(),
            chunks: chunks.into_iter().map(bytes::Bytes::from).collect(),
        };
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                upstream_listener,
                Router::new()
                    .route("/{*path}", any(record_stream))
                    .with_state(recorded),
            )
            .await
            .unwrap();
        });
        (format!("http://{upstream_addr}"), request)
    }

    async fn record_error(
        State(recorded): State<RecordedError>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response<Body> {
        *recorded.request.lock().await = Some(CapturedRequest {
            method,
            uri,
            headers,
            body,
        });
        Response::builder()
            .status(recorded.status)
            .header("content-type", "application/json")
            .header("retry-after", "3")
            .header("x-request-id", "req_error")
            .body(Body::from(recorded.response.to_string()))
            .unwrap()
    }

    async fn start_error_upstream(status: StatusCode, response: Value) -> String {
        let recorded = RecordedError {
            request: Arc::new(Mutex::new(None)),
            status,
            response,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/{*path}", any(record_error))
                    .with_state(recorded),
            )
            .await
            .unwrap();
        });
        format!("http://{address}")
    }

    fn sse_json_events(body: &str) -> Vec<Value> {
        body.split("\n\n")
            .filter_map(|event| {
                let data = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .collect::<Vec<_>>()
                    .join("\n");
                (!data.is_empty() && data != "[DONE]").then(|| serde_json::from_str(&data).unwrap())
            })
            .collect()
    }

    fn test_config(base_url: &str) -> Config {
        Config {
            server: "127.0.0.1:0".into(),
            default: "deepseek".into(),
            providers: BTreeMap::from([
                (
                    "deepseek".into(),
                    ProviderConfig {
                        provider_type: Provider::OpenAiChat,
                        base_url: base_url.into(),
                        api_key: Some("sk-deepseek".into()),
                    },
                ),
                (
                    "openai".into(),
                    ProviderConfig {
                        provider_type: Provider::OpenAiResponses,
                        base_url: base_url.into(),
                        api_key: Some("sk-openai".into()),
                    },
                ),
                (
                    "anthropic".into(),
                    ProviderConfig {
                        provider_type: Provider::Anthropic,
                        base_url: base_url.into(),
                        api_key: Some("sk-anthropic".into()),
                    },
                ),
            ]),
        }
    }

    async fn start_proxy(config: Config) -> SocketAddr {
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(proxy_listener, app(config)).await.unwrap();
        });
        address
    }

    #[tokio::test]
    async fn reloads_valid_config_and_keeps_last_good_on_invalid_change() {
        let (first_upstream, _) = start_upstream(json!({
            "object": "list", "data": [{"id": "first", "object": "model"}]
        }))
        .await;
        let (second_upstream, _) = start_upstream(json!({
            "object": "list", "data": [{"id": "second", "object": "model"}]
        }))
        .await;
        let mut config = test_config(&first_upstream);
        config.providers.get_mut("openai").unwrap().base_url = second_upstream;
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        config.save(&path).unwrap();

        let (sender, mut receiver) = watch::channel(Arc::new(config.clone()));
        let watcher = tokio::spawn(watch_config(
            path.clone(),
            config.server.clone(),
            false,
            None,
            sender,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_receiver = receiver.clone();
        tokio::spawn(async move {
            axum::serve(listener, app_with_config(server_receiver))
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let list_models = || client.get(format!("http://{address}/models"));

        let first: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(first["data"][0]["id"], "first");

        config.default = "openai".into();
        config.server = "127.0.0.1:9090".into();
        config.save(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receiver.borrow().default, "openai");
        assert_eq!(receiver.borrow().server, "127.0.0.1:0");
        let second: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(second["data"][0]["id"], "second");

        fs::write(&path, "default: missing\nproviders: {}\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(800), receiver.changed())
                .await
                .is_err()
        );
        let still_second: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(still_second["data"][0]["id"], "second");

        fs::remove_file(&path).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(800), receiver.changed())
                .await
                .is_err()
        );
        let still_second: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(still_second["data"][0]["id"], "second");

        config.default = "deepseek".into();
        config.save(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        let first_again: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(first_again["data"][0]["id"], "first");
        watcher.abort();
    }

    #[tokio::test]
    async fn reload_preserves_cli_default_override() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut file_config = test_config("https://first.test");
        file_config.save(&path).unwrap();
        let mut active = file_config.clone();
        active.default = "openai".into();
        let (sender, mut receiver) = watch::channel(Arc::new(active));
        let watcher = tokio::spawn(watch_config(
            path.clone(),
            file_config.server.clone(),
            false,
            Some("openai".into()),
            sender,
        ));

        file_config.providers.get_mut("openai").unwrap().base_url = "https://second.test".into();
        file_config.save(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receiver.borrow().default, "openai");
        assert_eq!(
            receiver.borrow().providers["openai"].base_url,
            "https://second.test"
        );

        file_config.providers.remove("openai");
        file_config.save(&path).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(800), receiver.changed())
                .await
                .is_err()
        );
        assert!(receiver.borrow().providers.contains_key("openai"));
        watcher.abort();
    }

    #[tokio::test]
    async fn config_watcher_stops_when_server_drops() {
        let config = test_config("https://example.test");
        let (sender, receiver) = watch::channel(Arc::new(config.clone()));
        let watcher = tokio::spawn(watch_config(
            PathBuf::from("missing.yaml"),
            config.server,
            false,
            None,
            sender,
        ));
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn responses_to_default_chat_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "chatcmpl_1",
            "model": "deepseek-chat",
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/responses"))
            .json(&json!({
                "model": "deepseek-chat",
                "input": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["object"], "response");
        assert_eq!(output["output_text"], "hello");
        let request = captured.lock().await;
        let request = request.as_ref().unwrap();
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.uri.path(), "/chat/completions");
        assert_eq!(request.body["messages"][0]["role"], "user");
        assert_eq!(request.headers["authorization"], "Bearer sk-deepseek");
    }

    #[tokio::test]
    async fn anthropic_messages_to_default_chat_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "chatcmpl_2",
            "model": "deepseek-chat",
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/messages"))
            .json(&json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 128
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["type"], "message");
        assert_eq!(output["content"][0]["text"], "hello");
        assert_eq!(
            captured.lock().await.as_ref().unwrap().uri.path(),
            "/chat/completions"
        );
    }

    #[tokio::test]
    async fn responses_to_named_anthropic_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "msg_1",
            "type": "message",
            "model": "claude-sonnet-4",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/anthropic/responses"))
            .json(&json!({
                "model": "claude-sonnet-4",
                "input": [{"role": "user", "content": "hi"}],
                "max_output_tokens": 128
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["object"], "response");
        assert_eq!(output["output_text"], "hello");
        let request = captured.lock().await;
        let request = request.as_ref().unwrap();
        assert_eq!(request.uri.path(), "/messages");
        assert_eq!(request.body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(request.headers["x-api-key"], "sk-anthropic");
        assert_eq!(request.headers["anthropic-version"], "2023-06-01");
    }

    #[tokio::test]
    async fn anthropic_messages_to_named_responses_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "resp_1",
            "object": "response",
            "model": "gpt-4.1",
            "output_text": "hello",
            "status": "completed"
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/openai/messages"))
            .json(&json!({
                "model": "gpt-4.1",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 128
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["type"], "message");
        assert_eq!(output["content"][0]["text"], "hello");
        let request = captured.lock().await;
        let request = request.as_ref().unwrap();
        assert_eq!(request.uri.path(), "/responses");
        assert_eq!(request.body["input"][0]["content"], "hi");
        assert_eq!(request.headers["authorization"], "Bearer sk-openai");
    }

    #[tokio::test]
    async fn rejects_unknown_route_and_provider() {
        let (upstream, _) = start_upstream(json!({})).await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let unknown_route = reqwest::get(format!("http://{proxy}/unknown"))
            .await
            .unwrap();
        let unknown_provider = reqwest::Client::new()
            .post(format!("http://{proxy}/missing/responses"))
            .json(&json!({"model": "test", "input": "hi"}))
            .send()
            .await
            .unwrap();

        assert_eq!(unknown_route.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(unknown_provider.status(), reqwest::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn providers_lists_active_config_without_exposing_api_keys() {
        let mut config = test_config("https://example.test");
        config.providers.get_mut("openai").unwrap().api_key = Some("${OPENAI_API_KEY}".into());
        config.providers.get_mut("anthropic").unwrap().api_key = None;
        let proxy = start_proxy(config).await;

        let response = reqwest::get(format!("http://{proxy}/providers"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["default"], "deepseek");
        assert_eq!(body["providers"].as_object().unwrap().len(), 3);
        assert_eq!(
            body["providers"]["deepseek"],
            json!({
                "type": "openai-chat", "baseurl": "https://example.test", "apikey": "***"
            })
        );
        assert_eq!(body["providers"]["openai"]["apikey"], "***");
        assert!(body["providers"]["anthropic"].get("apikey").is_none());
        assert!(!body.to_string().contains("sk-deepseek"));
        assert!(!body.to_string().contains("OPENAI_API_KEY"));

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/providers"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn model_routes_return_client_response_shape() {
        let data = json!({
            "object": "list",
            "data": [{"created": 1790035200, "id": "gpt-6-sol", "object": "model", "owned_by": "openai"}]
        });
        let models = json!({
            "models": [{
                "slug": "gpt-6-sol",
                "display_name": "GPT 6.0 Sol",
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [{"effort": "low", "description": "Fast responses"}],
                "context_window": 272000,
                "available_access_programs": {"cyber": ["standard"]}
            }]
        });
        for (path, payload, api_key, base_suffix, codex) in [
            ("/models", &data, "sk-deepseek", "", false),
            ("/v1/models", &models, "sk-deepseek", "/v1", true),
            ("/openai/models", &models, "sk-openai", "", false),
            ("/openai/v1/models", &data, "sk-openai", "/v1", true),
            ("/models", &models, "sk-deepseek", "", true),
            ("/v1/models", &data, "sk-deepseek", "/v1", false),
        ] {
            let (upstream, captured) = start_upstream(payload.clone()).await;
            let proxy = start_proxy(test_config(&format!("{upstream}{base_suffix}"))).await;
            let query = if codex {
                "limit=10&key=sk-client&client_version=1.2.3"
            } else {
                "limit=10&key=sk-client"
            };
            let response = reqwest::get(format!("http://{proxy}{path}?{query}"))
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(response
                .headers()
                .get_all("vary")
                .iter()
                .any(|value| value == "User-Agent"));
            let output: Value = response.json().await.unwrap();
            if codex {
                assert!(output["models"].is_array(), "{path}");
                assert_eq!(output["models"][0]["slug"], "gpt-6-sol");
                assert!(output.get("data").is_none());
                if payload.get("models").is_some() {
                    assert_eq!(output, *payload);
                } else {
                    assert_eq!(output["models"][0]["base_instructions"], "");
                }
            } else {
                assert_eq!(output["object"], "list");
                assert_eq!(output["data"][0]["id"], "gpt-6-sol");
                assert!(output.get("models").is_none());
                if payload.get("data").is_some() {
                    assert_eq!(output, *payload);
                }
            }
            let request = captured.lock().await;
            let request = request.as_ref().unwrap();
            assert_eq!(request.method, Method::GET, "{path}");
            assert_eq!(
                request.uri.path(),
                format!("{base_suffix}/models"),
                "{path}"
            );
            let forwarded_query = if codex {
                "limit=10&client_version=1.2.3"
            } else {
                "limit=10"
            };
            assert_eq!(request.uri.query(), Some(forwarded_query), "{path}");
            assert_eq!(
                request.headers["authorization"],
                format!("Bearer {api_key}")
            );
            assert_eq!(request.body, Value::Null);
        }
    }

    #[tokio::test]
    async fn models_rejects_post_without_calling_upstream() {
        let (upstream, captured) = start_upstream(json!({"data": []})).await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/models"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["type"],
            "invalid_request_error"
        );
        assert!(captured.lock().await.is_none());
    }

    #[tokio::test]
    async fn codex_user_agent_selects_models_without_version_query() {
        let (upstream, captured) = start_upstream(json!({
            "object": "list",
            "data": [{"id": "gpt-6-sol", "object": "model", "created": 1790035200, "owned_by": "openai"}]
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .get(format!("http://{proxy}/models"))
            .header("user-agent", "codex_cli_rs/1.0")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap()["models"][0]["slug"],
            "gpt-6-sol"
        );
        assert_eq!(
            captured.lock().await.as_ref().unwrap().uri.path(),
            "/models"
        );
    }

    #[tokio::test]
    async fn models_preserves_conditional_etag_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/models",
                    any(|headers: HeaderMap| async move {
                        assert_eq!(headers["if-none-match"], "\"catalog-v1\"");
                        Response::builder()
                            .status(StatusCode::NOT_MODIFIED)
                            .header("etag", "\"catalog-v1\"")
                            .body(Body::empty())
                            .unwrap()
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .get(format!("http://{proxy}/models?client_version=1.0"))
            .header("if-none-match", "\"catalog-v1\"")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()["etag"], "\"catalog-v1\"");
        assert!(response
            .headers()
            .get_all("vary")
            .iter()
            .any(|value| value == "User-Agent"));
    }

    #[tokio::test]
    async fn models_rejects_malformed_success_response() {
        let (upstream, _) = start_upstream(json!({"error": "not a catalog"})).await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::get(format!("http://{proxy}/models?client_version=1.0"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["type"],
            "api_error"
        );
    }

    #[tokio::test]
    async fn conversion_errors_use_the_client_protocol_schema() {
        let (upstream, _) = start_upstream(json!({})).await;
        let proxy = start_proxy(test_config(&upstream)).await;
        let client = reqwest::Client::new();

        let responses = client
            .post(format!("http://{proxy}/responses"))
            .json(&json!({"input": "missing model"}))
            .send()
            .await
            .unwrap();
        assert_eq!(responses.status(), StatusCode::BAD_REQUEST);
        let responses_error: Value = responses.json().await.unwrap();
        assert_eq!(responses_error["error"]["type"], "invalid_request_error");

        let anthropic = client
            .post(format!("http://{proxy}/messages"))
            .json(&json!({"messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(anthropic.status(), StatusCode::BAD_REQUEST);
        let anthropic_error: Value = anthropic.json().await.unwrap();
        assert_eq!(anthropic_error["type"], "error");
        assert_eq!(anthropic_error["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn response_conversion_errors_use_the_client_protocol_schema() {
        let (upstream, _) = start_upstream(json!({
            "id": "chatcmpl_audio",
            "model": "gpt-audio",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "audio": {"id": "audio_1", "data": "AAAA"}
                },
                "finish_reason": "stop"
            }]
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/messages"))
            .json(&json!({
                "model": "claude",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "say hello"}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "api_error");
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("audio"));
    }

    #[tokio::test]
    async fn unrepresentable_request_fields_fail_instead_of_being_dropped() {
        let (upstream, captured) = start_upstream(json!({})).await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/openai/chat/completions"))
            .json(&json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                "n": 2
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("field `n`"));
        assert!(captured.lock().await.is_none());
    }

    #[tokio::test]
    async fn upstream_errors_are_converted_and_keep_retry_metadata() {
        let upstream = start_error_upstream(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
        )
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/anthropic/responses"))
            .json(&json!({"model": "claude", "input": "hi"}))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "3");
        assert_eq!(response.headers()["x-request-id"], "req_error");
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["type"], "rate_limit_error");
        assert_eq!(error["error"]["message"], "slow down");
    }

    #[tokio::test]
    async fn chat_tools_and_multimodal_content_to_anthropic_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "msg_tool",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "text", "text": "sunny", "citations": [{"type": "web_search_result_location", "url": "https://weather.test/paris", "title": "Paris weather", "cited_text": "sunny", "encrypted_index": "enc-index"}]},
                {"type": "thinking", "thinking": "check weather", "signature": "sig"},
                {"type": "tool_use", "id": "toolu_2", "name": "weather", "input": {"city": "Paris"}}
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 4}
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/anthropic/chat/completions"))
            .json(&json!({
                "model": "claude-sonnet-4-5",
                "messages": [
                    {"role": "system", "content": "Be concise"},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Weather?"},
                        {"type": "image_url", "image_url": {"url": "https://example.test/map.png", "detail": "high"}}
                    ]},
                    {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
                ],
                "tools": [{"type": "function", "function": {"name": "weather", "description": "Get weather", "parameters": {"type": "object"}, "strict": true}}],
                "tool_choice": {"type": "function", "function": {"name": "weather"}},
                "parallel_tool_calls": false,
                "reasoning_effort": "high",
                "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": {"type": "object"}, "strict": true}},
                "max_tokens": 256
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            output["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "weather"
        );
        assert_eq!(
            output["choices"][0]["message"]["reasoning_content"],
            "check weather"
        );
        assert_eq!(output["choices"][0]["message"]["content"], "sunny");
        assert_eq!(
            output["choices"][0]["message"]["annotations"][0]["type"],
            "url_citation"
        );
        assert_eq!(
            output["choices"][0]["message"]["annotations"][0]["url_citation"]["url"],
            "https://weather.test/paris"
        );
        assert!(
            output["choices"][0]["message"]["annotations"][0]["url_citation"]
                .get("encrypted_index")
                .is_none()
        );
        assert_eq!(output["usage"]["prompt_tokens"], 12);
        assert_eq!(output["usage"]["prompt_tokens_details"]["cached_tokens"], 4);

        let request = captured.lock().await;
        let body = &request.as_ref().unwrap().body;
        assert_eq!(body["system"][0]["text"], "Be concise");
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["name"], "answer");
    }

    #[tokio::test]
    async fn chat_tools_to_responses_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "resp_tool",
            "object": "response",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {"id": "rs_1", "type": "reasoning", "status": "completed", "summary": [{"type": "summary_text", "text": "checked"}], "encrypted_content": "enc"},
                {"id": "fc_1", "type": "function_call", "status": "completed", "call_id": "call_2", "name": "weather", "arguments": "{\"city\":\"Paris\"}"}
            ],
            "usage": {"input_tokens": 9, "output_tokens": 3, "total_tokens": 12, "output_tokens_details": {"reasoning_tokens": 2}}
        }))
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/openai/chat/completions"))
            .json(&json!({
                "model": "gpt-5",
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{}"}}]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
                ],
                "tools": [{"type": "function", "function": {"name": "weather", "parameters": {"type": "object"}}}],
                "reasoning_effort": "high",
                "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": {"type": "object"}, "strict": true}},
                "verbosity": "low"
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            output["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "weather"
        );
        assert_eq!(
            output["choices"][0]["message"]["reasoning_content"],
            "checked"
        );
        assert_eq!(
            output["usage"]["completion_tokens_details"]["reasoning_tokens"],
            2
        );

        let request = captured.lock().await;
        let body = &request.as_ref().unwrap().body;
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["name"], "answer");
        assert!(body["text"]["format"].get("json_schema").is_none());
    }

    #[tokio::test]
    async fn chat_stream_to_anthropic_has_complete_lifecycle_and_split_utf8() {
        let stream = concat!(
            "data: {\"id\":\"chatcmpl_s1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\r\n\r\n",
            "data: {\"id\":\"chatcmpl_s1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"}}]}\r\n\r\n",
            "data: {\"id\":\"chatcmpl_s1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n",
            "data: {\"id\":\"chatcmpl_s1\",\"model\":\"deepseek-chat\",\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}}\r\n\r\n",
            "data: [DONE]\r\n\r\n"
        )
        .as_bytes()
        .to_vec();
        let split = stream.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
        let (upstream, captured) = start_stream_upstream(vec![
            stream[..split].to_vec(),
            stream[split..split + 1].to_vec(),
            stream[split + 1..].to_vec(),
        ])
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/messages"))
            .json(&json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 64,
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.text().await.unwrap();
        let events = sse_json_events(&body);
        let types = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[2]["delta"]["text"], "你");
        assert_eq!(events[4]["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[4]["usage"]["input_tokens"], 4);
        assert_eq!(events[4]["usage"]["output_tokens"], 1);
        assert_eq!(captured.lock().await.as_ref().unwrap().body["stream"], true);
    }

    #[tokio::test]
    async fn anthropic_tool_stream_to_responses_has_complete_lifecycle() {
        let stream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":6,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"weather\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let midpoint = stream.len() / 2;
        let (upstream, _) = start_stream_upstream(vec![
            stream.as_bytes()[..midpoint].to_vec(),
            stream.as_bytes()[midpoint..].to_vec(),
        ])
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/anthropic/responses"))
            .json(&json!({"model": "claude", "input": "weather", "stream": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let events = sse_json_events(&response.text().await.unwrap());
        let types = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(types.starts_with(&[
            "response.created",
            "response.output_item.added",
            "response.function_call_arguments.delta"
        ]));
        assert!(types.contains(&"response.function_call_arguments.done"));
        assert!(types.contains(&"response.output_item.done"));
        assert_eq!(types.last(), Some(&"response.completed"));
        let completed = events.last().unwrap();
        assert_eq!(completed["response"]["output"][0]["type"], "function_call");
        assert_eq!(completed["response"]["output"][0]["call_id"], "toolu_1");
        assert_eq!(completed["response"]["output"][0]["name"], "weather");
        assert_eq!(
            completed["response"]["output"][0]["arguments"],
            "{\"city\":\"Paris\"}"
        );
        assert_eq!(completed["response"]["usage"]["input_tokens"], 6);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 3);
    }

    #[tokio::test]
    async fn responses_stream_to_chat_has_finish_usage_and_done_sequence() {
        let stream = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_s1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-5\",\"output\":[]}}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_s1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_s1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_s1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n",
            "event: response.output_text.annotation.added\ndata: {\"type\":\"response.output_text.annotation.added\",\"sequence_number\":4,\"item_id\":\"msg_s1\",\"output_index\":0,\"content_index\":0,\"annotation_index\":0,\"annotation\":{\"type\":\"url_citation\",\"start_index\":0,\"end_index\":5,\"url\":\"https://example.test\",\"title\":\"Example\"}}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"sequence_number\":5,\"output_index\":0,\"item\":{\"id\":\"msg_s1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\",\"annotations\":[]}]}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":6,\"response\":{\"id\":\"resp_s1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-5\",\"output\":[{\"id\":\"msg_s1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n"
        );
        let split = stream.find("hello").unwrap() + 2;
        let (upstream, _) = start_stream_upstream(vec![
            stream.as_bytes()[..split].to_vec(),
            stream.as_bytes()[split..].to_vec(),
        ])
        .await;
        let proxy = start_proxy(test_config(&upstream)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/openai/chat/completions"))
            .json(&json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.ends_with("data: [DONE]\n\n"));
        let events = sse_json_events(&body);
        assert_eq!(events.len(), 5);
        assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(events[1]["choices"][0]["delta"]["content"], "hello");
        assert_eq!(
            events[2]["choices"][0]["delta"]["annotations"][0]["url_citation"]["url"],
            "https://example.test"
        );
        assert_eq!(events[3]["choices"][0]["finish_reason"], "stop");
        assert_eq!(events[4]["choices"], json!([]));
        assert_eq!(events[4]["usage"]["prompt_tokens"], 7);
        assert_eq!(events[4]["usage"]["completion_tokens"], 2);
    }
}
