//! Load-balancer behavior: scheduling, failover, routing, models, hot reload.

use super::*;
use crate::config::Protocol;
use axum::extract::ws::WebSocketUpgrade;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Debug, Clone, Default)]
struct Counter(Arc<AtomicUsize>);

impl Counter {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn inc(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}")
}

#[derive(Clone)]
struct JsonMock {
    counter: Counter,
    status: StatusCode,
    body: Value,
}

async fn json_mock(State(mock): State<JsonMock>) -> impl IntoResponse {
    mock.counter.inc();
    (mock.status, Json(mock.body.clone()))
}

async fn json_member(status: StatusCode, body: Value) -> (String, Counter) {
    let counter = Counter::default();
    let url = serve(
        Router::new()
            .route("/{*path}", any(json_mock))
            .with_state(JsonMock {
                counter: counter.clone(),
                status,
                body,
            }),
    )
    .await;
    (url, counter)
}

async fn models_member(id: &str) -> (String, Counter) {
    json_member(
        StatusCode::OK,
        json!({"object": "list", "data": [{"id": id, "object": "model"}]}),
    )
    .await
}

async fn chat_member(text: &str) -> (String, Counter) {
    json_member(
        StatusCode::OK,
        json!({
            "id": "chatcmpl_lb",
            "model": "shared-model",
            "choices": [{"message": {"content": text}, "finish_reason": "stop"}]
        }),
    )
    .await
}

fn config_with(members: &[&str], upstreams: &[(&str, &str, Protocol)]) -> Config {
    let mut config = Config::default();
    for (name, url, protocol) in upstreams {
        config.providers.insert(
            (*name).into(),
            ProviderConfig::upstream(*protocol, *url, Some(format!("sk-{name}"))),
        );
    }
    config.providers.insert(
        "pool".into(),
        ProviderConfig::lb(members.iter().map(|member| member.to_string()).collect()),
    );
    config.default = "pool".into();
    config
}

fn chat_payload() -> Value {
    json!({"model": "shared-model", "messages": [{"role": "user", "content": "hi"}]})
}

