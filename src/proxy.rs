use axum::{
    body::{to_bytes, Body},
    extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade},
    http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode},
};
use bytes::{Bytes, BytesMut};
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, error, info, warn};

use super::{
    adapters::{
        anthropic::AnthropicAdapter, openai_chat::OpenAiChatAdapter,
        openai_responses::OpenAiResponsesAdapter, AdapterError, RequestAdapter, ResponseAdapter,
        SseDecoder, SseEvent, StreamAdapter, StreamState,
    },
    auth,
    config::{Provider, ProviderConfig},
    model::{LLMRequest, LLMResponse},
    models,
    protocol::{detect_route, is_transparent, upstream_for, InputFormat, UpstreamFormat},
    redact,
    server::AppState,
};

const MAX_JSON_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;

struct ForwardRequest<'a> {
    state: AppState,
    provider: ProviderConfig,
    headers: &'a HeaderMap,
    query: Option<&'a str>,
    input: &'a InputFormat,
    upstream: UpstreamFormat,
    path: &'a str,
    body: Bytes,
    upstream_raw: bool,
}

pub async fn handle(
    state: AppState,
    headers: HeaderMap,
    request: axum::http::Request<Body>,
) -> Response<Body> {
    handle_with_websocket(state, headers, request, None).await
}

