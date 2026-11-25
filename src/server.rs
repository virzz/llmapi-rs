use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, State, ws::WebSocketUpgrade},
    http::{HeaderMap, Method, Request, Response, StatusCode, Uri},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{any, get},
};
use serde_json::json;
use tokio::sync::{oneshot, watch};
use tracing::{info, warn};

use super::{
    auth, codex,
    config::{Config, ConfigError, ProviderConfig},
    lb, proxy, redact,
};

#[derive(Clone)]
pub struct AppState {
    pub(crate) discovery: Option<Arc<crate::discovery::Discovery>>,
    pub(crate) usage: Option<Arc<crate::usage::Store>>,
    pub config: Arc<Config>,
    pub client: reqwest::Client,
    pub gemini_client: reqwest::Client,
    /// Client used for load-balancer attempts: never follows redirects, so one
    /// member attempt always maps to at most one upstream request.
    pub lb_client: reqwest::Client,
    pub codex_headers: Option<HeaderMap>,
    /// Round-robin cursors published together with this config version.
    pub lb: Arc<lb::State>,
    /// Upper bound for waiting on one LB member's response headers.
    pub lb_header_timeout: Duration,
}

/// One atomically published config version plus its load-balancer cursors.
pub struct Runtime {
    instance_id: String,
    discovery: Option<Arc<crate::discovery::Discovery>>,
    usage: Option<Arc<crate::usage::Store>>,
    pub config: Arc<Config>,
    pub lb: Arc<lb::State>,
    lb_header_timeout: Duration,
}

