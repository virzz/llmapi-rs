use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use tokio::sync::Mutex;
use url::{Host, Url};

use crate::config::{Config, Protocol, ProviderConfig, UpstreamConfig};

#[derive(Deserialize)]
struct Health {
    service: String,
    instance_id: String,
}

struct Binding {
    source: UpstreamConfig,
    base_url: String,
    instance_id: Option<String>,
    verified: bool,
}

pub(crate) struct Discovery {
    client: reqwest::Client,
    path: Option<PathBuf>,
    // Serializes recovery and file updates so simultaneous failures share one scan.
    bindings: Mutex<BTreeMap<String, Binding>>,
}

impl Discovery {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_millis(750))
                .build()
                .expect("build discovery client"),
            path,
            bindings: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn initialize(&self, config: &mut Config) -> Result<()> {
        if let Some(path) = &self.path {
            let mut stored = Config::load_for_update(path)?;
            if stored.instance_id.is_none() {
                stored.instance_id = Some(uuid::Uuid::new_v4().to_string());
                save_atomic(&stored, path)?;
            }
            config.instance_id = stored.instance_id;
        } else if config.instance_id.is_none() {
            config.instance_id = Some(uuid::Uuid::new_v4().to_string());
        }
        for (name, provider) in &mut config.providers {
            let Some(upstream) = provider
                .as_upstream_mut()
                .filter(|p| p.protocol == Protocol::Llmapi)
            else {
                continue;
            };
            match self.recover(name, upstream, &upstream.base_url).await {
                Ok(base_url) => {
                    let mut bindings = self.bindings.lock().await;
                    upstream.base_url = base_url;
                    upstream.instance_id = bindings[name].instance_id.clone();
                    bindings.get_mut(name).unwrap().source = upstream.clone();
                }
                Err(error) => {
                    tracing::warn!(provider = %name, %error, "llmapi health check failed")
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn current(&self, name: &str, upstream: &UpstreamConfig) -> Result<String> {
        {
            let bindings = self.bindings.lock().await;
            if let Some(binding) = bindings
                .get(name)
                .filter(|b| b.source == *upstream && b.verified)
            {
                return Ok(binding.base_url.clone());
            }
        }
        // Also verifies newly added or edited providers when the config is reloaded.
        self.recover(name, upstream, &upstream.base_url).await
    }

    pub(crate) async fn recover(
        &self,
        name: &str,
        upstream: &UpstreamConfig,
        failed_url: &str,
    ) -> Result<String> {
        let mut bindings = self.bindings.lock().await;
        let binding = bindings.entry(name.to_owned()).or_insert_with(|| Binding {
            source: upstream.clone(),
            base_url: upstream.base_url.clone(),
            instance_id: upstream.instance_id.clone(),
            verified: false,
        });
        if binding.source != *upstream {
            *binding = Binding {
                source: upstream.clone(),
                base_url: upstream.base_url.clone(),
                instance_id: upstream.instance_id.clone(),
                verified: false,
            };
        }
        if binding.verified && binding.base_url != failed_url {
            return Ok(binding.base_url.clone());
        }
        binding.verified = false;
        let observed = self.health(&binding.base_url).await;
        let (base_url, id) = match (&binding.instance_id, observed) {
            (None, Some(id)) => (binding.base_url.clone(), id),
            (Some(expected), Some(id)) if expected == &id => (binding.base_url.clone(), id),
            (Some(expected), _) => (
                self.find(expected, candidates(&binding.base_url)?).await?,
                expected.clone(),
            ),
            (None, None) => bail!(
                "provider `{name}` is unreachable and has no instance_id; first connect to it or configure its instance_id"
            ),
        };
        if let Some(path) = &self.path {
            let mut latest = Config::load(path)?;
            let current = latest
                .providers
                .get_mut(name)
                .and_then(ProviderConfig::as_upstream_mut)
                .context("provider was removed during discovery")?;
            ensure!(
                current == upstream,
                "provider changed during discovery; retry with the reloaded configuration"
            );
            if current.base_url != base_url || current.instance_id.as_ref() != Some(&id) {
                current.base_url = base_url.clone();
                current.instance_id = Some(id.clone());
                save_atomic(&latest, path)?;
            }
        }
        binding.base_url = base_url.clone();
        binding.instance_id = Some(id);
        binding.verified = true;
        Ok(base_url)
    }

    async fn health(&self, base: &str) -> Option<String> {
        let mut url = Url::parse(base).ok()?;
        url.set_path("/health");
        url.set_query(None);
        url.set_fragment(None);
        url.set_username("").ok()?;
        url.set_password(None).ok()?;
        // Discovery never broadcasts provider credentials to other hosts.
        let response = self
            .client
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let bytes = crate::proxy::read_upstream_body(response, 4096)
            .await
            .ok()?;
        let health: Health = serde_json::from_slice(&bytes).ok()?;
        (health.service == "llmapi" && !health.instance_id.is_empty()).then_some(health.instance_id)
    }

    async fn find(&self, expected: &str, urls: Vec<String>) -> Result<String> {
        let mut probes = stream::iter(urls)
            .map(|url| async move {
                let id = self.health(&url).await;
                (url, id)
            })
            .buffer_unordered(32);
        let mut matched = None;
        while let Some((url, id)) = probes.next().await {
            if id.as_deref() == Some(expected) {
                ensure!(
                    matched.is_none(),
                    "multiple hosts report instance_id `{expected}`; refusing ambiguous discovery"
                );
                matched = Some(url);
            }
        }
        matched.context("no matching llmapi instance in the original /24 subnet")
    }
}

fn candidates(base: &str) -> Result<Vec<String>> {
    let mut url = Url::parse(base)?;
    let Some(Host::Ipv4(ip)) = url.host() else {
        bail!("automatic discovery requires an IPv4 baseurl")
    };
    let [a, b, c, _] = ip.octets();
    (1..=254)
        .map(|last| {
            url.set_ip_host(Ipv4Addr::new(a, b, c, last).into())
                .map_err(|_| anyhow::anyhow!("invalid discovery host"))?;
            Ok(url.to_string())
        })
        .collect()
}

pub(crate) fn relocate(request: &str, base: &str) -> Result<String> {
    let mut request = Url::parse(request)?;
    let base = Url::parse(base)?;
    request.set_host(base.host_str())?;
    Ok(request.to_string())
}

fn save_atomic(config: &Config, path: &Path) -> Result<()> {
    let temporary = path.with_file_name(format!(
        ".{}.{}.{}",
        path.file_stem().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4(),
        path.extension().unwrap().to_string_lossy()
    ));
    let result = (|| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        match std::fs::metadata(path) {
            Ok(metadata) => file.set_permissions(metadata.permissions())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(file);
        config.save(&temporary)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;

    async fn endpoint(id: &str) -> String {
        let id = id.to_owned();
        let app = Router::new().route(
            "/health",
            get(move || async move { Json(json!({"service": "llmapi", "instance_id": id})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[test]
    fn scan_stays_in_original_subnet_and_preserves_endpoint() {
        let urls = candidates("http://192.168.2.42:8080/team/v1").unwrap();
        assert_eq!(urls.len(), 254);
        assert_eq!(urls[0], "http://192.168.2.1:8080/team/v1");
        assert_eq!(urls[253], "http://192.168.2.254:8080/team/v1");
        assert!(candidates("http://example.com:8080").is_err());
        assert!(candidates("http://[::1]:8080").is_err());
    }

    #[tokio::test]
    async fn scan_matches_identity_and_rejects_duplicates() {
        let discovery = Discovery::new(None);
        let other = endpoint("other").await;
        let wanted = endpoint("wanted").await;
        assert_eq!(
            discovery
                .find("wanted", vec![other.clone(), wanted.clone()])
                .await
                .unwrap(),
            wanted
        );
        assert!(discovery.find("missing", vec![other]).await.is_err());
        let duplicate = endpoint("wanted").await;
        assert!(
            discovery
                .find("wanted", vec![wanted, duplicate])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn startup_pins_each_provider_and_preserves_local_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let first = endpoint("first").await;
        let second = endpoint("second").await;
        let mut config = Config::default();
        config
            .add_provider(
                "first".into(),
                ProviderConfig::upstream(Protocol::Llmapi, first, None),
            )
            .unwrap();
        config
            .add_provider(
                "second".into(),
                ProviderConfig::upstream(Protocol::Llmapi, second, None),
            )
            .unwrap();
        config.save(&path).unwrap();
        let discovery = Discovery::new(Some(path.clone()));
        discovery.initialize(&mut config).await.unwrap();
        let stored = Config::load(&path).unwrap();
        assert_eq!(
            stored.providers["first"]
                .as_upstream()
                .unwrap()
                .instance_id
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            stored.providers["second"]
                .as_upstream()
                .unwrap()
                .instance_id
                .as_deref(),
            Some("second")
        );
        let id = stored.instance_id.clone().unwrap();
        let mut restarted = Config::load(&path).unwrap();
        discovery.initialize(&mut restarted).await.unwrap();
        assert_eq!(restarted.instance_id.as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn unknown_identity_is_not_bound_by_scanning() {
        let discovery = Discovery::new(None);
        let provider = ProviderConfig::upstream(Protocol::Llmapi, "http://127.0.0.1:0", None);
        let error = discovery
            .recover(
                "relay",
                provider.as_upstream().unwrap(),
                "http://127.0.0.1:0",
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("instance_id"));
    }

    #[tokio::test]
    async fn initialization_keeps_existing_identity_without_rewriting_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let id = uuid::Uuid::new_v4().to_string();
        let body = format!("# Preserve this comment and formatting.\ninstance_id = '{id}'\n");
        std::fs::write(&path, &body).unwrap();
        let mut config = Config::load(&path).unwrap();
        Discovery::new(Some(path.clone()))
            .initialize(&mut config)
            .await
            .unwrap();
        assert_eq!(config.instance_id.as_deref(), Some(id.as_str()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    }

    #[tokio::test]
    async fn initialization_creates_missing_config_without_persisting_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/config.json");
        let mut config = Config {
            server: "0.0.0.0:12345".into(),
            ..Config::default()
        };
        Discovery::new(Some(path.clone()))
            .initialize(&mut config)
            .await
            .unwrap();
        let saved = Config::load(&path).unwrap();
        assert!(saved.instance_id.is_some());
        assert_eq!(saved.instance_id, config.instance_id);
        assert_eq!(saved.server, Config::default().server);
        assert_eq!(config.server, "0.0.0.0:12345");
    }

    #[tokio::test]
    async fn recovery_updates_only_the_matched_provider_and_survives_reload() {
        use axum::{
            body::Body,
            http::{HeaderMap, Request, StatusCode},
            routing::post,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let health_calls = Arc::new(AtomicUsize::new(0));
        let calls = health_calls.clone();
        let app = Router::new()
            .route(
                "/health",
                get(move || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Json(json!({"service": "llmapi", "instance_id": "wanted"})) }
                }),
            )
            .route(
                "/chat/completions",
                post(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer secret");
                    StatusCode::TOO_MANY_REQUESTS
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let stale = live.replace("127.0.0.1", "127.0.0.2");
        let mut provider =
            ProviderConfig::upstream(Protocol::Llmapi, &stale, Some("secret".into()));
        provider.as_upstream_mut().unwrap().instance_id = Some("wanted".into());
        let mut config = Config::default();
        config
            .add_provider("relay".into(), provider.clone())
            .unwrap();
        config
            .add_provider(
                "other".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, "http://other.test", None),
            )
            .unwrap();
        config.save(&path).unwrap();
        let discovery = Arc::new(Discovery::new(Some(path.clone())));
        let upstream = provider.as_upstream().unwrap();
        // Simulate an already verified address that later stopped accepting connections.
        discovery.bindings.lock().await.insert(
            "relay".into(),
            Binding {
                source: upstream.clone(),
                base_url: stale.clone(),
                instance_id: Some("wanted".into()),
                verified: true,
            },
        );
        assert_eq!(discovery.current("relay", upstream).await.unwrap(), stale);
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let state = crate::server::AppState {
            discovery: Some(discovery),
            usage: None,
            config: Arc::new(config.clone()),
            client: client.clone(),
            gemini_client: client.clone(),
            lb_client: client,
            codex_headers: None,
            lb: Arc::new(crate::lb::State::new(&config)),
            lb_header_timeout: Duration::from_secs(30),
        };
        for _ in 0..2 {
            let request = Request::builder()
                .method("POST")
                .uri("/relay/chat/completions")
                .body(Body::from(r#"{"model":"test","messages":[]}"#))
                .unwrap();
            let response = crate::proxy::handle(state.clone(), HeaderMap::new(), request).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        }
        // The second request and HTTP 429 must not trigger another health probe or scan.
        assert_eq!(health_calls.load(Ordering::Relaxed), 1);
        let stored = Config::load(&path).unwrap();
        let updated = stored.providers["relay"].as_upstream().unwrap();
        assert_eq!(updated.base_url, live);
        assert_eq!(updated.instance_id.as_deref(), Some("wanted"));
        assert_eq!(updated.api_key.as_deref(), Some("secret"));
        assert_eq!(stored.providers["other"], config.providers["other"]);
        assert_eq!(stored.default, config.default);
    }

    #[tokio::test]
    async fn discovery_does_not_send_url_credentials_or_queries() {
        let app = Router::new().route(
            "/health",
            get(
                |headers: axum::http::HeaderMap, uri: axum::http::Uri| async move {
                    assert!(!headers.contains_key("authorization"));
                    assert!(!headers.contains_key("x-api-key"));
                    assert!(uri.query().is_none());
                    Json(json!({"service": "llmapi", "instance_id": "wanted"}))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let discovery = Discovery::new(None);
        assert_eq!(
            discovery
                .health(&format!(
                    "http://user:password@{addr}/provider/v1?key=secret"
                ))
                .await
                .as_deref(),
            Some("wanted")
        );
    }
}