pub async fn handle_with_websocket(
    state: AppState,
    headers: HeaderMap,
    request: axum::http::Request<Body>,
    websocket: Option<WebSocketUpgrade>,
) -> Response<Body> {
    let upstream_raw = wants_upstream_raw(request.uri(), &headers);
    let query = request.uri().query().map(ToString::to_string);
    let method = request.method().clone();
    let request_path = request.uri().path().to_string();
    let Some(route) = detect_route(&request_path) else {
        return response(
            StatusCode::NOT_FOUND,
            Bytes::from("unknown llmapi route"),
            "text/plain",
        );
    };
    if let Some(websocket) = websocket {
        if route.input != InputFormat::OpenAiResponses {
            return protocol_error_response(
                &route.input,
                StatusCode::METHOD_NOT_ALLOWED,
                "WebSocket is supported only for the Responses endpoint",
                "invalid_request_error",
            );
        }
        let Some((_, provider)) = state.config.provider(route.provider.as_deref()).ok() else {
            return protocol_error_response(
                &route.input,
                StatusCode::NOT_FOUND,
                "provider not found",
                "invalid_request_error",
            );
        };
        if provider.provider_type != Provider::OpenAiResponses {
            return protocol_error_response(
                &route.input,
                StatusCode::BAD_REQUEST,
                "WebSocket requires an openai-responses provider",
                "invalid_request_error",
            );
        }
        let provider = provider.clone();
        let query = request.uri().query().map(ToString::to_string);
        return websocket
            .on_upgrade(move |socket| proxy_websocket(state, headers, provider, query, socket));
    }
    let expected_method = if route.input == InputFormat::Models {
        Method::GET
    } else {
        Method::POST
    };
    if method != expected_method {
        return protocol_error_response(
            &route.input,
            StatusCode::METHOD_NOT_ALLOWED,
            if expected_method == Method::GET {
                "llmapi models endpoint requires GET"
            } else {
                "llmapi endpoints require POST"
            },
            "invalid_request_error",
        );
    }
    let mut provider = match state.config.provider(route.provider.as_deref()) {
        Ok((_, provider)) => provider.clone(),
        Err(err) => {
            return protocol_error_response(
                &route.input,
                StatusCode::NOT_FOUND,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };
    provider.api_key = match provider.api_key() {
        Ok(api_key) => api_key,
        Err(err) => {
            return protocol_error_response(
                &route.input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };
    if route.input == InputFormat::Models {
        let codex_client = models::is_codex_client(&headers, query.as_deref());
        let url = upstream_url(&provider.base_url, "/models", query.as_deref());
        return match send_upstream_request(
            state,
            provider,
            &headers,
            Method::GET,
            query.as_deref(),
            &url,
            Bytes::new(),
        )
        .await
        {
            Ok(upstream) => models_response(upstream, codex_client).await,
            Err(err) => protocol_error_response(
                &route.input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            ),
        };
    }
    let body = match to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return protocol_error_response(
                &route.input,
                StatusCode::BAD_REQUEST,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };
    let input = route.input;
    let (upstream, path) = upstream_for(provider.provider_type);

    if is_transparent(input, upstream) {
        debug!(target: "llmapi", path, "transparent proxy");
        return forward_raw(
            state,
            provider,
            &headers,
            method,
            query.as_deref(),
            path,
            body,
        )
        .await;
    }

    debug!(target: "llmapi", ?input, ?upstream, "protocol adapter selected");
    convert_and_forward(ForwardRequest {
        state,
        provider,
        headers: &headers,
        query: query.as_deref(),
        input: &input,
        upstream,
        path,
        body,
        upstream_raw,
    })
    .await
}

async fn proxy_websocket(
    state: AppState,
    headers: HeaderMap,
    provider: ProviderConfig,
    query: Option<String>,
    mut client: WebSocket,
) {
    let provider_key = match provider.api_key() {
        Ok(key) => key,
        Err(err) => {
            let _ = client
                .send(AxumMessage::Text(format!("{{\"error\":\"{err}\"}}").into()))
                .await;
            let _ = client.close().await;
            return;
        }
    };
    let extracted = if state.config.api_key.is_none() {
        auth::extract_api_key(&headers, query.as_deref())
    } else {
        None
    };
    let mut upstream_headers = HeaderMap::new();
    auth::apply_api_key(
        &mut upstream_headers,
        provider.provider_type,
        provider_key.as_deref(),
        extracted.as_deref(),
    );
    auth::apply_protocol_headers(&mut upstream_headers, provider.provider_type, &headers);
    let url = websocket_url(&provider.base_url, "/responses", query.as_deref());
    let mut upstream_request = match url.into_client_request() {
        Ok(request) => request,
        Err(err) => {
            warn!(target: "llmapi", error = %err, "invalid upstream WebSocket URL");
            let _ = client.close().await;
            return;
        }
    };
    *upstream_request.headers_mut() = upstream_headers;
    let (upstream, _) = match tokio_tungstenite::connect_async(upstream_request).await {
        Ok(connection) => connection,
        Err(err) => {
            warn!(target: "llmapi", error = %err, "upstream WebSocket connection failed");
            let _ = client.close().await;
            return;
        }
    };
    let (mut client_sink, mut client_stream) = client.split();
    let (mut upstream_sink, mut upstream_stream) = upstream.split();
    tokio::select! {
        _ = async {
            while let Some(message) = client_stream.next().await {
                let Some(message) = message.ok() else { break };
                let Some(message) = axum_to_tungstenite(message) else { break };
                if upstream_sink.send(message).await.is_err() { break; }
            }
        } => {}
        _ = async {
            while let Some(message) = upstream_stream.next().await {
                let Some(message) = message.ok() else { break };
                let Some(message) = tungstenite_to_axum(message) else { break; };
                if client_sink.send(message).await.is_err() { break; }
            }
        } => {}
    }
}

fn axum_to_tungstenite(message: AxumMessage) -> Option<tokio_tungstenite::tungstenite::Message> {
    use tokio_tungstenite::tungstenite::Message;
    match message {
        AxumMessage::Text(text) => Some(Message::Text(text.to_string().into())),
        AxumMessage::Binary(bytes) => Some(Message::Binary(bytes.to_vec().into())),
        AxumMessage::Ping(bytes) => Some(Message::Ping(bytes.to_vec().into())),
        AxumMessage::Pong(bytes) => Some(Message::Pong(bytes.to_vec().into())),
        AxumMessage::Close(frame) => Some(Message::Close(frame.map(|frame| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.to_string().into(),
            }
        }))),
    }
}

fn tungstenite_to_axum(message: tokio_tungstenite::tungstenite::Message) -> Option<AxumMessage> {
    use tokio_tungstenite::tungstenite::Message;
    match message {
        Message::Text(text) => Some(AxumMessage::Text(text.to_string().into())),
        Message::Binary(bytes) => Some(AxumMessage::Binary(bytes.to_vec().into())),
        Message::Ping(bytes) => Some(AxumMessage::Ping(bytes.to_vec().into())),
        Message::Pong(bytes) => Some(AxumMessage::Pong(bytes.to_vec().into())),
        Message::Close(frame) => Some(AxumMessage::Close(frame.map(|frame| {
            axum::extract::ws::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.to_string().into(),
            }
        }))),
        Message::Frame(_) => None,
    }
}

async fn models_response(upstream: reqwest::Response, codex_client: bool) -> Response<Body> {
    if !upstream.status().is_success() {
        let mut response = raw_upstream_response(upstream);
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("User-Agent"));
        return response;
    }
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let body = match read_upstream_body(upstream, MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return protocol_error_response(
                &InputFormat::Models,
                StatusCode::BAD_GATEWAY,
                &err,
                "api_error",
            )
        }
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return protocol_error_response(
                &InputFormat::Models,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            )
        }
    };
    let (desired_key, other_key) = if codex_client {
        ("models", "data")
    } else {
        ("data", "models")
    };
    let native = value.get(desired_key).and_then(Value::as_array).is_some()
        && value.get(other_key).is_none();
    if native {
        let mut response = response(status, body, "application/json");
        copy_response_metadata_headers(&headers, response.headers_mut());
        if let Some(etag) = headers.get(header::ETAG) {
            response.headers_mut().insert(header::ETAG, etag.clone());
        }
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("User-Agent"));
        return response;
    }
    let output = match models::for_client(value, codex_client) {
        Ok(output) => output,
        Err(err) => {
            return protocol_error_response(
                &InputFormat::Models,
                StatusCode::BAD_GATEWAY,
                err,
                "api_error",
            )
        }
    };
    let mut response = response(
        status,
        Bytes::from(serde_json::to_vec(&output).unwrap()),
        "application/json",
    );
    copy_response_metadata_headers(&headers, response.headers_mut());
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("User-Agent"));
    response
}