async fn post_chat(path: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(path)
        .json(&chat_payload())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn model_allowlist_rejects_direct_requests_and_skips_ineligible_members() {
    let (a, a_calls) = chat_member("a").await;
    let (b, b_calls) = chat_member("b").await;
    let mut config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    for (provider, model) in [("a", "other-model"), ("b", "shared-model")] {
        config
            .providers
            .get_mut(provider)
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .models = vec![model.into()];
    }
    let proxy = start_proxy(config).await;

    for path in ["chat/completions", "messages"] {
        let response = post_chat(format!("http://{proxy}/a/{path}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("model is not allowed"));
    }

    let response = post_chat(format!("http://{proxy}/pool/chat/completions")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "b"
    );
    let response = reqwest::Client::new()
        .post(format!("http://{proxy}/pool/chat/completions"))
        .json(&json!({"model": "unknown-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(a_calls.count(), 0);
    assert_eq!(b_calls.count(), 1);
}

#[tokio::test]
async fn model_allowlist_filters_native_and_converted_catalogs() {
    let (a, _) = json_member(
        StatusCode::OK,
        json!({"data": [{"id": "allowed"}, {"id": "other"}]}),
    )
    .await;
    let (b, _) = json_member(
        StatusCode::OK,
        json!({"models": [{"slug": "allowed"}, {"slug": "other"}]}),
    )
    .await;
    let mut config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    for provider in ["a", "b"] {
        config
            .providers
            .get_mut(provider)
            .unwrap()
            .as_upstream_mut()
            .unwrap()
            .models = vec!["allowed".into()];
    }
    let proxy = start_proxy(config).await;

    for provider in ["a", "b", "pool"] {
        for (query, list, id) in [("", "data", "id"), ("?client_version=1", "models", "slug")] {
            let body: Value = reqwest::get(format!("http://{proxy}/{provider}/models{query}"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let models = body[list].as_array().unwrap();
            assert_eq!(models.len(), 1);
            assert_eq!(models[0][id], "allowed");
        }
    }
}

#[tokio::test]
async fn rotates_starting_member_across_sequential_requests() {
    let (a, a_calls) = models_member("a").await;
    let (b, b_calls) = models_member("b").await;
    let (c, c_calls) = models_member("c").await;
    let config = config_with(
        &["a", "b", "c"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
            ("c", &c, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    for expected in ["a", "b", "c", "a", "b", "c"] {
        let body: Value = reqwest::get(format!("http://{proxy}/pool/v1/models"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["data"][0]["id"], expected);
    }
    assert_eq!(a_calls.count(), 2);
    assert_eq!(b_calls.count(), 2);
    assert_eq!(c_calls.count(), 2);
}

#[tokio::test]
async fn single_member_and_independent_groups() {
    let (a, a_calls) = models_member("a").await;
    let (b, b_calls) = models_member("b").await;
    let mut config = config_with(
        &["a"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    config
        .providers
        .insert("other".into(), ProviderConfig::lb(vec!["b".into()]));
    let proxy = start_proxy(config).await;

    for _ in 0..2 {
        let body: Value = reqwest::get(format!("http://{proxy}/models"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["data"][0]["id"], "a");
    }
    for _ in 0..2 {
        let body: Value = reqwest::get(format!("http://{proxy}/other/models"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["data"][0]["id"], "b");
    }
    assert_eq!(a_calls.count(), 2);
    assert_eq!(b_calls.count(), 2);
}

#[tokio::test]
async fn concurrent_requests_spread_members_evenly() {
    let (a, a_calls) = models_member("a").await;
    let (b, b_calls) = models_member("b").await;
    let (c, c_calls) = models_member("c").await;
    let config = config_with(
        &["a", "b", "c"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
            ("c", &c, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;
    let client = reqwest::Client::new();

    let requests = (0..30).map(|_| {
        let client = client.clone();
        let url = format!("http://{proxy}/models");
        async move {
            let response = client.get(url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    });
    futures_util::future::join_all(requests).await;

    let counts = [a_calls.count(), b_calls.count(), c_calls.count()];
    assert_eq!(counts.iter().sum::<usize>(), 30);
    let min = *counts.iter().min().unwrap();
    let max = *counts.iter().max().unwrap();
    assert!(max - min <= 1, "{counts:?}");
}

#[tokio::test]
async fn failover_is_bounded_and_does_not_advance_the_cursor() {
    let (a, a_calls) = chat_member("a").await;
    let (b, b_calls) =
        json_member(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "down"})).await;
    let (c, c_calls) = chat_member("c").await;
    let config = config_with(
        &["a", "b", "c"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
            ("c", &c, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    // Request 1 starts at a and succeeds.
    let first = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "a"
    );
    assert_eq!(
        (a_calls.count(), b_calls.count(), c_calls.count()),
        (1, 0, 0)
    );

    // Request 2 starts at b, fails once, then succeeds on c exactly once.
    let second = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "c"
    );
    assert_eq!(
        (a_calls.count(), b_calls.count(), c_calls.count()),
        (1, 1, 1)
    );

    // Request 3 starts at c again (the failed request did not push the cursor).
    let third = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        third.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "c"
    );
    assert_eq!(
        (a_calls.count(), b_calls.count(), c_calls.count()),
        (1, 1, 2)
    );
}

#[tokio::test]
async fn failover_wraps_to_members_before_the_starting_member() {
    let (a, a_calls) = chat_member("a").await;
    let (b, b_calls) = json_member(StatusCode::SERVICE_UNAVAILABLE, json!({})).await;
    let (c, c_calls) = json_member(StatusCode::SERVICE_UNAVAILABLE, json!({})).await;
    let config = config_with(
        &["a", "b", "c"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
            ("c", &c, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    // Request 1 starts at a and succeeds.
    let first = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        first.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "a"
    );
    // Request 2 starts at b, fails over c, then wraps to a.
    let second = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        second.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "a"
    );
    // Request 3 starts at c, fails over a.
    let third = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        third.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "a"
    );
    assert_eq!(
        (a_calls.count(), b_calls.count(), c_calls.count()),
        (3, 1, 2)
    );
}

#[tokio::test]
async fn switches_on_retryable_statuses() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::REQUEST_TIMEOUT,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let (bad, bad_calls) = json_member(status, json!({"error": "down"})).await;
        let (good, good_calls) = chat_member("ok").await;
        let config = config_with(
            &["bad", "good"],
            &[
                ("bad", &bad, Protocol::OpenAiChat),
                ("good", &good, Protocol::OpenAiChat),
            ],
        );
        let proxy = start_proxy(config).await;

        let response = post_chat(format!("http://{proxy}/chat/completions")).await;
        assert_eq!(response.status(), StatusCode::OK, "{status}");
        assert_eq!(bad_calls.count(), 1);
        assert_eq!(good_calls.count(), 1);
    }
}

#[tokio::test]
async fn returns_non_retryable_status_without_switching() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::NOT_FOUND,
        StatusCode::UNPROCESSABLE_ENTITY,
    ] {
        let (bad, bad_calls) = json_member(
            status,
            json!({"error": {"type": "invalid_request_error", "message": "bad"}}),
        )
        .await;
        let (good, good_calls) = chat_member("ok").await;
        let config = config_with(
            &["bad", "good"],
            &[
                ("bad", &bad, Protocol::OpenAiChat),
                ("good", &good, Protocol::OpenAiChat),
            ],
        );
        let proxy = start_proxy(config).await;

        let response = post_chat(format!("http://{proxy}/messages")).await;
        assert_eq!(response.status(), status, "{status}");
        assert_eq!(bad_calls.count(), 1);
        assert_eq!(good_calls.count(), 0);
    }
}

#[tokio::test]
async fn all_members_unavailable_returns_protocol_shaped_503() {
    let (a, a_calls) = json_member(StatusCode::INTERNAL_SERVER_ERROR, json!({})).await;
    let (b, b_calls) = json_member(StatusCode::BAD_GATEWAY, json!({})).await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = post_chat(format!("http://{proxy}/pool/messages")).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("pool"), "{message}");
    assert_eq!(a_calls.count(), 1);
    assert_eq!(b_calls.count(), 1);
}

#[tokio::test]
async fn switches_on_transport_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (good, good_calls) = chat_member("ok").await;
    let config = config_with(
        &["dead", "good"],
        &[
            ("dead", &dead, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(good_calls.count(), 1);
}

async fn start_proxy_with_header_timeout(config: Config, header_timeout: Duration) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_, runtime) = watch::channel(Arc::new(Runtime::with_lb_header_timeout(
            config,
            header_timeout,
        )));
        let (_, auth) = watch::channel(None);
        axum::serve(listener, app_with_auth(runtime, auth))
            .await
            .unwrap();
    });
    address
}

#[derive(Clone)]
struct SlowMock {
    counter: Counter,
    delay: Duration,
}

async fn slow_mock(State(mock): State<SlowMock>) -> impl IntoResponse {
    mock.counter.inc();
    tokio::time::sleep(mock.delay).await;
    Json(json!({"data": [{"id": "slow"}]}))
}

#[tokio::test]
async fn switches_on_response_header_timeout() {
    let slow_calls = Counter::default();
    let slow = serve(
        Router::new()
            .route("/{*path}", any(slow_mock))
            .with_state(SlowMock {
                counter: slow_calls.clone(),
                delay: Duration::from_secs(2),
            }),
    )
    .await;
    let (good, good_calls) = models_member("good").await;
    let config = config_with(
        &["slow", "good"],
        &[
            ("slow", &slow, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy_with_header_timeout(config, Duration::from_millis(300)).await;

    let body: Value = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["data"][0]["id"], "good");
    assert_eq!(slow_calls.count(), 1);
    assert_eq!(good_calls.count(), 1);
}

#[derive(Clone)]
struct FlakyMock {
    counter: Counter,
}

async fn flaky_mock(State(mock): State<FlakyMock>) -> impl IntoResponse {
    let call = mock.counter.count();
    mock.counter.inc();
    if call == 0 {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "starting"})),
        )
            .into_response()
    } else {
        Json(json!({
            "id": "chatcmpl_flaky",
            "choices": [{"message": {"content": "recovered"}, "finish_reason": "stop"}]
        }))
        .into_response()
    }
}

#[tokio::test]
async fn recovered_member_is_used_again_without_permanent_isolation() {
    let flaky_calls = Counter::default();
    let flaky = serve(
        Router::new()
            .route("/{*path}", any(flaky_mock))
            .with_state(FlakyMock {
                counter: flaky_calls.clone(),
            }),
    )
    .await;
    let (stable, stable_calls) = chat_member("stable").await;
    let config = config_with(
        &["flaky", "stable"],
        &[
            ("flaky", &flaky, Protocol::OpenAiChat),
            ("stable", &stable, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let first = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        first.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "stable"
    );
    let second = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        second.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "stable"
    );
    let third = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(
        third.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "recovered"
    );
    assert_eq!(flaky_calls.count(), 2);
    assert_eq!(stable_calls.count(), 2);
}

#[derive(Clone)]
struct ControlledMember {
    counter: Counter,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

async fn controlled_member(State(mock): State<ControlledMember>) -> impl IntoResponse {
    mock.counter.inc();
    mock.entered.notify_one();
    mock.release.notified().await;
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "unavailable"})),
    )
}

#[derive(Clone)]
struct GoodMember {
    counter: Counter,
    hit: Arc<tokio::sync::Notify>,
}

async fn good_member(State(mock): State<GoodMember>) -> impl IntoResponse {
    mock.counter.inc();
    mock.hit.notify_one();
    Json(json!({"data": [{"id": "good"}]}))
}

/// Run the same scenario with and without client cancellation. The first
/// member always becomes retryable once released, so an implementation that
/// ignores cancellation must attempt the second member.
async fn cancellation_scenario(cancel: bool) -> (usize, usize) {
    let slow_calls = Counter::default();
    let good_calls = Counter::default();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let good_hits = Arc::new(tokio::sync::Notify::new());
    let slow = serve(
        Router::new()
            .route("/{*path}", any(controlled_member))
            .with_state(ControlledMember {
                counter: slow_calls.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }),
    )
    .await;
    let good = serve(
        Router::new()
            .route("/{*path}", any(good_member))
            .with_state(GoodMember {
                counter: good_calls.clone(),
                hit: good_hits.clone(),
            }),
    )
    .await;
    let config = config_with(
        &["slow", "good"],
        &[
            ("slow", &slow, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("http://{proxy}/models"))
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("first member must be reached before the scenario proceeds");
    if cancel {
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        // Let the server observe the closed client connection before the
        // member produces a retryable failure.
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.notify_one();
        let switched = tokio::time::timeout(Duration::from_millis(400), good_hits.notified()).await;
        assert!(
            switched.is_err(),
            "a cancelled request must not try the next member"
        );
    } else {
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), good_hits.notified())
            .await
            .expect("without cancellation the retryable member must be switched");
        assert_eq!(request.await.unwrap().unwrap().status(), StatusCode::OK);
    }
    (slow_calls.count(), good_calls.count())
}

#[tokio::test]
async fn cancelled_request_does_not_attempt_the_next_member() {
    assert_eq!(cancellation_scenario(false).await, (1, 1));
    assert_eq!(cancellation_scenario(true).await, (1, 0));
}

#[tokio::test]
async fn chat_responses_and_messages_clients_all_use_the_lb() {
    let (chat, chat_calls) = chat_member("hello").await;
    let (responses, responses_calls) = json_member(
        StatusCode::OK,
        json!({"id": "resp_lb", "object": "response", "model": "shared-model", "output_text": "hello", "status": "completed"}),
    )
    .await;
    let (messages, messages_calls) = json_member(
        StatusCode::OK,
        json!({"id": "msg_lb", "type": "message", "role": "assistant", "model": "shared-model", "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn"}),
    )
    .await;
    let config = config_with(
        &["chat", "responses", "messages"],
        &[
            ("chat", &chat, Protocol::OpenAiChat),
            ("responses", &responses, Protocol::OpenAiResponses),
            ("messages", &messages, Protocol::Anthropic),
        ],
    );
    let proxy = start_proxy(config).await;
    let client = reqwest::Client::new();

    let chat_response: Value = post_chat(format!("http://{proxy}/pool/chat/completions"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(chat_response["choices"][0]["message"]["content"], "hello");

    let responses_response: Value = client
        .post(format!("http://{proxy}/pool/responses"))
        .json(&json!({"model": "shared-model", "input": "hi"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(responses_response["output_text"], "hello");

    let messages_response: Value = client
        .post(format!("http://{proxy}/pool/messages"))
        .json(&json!({"model": "shared-model", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(messages_response["content"][0]["text"], "hello");

    assert_eq!(chat_calls.count(), 1);
    assert_eq!(responses_calls.count(), 1);
    assert_eq!(messages_calls.count(), 1);
}

#[tokio::test]
async fn cross_protocol_failover_rebuilds_the_request_for_the_next_member() {
    #[derive(Clone)]
    struct Capture {
        counter: Counter,
        captured: Arc<Mutex<Option<(HeaderMap, Value)>>>,
        status: StatusCode,
        body: Value,
    }
    async fn capture(
        State(state): State<Capture>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response<Body> {
        state.counter.inc();
        *state.captured.lock().unwrap() = Some((headers, body));
        (state.status, Json(state.body.clone())).into_response()
    }

    let chat_captured = Arc::new(Mutex::new(None));
    let chat_calls = Counter::default();
    let chat = serve(
        Router::new()
            .route("/{*path}", axum::routing::post(capture))
            .with_state(Capture {
                counter: chat_calls.clone(),
                captured: chat_captured.clone(),
                status: StatusCode::BAD_GATEWAY,
                body: json!({"error": "down"}),
            }),
    )
    .await;
    let anthropic_captured = Arc::new(Mutex::new(None));
    let anthropic_calls = Counter::default();
    let anthropic = serve(
        Router::new()
            .route("/{*path}", axum::routing::post(capture))
            .with_state(Capture {
                counter: anthropic_calls.clone(),
                captured: anthropic_captured.clone(),
                status: StatusCode::OK,
                body: json!({
                    "id": "msg_lb", "type": "message", "role": "assistant", "model": "shared-model",
                    "content": [{"type": "text", "text": "converted"}], "stop_reason": "end_turn"
                }),
            }),
    )
    .await;
    let config = config_with(
        &["chat", "anthropic"],
        &[
            ("chat", &chat, Protocol::OpenAiChat),
            ("anthropic", &anthropic, Protocol::Anthropic),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy}/pool/responses"))
        .json(&json!({
            "model": "shared-model",
            "max_output_tokens": 32,
            "input": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "name": "weather",
                "description": "Get the weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let output: Value = response.json().await.unwrap();
    assert_eq!(output["output_text"], "converted");
    assert_eq!(chat_calls.count(), 1);
    assert_eq!(anthropic_calls.count(), 1);

    let chat_request = chat_captured.lock().unwrap();
    let (chat_headers, chat_body) = chat_request.as_ref().unwrap();
    assert_eq!(chat_body["model"], "shared-model");
    assert_eq!(chat_body["messages"][0]["content"], "hi");
    assert_eq!(chat_body["tools"][0]["function"]["name"], "weather");
    assert_eq!(
        chat_body["tools"][0]["function"]["parameters"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(chat_headers["authorization"], "Bearer sk-chat");
    assert!(!chat_headers.contains_key("x-api-key"));

    let anthropic_request = anthropic_captured.lock().unwrap();
    let (headers, body) = anthropic_request.as_ref().unwrap();
    assert_eq!(body["model"], "shared-model");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    assert_eq!(body["tools"][0]["name"], "weather");
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(headers["x-api-key"], "sk-anthropic");
    assert!(!headers.contains_key("authorization"));
}

#[tokio::test]
async fn conversion_errors_are_final_and_do_not_call_members() {
    let (a, a_calls) = models_member("a").await;
    let (b, b_calls) = models_member("b").await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiResponses),
            ("b", &b, Protocol::OpenAiResponses),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy}/pool/chat/completions"))
        .json(&json!({
            "model": "shared-model",
            "messages": [{"role": "user", "content": "hi"}],
            "n": 2
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(a_calls.count(), 0);
    assert_eq!(b_calls.count(), 0);
}

#[tokio::test]
async fn failover_can_commit_an_sse_response() {
    let (bad, bad_calls) = json_member(StatusCode::TOO_MANY_REQUESTS, json!({})).await;
    let (stream, stream_calls) = start_stream_upstream(vec![
        b"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n"
            .to_vec(),
        b"data: [DONE]\n\n".to_vec(),
    ])
    .await;
    let config = config_with(
        &["bad", "stream"],
        &[
            ("bad", &bad, Protocol::OpenAiChat),
            ("stream", &stream, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let body = response.text().await.unwrap();
    assert!(body.contains("hi"), "{body}");
    assert!(body.contains("[DONE]"));
    assert_eq!(bad_calls.count(), 1);
    assert_eq!(
        stream_calls.lock().await.as_ref().unwrap().body["model"],
        "shared-model"
    );
}

#[tokio::test]
async fn stream_interruption_after_commit_never_calls_another_member() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/{*path}",
                any(|| async {
                    let chunks = async_stream::stream! {
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
                        ));
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        yield Err(std::io::Error::other("stream broke"));
                    };
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (good, good_calls) = chat_member("should-not-be-used").await;
    let config = config_with(
        &["broken", "good"],
        &[
            ("broken", &upstream, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let mut response = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("partial"), "{body}");
    assert_eq!(good_calls.count(), 0);
}

#[tokio::test]
async fn upstream_raw_mode_returns_the_selected_member_response() {
    let (bad, _) = json_member(StatusCode::SERVICE_UNAVAILABLE, json!({})).await;
    let (good, good_calls) = json_member(
        StatusCode::OK,
        json!({
            "id": "chatcmpl_raw",
            "model": "shared-model",
            "choices": [{"message": {"content": "raw"}, "finish_reason": "stop"}]
        }),
    )
    .await;
    let config = config_with(
        &["bad", "good"],
        &[
            ("bad", &bad, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .post(format!(
            "http://{proxy}/pool/messages?llmapi_response_mode=upstream_raw"
        ))
        .json(&json!({"model": "shared-model", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "raw");
    assert!(body.get("content").is_none());
    assert_eq!(good_calls.count(), 1);
}

#[tokio::test]
async fn lb_rejects_websocket_upgrade_before_connecting() {
    let (a, a_calls) = models_member("a").await;
    let config = config_with(&["a"], &[("a", &a, Protocol::OpenAiResponses)]);
    let proxy = start_proxy(config).await;

    let result = tokio_tungstenite::connect_async(format!("ws://{proxy}/pool/responses")).await;
    let error = result.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("unexpected websocket error: {error}");
    };
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a_calls.count(), 0);
}

async fn ws_echo(ws: WebSocketUpgrade) -> Response<Body> {
    ws.on_upgrade(|mut socket| async move {
        use axum::extract::ws::Message;
        while let Some(Ok(message)) = socket.recv().await {
            let reply = match message {
                Message::Text(text) => Message::Text(format!("echo:{text}").into()),
                other => other,
            };
            if socket.send(reply).await.is_err() {
                break;
            }
        }
    })
}

#[tokio::test]
async fn concrete_responses_provider_websocket_still_works() {
    let upstream = serve(Router::new().route("/responses", any(ws_echo))).await;
    let config = config_with(&["a"], &[("a", &upstream, Protocol::OpenAiResponses)]);
    let proxy = start_proxy(config).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{proxy}/a/responses"))
        .await
        .unwrap();
    use futures_util::{SinkExt, StreamExt};
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text("hi".into()))
        .await
        .unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    assert_eq!(reply.to_text().unwrap(), "echo:hi");
}

#[tokio::test]
async fn websocket_enforces_the_allowlist_on_each_request() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{protocol::frame::coding::CloseCode, Message};

    let upstream = serve(Router::new().route("/responses", any(ws_echo))).await;
    let mut config = config_with(&["a"], &[("a", &upstream, Protocol::OpenAiResponses)]);
    config
        .providers
        .get_mut("a")
        .unwrap()
        .as_upstream_mut()
        .unwrap()
        .models = vec!["allowed".into()];
    let proxy = start_proxy(config).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{proxy}/a/responses"))
        .await
        .unwrap();

    let allowed = json!({"type": "response.create", "model": "allowed"}).to_string();
    socket
        .send(Message::Text(allowed.clone().into()))
        .await
        .unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    assert_eq!(reply.to_text().unwrap(), format!("echo:{allowed}"));

    let rejected = json!({"type": "response.create", "model": "other"}).to_string();
    socket.send(Message::Text(rejected.into())).await.unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    assert!(matches!(reply, Message::Close(Some(frame)) if frame.code == CloseCode::Policy));
}

#[derive(Clone)]
struct EtagMock {
    counter: Counter,
    if_none_match: Arc<Mutex<Option<String>>>,
}

async fn etag_models(State(mock): State<EtagMock>, headers: HeaderMap) -> Response<Body> {
    mock.counter.inc();
    *mock.if_none_match.lock().unwrap() = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("etag", "\"member-catalog\"")
        .body(Body::from(
            json!({"data": [{"id": "etag-model"}]}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn lb_models_skip_conditional_requests_and_member_etags() {
    let (bad, _) = json_member(StatusCode::INTERNAL_SERVER_ERROR, json!({})).await;
    let if_none_match = Arc::new(Mutex::new(None));
    let counter = Counter::default();
    let good = serve(
        Router::new()
            .route("/{*path}", any(etag_models))
            .with_state(EtagMock {
                counter: counter.clone(),
                if_none_match: if_none_match.clone(),
            }),
    )
    .await;
    let config = config_with(
        &["bad", "good"],
        &[
            ("bad", &bad, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let response = reqwest::Client::new()
        .get(format!("http://{proxy}/models"))
        .header("if-none-match", "\"client-catalog\"")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("etag").is_none());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["id"], "etag-model");
    assert_eq!(counter.count(), 1);
    assert_eq!(*if_none_match.lock().unwrap(), None);
}

#[tokio::test]
async fn lb_skips_codex_when_credentials_are_unavailable() {
    let (good, good_calls) = models_member("good").await;
    let config = config_with(&["codex", "good"], &[("good", &good, Protocol::OpenAiChat)]);
    let proxy = start_proxy(config).await;

    let response = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(good_calls.count(), 1);
}

#[tokio::test]
async fn configured_codex_provider_takes_priority_inside_the_lb() {
    let (configured, configured_calls) = models_member("configured-codex").await;
    let (good, good_calls) = models_member("good").await;
    let mut config = config_with(&["codex", "good"], &[("good", &good, Protocol::OpenAiChat)]);
    config.providers.insert(
        "codex".into(),
        ProviderConfig::upstream(Protocol::OpenAiChat, configured, None),
    );
    let proxy = start_proxy(config).await;

    let response = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["id"], "configured-codex");
    assert_eq!(configured_calls.count(), 1);
    assert_eq!(good_calls.count(), 0);
}

fn codex_credentials() -> HeaderMap {
    HeaderMap::from_iter([(
        axum::http::header::AUTHORIZATION,
        "Bearer local-token".parse().unwrap(),
    )])
}

async fn start_proxy_with_auth(
    config: Config,
    auth: watch::Receiver<Option<HeaderMap>>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_, runtime) = watch::channel(Arc::new(Runtime::new(config)));
        axum::serve(listener, app_with_auth(runtime, auth))
            .await
            .unwrap();
    });
    address
}

#[tokio::test]
async fn lb_switches_away_from_codex_401_while_direct_codex_does_not() {
    let (codex, codex_calls) =
        json_member(StatusCode::UNAUTHORIZED, json!({"error": "expired"})).await;
    let (good, good_calls) = models_member("good").await;
    let mut config = config_with(&["codex", "good"], &[("good", &good, Protocol::OpenAiChat)]);
    config.providers.insert(
        "codex".into(),
        ProviderConfig::upstream(Protocol::Codex, codex, None),
    );
    let (_sender, auth) = watch::channel(Some(codex_credentials()));
    let proxy = start_proxy_with_auth(config, auth).await;

    let response = reqwest::get(format!("http://{proxy}/pool/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(codex_calls.count(), 1);
    assert_eq!(good_calls.count(), 1);

    let response = reqwest::get(format!("http://{proxy}/codex/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(codex_calls.count(), 2);
    assert_eq!(good_calls.count(), 1);
}

#[tokio::test]
async fn reload_resets_cursors_only_on_effective_config_change() {
    let (a, a_calls) = models_member("a").await;
    let (b, b_calls) = models_member("b").await;
    let mut config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    config.server = "127.0.0.1:0".into();
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
    let member = || async {
        let body: Value = client
            .get(format!("http://{address}/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["data"][0]["id"].as_str().unwrap().to_string()
    };

    assert_eq!(member().await, "a");
    assert_eq!(member().await, "b");
    assert_eq!(member().await, "a");

    // Saving identical content must not publish a new version.
    let body = fs::read_to_string(&path).unwrap();
    write_config_atomically(&path, &body);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(member().await, "b");

    // An effective change rebuilds the cursors from zero.
    config
        .providers
        .get_mut("a")
        .unwrap()
        .as_upstream_mut()
        .unwrap()
        .api_key = Some("sk-changed".into());
    save_config_atomically(&config, &path);
    tokio::time::timeout(Duration::from_secs(3), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(member().await, "a");

    // Invalid updates keep the old version and the old cursor.
    write_config_atomically(&path, "default: missing\nproviders: {}\n");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(member().await, "b");

    assert_eq!(a_calls.count(), 3);
    assert_eq!(b_calls.count(), 3);
    watcher.abort();
}

#[tokio::test]
async fn snapshot_keeps_config_and_cursors_on_the_same_version() {
    let (a, _) = models_member("a").await;
    let (b, _) = models_member("b").await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    let (sender, runtime) = watch::channel(Arc::new(Runtime::new(config)));
    let state = ServerState {
        runtime,
        client: reqwest::Client::new(),
        lb_client: reqwest::Client::new(),
        codex_auth: watch::channel(None).1,
    };

    let before = state.snapshot();
    assert_eq!(before.lb.candidates("pool").unwrap()[0], "a");

    let mut changed = before.config.as_ref().clone();
    changed.api_key = Some("server-secret".into());
    sender.send_replace(Arc::new(Runtime::new(changed)));

    // The in-flight snapshot keeps its own config and cursor position.
    assert_eq!(before.lb.candidates("pool").unwrap()[0], "b");
    let after = state.snapshot();
    assert!(!Arc::ptr_eq(&before.config, &after.config));
    assert!(!Arc::ptr_eq(&before.lb, &after.lb));
    assert_eq!(after.lb.candidates("pool").unwrap()[0], "a");
}

#[tokio::test]
async fn in_flight_request_keeps_old_members_and_cursor_across_reload() {
    let slow_calls = Counter::default();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let slow = serve(
        Router::new()
            .route("/{*path}", any(controlled_member))
            .with_state(ControlledMember {
                counter: slow_calls.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }),
    )
    .await;
    let (old, old_calls) = chat_member("old-c").await;
    let (new_a, new_a_calls) = chat_member("new-a").await;
    let (new_b, new_b_calls) = chat_member("new-b").await;

    let old_config = config_with(
        &["slow", "old"],
        &[
            ("slow", &slow, Protocol::OpenAiChat),
            ("old", &old, Protocol::OpenAiChat),
        ],
    );
    let new_config = config_with(
        &["new-a", "new-b"],
        &[
            ("new-a", &new_a, Protocol::OpenAiChat),
            ("new-b", &new_b, Protocol::OpenAiChat),
        ],
    );
    let (sender, receiver) = watch::channel(Arc::new(Runtime::new(old_config)));
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

    // The in-flight request starts on the old version's first member and blocks.
    let old_request = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .post(format!("http://{address}/pool/chat/completions"))
                .json(&chat_payload())
                .send()
                .await
        }
    });
    entered.notified().await;

    sender.send_replace(Arc::new(Runtime::new(new_config)));

    // New requests use the new version, whose cursor starts from zero.
    for expected in ["new-a", "new-b"] {
        let body: Value = client
            .post(format!("http://{address}/pool/chat/completions"))
            .json(&chat_payload())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], expected);
    }

    // The in-flight request still switches within the old member list.
    release.notify_one();
    let old_body: Value = old_request.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(old_body["choices"][0]["message"]["content"], "old-c");
    assert_eq!(slow_calls.count(), 1);
    assert_eq!(old_calls.count(), 1);
    assert_eq!((new_a_calls.count(), new_b_calls.count()), (1, 1));
}

#[tokio::test]
async fn providers_endpoint_reports_lb_members_without_keys() {
    let (a, _) = models_member("a").await;
    let (b, _) = models_member("b").await;
    let mut config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    config.providers.insert(
        "secured".into(),
        ProviderConfig::upstream(
            Protocol::OpenAiChat,
            "https://user:pass@secured.test/v1?api_key=sk-hidden",
            Some("sk-secured".into()),
        ),
    );
    config.providers.insert(
        "secured-extra".into(),
        ProviderConfig::upstream(
            Protocol::OpenAiChat,
            "https:///alice:s3cr3t@secured.test/v1",
            None,
        ),
    );
    config.providers.insert(
        "secured-network".into(),
        ProviderConfig::upstream(Protocol::OpenAiChat, "//alice:s3cr3t@secured.test/v1", None),
    );
    config.providers.insert(
        "secured-ftp".into(),
        ProviderConfig::upstream(
            Protocol::OpenAiChat,
            "ftp://alice:s3cr3t@secured.test/v1",
            None,
        ),
    );
    config.providers.insert(
        "secured-ftp-extra".into(),
        ProviderConfig::upstream(
            Protocol::OpenAiChat,
            "ftp://alice:s3cr3t@secured.test:bad/v1",
            None,
        ),
    );
    let proxy = start_proxy(config).await;

    let body: Value = reqwest::get(format!("http://{proxy}/providers"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["providers"]["pool"],
        json!({"type": "lb", "members": ["a", "b"]})
    );
    assert_eq!(body["providers"]["a"]["apikey"], "***");
    assert!(body["providers"]["pool"].get("apikey").is_none());
    assert!(body["providers"]["pool"].get("baseurl").is_none());
    assert_eq!(
        body["providers"]["secured"]["baseurl"],
        "https://***@secured.test/v1?api_key=***"
    );
    assert_eq!(body["providers"]["secured"]["apikey"], "***");
    assert_eq!(
        body["providers"]["secured-extra"]["baseurl"],
        "https://***@secured.test/v1"
    );
    assert_eq!(
        body["providers"]["secured-network"]["baseurl"],
        "***@secured.test/v1"
    );
    assert_eq!(
        body["providers"]["secured-ftp"]["baseurl"],
        "ftp://***@secured.test/v1"
    );
    assert_eq!(
        body["providers"]["secured-ftp-extra"]["baseurl"],
        "ftp://***@secured.test:bad/v1"
    );
    let text = body.to_string();
    assert!(!text.contains("sk-a"), "{text}");
    assert!(!text.contains("sk-secured"), "{text}");
    assert!(!text.contains("sk-hidden"), "{text}");
    assert!(!text.contains("pass@"), "{text}");
    assert!(!text.contains("alice"), "{text}");
    assert!(!text.contains("s3cr3t"), "{text}");
}

#[tokio::test]
async fn concrete_provider_requests_do_not_consume_the_lb_cursor() {
    let (a, a_calls) = models_member("a").await;
    let (b, b_calls) = models_member("b").await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let direct: Value = reqwest::get(format!("http://{proxy}/a/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(direct["data"][0]["id"], "a");

    let first: Value = reqwest::get(format!("http://{proxy}/pool/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        first["data"][0]["id"], "a",
        "direct request must not rotate"
    );
    assert_eq!(a_calls.count(), 2);
    assert_eq!(b_calls.count(), 0);
}

async fn dual_member(id: &str) -> (String, Counter) {
    json_member(
        StatusCode::OK,
        json!({
            "data": [{"id": id, "object": "model"}],
            "choices": [{"message": {"content": id}, "finish_reason": "stop"}]
        }),
    )
    .await
}

#[tokio::test]
async fn models_and_generation_requests_share_the_lb_cursor() {
    let (a, a_calls) = dual_member("a").await;
    let (b, b_calls) = dual_member("b").await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let models: Value = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "a");
    let chat: Value = post_chat(format!("http://{proxy}/chat/completions"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(chat["choices"][0]["message"]["content"], "b");
    let models: Value = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "a");
    assert_eq!(a_calls.count(), 2);
    assert_eq!(b_calls.count(), 1);
}

#[tokio::test]
async fn oversized_request_body_is_rejected_before_any_member_attempt() {
    let (a, a_calls) = chat_member("a").await;
    let (b, b_calls) = chat_member("b").await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let oversized = vec![b'x'; 64 * 1024 * 1024 + 1];
    let response = reqwest::Client::new()
        .post(format!("http://{proxy}/chat/completions"))
        .header("content-type", "application/json")
        .body(oversized)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a_calls.count(), 0);
    assert_eq!(b_calls.count(), 0);
}

#[tokio::test]
async fn codex_credential_refresh_does_not_reset_lb_cursors() {
    let (a, _) = models_member("a").await;
    let (b, _) = models_member("b").await;
    let config = config_with(
        &["a", "b"],
        &[
            ("a", &a, Protocol::OpenAiChat),
            ("b", &b, Protocol::OpenAiChat),
        ],
    );
    let (auth, auth_receiver) = watch::channel(None);
    let proxy = start_proxy_with_auth(config, auth_receiver).await;

    let first: Value = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["data"][0]["id"], "a");

    auth.send_replace(Some(codex_credentials()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let second: Value = reqwest::get(format!("http://{proxy}/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["data"][0]["id"], "b");
}

#[tokio::test]
async fn stream_break_before_first_event_does_not_switch() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/{*path}",
                any(|| async {
                    let chunks = async_stream::stream! {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        yield Err::<bytes::Bytes, std::io::Error>(std::io::Error::other(
                            "stream broke before any event",
                        ));
                    };
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (good, good_calls) = chat_member("should-not-be-used").await;
    let config = config_with(
        &["broken", "good"],
        &[
            ("broken", &upstream, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;

    let mut response = post_chat(format!("http://{proxy}/chat/completions")).await;
    assert_eq!(response.status(), StatusCode::OK);
    while let Ok(Some(_)) = response.chunk().await {}
    assert_eq!(good_calls.count(), 0);
}

#[tokio::test]
async fn transport_failure_does_not_leak_query_secrets() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let config = config_with(&["dead"], &[("dead", &dead, Protocol::OpenAiChat)]);
    let proxy = start_proxy(config).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!(
            "http://{proxy}/pool/chat/completions?api_key=sk-super-secret"
        ))
        .json(&chat_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.text().await.unwrap();
    assert!(!body.contains("sk-super-secret"), "{body}");

    let response = client
        .post(format!(
            "http://{proxy}/dead/chat/completions?api_key=sk-super-secret"
        ))
        .json(&chat_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.text().await.unwrap();
    assert!(!body.contains("sk-super-secret"), "{body}");
}

#[derive(Clone)]
struct RedirectMock {
    counter: Counter,
}

async fn redirect_mock(State(mock): State<RedirectMock>) -> impl IntoResponse {
    mock.counter.inc();
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(axum::http::header::LOCATION, "/chat/completions")],
        Json(json!({"error": "redirect"})),
    )
}

#[tokio::test]
async fn lb_does_not_follow_upstream_redirects() {
    let counter = Counter::default();
    let redirect = serve(
        Router::new()
            .route("/{*path}", any(redirect_mock))
            .with_state(RedirectMock {
                counter: counter.clone(),
            }),
    )
    .await;
    let (good, good_calls) = chat_member("good").await;
    let config = config_with(
        &["redirect", "good"],
        &[
            ("redirect", &redirect, Protocol::OpenAiChat),
            ("good", &good, Protocol::OpenAiChat),
        ],
    );
    let proxy = start_proxy(config).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = client
        .post(format!("http://{proxy}/pool/chat/completions"))
        .json(&chat_payload())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(counter.count(), 1, "a member must be attempted once");
    assert_eq!(
        good_calls.count(),
        0,
        "307 is a final response, not a failover"
    );
}

#[derive(Clone)]
struct LocationMock {
    counter: Counter,
}

async fn location_mock(State(mock): State<LocationMock>) -> impl IntoResponse {
    mock.counter.inc();
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(axum::http::header::LOCATION, "https://target.example/next")],
        Json(json!({"error": "redirect"})),
    )
}

#[tokio::test]
async fn lb_preserves_redirect_location_for_every_route() {
    let counter = Counter::default();
    let redirect = serve(
        Router::new()
            .route("/{*path}", any(location_mock))
            .with_state(LocationMock {
                counter: counter.clone(),
            }),
    )
    .await;
    let config = config_with(
        &["redirect"],
        &[("redirect", &redirect, Protocol::OpenAiChat)],
    );
    let proxy = start_proxy(config).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let models = client
        .get(format!("http://{proxy}/pool/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(models.headers()["location"], "https://target.example/next");

    let chat = client
        .post(format!("http://{proxy}/pool/chat/completions"))
        .json(&chat_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(chat.headers()["location"], "https://target.example/next");

    let messages = client
        .post(format!("http://{proxy}/pool/messages"))
        .json(&json!({
            "model": "shared-model",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(messages.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        messages.headers()["location"],
        "https://target.example/next"
    );
    assert_eq!(counter.count(), 3);
}
