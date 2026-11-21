use axum::{
    body::{to_bytes, Body},
    extract::ws::{close_code, CloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade},
    http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode},
};
use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, error, info, warn};

use super::{
    adapters::{
        anthropic::AnthropicAdapter, gemini::GeminiAdapter, openai_chat::OpenAiChatAdapter,
        openai_responses::OpenAiResponsesAdapter, AdapterError, RequestAdapter, ResponseAdapter,
        SseDecoder, SseEvent, StreamAdapter, StreamState,
    },
    auth, codex,
    config::{Protocol, ProviderConfig, UpstreamConfig},
    lb,
    model::{LLMRequest, LLMResponse},
    models,
    protocol::{
        detect_route, interactions_endpoint, is_transparent, upstream_for_input, InputFormat,
        InteractionsEndpoint, UpstreamFormat,
    },
    redact,
    server::AppState,
};

pub(crate) const MAX_JSON_BODY_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;
const RESPONSE_MODE_HEADER: &str = "x-llmapi-response-mode";

/// How an upstream response becomes a client response.
enum ResponseMode {
    Models {
        protocol: Protocol,
        codex_client: bool,
        allow_etag: bool,
        models: Vec<String>,
    },
    /// Transparent same-protocol passthrough.
    Raw,
    /// Converted request with the original upstream response returned as-is.
    RawUpstream,
    Converted {
        input: InputFormat,
        upstream: UpstreamFormat,
    },
    Codex {
        input: InputFormat,
        stream: bool,
    },
}

/// Everything needed to send one attempt to a concrete upstream.
struct RequestPlan {
    protocol: Protocol,
    method: Method,
    url: String,
    query: Option<String>,
    api_key: Option<String>,
    client_headers: HeaderMap,
    derived_headers: HeaderMap,
    body: Bytes,
    mode: ResponseMode,
    conditional: bool,
    /// LB attempts disable redirects so one member attempt equals one request.
    follow_redirects: bool,
}

enum SendError {
    Timeout,
    Request(reqwest::Error),
}

enum Attempt {
    Response(Response<Body>),
    Switch(String),
}

enum LbRequest<'a> {
    Models {
        query: Option<&'a str>,
        codex_client: bool,
    },
    Generate {
        method: Method,
        query: Option<&'a str>,
        input: InputFormat,
        body: Bytes,
        upstream_raw: bool,
    },
}