pub fn forward_raw(
    state: AppState,
    provider: ProviderConfig,
    headers: &HeaderMap,
    method: Method,
    query: Option<&str>,
    path: &str,
    body: Bytes,
) -> impl std::future::Future<Output = Response<Body>> + Send {
    let url = upstream_url(&provider.base_url, path, query);
    let upstream_method = method.as_str().to_string();
    let input = match provider.provider_type {
        Provider::OpenAiChat => InputFormat::OpenAiChat,
        Provider::OpenAiResponses => InputFormat::OpenAiResponses,
        Provider::Anthropic => InputFormat::AnthropicMessages,
    };
    send_upstream_request(state, provider, headers, method, query, &url, body).map(move |result| {
        match result {
            Ok(upstream) => {
                info!(
                    target: "llmapi",
                    method = %upstream_method,
                    url = %redact::url(upstream.url().as_str()),
                    status = %upstream.status(),
                    "upstream response received"
                );
                raw_upstream_response(upstream)
            }
            Err(err) => {
                error!(target: "llmapi", error = %err, "upstream request failed");
                protocol_error_response(
                    &input,
                    StatusCode::BAD_GATEWAY,
                    &err.to_string(),
                    "api_error",
                )
            }
        }
    })
}

fn send_upstream_request(
    state: AppState,
    provider: ProviderConfig,
    headers: &HeaderMap,
    method: Method,
    auth_query: Option<&str>,
    url: &str,
    body: Bytes,
) -> impl std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send {
    let extracted = if state.config.api_key.is_none() {
        auth::extract_api_key(headers, auth_query)
    } else {
        None
    };
    let mut upstream_headers = HeaderMap::new();
    auth::apply_api_key(
        &mut upstream_headers,
        provider.provider_type,
        provider.api_key.as_deref(),
        extracted.as_deref(),
    );
    auth::apply_protocol_headers(&mut upstream_headers, provider.provider_type, headers);
    if method == Method::GET {
        if let Some(etag) = headers.get(header::IF_NONE_MATCH) {
            upstream_headers.insert(header::IF_NONE_MATCH, etag.clone());
        }
    }
    state
        .client
        .request(method, url)
        .headers(upstream_headers)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
}

