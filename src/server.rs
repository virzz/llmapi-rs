use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, Request, Response, Uri},
    routing::any,
    Router,
};
use tracing::{info, warn};

use super::{config::Config, proxy, redact};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
}

pub fn app(config: Config) -> Router {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|err| {
            warn!(target: "llmapi", error = %err, "failed to configure upstream client");
            reqwest::Client::new()
        });
    let state = AppState {
        config: Arc::new(config),
        client,
    };

    Router::new()
        .route("/{*path}", any(handler))
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(target: "llmapi", address = %listener.local_addr()?, "server listening");
    axum::serve(listener, app(config)).await?;
    Ok(())
}

fn handler(
    State(state): State<AppState>,
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

    proxy::handle(state, headers, request)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, convert::Infallible, sync::Arc};

    use axum::{
        extract::State,
        http::{HeaderMap, Method, StatusCode},
        response::IntoResponse,
        routing::any,
        Json, Router,
    };
    use serde_json::{json, Value};
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
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
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

        let unknown_route = reqwest::get(format!("http://{proxy}/models"))
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