impl LbRequest<'_> {
    fn input(&self) -> InputFormat {
        match self {
            Self::Models { .. } => InputFormat::Models,
            Self::Generate { input, .. } => *input,
        }
    }
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
        let provider = match state.config.provider(route.provider.as_deref()) {
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
        let Some(upstream) = provider.as_upstream() else {
            return protocol_error_response(
                &route.input,
                StatusCode::BAD_REQUEST,
                "load balancer providers do not support WebSocket connections",
                "invalid_request_error",
            );
        };
        if !matches!(
            upstream.protocol,
            Protocol::OpenAiResponses | Protocol::Llmapi
        ) {
            return protocol_error_response(
                &route.input,
                StatusCode::BAD_REQUEST,
                "WebSocket requires an openai-responses or llmapi provider",
                "invalid_request_error",
            );
        }
        let upstream = upstream.clone();
        let query = request.uri().query().map(ToString::to_string);
        return websocket
            .on_upgrade(move |socket| proxy_websocket(state, headers, upstream, query, socket));
    }
    let expected_method = if route.input == InputFormat::Models {
        Method::GET
    } else {
        Method::POST
    };
    if route.input != InputFormat::GeminiInteractions && method != expected_method {
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
    let (provider_name, provider) = match state.config.provider(route.provider.as_deref()) {
        Ok((name, provider)) => (name.to_owned(), provider.clone()),
        Err(err) => {
            return protocol_error_response(
                &route.input,
                StatusCode::NOT_FOUND,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };
    let input = route.input;

    if input == InputFormat::GeminiInteractions {
        let path = route
            .provider
            .as_deref()
            .map_or(request_path.as_str(), |provider| {
                &request_path[provider.len() + 1..]
            });
        let (path, endpoint) =
            interactions_endpoint(path).expect("interactions route contains a validated endpoint");
        return proxy_interactions(&state, &provider, headers, request, path, endpoint).await;
    }

    if input == InputFormat::Models {
        let codex_client = models::is_codex_client(&headers, query.as_deref());
        if let Some(lb_config) = provider.as_lb() {
            let members = state
                .lb
                .candidates(&provider_name)
                .unwrap_or_else(|| lb_config.members.clone());
            return lb_dispatch(
                state,
                &provider_name,
                &members,
                &headers,
                LbRequest::Models {
                    query: query.as_deref(),
                    codex_client,
                },
            )
            .await;
        }
        let upstream = provider
            .as_upstream()
            .expect("non-lb provider resolves to an upstream");
        if upstream.protocol == Protocol::Codex && state.codex_headers.is_none() {
            return protocol_error_response(
                &input,
                StatusCode::BAD_GATEWAY,
                "Codex ChatGPT credentials are unavailable",
                "api_error",
            );
        }
        let api_key = match upstream.api_key() {
            Ok(api_key) => api_key,
            Err(err) => {
                return protocol_error_response(
                    &input,
                    StatusCode::BAD_GATEWAY,
                    &err.to_string(),
                    "api_error",
                );
            }
        };
        let plan = plan_models(
            upstream,
            api_key,
            &headers,
            query.as_deref(),
            codex_client,
            true,
            true,
        );
        return match send_plan(&state, &plan, None).await {
            Ok(upstream_response) => build_response(plan.mode, upstream_response).await,
            Err(err) => send_error_response(&input, err, None),
        };
    }

    let body = match to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return protocol_error_response(
                &input,
                StatusCode::BAD_REQUEST,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };

    if let Some(lb_config) = provider.as_lb() {
        let members = state
            .lb
            .candidates(&provider_name)
            .unwrap_or_else(|| lb_config.members.clone());
        return lb_dispatch(
            state,
            &provider_name,
            &members,
            &headers,
            LbRequest::Generate {
                method,
                query: query.as_deref(),
                input,
                body,
                upstream_raw,
            },
        )
        .await;
    }

    let upstream = provider
        .as_upstream()
        .expect("non-lb provider resolves to an upstream");
    if let Err(reason) = check_request_model(upstream, &body) {
        return protocol_error_response(
            &input,
            StatusCode::BAD_REQUEST,
            reason,
            "invalid_request_error",
        );
    }
    if upstream.protocol == Protocol::Codex && state.codex_headers.is_none() {
        return protocol_error_response(
            &input,
            StatusCode::BAD_GATEWAY,
            "Codex ChatGPT credentials are unavailable",
            "api_error",
        );
    }
    let api_key = match upstream.api_key() {
        Ok(api_key) => api_key,
        Err(err) => {
            return protocol_error_response(
                &input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };
    let plan = match plan_generate(
        upstream,
        api_key,
        GenerateRequest {
            input,
            client_headers: &headers,
            method,
            query: query.as_deref(),
            body: &body,
            upstream_raw,
            follow_redirects: true,
        },
    ) {
        Ok(plan) => plan,
        Err(response) => return *response,
    };
    let upstream_response = match send_plan(&state, &plan, None).await {
        Ok(upstream_response) => upstream_response,
        Err(err) => return send_error_response(&input, err, None),
    };
    info!(
        target: "llmapi",
        method = %plan.method,
        url = %redact::url(upstream_response.url().as_str()),
        status = %upstream_response.status(),
        "upstream response received"
    );
    build_response(plan.mode, upstream_response).await
}

async fn proxy_interactions(
    state: &AppState,
    provider: &ProviderConfig,
    client_headers: HeaderMap,
    request: axum::http::Request<Body>,
    path: &str,
    endpoint: InteractionsEndpoint,
) -> Response<Body> {
    let input = InputFormat::GeminiInteractions;
    let (allow, supported) = match endpoint {
        InteractionsEndpoint::Create | InteractionsEndpoint::Cancel => {
            ("POST", request.method() == Method::POST)
        }
        InteractionsEndpoint::Item => (
            "GET, DELETE",
            matches!(*request.method(), Method::GET | Method::DELETE),
        ),
    };
    if !supported {
        let mut response = protocol_error_response(
            &input,
            StatusCode::METHOD_NOT_ALLOWED,
            "method is not supported for this Interaction endpoint",
            "invalid_request_error",
        );
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static(allow));
        return response;
    }
    // Stateful requests must use the same upstream and credentials throughout their lifecycle.
    let Some(upstream) = provider
        .as_upstream()
        .filter(|upstream| upstream.protocol == Protocol::Gemini)
    else {
        return protocol_error_response(
            &input,
            StatusCode::BAD_REQUEST,
            "Interactions requires a concrete gemini provider",
            "invalid_request_error",
        );
    };
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return protocol_error_response(
                &input,
                StatusCode::BAD_REQUEST,
                &err.to_string(),
                "invalid_request_error",
            );
        }
    };
    if endpoint == InteractionsEndpoint::Create
        && let Err(reason) = check_request_model(upstream, &body)
    {
        return protocol_error_response(
            &input,
            StatusCode::BAD_REQUEST,
            reason,
            "invalid_request_error",
        );
    }
    let api_key = match upstream.api_key() {
        Ok(api_key) => api_key,
        Err(err) => {
            return protocol_error_response(
                &input,
                StatusCode::BAD_GATEWAY,
                &err.to_string(),
                "api_error",
            );
        }
    };
    let query = parts.uri.query();
    let plan = RequestPlan {
        protocol: upstream.protocol,
        method: parts.method,
        url: upstream_url(&upstream.base_url, path, query),
        query: query.map(ToString::to_string),
        api_key,
        client_headers,
        derived_headers: HeaderMap::new(),
        body,
        mode: ResponseMode::Raw,
        conditional: false,
        follow_redirects: false,
    };
    match send_plan(state, &plan, None).await {
        Ok(upstream_response) => build_response(plan.mode, upstream_response).await,
        Err(err) => send_error_response(&input, err, None),
    }
}

fn send_error_response(
    input: &InputFormat,
    err: SendError,
    member: Option<&str>,
) -> Response<Body> {
    let message = match err {
        SendError::Timeout => "upstream response header timeout".to_string(),
        SendError::Request(err) => redact::transport_error(err),
    };
    match member {
        Some(member) => {
            error!(target: "llmapi", member = %member, error = %message, "upstream request failed");
        }
        None => {
            error!(target: "llmapi", error = %message, "upstream request failed");
        }
    }
    protocol_error_response(input, StatusCode::BAD_GATEWAY, &message, "api_error")
}

async fn proxy_websocket(
    state: AppState,
    headers: HeaderMap,
    upstream: UpstreamConfig,
    query: Option<String>,
    mut client: WebSocket,
) {
    let provider_key = match upstream.api_key() {
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
        upstream.protocol,
        provider_key.as_deref(),
        extracted.as_deref(),
    );
    auth::apply_protocol_headers(&mut upstream_headers, upstream.protocol, &headers);
    let url = websocket_url(&upstream.base_url, "/responses", query.as_deref());
    let mut upstream_request = match url.into_client_request() {
        Ok(request) => request,
        Err(err) => {
            warn!(target: "llmapi", error = %err, "invalid upstream WebSocket URL");
            let _ = client.close().await;
            return;
        }
    };
    // Keep the generated WebSocket handshake headers and layer auth on top.
    for (name, value) in &upstream_headers {
        upstream_request.headers_mut().insert(name, value.clone());
    }
    let (upstream_socket, _) = match tokio_tungstenite::connect_async(upstream_request).await {
        Ok(connection) => connection,
        Err(err) => {
            warn!(target: "llmapi", error = %err, "upstream WebSocket connection failed");
            let _ = client.close().await;
            return;
        }
    };
    let (mut client_sink, mut client_stream) = client.split();
    let (mut upstream_sink, mut upstream_stream) = upstream_socket.split();
    let rejected = tokio::select! {
        reason = async {
            while let Some(message) = client_stream.next().await {
                let Some(message) = message.ok() else { break };
                let payload = match &message {
                    AxumMessage::Text(text) => Some(text.as_bytes()),
                    AxumMessage::Binary(bytes) => Some(bytes.as_ref()),
                    _ => None,
                };
                if let Some(payload) = payload
                    && let Err(reason) = check_request_model(&upstream, payload)
                {
                    return Some(reason);
                }
                let Some(message) = axum_to_tungstenite(message) else { break };
                if upstream_sink.send(message).await.is_err() { break; }
            }
            None
        } => reason,
        _ = async {
            while let Some(message) = upstream_stream.next().await {
                let Some(message) = message.ok() else { break };
                let Some(message) = tungstenite_to_axum(message) else { break; };
                if client_sink.send(message).await.is_err() { break; }
            }
        } => None,
    };
    if let Some(reason) = rejected {
        let _ = client_sink
            .send(AxumMessage::Close(Some(CloseFrame {
                code: close_code::POLICY,
                reason: reason.into(),
            })))
            .await;
    }
}

fn check_request_model(upstream: &UpstreamConfig, body: &[u8]) -> Result<(), &'static str> {
    if upstream.models.is_empty() {
        return Ok(());
    }

    #[derive(serde::Deserialize)]
    struct RequestModel {
        model: String,
    }

    let request = serde_json::from_slice::<RequestModel>(body)
        .map_err(|_| "request requires a valid model")?;
    if !upstream.allows_model(&request.model) {
        return Err("model is not allowed by this provider");
    }
    Ok(())
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

async fn models_response(
    upstream: reqwest::Response,
    protocol: Protocol,
    codex_client: bool,
    allow_etag: bool,
    models: &[String],
) -> Response<Body> {
    let mut response = models_response_inner(upstream, protocol, codex_client, models).await;
    if !allow_etag {
        // The upstream validator does not describe this catalog.
        response.headers_mut().remove(header::ETAG);
    }
    response
}

async fn models_response_inner(
    upstream: reqwest::Response,
    protocol: Protocol,
    codex_client: bool,
    allowed_models: &[String],
) -> Response<Body> {
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
    let mut value: Value = match serde_json::from_slice(&body) {
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
    if protocol != Protocol::Gemini && native && allowed_models.is_empty() {
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
    if protocol == Protocol::Gemini {
        value = match models::from_gemini_catalog(value) {
            Ok(value) => value,
            Err(err) => {
                return protocol_error_response(
                    &InputFormat::Models,
                    StatusCode::BAD_GATEWAY,
                    err,
                    "api_error",
                );
            }
        };
    }
    if !allowed_models.is_empty() {
        models::restrict_catalog(&mut value, allowed_models);
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

fn plan_models(
    upstream: &UpstreamConfig,
    api_key: Option<String>,
    client_headers: &HeaderMap,
    query: Option<&str>,
    codex_client: bool,
    allow_etag: bool,
    follow_redirects: bool,
) -> RequestPlan {
    let allow_etag =
        allow_etag && upstream.models.is_empty() && upstream.protocol != Protocol::Gemini;
    let mut url = upstream_url(&upstream.base_url, "/models", query);
    if upstream.protocol == Protocol::Codex && !has_client_version(query) {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("client_version=");
        url.push_str(codex::CLIENT_VERSION);
    }
    RequestPlan {
        protocol: upstream.protocol,
        method: Method::GET,
        url,
        query: query.map(ToString::to_string),
        api_key,
        client_headers: client_headers.clone(),
        derived_headers: HeaderMap::new(),
        body: Bytes::new(),
        mode: ResponseMode::Models {
            protocol: upstream.protocol,
            codex_client,
            allow_etag,
            models: upstream.models.clone(),
        },
        conditional: allow_etag,
        follow_redirects,
    }
}

fn has_client_version(query: Option<&str>) -> bool {
    query.is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == "client_version")
    })
}

struct GenerateRequest<'a> {
    input: InputFormat,
    client_headers: &'a HeaderMap,
    method: Method,
    query: Option<&'a str>,
    body: &'a Bytes,
    upstream_raw: bool,
    follow_redirects: bool,
}

/// A request/conversion error that is returned to the client without failover.
type PlanFailure = Box<Response<Body>>;

fn plan_generate(
    upstream: &UpstreamConfig,
    api_key: Option<String>,
    request: GenerateRequest<'_>,
) -> Result<RequestPlan, PlanFailure> {
    let GenerateRequest {
        input,
        client_headers,
        method,
        query,
        body,
        upstream_raw,
        follow_redirects,
    } = request;
    let (upstream_format, path) = upstream_for_input(upstream.protocol, input);
    let mut url = upstream_url(&upstream.base_url, path, query);
    let make =
        |url: String, body: Bytes, derived_headers: HeaderMap, mode: ResponseMode| RequestPlan {
            protocol: upstream.protocol,
            method: method.clone(),
            url,
            query: query.map(ToString::to_string),
            api_key: api_key.clone(),
            client_headers: client_headers.clone(),
            derived_headers,
            body,
            mode,
            conditional: false,
            follow_redirects,
        };
    if upstream.protocol != Protocol::Codex && is_transparent(input, upstream_format) {
        debug!(target: "llmapi", path, "transparent proxy");
        let mut derived_headers = HeaderMap::new();
        if upstream_raw && upstream.protocol == Protocol::Llmapi {
            derived_headers.insert(
                RESPONSE_MODE_HEADER,
                HeaderValue::from_static("upstream_raw"),
            );
        }
        return Ok(make(url, body.clone(), derived_headers, ResponseMode::Raw));
    }

    debug!(target: "llmapi", ?input, ?upstream_format, "protocol adapter selected");
    let input_json: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(err) => {
            return Err(Box::new(protocol_error_response(
                &input,
                StatusCode::BAD_REQUEST,
                &err.to_string(),
                "invalid_request_error",
            )));
        }
    };
    let is_codex = upstream.protocol == Protocol::Codex;
    let client_stream = input_json["stream"].as_bool().unwrap_or(false);
    let mut upstream_json = if is_codex && input == InputFormat::OpenAiResponses {
        input_json
    } else {
        let llm_request = match input_to_llm(&input, input_json) {
            Ok(request) => request,
            Err(err) => {
                return Err(Box::new(protocol_error_response(
                    &input,
                    StatusCode::BAD_REQUEST,
                    &err.to_string(),
                    "invalid_request_error",
                )));
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
        if upstream_format == UpstreamFormat::Gemini {
            url = gemini_generate_url(
                &upstream.base_url,
                &llm_request.model,
                llm_request.stream,
                query,
            )
            .map_err(|err| {
                Box::new(protocol_error_response(
                    &input,
                    StatusCode::BAD_REQUEST,
                    &err.to_string(),
                    "invalid_request_error",
                ))
            })?;
        }
        match llm_to_upstream(upstream_format, &llm_request) {
            Ok(value) => value,
            Err(err) => {
                let (status, error_type) = if matches!(&err, AdapterError::Unsupported(_)) {
                    (StatusCode::BAD_REQUEST, "invalid_request_error")
                } else {
                    (StatusCode::BAD_GATEWAY, "api_error")
                };
                return Err(Box::new(protocol_error_response(
                    &input,
                    status,
                    &err.to_string(),
                    error_type,
                )));
            }
        }
    };
    if is_codex && let Err(err) = codex::prepare_request(&mut upstream_json) {
        return Err(Box::new(protocol_error_response(
            &input,
            StatusCode::BAD_REQUEST,
            &err.to_string(),
            "invalid_request_error",
        )));
    }

    let mut derived_headers = HeaderMap::new();
    if is_codex {
        // Use the cache key as a fallback for clients that omit session headers.
        if let Some(key) = upstream_json["prompt_cache_key"]
            .as_str()
            .and_then(|key| HeaderValue::from_str(key).ok())
        {
            for name in ["session-id", "thread-id"] {
                if !client_headers.contains_key(name) {
                    derived_headers.insert(name, key.clone());
                }
            }
        }
    }
    let mode = if upstream_raw {
        ResponseMode::RawUpstream
    } else if is_codex {
        ResponseMode::Codex {
            input,
            stream: client_stream,
        }
    } else {
        ResponseMode::Converted {
            input,
            upstream: upstream_format,
        }
    };
    Ok(make(
        url,
        Bytes::from(serde_json::to_vec(&upstream_json).unwrap()),
        derived_headers,
        mode,
    ))
}

async fn send_plan(
    state: &AppState,
    plan: &RequestPlan,
    timeout: Option<std::time::Duration>,
) -> Result<reqwest::Response, SendError> {
    let extracted = if state.config.api_key.is_none() {
        auth::extract_api_key(&plan.client_headers, plan.query.as_deref())
    } else {
        None
    };
    let mut headers = HeaderMap::new();
    auth::apply_api_key(
        &mut headers,
        plan.protocol,
        plan.api_key.as_deref(),
        extracted.as_deref(),
    );
    auth::apply_protocol_headers(&mut headers, plan.protocol, &plan.client_headers);
    for (name, value) in &plan.derived_headers {
        headers.insert(name.clone(), value.clone());
    }
    if plan.protocol == Protocol::Codex
        && let Some(auth) = &state.codex_headers
    {
        codex::apply_headers(&mut headers, auth, plan.method == Method::POST);
    }
    if plan.conditional
        && let Some(etag) = plan.client_headers.get(header::IF_NONE_MATCH)
    {
        headers.insert(header::IF_NONE_MATCH, etag.clone());
    }
    let client = if !plan.follow_redirects {
        &state.lb_client
    } else if plan.protocol == Protocol::Gemini {
        &state.gemini_client
    } else {
        &state.client
    };
    let request = client
        .request(plan.method.clone(), &plan.url)
        .headers(headers)
        .header(header::CONTENT_TYPE, "application/json")
        .body(plan.body.clone());
    match timeout {
        Some(limit) => match tokio::time::timeout(limit, request.send()).await {
            Ok(result) => result.map_err(SendError::Request),
            Err(_) => Err(SendError::Timeout),
        },
        None => request.send().await.map_err(SendError::Request),
    }
}

async fn build_response(mode: ResponseMode, upstream: reqwest::Response) -> Response<Body> {
    match mode {
        ResponseMode::Models {
            protocol,
            codex_client,
            allow_etag,
            models,
        } => models_response(upstream, protocol, codex_client, allow_etag, &models).await,
        ResponseMode::Raw | ResponseMode::RawUpstream => raw_upstream_response(upstream),
        ResponseMode::Converted {
            input,
            upstream: format,
        } => convert_response_back(upstream, &input, format).await,
        ResponseMode::Codex { input, stream } => codex_response(upstream, &input, stream).await,
    }
}

async fn lb_dispatch(
    state: AppState,
    name: &str,
    members: &[String],
    client_headers: &HeaderMap,
    request: LbRequest<'_>,
) -> Response<Body> {
    let input = request.input();
    let mut failures = Vec::new();
    for (index, member) in members.iter().enumerate() {
        match attempt_member(&state, member, client_headers, &request).await {
            Attempt::Response(response) => return response,
            Attempt::Switch(reason) => {
                warn!(
                    target: "llmapi",
                    lb = %name,
                    member = %member,
                    attempt = index + 1,
                    reason = %reason,
                    "load balancer member unavailable; trying next member"
                );
                failures.push(format!("{member}: {reason}"));
            }
        }
    }
    let summary = if failures.is_empty() {
        "no members attempted".to_string()
    } else {
        failures.join("; ")
    };
    protocol_error_response(
        &input,
        StatusCode::SERVICE_UNAVAILABLE,
        &format!(
            "load balancer `{name}` has no available member after {} attempt(s): {summary}",
            members.len()
        ),
        "api_error",
    )
}

async fn attempt_member(
    state: &AppState,
    member: &str,
    client_headers: &HeaderMap,
    request: &LbRequest<'_>,
) -> Attempt {
    let Some(upstream) = state
        .config
        .providers
        .get(member)
        .and_then(ProviderConfig::as_upstream)
    else {
        return Attempt::Switch("provider is not configured".into());
    };
    if let LbRequest::Generate { body, .. } = request
        && let Err(reason) = check_request_model(upstream, body)
    {
        return Attempt::Switch(reason.into());
    }
    if upstream.protocol == Protocol::Codex && state.codex_headers.is_none() {
        return Attempt::Switch("Codex credentials are unavailable".into());
    }
    let api_key = match upstream.api_key() {
        Ok(api_key) => api_key,
        Err(err) => return Attempt::Switch(format!("credentials unavailable: {err}")),
    };
    let plan = match request {
        LbRequest::Models {
            query,
            codex_client,
        } => plan_models(
            upstream,
            api_key,
            client_headers,
            *query,
            *codex_client,
            false,
            false,
        ),
        LbRequest::Generate {
            method,
            query,
            input,
            body,
            upstream_raw,
        } => match plan_generate(
            upstream,
            api_key,
            GenerateRequest {
                input: *input,
                client_headers,
                method: method.clone(),
                query: *query,
                body,
                upstream_raw: *upstream_raw,
                follow_redirects: false,
            },
        ) {
            Ok(plan) => plan,
            // Recognized request/conversion errors are final: never replayed.
            Err(response) => return Attempt::Response(*response),
        },
    };
    let timeout = state.lb_header_timeout;
    let upstream_response = match send_plan(state, &plan, Some(timeout)).await {
        Ok(response) => response,
        Err(SendError::Timeout) => {
            return Attempt::Switch(format!(
                "response header timeout after {}ms",
                timeout.as_millis()
            ))
        }
        Err(SendError::Request(err)) => return Attempt::Switch(redact::transport_error(err)),
    };
    if lb::is_retryable_status(upstream_response.status()) {
        return Attempt::Switch(format!("upstream status {}", upstream_response.status()));
    }
    info!(
        target: "llmapi",
        member = %member,
        method = %plan.method,
        url = %redact::url(upstream_response.url().as_str()),
        status = %upstream_response.status(),
        "load balancer member responded"
    );
    Attempt::Response(build_response(plan.mode, upstream_response).await)
}

pub fn response(status: StatusCode, body: Bytes, content_type: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}
async fn codex_response(
    upstream: reqwest::Response,
    input: &InputFormat,
    stream: bool,
) -> Response<Body> {
    if !upstream.status().is_success() {
        return converted_error_response(upstream, input).await;
    }
    if stream {
        return if *input == InputFormat::OpenAiResponses {
            raw_upstream_response(upstream)
        } else {
            converted_sse_response(upstream, *input, UpstreamFormat::OpenAiResponses)
        };
    }
    let headers = upstream.headers().clone();
    match collect_codex_response(upstream).await {
        Ok(value) if *input == InputFormat::OpenAiResponses => {
            let mut result = response(
                StatusCode::OK,
                Bytes::from(value.to_string()),
                "application/json",
            );
            copy_response_metadata_headers(&headers, result.headers_mut());
            result
        }
        Ok(value) => {
            converted_json_response(value, input, UpstreamFormat::OpenAiResponses, &headers)
        }
        Err(err) => {
            let mut result =
                protocol_error_response(input, StatusCode::BAD_GATEWAY, &err, "api_error");
            copy_response_metadata_headers(&headers, result.headers_mut());
            result
        }
    }
}

async fn collect_codex_response(upstream: reqwest::Response) -> Result<Value, String> {
    let mut stream = upstream.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut size = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(redact::transport_error)?;
        size += chunk.len();
        if size > MAX_JSON_BODY_BYTES {
            return Err("Codex response exceeds the buffered response limit".into());
        }
        for event in decoder.push(&chunk).map_err(|err| err.to_string())? {
            if let Some(response) = codex_terminal_response(&event)? {
                return Ok(response);
            }
        }
    }
    for event in decoder.finish().map_err(|err| err.to_string())? {
        if let Some(response) = codex_terminal_response(&event)? {
            return Ok(response);
        }
    }
    Err("Codex stream ended before a terminal event".into())
}

fn codex_terminal_response(event: &SseEvent) -> Result<Option<Value>, String> {
    if event.data.is_empty() || event.data == "[DONE]" {
        return Ok(None);
    }
    let mut value: Value = serde_json::from_str(&event.data).map_err(|err| err.to_string())?;
    match value["type"].as_str().or(event.event.as_deref()) {
        Some("response.completed" | "response.incomplete") => {
            if !value["response"].is_object() {
                return Err("Codex terminal event has no response object".into());
            }
            Ok(Some(value["response"].take()))
        }
        Some("response.failed" | "error") => Err(value["response"]["error"]["message"]
            .as_str()
            .or_else(|| value["error"]["message"].as_str())
            .or_else(|| value["message"].as_str())
            .unwrap_or("Codex response failed")
            .to_owned()),
        _ => Ok(None),
    }
}

fn input_to_llm(input: &InputFormat, value: Value) -> Result<LLMRequest, AdapterError> {
    match input {
        InputFormat::OpenAiChat => OpenAiChatAdapter::to_llm_request(value),
        InputFormat::OpenAiResponses => OpenAiResponsesAdapter::to_llm_request(value),
        InputFormat::AnthropicMessages => AnthropicAdapter::to_llm_request(value),
        InputFormat::Models | InputFormat::GeminiInteractions => {
            unreachable!("native requests are forwarded before conversion")
        }
    }
}

fn llm_to_upstream(upstream: UpstreamFormat, request: &LLMRequest) -> Result<Value, AdapterError> {
    match upstream {
        UpstreamFormat::OpenAiChat => OpenAiChatAdapter::from_llm_request(request),
        UpstreamFormat::OpenAiResponses => OpenAiResponsesAdapter::from_llm_request(request),
        UpstreamFormat::AnthropicMessages => AnthropicAdapter::from_llm_request(request),
        UpstreamFormat::Gemini => GeminiAdapter::from_llm_request(request),
    }
}

pub(crate) fn gemini_generate_url(
    base_url: &str,
    model: &str,
    stream: bool,
    query: Option<&str>,
) -> Result<String, AdapterError> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AdapterError::InvalidField("model"));
    }
    if !stream {
        return Ok(upstream_url(
            base_url,
            &format!("/models/{model}:generateContent"),
            query,
        ));
    }
    let mut stream_query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(query) = query {
        stream_query.extend_pairs(
            url::form_urlencoded::parse(query.as_bytes()).filter(|(key, _)| key != "alt"),
        );
    }
    stream_query.append_pair("alt", "sse");
    Ok(upstream_url(
        base_url,
        &format!("/models/{model}:streamGenerateContent"),
        Some(&stream_query.finish()),
    ))
}