pub fn response(status: StatusCode, body: Bytes, content_type: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}

async fn convert_and_forward(forward: ForwardRequest<'_>) -> Response<Body> {
    let input_json: Value = match serde_json::from_slice(&forward.body) {
        Ok(value) => value,
        Err(err) => {
            return protocol_error_response(
                forward.input,
                StatusCode::BAD_REQUEST,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };
    let llm_request = match input_to_llm(forward.input, input_json) {
        Ok(request) => request,
        Err(err) => {
            return protocol_error_response(
                forward.input,
                StatusCode::BAD_REQUEST,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };
    debug!(
        target: "llmapi",
        source = ?llm_request.source,
        model = %llm_request.model,
        stream = llm_request.stream,
        messages = llm_request.messages.len(),
        tools = llm_request.tools.len(),
        "normalized LLM request"
    );
    let upstream_json = match llm_to_upstream(forward.upstream, &llm_request) {
        Ok(value) => value,
        Err(err) => {
            let (status, error_type) = if matches!(&err, AdapterError::Unsupported(_)) {
                (StatusCode::BAD_REQUEST, "invalid_request_error")
            } else {
                (StatusCode::BAD_GATEWAY, "api_error")
            };
            return protocol_error_response(forward.input, status, &err.to_string(), error_type);
        }
    };

    let url = upstream_url(&forward.provider.base_url, forward.path, forward.query);
    let upstream_response = match send_upstream_request(
        forward.state.clone(),
        forward.provider,
        forward.headers,
        Method::POST,
        forward.query,
        &url,
        Bytes::from(serde_json::to_vec(&upstream_json).unwrap()),
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            error!(target: "llmapi", error = %err, "upstream request failed");
            return protocol_error_response(
                forward.input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };
    info!(
        target: "llmapi",
        method = "POST",
        url = %redact::url(upstream_response.url().as_str()),
        status = %upstream_response.status(),
        "upstream response received"
    );
    if forward.upstream_raw {
        return raw_upstream_response(upstream_response);
    }
    convert_response_back(upstream_response, forward.input, forward.upstream).await
}

fn input_to_llm(input: &InputFormat, value: Value) -> Result<LLMRequest, AdapterError> {
    match input {
        InputFormat::OpenAiChat => OpenAiChatAdapter::to_llm_request(value),
        InputFormat::OpenAiResponses => OpenAiResponsesAdapter::to_llm_request(value),
        InputFormat::AnthropicMessages => AnthropicAdapter::to_llm_request(value),
        InputFormat::Models => unreachable!("models requests are forwarded before conversion"),
    }
}

fn llm_to_upstream(upstream: UpstreamFormat, request: &LLMRequest) -> Result<Value, AdapterError> {
    match upstream {
        UpstreamFormat::OpenAiChat => OpenAiChatAdapter::from_llm_request(request),
        UpstreamFormat::OpenAiResponses => OpenAiResponsesAdapter::from_llm_request(request),
        UpstreamFormat::AnthropicMessages => AnthropicAdapter::from_llm_request(request),
    }
}

fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base_url = base_url.trim_end_matches('/');
    let endpoint = if base_url.ends_with(path) {
        base_url.to_string()
    } else {
        format!("{base_url}{path}")
    };
    match query.and_then(forwarded_query) {
        Some(query) => format!("{endpoint}?{query}"),
        None => endpoint,
    }
}

fn websocket_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let http_url = upstream_url(base_url, path, query);
    if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http_url
    }
}

fn forwarded_query(query: &str) -> Option<String> {
    let values: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key != "key" && key != "llmapi_response_mode")
        .collect();
    if values.is_empty() {
        return None;
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(values);
    Some(serializer.finish())
}

async fn convert_response_back(
    upstream_response: reqwest::Response,
    input: &InputFormat,
    upstream: UpstreamFormat,
) -> Response<Body> {
    let status = upstream_response.status();
    let content_type = upstream_response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let response_headers = upstream_response.headers().clone();
    if !status.is_success() {
        return converted_error_response(upstream_response, input).await;
    }

    if content_type.contains("text/event-stream") {
        return converted_sse_response(upstream_response, *input, upstream);
    }

    let body = match read_upstream_body(upstream_response, MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            let mut response =
                protocol_error_response(input, StatusCode::BAD_GATEWAY, &err, "api_error");
            copy_response_metadata_headers(&response_headers, response.headers_mut());
            return response;
        }
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return protocol_error_response(
                input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };
    let llm = match upstream_to_llm(upstream, value) {
        Ok(value) => value,
        Err(err) => {
            return protocol_error_response(
                input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };
    debug!(
        target: "llmapi",
        source = ?llm.source,
        id = llm.id.as_deref(),
        model = llm.model.as_deref(),
        choices = llm.choices.len(),
        has_usage = llm.usage.is_some(),
        "normalized LLM response"
    );
    let output = match llm_to_input(input, &llm) {
        Ok(value) => value,
        Err(err) => {
            return protocol_error_response(
                input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };

    let mut response = response(
        StatusCode::OK,
        Bytes::from(serde_json::to_vec(&output).unwrap()),
        "application/json",
    );
    copy_response_metadata_headers(&response_headers, response.headers_mut());
    response
}

fn raw_upstream_response(upstream: reqwest::Response) -> Response<Body> {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = upstream.headers().clone();
    let mut builder = Response::builder().status(status);
    copy_response_headers(&headers, builder.headers_mut().unwrap());
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap()
}

async fn converted_error_response(
    upstream: reqwest::Response,
    input: &InputFormat,
) -> Response<Body> {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = upstream.headers().clone();
    let body = match read_upstream_body(upstream, MAX_ERROR_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            let mut response = protocol_error_response(input, status, &err, "api_error");
            copy_response_metadata_headers(&headers, response.headers_mut());
            return response;
        }
    };
    let value = serde_json::from_slice::<Value>(&body).ok();
    let error = value.as_ref().and_then(|value| value.get("error"));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or_else(|| std::str::from_utf8(&body).unwrap_or("upstream request failed"));
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let mut response = protocol_error_response(input, status, message, error_type);
    copy_response_metadata_headers(&headers, response.headers_mut());
    response
}

async fn read_upstream_body(upstream: reqwest::Response, limit: usize) -> Result<Bytes, String> {
    if upstream
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("upstream response body exceeds {limit} bytes"));
    }
    let mut stream = upstream.bytes_stream();
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| format!("read upstream response body: {err}"))?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(format!("upstream response body exceeds {limit} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn protocol_error_response(
    input: &InputFormat,
    status: StatusCode,
    message: &str,
    error_type: &str,
) -> Response<Body> {
    let value = match input {
        InputFormat::AnthropicMessages => json!({
            "type": "error",
            "error": {"type": error_type, "message": message}
        }),
        InputFormat::OpenAiChat | InputFormat::OpenAiResponses | InputFormat::Models => json!({
            "error": {"message": message, "type": error_type, "param": null, "code": null}
        }),
    };
    response(
        status,
        Bytes::from(serde_json::to_vec(&value).unwrap()),
        "application/json",
    )
}

fn copy_response_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for (name, value) in source {
        let name_text = name.as_str();
        if name == header::CONTENT_TYPE
            || name == header::CACHE_CONTROL
            || name == header::ETAG
            || name == header::VARY
            || name == header::RETRY_AFTER
            || matches!(
                name_text,
                "x-request-id" | "request-id" | "anthropic-request-id" | "openai-processing-ms"
            )
            || name_text.starts_with("x-ratelimit-")
        {
            target.insert(name.clone(), value.clone());
        }
    }
}

fn copy_response_metadata_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for (name, value) in source {
        let name_text = name.as_str();
        if name == header::CACHE_CONTROL
            || name == header::VARY
            || name == header::RETRY_AFTER
            || matches!(
                name_text,
                "x-request-id" | "request-id" | "anthropic-request-id" | "openai-processing-ms"
            )
            || name_text.starts_with("x-ratelimit-")
        {
            target.insert(name.clone(), value.clone());
        }
    }
}

fn upstream_to_llm(upstream: UpstreamFormat, value: Value) -> Result<LLMResponse, AdapterError> {
    match upstream {
        UpstreamFormat::OpenAiChat => OpenAiChatAdapter::to_llm_response(value),
        UpstreamFormat::OpenAiResponses => OpenAiResponsesAdapter::to_llm_response(value),
        UpstreamFormat::AnthropicMessages => AnthropicAdapter::to_llm_response(value),
    }
}

fn llm_to_input(input: &InputFormat, response: &LLMResponse) -> Result<Value, AdapterError> {
    match input {
        InputFormat::OpenAiChat => OpenAiChatAdapter::from_llm_response(response),
        InputFormat::OpenAiResponses => OpenAiResponsesAdapter::from_llm_response(response),
        InputFormat::AnthropicMessages => AnthropicAdapter::from_llm_response(response),
        InputFormat::Models => unreachable!("models responses are forwarded before conversion"),
    }
}

fn parse_stream_event(
    upstream: UpstreamFormat,
    event: &SseEvent,
    state: &mut StreamState,
) -> Result<Vec<super::model::LLMStreamEvent>, AdapterError> {
    match upstream {
        UpstreamFormat::OpenAiChat => OpenAiChatAdapter::parse_stream_event(event, state),
        UpstreamFormat::OpenAiResponses => OpenAiResponsesAdapter::parse_stream_event(event, state),
        UpstreamFormat::AnthropicMessages => AnthropicAdapter::parse_stream_event(event, state),
    }
}

fn format_stream_events(
    input: &InputFormat,
    events: &[super::model::LLMStreamEvent],
    state: &mut StreamState,
) -> Result<Vec<SseEvent>, AdapterError> {
    match input {
        InputFormat::OpenAiChat => OpenAiChatAdapter::format_stream_events(events, state),
        InputFormat::OpenAiResponses => OpenAiResponsesAdapter::format_stream_events(events, state),
        InputFormat::AnthropicMessages => AnthropicAdapter::format_stream_events(events, state),
        InputFormat::Models => unreachable!("models responses are forwarded before conversion"),
    }
}

fn converted_sse_response(
    upstream_response: reqwest::Response,
    input: InputFormat,
    upstream: UpstreamFormat,
) -> Response<Body> {
    let upstream_headers = upstream_response.headers().clone();
    let stream = upstream_response.bytes_stream();
    let converted = async_stream::stream! {
        let mut stream = stream;
        let mut decoder = SseDecoder::default();
        let mut source_state = StreamState::default();
        let mut target_state = StreamState::default();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    for output in stream_error_events(&input, &mut target_state, err.to_string()) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                    }
                    return;
                }
            };
            let events = match decoder.push(&chunk) {
                Ok(events) => events,
                Err(err) => {
                    warn!(target: "llmapi", error = %err, "stream decoding failed");
                    for output in stream_error_events(&input, &mut target_state, err.to_string()) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                    }
                    return;
                }
            };
            for event in events {
                match convert_sse_event(&event, &input, upstream, &mut source_state, &mut target_state) {
                    Ok(outputs) => for output in outputs {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                    },
                    Err(err) => {
                        warn!(target: "llmapi", error = %err, "stream event conversion failed");
                        for output in stream_error_events(&input, &mut target_state, err.to_string()) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                        }
                        return;
                    }
                }
            }
        }
        match decoder.finish() {
            Ok(events) => for event in events {
                match convert_sse_event(&event, &input, upstream, &mut source_state, &mut target_state) {
                    Ok(outputs) => for output in outputs {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                    },
                    Err(err) => {
                        for output in stream_error_events(&input, &mut target_state, err.to_string()) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                        }
                        return;
                    }
                }
            },
            Err(err) => {
                for output in stream_error_events(&input, &mut target_state, err.to_string()) {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
                }
                return;
            }
        }
        if !source_state.ended {
            for output in stream_error_events(&input, &mut target_state, "upstream stream ended before a terminal event".into()) {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(output.encode()));
            }
        }
    };

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(converted))
        .unwrap();
    copy_response_metadata_headers(&upstream_headers, response.headers_mut());
    response
}