impl Runtime {
    fn new(config: Config) -> Self {
        let lb = Arc::new(lb::State::new(&config));
        Self {
            instance_id: config
                .instance_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            discovery: None,
            usage: config
                .usage_db
                .path()
                .map(crate::usage::Store::new)
                .map(Arc::new),
            config: Arc::new(config),
            lb,
            lb_header_timeout: lb::ATTEMPT_HEADER_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_lb_header_timeout(config: Config, lb_header_timeout: Duration) -> Self {
        Self {
            lb_header_timeout,
            ..Self::new(config)
        }
    }
}

#[derive(Clone)]
struct ServerState {
    runtime: watch::Receiver<Arc<Runtime>>,
    client: reqwest::Client,
    gemini_client: reqwest::Client,
    lb_client: reqwest::Client,
    codex_auth: watch::Receiver<Option<HeaderMap>>,
}

impl ServerState {
    fn snapshot(&self) -> AppState {
        let runtime = self.runtime.borrow().clone();
        let codex_headers = if runtime.config.injects.codex {
            self.codex_auth.borrow().clone()
        } else {
            None
        };
        let config = if codex_headers.is_some() {
            Arc::new(runtime.config.as_ref().clone().with_codex(true))
        } else {
            runtime.config.clone()
        };
        AppState {
            discovery: runtime.discovery.clone(),
            usage: runtime.usage.clone(),
            config,
            client: self.client.clone(),
            gemini_client: self.gemini_client.clone(),
            lb_client: self.lb_client.clone(),
            codex_headers,
            lb: runtime.lb.clone(),
            lb_header_timeout: runtime.lb_header_timeout,
        }
    }
}

pub fn app(config: Config) -> Router {
    app_runtime(Runtime::new(config))
}

fn app_runtime(runtime: Runtime) -> Router {
    let auth_path = if runtime.config.injects.codex {
        codex::auth_path()
    } else {
        None
    };
    let (_, runtime) = watch::channel(Arc::new(runtime));
    app_with_auth(runtime, codex::watch_auth(auth_path))
}

fn app_with_auth(
    runtime: watch::Receiver<Arc<Runtime>>,
    codex_auth: watch::Receiver<Option<HeaderMap>>,
) -> Router {
    let client = build_client(true);
    let lb_client = build_client(false);
    let state = ServerState {
        runtime,
        client,
        gemini_client: auth::gemini_http_client().expect("build Gemini HTTP client"),
        lb_client,
        codex_auth,
    };

    Router::new()
        .route("/providers", get(providers))
        .route("/{*path}", any(handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        .route("/health", get(health))
        .layer(middleware::from_fn_with_state(state.clone(), access_log))
        .with_state(state)
}

async fn health(State(state): State<ServerState>) -> impl IntoResponse {
    let id = state.runtime.borrow().instance_id.clone();
    Json(json!({"service": "llmapi", "instance_id": id}))
}

fn build_client(follow_redirects: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(15));
    if !follow_redirects {
        // One member attempt must map to exactly one upstream request: no
        // redirects and no implicit protocol-NACK retries (for example an
        // HTTP/2 REFUSED_STREAM must surface as a normal LB failure).
        builder = builder
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never());
    }
    builder.build().unwrap_or_else(|err| {
        warn!(target: "llmapi", error = %err, "failed to configure upstream client");
        if follow_redirects {
            reqwest::Client::new()
        } else {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .unwrap_or_default()
        }
    })
}

async fn require_api_key(
    State(state): State<ServerState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let config = state.runtime.borrow().config.clone();
    match config.api_key() {
        Ok(None) => next.run(request).await,
        Ok(Some(expected)) => {
            let supplied = auth::extract_api_key(request.headers(), request.uri().query());
            if supplied.as_deref() == Some(expected.as_str()) {
                next.run(request).await
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }
        Err(err) => {
            warn!(target: "llmapi", error = %err, "server API key is unavailable");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn access_log(
    State(state): State<ServerState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let enabled = state.runtime.borrow().config.access_log;
    if !enabled {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let uri = request.uri().clone();
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.to_string())
        .unwrap_or_else(|| "-".into());
    let started = Instant::now();
    let response = next.run(request).await;
    info!(
        target: "llmapi::access",
        remote = %remote,
        request = %redact::request(method.as_str(), &uri),
        status = response.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request completed"
    );
    response
}

async fn providers(State(state): State<ServerState>) -> impl IntoResponse {
    let mut config = state.snapshot().config.as_ref().clone();
    for provider in config.providers.values_mut() {
        if let ProviderConfig::Upstream(upstream) = provider {
            if upstream.api_key.is_some() {
                upstream.api_key = Some("***".into());
            }
            // Never expose credentials embedded in the configured URL.
            upstream.base_url = redact::url(&upstream.base_url);
        }
    }
    Json(json!({"default": config.default, "providers": config.providers}))
}

pub async fn serve(addr: SocketAddr, mut config: Config) -> Result<()> {
    let discovery = Arc::new(crate::discovery::Discovery::new(None));
    discovery.initialize(&mut config).await?;
    let mut runtime = Runtime::new(config);
    runtime.discovery = Some(discovery);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(target: "llmapi", address = %listener.local_addr()?, "server listening");
    axum::serve(
        listener,
        app_runtime(runtime).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

pub(crate) async fn serve_reloading(
    addr: SocketAddr,
    mut config: Config,
    path: PathBuf,
    server_override: bool,
    default_override: Option<String>,
) -> Result<()> {
    let discovery = Arc::new(crate::discovery::Discovery::new(Some(path.clone())));
    discovery.initialize(&mut config).await?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(target: "llmapi", address = %listener.local_addr()?, "server listening");
    let mut listen_address = config.server.clone();
    let mut runtime = Runtime::new(config);
    runtime.discovery = Some(discovery);
    let (sender, mut receiver) = watch::channel(Arc::new(runtime));
    let watcher = watch_config(
        path,
        listen_address.clone(),
        server_override,
        default_override,
        sender,
    );
    tokio::pin!(watcher);
    let (mut shutdown, shutdown_receiver) = oneshot::channel();
    let codex_auth = watch_reloading_codex_auth(receiver.clone(), codex::auth_path());
    let mut server = Box::pin(run_server(
        listener,
        receiver.clone(),
        codex_auth.clone(),
        shutdown_receiver,
    ));
    let mut retry = tokio::time::interval(Duration::from_millis(500));
    let mut last_bind_error = None;

    loop {
        tokio::select! {
            result = &mut server => return result,
            _ = &mut watcher => return Ok(()),
            changed = receiver.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            _ = retry.tick() => {}
        }
        let requested_address = receiver.borrow().config.server.clone();
        if requested_address == listen_address {
            last_bind_error = None;
            continue;
        }
        let listener = async {
            let addr: SocketAddr = requested_address.parse()?;
            Ok::<_, anyhow::Error>(tokio::net::TcpListener::bind(addr).await?)
        }
        .await;
        let listener = match listener {
            Ok(listener) => listener,
            Err(err) => {
                let message = err.to_string();
                if last_bind_error.as_deref() != Some(message.as_str()) {
                    warn!(target: "llmapi", address = %requested_address, error = %err, "listen address change failed; keeping previous listener");
                    last_bind_error = Some(message);
                }
                continue;
            }
        };
        last_bind_error = None;
        info!(target: "llmapi", address = %listener.local_addr()?, "server listening");
        let (next_shutdown, shutdown_receiver) = oneshot::channel();
        let old_server = std::mem::replace(
            &mut server,
            Box::pin(run_server(
                listener,
                receiver.clone(),
                codex_auth.clone(),
                shutdown_receiver,
            )),
        );
        tokio::spawn(async move {
            if let Err(err) = old_server.await {
                warn!(target: "llmapi", error = %err, "previous listener stopped with error");
            }
        });
        let _ = shutdown.send(());
        shutdown = next_shutdown;
        listen_address = requested_address;
    }
}

async fn run_server(
    listener: tokio::net::TcpListener,
    config: watch::Receiver<Arc<Runtime>>,
    codex_auth: watch::Receiver<Option<HeaderMap>>,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    axum::serve(
        listener,
        app_with_auth(config, codex_auth).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = shutdown.await;
    })
    .await?;
    Ok(())
}

fn watch_reloading_codex_auth(
    mut runtime: watch::Receiver<Arc<Runtime>>,
    path: Option<PathBuf>,
) -> watch::Receiver<Option<HeaderMap>> {
    let mut enabled = runtime.borrow_and_update().config.injects.codex;
    let mut auth = codex::watch_auth(path.clone().filter(|_| enabled));
    let (sender, receiver) = watch::channel(auth.borrow_and_update().clone());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = sender.closed() => return,
                changed = runtime.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let next = runtime.borrow_and_update().config.injects.codex;
                    if enabled != next {
                        enabled = next;
                        auth = codex::watch_auth(path.clone().filter(|_| enabled));
                        sender.send_replace(auth.borrow_and_update().clone());
                    }
                }
                Ok(()) = auth.changed() => {
                    sender.send_replace(auth.borrow_and_update().clone());
                }
            }
        }
    });
    receiver
}

async fn watch_config(
    path: PathBuf,
    listen_address: String,
    server_override: bool,
    default_override: Option<String>,
    sender: watch::Sender<Arc<Runtime>>,
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
                match config.set_default(default) {
                    Err(ConfigError::CodexInjectionDisabled) => {}
                    result => result?,
                }
            }
            if server_override {
                config.server = listen_address.clone();
            }
            Ok(config)
        });
        last_body = Some(body);
        match config {
            Ok(config) => {
                last_error = None;
                if sender.borrow().config.as_ref() != &config {
                    let previous = sender.borrow().clone();
                    let mut next = Runtime::new(config);
                    next.discovery = previous.discovery.clone();
                    next.instance_id = previous.instance_id.clone();
                    sender.send_replace(Arc::new(next));
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
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
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

    let snapshot = state.snapshot();
    proxy::handle_with_websocket(snapshot, headers, request, ws.ok())
}

#[cfg(test)]
mod tests {
    mod codex_tests;
    mod lb_tests;
    use std::{collections::BTreeMap, convert::Infallible, fs, path::Path, sync::Arc};

    use axum::{
        Json, Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, Method, StatusCode},
        response::IntoResponse,
        routing::any,
    };
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::config::{Config, Protocol, ProviderConfig};

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Value,
    }

    #[tokio::test]
    async fn health_exposes_only_stable_identity_without_authentication() {
        let mut config = test_config("http://127.0.0.1:1");
        config.instance_id = Some("test-instance".into());
        config.api_key = Some("server-secret".into());
        let proxy = start_proxy(config.clone()).await;
        let client = reqwest::Client::new();
        for _ in 0..2 {
            let response = client
                .get(format!("http://{proxy}/health"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({"service": "llmapi", "instance_id": "test-instance"})
            );
        }
        let protected = client
            .get(format!("http://{proxy}/providers"))
            .send()
            .await
            .unwrap();
        assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
        let restarted = start_proxy(config).await;
        assert_eq!(
            client
                .get(format!("http://{restarted}/health"))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["instance_id"],
            "test-instance"
        );
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
        body: Bytes,
    ) -> Response<Body> {
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
            instance_id: None,
            server: "127.0.0.1:0".into(),
            access_log: false,
            usage_db: false.into(),
            injects: Default::default(),
            api_key: None,
            default: "deepseek".into(),
            providers: BTreeMap::from([
                (
                    "deepseek".into(),
                    ProviderConfig::upstream(
                        Protocol::OpenAiChat,
                        base_url,
                        Some("sk-deepseek".into()),
                    ),
                ),
                (
                    "openai".into(),
                    ProviderConfig::upstream(
                        Protocol::OpenAiResponses,
                        base_url,
                        Some("sk-openai".into()),
                    ),
                ),
                (
                    "anthropic".into(),
                    ProviderConfig::upstream(
                        Protocol::Anthropic,
                        base_url,
                        Some("sk-anthropic".into()),
                    ),
                ),
            ]),
        }
    }

    fn atomic_config_path(path: &Path) -> std::path::PathBuf {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("yaml");
        path.with_file_name(format!("config.next.{extension}"))
    }

    /// Replace the config file atomically so the watcher can never observe a
    /// partially written body.
    fn write_config_atomically(path: &Path, body: &str) {
        let temporary = atomic_config_path(path);
        fs::write(&temporary, body).unwrap();
        fs::rename(&temporary, path).unwrap();
    }

    fn save_config_atomically(config: &Config, path: &Path) {
        let temporary = atomic_config_path(path);
        config.save(&temporary).unwrap();
        fs::rename(&temporary, path).unwrap();
    }

    async fn start_proxy(config: Config) -> SocketAddr {
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_, runtime) = watch::channel(Arc::new(Runtime::new(config)));
            let (_, auth) = watch::channel(None);
            axum::serve(proxy_listener, app_with_auth(runtime, auth))
                .await
                .unwrap();
        });
        address
    }

    #[tokio::test]
    async fn usage_db_records_the_serving_member_before_response_conversion() {
        let (upstream, _) = start_upstream(json!({
            "model":"upstream-model", "choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}
        })).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let mut config = test_config(&upstream);
        config.usage_db = Default::default();
        config
            .providers
            .insert("pool".into(), ProviderConfig::lb(vec!["deepseek".into()]));
        let config_path = dir.path().join("config.toml");
        config.save(&config_path).unwrap();
        let runtime = Arc::new(Runtime::new(Config::load(config_path).unwrap()));
        let usage = runtime.usage.clone().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app_with_auth(watch::channel(runtime).1, watch::channel(None).1),
            )
            .await
            .unwrap();
        });
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/pool/v1/responses"))
            .json(&json!({"model":"alias","input":"ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap()["usage"]["total_tokens"],
            10
        );
        usage.flush().await;
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn
            .query("SELECT provider, model, total_tokens FROM token_usage", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "deepseek");
        assert_eq!(row.get::<String>(1).unwrap(), "upstream-model");
        assert_eq!(row.get::<i64>(2).unwrap(), 10);
        assert!(rows.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn interaction_usage_is_recorded_once_across_repeated_gets() {
        let (upstream, _) = start_upstream(json!({
            "id":"interaction_1", "model":"gemini", "status":"completed",
            "usage":{"total_input_tokens":7,"total_output_tokens":3,"total_tokens":10}
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = test_config(&upstream);
        config.usage_db = true.into();
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, upstream, None),
        );
        config.save(&path).unwrap();
        let runtime = Arc::new(Runtime::new(Config::load(&path).unwrap()));
        let usage = runtime.usage.clone().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app_with_auth(watch::channel(runtime).1, watch::channel(None).1),
            )
            .await
            .unwrap();
        });
        for _ in 0..2 {
            let response = reqwest::Client::new()
                .get(format!(
                    "http://{proxy}/gemini/v1/interactions/interaction_1"
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap()["usage"]["total_tokens"],
                10
            );
        }
        usage.flush().await;
        let db = turso::Builder::new_local(dir.path().join("usage.db").to_str().unwrap())
            .experimental_multiprocess_wal(true)
            .read_only(true)
            .build()
            .await
            .unwrap();
        let connection = db.connect().unwrap();
        let mut rows = connection
            .query("SELECT count(*), sum(total_tokens) FROM token_usage", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 1);
        assert_eq!(row.get::<i64>(1).unwrap(), 10);
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
        config
            .providers
            .get_mut("openai")
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .base_url = second_upstream;
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        config.save(&path).unwrap();

        let (sender, mut receiver) = watch::channel(Arc::new(Runtime::new(config.clone())));
        let watcher = tokio::spawn(watch_config(
            path.clone(),
            config.server.clone(),
            true,
            None,
            sender,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_receiver = receiver.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app_with_auth(server_receiver, watch::channel(None).1),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::new();
        let list_models = || client.get(format!("http://{address}/models"));

        let first: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(first["data"][0]["id"], "first");

        config.usage_db = true.into();
        config.default = "openai".into();
        config.server = "127.0.0.1:9090".into();
        save_config_atomically(&config, &path);
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(receiver.borrow().usage.is_some());
        assert_eq!(
            receiver.borrow().config.usage_db.path(),
            Some(directory.path().join("usage.db"))
        );
        assert_eq!(receiver.borrow().config.default, "openai");
        assert_eq!(receiver.borrow().config.server, "127.0.0.1:0");
        let second: Value = list_models().send().await.unwrap().json().await.unwrap();
        assert_eq!(second["data"][0]["id"], "second");

        write_config_atomically(&path, "default: missing\nproviders: {}\n");
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

        config.usage_db = false.into();
        config.default = "deepseek".into();
        save_config_atomically(&config, &path);
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(receiver.borrow().usage.is_none());
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
        active.server = "127.0.0.1:8081".into();
        let (sender, mut receiver) = watch::channel(Arc::new(Runtime::new(active)));
        let watcher = tokio::spawn(watch_config(
            path.clone(),
            "127.0.0.1:8081".into(),
            true,
            Some("openai".into()),
            sender,
        ));

        file_config
            .providers
            .get_mut("openai")
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .base_url = "https://second.test".into();
        file_config.server = "127.0.0.1:9090".into();
        save_config_atomically(&file_config, &path);
        tokio::time::timeout(Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receiver.borrow().config.default, "openai");
        assert_eq!(receiver.borrow().config.server, "127.0.0.1:8081");
        assert_eq!(
            receiver.borrow().config.providers["openai"]
                .as_upstream()
                .unwrap()
                .base_url,
            "https://second.test"
        );

        file_config.providers.remove("openai");
        save_config_atomically(&file_config, &path);
        assert!(
            tokio::time::timeout(Duration::from_millis(800), receiver.changed())
                .await
                .is_err()
        );
        assert!(receiver.borrow().config.providers.contains_key("openai"));
        watcher.abort();
    }

    #[tokio::test]
    async fn reloads_listen_address_and_keeps_old_listener_on_bind_failure() {
        let initial = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_address = initial.local_addr().unwrap();
        drop(initial);
        let mut config = test_config("https://example.test");
        config.server = first_address.to_string();
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        config.save(&path).unwrap();
        let server = tokio::spawn(serve_reloading(
            first_address,
            config.clone(),
            path.clone(),
            false,
            None,
        ));
        let client = reqwest::Client::new();
        let first_url = format!("http://{first_address}/providers");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if client.get(&first_url).send().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_address = occupied.local_addr().unwrap();
        config.server = second_address.to_string();
        config.default = "openai".into();
        save_config_atomically(&config, &path);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let still_first: Value = client
            .get(&first_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(still_first["default"], "openai");
        drop(occupied);

        let second_url = format!("http://{second_address}/providers");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(response) = client.get(&second_url).send().await {
                    let body: Value = response.json().await.unwrap();
                    assert_eq!(body["default"], "openai");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if tokio::net::TcpStream::connect(first_address).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn config_watcher_stops_when_server_drops() {
        let config = test_config("https://example.test");
        let (sender, receiver) = watch::channel(Arc::new(Runtime::new(config.clone())));
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
    async fn llmapi_relay_routes_each_protocol_through_the_remote_provider() {
        let (upstream, captured) = start_upstream(json!({
            "id": "resp_relay",
            "object": "response",
            "model": "shared-model",
            "output_text": "relayed",
            "status": "completed"
        }))
        .await;
        let mut remote = test_config(&upstream);
        remote.api_key = Some("remote-key".into());
        let remote = start_proxy(remote).await;
        let mut local = Config {
            usage_db: false.into(),
            default: "relay".into(),
            api_key: Some("local-key".into()),
            ..Config::default()
        };
        local.providers.insert(
            "relay".into(),
            ProviderConfig::upstream(
                Protocol::Llmapi,
                format!("http://{remote}/openai/v1"),
                Some("remote-key".into()),
            ),
        );
        let local = start_proxy(local).await;
        let client = reqwest::Client::new();
        let messages = json!([{"role": "user", "content": "hi"}]);

        for (path, payload, pointer) in [
            (
                "chat/completions",
                json!({"model": "shared-model", "messages": messages}),
                "/choices/0/message/content",
            ),
            (
                "responses",
                json!({"model": "shared-model", "input": "hi", "previous_response_id": "resp_old"}),
                "/output_text",
            ),
            (
                "messages",
                json!({"model": "shared-model", "messages": messages, "max_tokens": 16}),
                "/content/0/text",
            ),
        ] {
            let response = client
                .post(format!(
                    "http://{local}/relay/v1/{path}?key=local-key&trace=test"
                ))
                .header("openai-beta", "responses=v1")
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = response.json().await.unwrap();
            assert_eq!(body.pointer(pointer).unwrap(), "relayed");
            let request = captured.lock().await;
            let request = request.as_ref().unwrap();
            assert_eq!(
                request.uri.path_and_query().unwrap().as_str(),
                "/responses?trace=test"
            );
            assert_eq!(request.headers["authorization"], "Bearer sk-openai");
            assert_eq!(request.headers["openai-beta"], "responses=v1");
            assert_eq!(request.body["model"], "shared-model");
            if path == "responses" {
                assert_eq!(request.body, payload);
            }
        }

        let raw: Value = client
            .post(format!(
                "http://{local}/relay/messages?llmapi_response_mode=upstream_raw"
            ))
            .bearer_auth("local-key")
            .json(&json!({"model": "shared-model", "messages": messages, "max_tokens": 16}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(raw["output_text"], "relayed");
    }

    #[tokio::test]
    async fn llmapi_relay_preserves_codex_catalog_metadata() {
        let catalog = json!({"models": [{"slug": "shared-model", "context_window": 42}]});
        let (upstream, _) = start_upstream(catalog.clone()).await;
        let remote = start_proxy(test_config(&upstream)).await;
        let local = Config {
            usage_db: false.into(),
            default: "relay".into(),
            providers: BTreeMap::from([(
                "relay".into(),
                ProviderConfig::upstream(Protocol::Llmapi, format!("http://{remote}"), None),
            )]),
            ..Config::default()
        };
        let local = start_proxy(local).await;

        let received: Value = reqwest::Client::new()
            .get(format!("http://{local}/models"))
            .header("user-agent", "codex_cli/test")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(received, catalog);
    }

    #[tokio::test]
    async fn llmapi_relay_preserves_sse() {
        let stream = "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
        let (upstream, _) = start_stream_upstream(vec![stream.as_bytes().to_vec()]).await;
        let remote = start_proxy(test_config(&upstream)).await;
        let local = Config {
            usage_db: false.into(),
            default: "relay".into(),
            providers: BTreeMap::from([(
                "relay".into(),
                ProviderConfig::upstream(Protocol::Llmapi, format!("http://{remote}/openai"), None),
            )]),
            ..Config::default()
        };
        let local = start_proxy(local).await;

        let response = reqwest::Client::new()
            .post(format!("http://{local}/responses"))
            .json(&json!({"model": "shared-model", "input": "hi", "stream": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.text().await.unwrap(), stream);
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
        config
            .providers
            .get_mut("openai")
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .api_key = Some("${OPENAI_API_KEY}".into());
        config
            .providers
            .get_mut("anthropic")
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .api_key = None;
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
    async fn server_api_key_protects_routes_and_is_not_forwarded() {
        let (upstream, captured) = start_upstream(json!({"object": "list", "data": []})).await;
        let mut config = test_config(&upstream);
        config.api_key = Some("client-secret".into());
        config
            .providers
            .get_mut("deepseek")
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .api_key = None;
        let proxy = start_proxy(config).await;
        let client = reqwest::Client::new();

        for path in ["providers", "models"] {
            let url = format!("http://{proxy}/{path}");
            assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
            assert_eq!(
                client
                    .get(&url)
                    .bearer_auth("wrong")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );
            assert_eq!(
                client
                    .get(&url)
                    .bearer_auth("client-secret")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        assert_eq!(
            client
                .get(format!("http://{proxy}/providers"))
                .header("x-api-key", "client-secret")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(format!("http://{proxy}/providers?key=client-secret"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let request = captured.lock().await;
        let headers = &request.as_ref().unwrap().headers;
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
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
            assert!(
                response
                    .headers()
                    .get_all("vary")
                    .iter()
                    .any(|value| value == "User-Agent")
            );
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
        assert!(
            response
                .headers()
                .get_all("vary")
                .iter()
                .any(|value| value == "User-Agent")
        );
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
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("audio")
        );
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
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("field `n`")
        );
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
    async fn gemini_provider_converts_chat_and_uses_the_model_in_the_upstream_path() {
        let (upstream, captured) = start_upstream(json!({
            "responseId": "gemini-reply",
            "modelVersion": "gemini-2.5-flash",
            "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "pong"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1, "totalTokenCount": 4}
        })).await;
        let config: Config = serde_yaml::from_str(&format!(
            "usage_db: false\ndefault: gemini\nproviders:\n  gemini:\n    type: gemini\n    baseurl: {upstream}/v1beta\n    apikey: gemini-upstream-key\n"
        )).unwrap();
        let proxy = start_proxy(config).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("http://{proxy}/gemini/v1/chat/completions?key=client-key"))
            .bearer_auth("client-key")
            .json(&json!({
                "model": "gemini-2.5-flash",
                "messages": [{"role": "system", "content": "Be brief"}, {"role": "user", "content": "ping"}],
                "max_tokens": 64,
            }))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "pong");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["total_tokens"], 4);

        let request = captured.lock().await.clone().unwrap();
        assert_eq!(
            request.uri,
            "/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(request.headers["x-goog-api-key"], "gemini-upstream-key");
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(
            request.body["systemInstruction"]["parts"][0]["text"],
            "Be brief"
        );
        assert_eq!(request.body["contents"][0]["parts"][0]["text"], "ping");
        assert_eq!(request.body["generationConfig"]["maxOutputTokens"], 64);
        assert!(request.body.get("model").is_none());

        let rejected = client.post(format!("http://{proxy}/chat/completions"))
            .json(&json!({"model": "gemini/../other", "messages": [{"role": "user", "content": "ping"}]}))
            .send().await.unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gemini_rejects_unsupported_responses_options_before_forwarding() {
        let (upstream, captured) = start_upstream(json!({})).await;
        let mut config = test_config(&upstream);
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, upstream, None),
        );
        let proxy = start_proxy(config).await;
        for (field, value) in [
            ("previous_response_id", json!("resp_1")),
            ("conversation", json!({"id": "conv_1"})),
            ("reasoning", json!({"effort": "high"})),
            ("background", json!(true)),
        ] {
            let mut input = json!({"model": "gemini", "input": "continue"});
            input[field] = value;
            let reply = reqwest::Client::new()
                .post(format!("http://{proxy}/gemini/v1/responses"))
                .json(&input)
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
            assert!(
                reply.json::<Value>().await.unwrap()["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(field)
            );
        }
        assert!(captured.lock().await.is_none());
    }

    #[tokio::test]
    async fn gemini_provider_converts_sse_with_terminal_usage() {
        let chunks = [
            json!({"responseId": "gemini-reply", "modelVersion": "gemini-2.5-flash", "candidates": [{"content": {"parts": [{"text": "pong"}]}}]}),
            json!({"candidates": [{"finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1, "totalTokenCount": 4}}),
        ].map(|value| format!("data: {value}\n\n").into_bytes());
        let (upstream, captured) = start_stream_upstream(chunks.into()).await;
        let mut config = test_config(&upstream);
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, format!("{upstream}/v1beta"), None),
        );
        let proxy = start_proxy(config).await;
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/gemini/v1/chat/completions?alt=json&key=ignored"))
            .header("x-goog-api-key", "gemini-client-key")
            .json(&json!({"model": "models/gemini-2.5-flash", "messages": [{"role": "user", "content": "ping"}], "stream": true, "stream_options": {"include_usage": true}}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let body = response.text().await.unwrap();
        let events = sse_json_events(&body);
        assert!(
            events
                .iter()
                .any(|event| event["choices"][0]["delta"]["content"] == "pong")
        );
        assert!(
            events
                .iter()
                .any(|event| event["choices"][0]["finish_reason"] == "stop")
        );
        assert_eq!(events.last().unwrap()["usage"]["total_tokens"], 4);
        assert!(body.ends_with("data: [DONE]\n\n"));
        let request = captured.lock().await.clone().unwrap();
        assert_eq!(
            request.uri,
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(request.headers["x-goog-api-key"], "gemini-client-key");
        assert!(request.body.get("stream").is_none());
    }

    #[tokio::test]
    async fn gemini_rejects_disabled_parallel_tools_before_contacting_upstream() {
        let (upstream, captured) = start_upstream(json!({})).await;
        let mut config = test_config(&upstream);
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, upstream, None),
        );
        let proxy = start_proxy(config).await;
        let function = json!({"name":"lookup","parameters":{"type":"object"}});
        let client = reqwest::Client::new();
        for (path, body) in [
            (
                "chat/completions",
                json!({
                    "model":"gemini","messages":[{"role":"user","content":"ping"}],
                    "tools":[{"type":"function","function":function}],"parallel_tool_calls":false
                }),
            ),
            (
                "responses",
                json!({
                    "model":"gemini","input":"ping",
                    "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                    "parallel_tool_calls":false
                }),
            ),
            (
                "messages",
                json!({
                    "model":"gemini","max_tokens":32,"messages":[{"role":"user","content":"ping"}],
                    "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
                    "tool_choice":{"type":"auto","disable_parallel_tool_use":true}
                }),
            ),
        ] {
            let reply = client
                .post(format!("http://{proxy}/gemini/v1/{path}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(
                reply.text().await.unwrap().contains("parallel_tool_calls"),
                "{path}"
            );
        }
        assert!(captured.lock().await.is_none());
    }

    #[tokio::test]
    async fn gemini_orders_parallel_tool_results_within_each_turn() {
        let (upstream, captured) = start_upstream(json!({
            "candidates":[{"content":{"parts":[{"text":"pong"}]},"finishReason":"STOP"}]
        }))
        .await;
        let mut config = test_config(&upstream);
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, upstream, None),
        );
        let proxy = start_proxy(config).await;
        let call = |id: &str, key: &str| {
            json!({"type":"function","id":id,
            "function":{"name":"lookup","arguments":json!({"key":key}).to_string()}})
        };
        let result = |id: &str, found: &str| json!({"role":"tool","tool_call_id":id,"content": json!({"found":found}).to_string()});
        let mut signed = call("call_A", "A");
        signed["extra_content"] = json!({"google":{"thought_signature":"c2ln"}});
        let reply = reqwest::Client::new()
            .post(format!("http://{proxy}/gemini/v1/chat/completions"))
            .json(&json!({"model":"gemini", "messages":[
                {"role":"user","content":"look up A and B"},
                {"role":"assistant","tool_calls":[signed,call("call_B","B")]},
                result("call_B","B"), result("call_A","A"),
                {"role":"assistant","tool_calls":[call("call_C","C")]}, result("call_C","C"),
            ]}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        let request = captured.lock().await.take().unwrap();
        let contents = request.body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 5);
        assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "call_A");
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "c2ln");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["id"], "call_B");
        assert!(contents[1]["parts"][1].get("thoughtSignature").is_none());
        assert_eq!(
            contents[2],
            json!({"role":"user","parts":[
                {"functionResponse":{"id":"call_A","name":"lookup","response":{"found":"A"}}},
                {"functionResponse":{"id":"call_B","name":"lookup","response":{"found":"B"}}}
            ]})
        );
        assert_eq!(contents[3]["parts"][0]["functionCall"]["id"], "call_C");
        assert_eq!(contents[4]["parts"][0]["functionResponse"]["id"], "call_C");
    }

    #[tokio::test]
    async fn gemini_groups_responses_function_items_and_preserves_call_order() {
        let (upstream, captured) = start_upstream(json!({
            "candidates":[{"content":{"parts":[{"text":"pong"}]},"finishReason":"STOP"}]
        }))
        .await;
        let mut config = test_config(&upstream);
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, upstream, None),
        );
        let proxy = start_proxy(config).await;
        let reply = reqwest::Client::new()
            .post(format!("http://{proxy}/gemini/v1/responses"))
            .json(&json!({"model":"gemini","input":[
                {"role":"user","content":"look up A and B"},
                {"type":"function_call","name":"lookup","call_id":"a","arguments":"{\"key\":\"A\"}",
                 "extra_content":{"google":{"thought_signature":"c2ln"}}},
                {"type":"function_call","name":"lookup","call_id":"b","arguments":"{\"key\":\"B\"}"},
                {"type":"function_call_output","call_id":"b","output":"{\"found\":\"B\"}"},
                {"type":"function_call_output","call_id":"a","output":"{\"found\":\"A\"}"},
            ]})).send().await.unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        let request = captured.lock().await.take().unwrap();
        let contents = request.body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(
            contents[1],
            json!({"role":"model","parts":[
                {"functionCall":{"id":"a","name":"lookup","args":{"key":"A"}},"thoughtSignature":"c2ln"},
                {"functionCall":{"id":"b","name":"lookup","args":{"key":"B"}}}
            ]})
        );
        assert_eq!(
            contents[2],
            json!({"role":"user","parts":[
                {"functionResponse":{"id":"a","name":"lookup","response":{"found":"A"}}},
                {"functionResponse":{"id":"b","name":"lookup","response":{"found":"B"}}}
            ]})
        );
    }

    #[tokio::test]
    async fn gemini_chat_stream_respects_usage_opt_in_and_late_accounting() {
        let client = reqwest::Client::new();
        for terminal in ["combined", "late", "missing", "error"] {
            let usage =
                json!({"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5});
            let mut frames = vec![
                json!({"candidates":[{"content":{"parts":[{"text":"pong"}]}}]}),
                json!({"candidates":[{"finishReason":"STOP"}]}),
            ];
            match terminal {
                "combined" => frames[1]["usageMetadata"] = usage,
                "late" => frames.push(json!({"usageMetadata":usage})),
                "error" => frames.push(json!({"error":{"message":"stream failed"}})),
                _ => {}
            }
            let chunks = frames
                .into_iter()
                .map(|frame| format!("data: {frame}\n\n").into_bytes())
                .collect();
            let (upstream, _) = start_stream_upstream(chunks).await;
            let mut config = test_config(&upstream);
            config.providers.insert(
                "gemini".into(),
                ProviderConfig::upstream(Protocol::Gemini, upstream, None),
            );
            let proxy = start_proxy(config).await;
            for include in [Some(true), Some(false), None] {
                let mut body = json!({"model":"gemini","stream":true,"messages":[{"role":"user","content":"ping"}]});
                if let Some(include) = include {
                    body["stream_options"] = json!({"include_usage":include});
                }
                let reply = client
                    .post(format!("http://{proxy}/gemini/v1/chat/completions"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(reply.status(), StatusCode::OK);
                let wire = reply.text().await.unwrap();
                let events = sse_json_events(&wire);
                assert!(
                    events
                        .iter()
                        .any(|e| e["choices"][0]["delta"]["content"] == "pong")
                );
                let done = wire.matches("data: [DONE]").count();
                if terminal == "error" {
                    assert_eq!(done, 0);
                    assert_eq!(events.last().unwrap()["error"]["message"], "stream failed");
                    continue;
                }
                assert_eq!(done, 1, "{terminal}, {include:?}");
                assert!(wire.ends_with("data: [DONE]\n\n"));
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| e["choices"][0]["finish_reason"] == "stop")
                        .count(),
                    1
                );
                if include == Some(true) {
                    let expected = terminal != "missing";
                    let last = events.last().unwrap();
                    assert_eq!(
                        last.get("usage").is_some_and(|usage| usage.is_object()),
                        expected
                    );
                    if expected {
                        assert_eq!(last["usage"]["total_tokens"], 5);
                        assert_eq!(last["choices"], json!([]));
                    }
                    assert!(
                        events[..events.len() - usize::from(expected)]
                            .iter()
                            .all(|e| e.get("usage") == Some(&Value::Null))
                    );
                } else {
                    assert!(events.iter().all(|e| e.get("usage").is_none()));
                }
            }
        }
    }

    #[tokio::test]
    async fn gemini_interactions_proxy_preserves_native_requests_and_lifecycle() {
        let reply = json!({"id": "v1_test", "status": "in_progress", "steps": []});
        let (upstream, captured) = start_upstream(reply.clone()).await;
        let config: Config = serde_yaml::from_str(&format!(
            "usage_db: false\ndefault: gemini\napikey: client-key\nproviders:\n  gemini:\n    type: gemini\n    baseurl: {upstream}/v1\n    apikey: upstream-key\n    models: [gemini-2.5-flash]\n"
        )).unwrap();
        let proxy = start_proxy(config).await;
        let client = reqwest::Client::new();
        let request = json!({
            "model": "gemini-2.5-flash", "input": "Continue", "previous_interaction_id": "v1_previous",
            "store": true, "tools": [{"type": "google_search"}]
        });
        let created = client
            .post(format!("http://{proxy}/v1/interactions?key=client-key"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        assert_eq!(created.json::<Value>().await.unwrap(), reply);
        let sent = captured.lock().await.take().unwrap();
        assert_eq!(sent.method, Method::POST);
        assert_eq!(sent.uri, "/v1/interactions");
        assert_eq!(sent.headers["x-goog-api-key"], "upstream-key");
        assert_eq!(sent.body, request);

        for (method, path, upstream_path) in [
            (
                Method::GET,
                "/gemini/v1/interactions/v1_test?include_input=true",
                "/v1/interactions/v1_test?include_input=true",
            ),
            (
                Method::POST,
                "/gemini/interactions/v1_test/cancel",
                "/v1/interactions/v1_test/cancel",
            ),
            (
                Method::DELETE,
                "/v1/interactions/v1_test",
                "/v1/interactions/v1_test",
            ),
        ] {
            let response = client
                .request(method.clone(), format!("http://{proxy}{path}"))
                .header("x-goog-api-key", "client-key")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let sent = captured.lock().await.take().unwrap();
            assert_eq!(sent.method, method);
            assert_eq!(sent.uri, upstream_path);
            assert!(sent.body.is_null());
        }

        let denied = client
            .post(format!("http://{proxy}/v1/interactions"))
            .bearer_auth("client-key")
            .json(&json!({"model": "gemini-2.5-pro", "input": "ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
        let wrong_method = client
            .get(format!("http://{proxy}/v1/interactions/v1_test/cancel"))
            .bearer_auth("client-key")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(wrong_method.headers()["allow"], "POST");
        assert!(captured.lock().await.is_none());
    }

    #[tokio::test]
    async fn gemini_interactions_proxy_preserves_native_sse_events() {
        let events = concat!(
            "data: {\"event_id\":\"next\",\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"v1_test\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"v1_test\",\"status\":\"completed\"}}\n\n"
        );
        let (upstream, captured) = start_stream_upstream(vec![events.as_bytes().to_vec()]).await;
        let mut config = test_config(&upstream);
        config.providers.insert(
            "gemini".into(),
            ProviderConfig::upstream(Protocol::Gemini, format!("{upstream}/v1"), None),
        );
        let proxy = start_proxy(config).await;
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/gemini/v1/interactions"))
            .header("accept", "text/event-stream")
            .json(&json!({"model": "gemini-2.5-flash", "input": "ping", "stream": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.text().await.unwrap(), events);
        let sent = captured.lock().await.take().unwrap();
        assert_eq!(sent.uri, "/v1/interactions");
        assert_eq!(sent.headers["accept"], "text/event-stream");
        assert_eq!(sent.body["stream"], true);

        let resumed = reqwest::Client::new()
            .get(format!(
                "http://{proxy}/gemini/v1/interactions/v1_test?stream=true&last_event_id=next"
            ))
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(resumed.status(), StatusCode::OK);
        assert_eq!(resumed.text().await.unwrap(), events);
        let sent = captured.lock().await.take().unwrap();
        assert_eq!(
            sent.uri,
            "/v1/interactions/v1_test?stream=true&last_event_id=next"
        );
        assert!(sent.body.is_null());
    }

    #[tokio::test]
    async fn gemini_provider_normalizes_and_filters_the_model_catalog() {
        let (upstream, captured) = start_upstream(json!({
            "models": [
                {"name": "models/gemini-2.5-flash", "displayName": "Gemini Flash", "inputTokenLimit": 1048576, "outputTokenLimit": 65536, "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/gemini-embedding", "supportedGenerationMethods": ["embedContent"]}
            ],
            "nextPageToken": "next"
        })).await;
        let mut config = test_config(&upstream);
        let mut provider =
            ProviderConfig::upstream(Protocol::Gemini, format!("{upstream}/v1"), None);
        provider.as_upstream_mut().unwrap().models =
            vec!["gemini-2.5-flash".into(), "gemini-embedding".into()];
        config.providers.insert("gemini".into(), provider);
        let proxy = start_proxy(config).await;
        let client = reqwest::Client::new();
        for codex in [false, true] {
            let response = client
                .get(format!(
                    "http://{proxy}/gemini/v1/models?pageSize=10&pageToken=previous"
                ))
                .header(
                    "user-agent",
                    if codex { "codex-cli" } else { "test-client" },
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = response.json().await.unwrap();
            if codex {
                assert_eq!(body["models"].as_array().unwrap().len(), 1);
                assert_eq!(body["models"][0]["slug"], "gemini-2.5-flash");
                assert_eq!(body["models"][0]["display_name"], "Gemini Flash");
                assert_eq!(body["models"][0]["context_window"], 1048576);
                assert_eq!(body["models"][0]["max_output_tokens"], 65536);
            } else {
                assert_eq!(body["data"].as_array().unwrap().len(), 1);
                assert_eq!(body["data"][0]["id"], "gemini-2.5-flash");
                assert_eq!(body["data"][0]["max_output_tokens"], 65536);
            }
            assert_eq!(body["nextPageToken"], "next");
        }
        let request = captured.lock().await.clone().unwrap();
        assert_eq!(request.uri, "/v1/models?pageSize=10&pageToken=previous");
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