pub(crate) fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
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
    converted_json_response(value, input, upstream, &response_headers)
}

fn converted_json_response(
    value: Value,
    input: &InputFormat,
    upstream: UpstreamFormat,
    response_headers: &HeaderMap,
) -> Response<Body> {
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
    copy_response_metadata_headers(response_headers, response.headers_mut());
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

pub(crate) async fn read_upstream_body(
    upstream: reqwest::Response,
    limit: usize,
) -> Result<Bytes, String> {
    if upstream
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("upstream response body exceeds {limit} bytes"));
    }
    let mut stream = upstream.bytes_stream();
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| {
            format!(
                "read upstream response body: {}",
                redact::transport_error(err)
            )
        })?;
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
        InputFormat::GeminiInteractions => json!({
            "error": {"code": status.as_u16(), "message": message}
        }),
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
            || name == header::LOCATION
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
            || name == header::LOCATION
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
        UpstreamFormat::Gemini => GeminiAdapter::to_llm_response(value),
    }
}

fn llm_to_input(input: &InputFormat, response: &LLMResponse) -> Result<Value, AdapterError> {
    match input {
        InputFormat::OpenAiChat => OpenAiChatAdapter::from_llm_response(response),
        InputFormat::OpenAiResponses => OpenAiResponsesAdapter::from_llm_response(response),
        InputFormat::AnthropicMessages => AnthropicAdapter::from_llm_response(response),
        InputFormat::Models | InputFormat::GeminiInteractions => {
            unreachable!("native responses are forwarded before conversion")
        }
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
        UpstreamFormat::Gemini => GeminiAdapter::parse_stream_event(event, state),
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
        InputFormat::Models | InputFormat::GeminiInteractions => {
            unreachable!("native responses are forwarded before conversion")
        }
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
                    let message = redact::transport_error(err);
                    for output in stream_error_events(&input, &mut target_state, message) {
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
        .get(RESPONSE_MODE_HEADER)
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