fn convert_sse_event(
    event: &SseEvent,
    input: &InputFormat,
    upstream: UpstreamFormat,
    source_state: &mut StreamState,
    target_state: &mut StreamState,
) -> Result<Vec<SseEvent>, AdapterError> {
    let events = parse_stream_event(upstream, event, source_state)?;
    format_stream_events(input, &events, target_state)
}

fn stream_error_events(
    input: &InputFormat,
    state: &mut StreamState,
    message: String,
) -> Vec<SseEvent> {
    let events = [super::model::LLMStreamEvent::Error {
        message,
        metadata: Default::default(),
    }];
    format_stream_events(input, &events, state).unwrap_or_default()
}

fn wants_upstream_raw(uri: &axum::http::Uri, headers: &HeaderMap) -> bool {
    if headers
        .get("x-llmapi-response-mode")
        .and_then(|value| value.to_str().ok())
        == Some("upstream_raw")
    {
        return true;
    }

    uri.query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .any(|(key, value)| key == "llmapi_response_mode" && value == "upstream_raw")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_auth_and_internal_query_parameters_from_upstream_url() {
        assert_eq!(
            upstream_url(
                "https://api.example.test/v1",
                "/responses",
                Some("key=sk-secret&beta=true&llmapi_response_mode=upstream_raw"),
            ),
            "https://api.example.test/v1/responses?beta=true"
        );
    }

    #[test]
    fn accepts_base_url_that_already_contains_endpoint() {
        assert_eq!(
            upstream_url("https://api.anthropic.test/v1/messages", "/messages", None),
            "https://api.anthropic.test/v1/messages"
        );
        assert_eq!(
            upstream_url("https://api.openai.test/v1", "/responses", None),
            "https://api.openai.test/v1/responses"
        );
    }

    #[test]
    fn builds_websocket_url_and_filters_client_secrets() {
        assert_eq!(
            websocket_url(
                "https://api.openai.test/v1",
                "/responses",
                Some("key=sk-secret&stream=true&llmapi_response_mode=upstream_raw"),
            ),
            "wss://api.openai.test/v1/responses?stream=true"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:9000", "/responses", None),
            "ws://127.0.0.1:9000/responses"
        );
    }

    #[tokio::test]
    async fn bounds_buffered_upstream_response_bodies() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/", axum::routing::get(|| async { "12345" })),
            )
            .await
            .unwrap();
        });
        let upstream = reqwest::get(format!("http://{address}")).await.unwrap();

        let error = read_upstream_body(upstream, 4).await.unwrap_err();
        assert!(error.contains("exceeds 4 bytes"));
    }
}
