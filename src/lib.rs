//! @about: LLM API protocol conversion proxy

pub mod adapters;
pub mod auth;
mod codex;
pub mod config;
#[cfg(target_os = "macos")]
pub mod daemon;
pub mod lb;
pub mod model;
mod models;
pub mod protocol;
pub mod proxy;
mod redact;
pub mod server;

use std::{
    io::{self, IsTerminal},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use axum::http::{header, HeaderMap, Method};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use comfy_table::Table;
use serde_json::{json, Value};

use config::{Config, Protocol, Provider, ProviderConfig, UpstreamConfig};

#[derive(Debug, Parser)]
#[command(name = "llmapi")]
pub struct Cmd {
    /// Config file path
    #[arg(short = 'c', long = "config", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Shell completion scripts
    #[clap(alias = "comp")]
    Completion {
        #[arg(help = "shell type")]
        shell: Option<Shell>,
    },
    /// List configured providers
    #[command(visible_aliases = ["ls", "l"])]
    List,
    /// Check that a provider is reachable
    Check(CheckArgs),
    /// Add a provider
    #[clap(visible_alias = "a")]
    Add(AddArgs),
    /// Remove a provider
    #[command(visible_alias = "rm")]
    Remove(RemoveArgs),
    /// Change llmapi settings
    Set(SetArgs),
    /// Start the HTTP server
    Server(ServerArgs),
    /// Manage the macOS LaunchAgent
    #[cfg(target_os = "macos")]
    Daemon(daemon::DaemonArgs),
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Provider name used in HTTP route prefixes
    name: String,
    /// Provider type: openai-chat|chat, openai-responses|responses, anthropic|messages, lb
    #[arg(long = "type")]
    provider_type: Provider,
    /// Upstream API base URL (required unless --members is used)
    #[arg(long, required_unless_present = "members", conflicts_with = "members")]
    baseurl: Option<String>,
    /// Comma-separated member providers for --type lb
    #[arg(
        long,
        value_delimiter = ',',
        required_if_eq("provider_type", "lb"),
        conflicts_with_all = ["baseurl", "apikey", "apikey_stdin"]
    )]
    members: Option<Vec<String>>,
    /// Read the upstream API key interactively without echoing it
    #[arg(long, conflicts_with = "apikey_stdin")]
    apikey: bool,
    /// Read the upstream API key from stdin
    #[arg(long = "apikey-stdin", conflicts_with = "apikey")]
    apikey_stdin: bool,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    /// Provider name
    name: String,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Provider name (defaults to the configured default)
    provider: Option<String>,
    /// Send a minimal completion request with this model instead of listing models
    #[arg(long)]
    model: Option<String>,
    /// Request timeout in seconds
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
}

#[derive(Debug, Args)]
struct SetArgs {
    #[command(subcommand)]
    command: SetCommand,
}

#[derive(Debug, Subcommand)]
enum SetCommand {
    /// Set the default provider
    Default { provider: String },
}

#[derive(Debug, Args)]
struct ServerArgs {
    /// Override the server listen address from the config file
    #[arg(long)]
    server: Option<String>,
    /// Override the default provider from the config file
    #[arg(long)]
    default: Option<String>,
}

impl ServerArgs {
    fn apply_to(&self, config: &mut Config) -> Result<()> {
        if let Some(default) = &self.default {
            config.set_default(default)?;
        }
        if let Some(server) = &self.server {
            config.server = server.clone();
        }
        Ok(())
    }
}

impl Cmd {
    fn default_config_path() -> PathBuf {
        dirs::home_dir()
            .map(|home| Self::default_config_path_in(&home))
            .unwrap_or_else(|| PathBuf::from(".config/enyo/llmapi.yaml"))
    }

    fn default_config_path_in(home: &Path) -> PathBuf {
        let directory = home.join(".config/enyo");
        for name in ["llmapi.yaml", "llmapi.toml", "llmapi.yml", "llmapi.json"] {
            let path = directory.join(name);
            if path.is_file() {
                return path;
            }
        }
        directory.join("llmapi.yaml")
    }

    fn config_path(&self) -> PathBuf {
        self.config_path_in(Path::new("."))
    }

    fn config_path_in(&self, directory: &Path) -> PathBuf {
        if let Some(path) = &self.config {
            return path.clone();
        }
        for name in ["config.toml", "config.yaml", "config.yml", "config.json"] {
            let path = directory.join(name);
            if path.is_file() {
                return path;
            }
        }
        Self::default_config_path()
    }

    fn list(&self) -> Result<()> {
        let config = self.load_config()?.with_codex(codex::available());
        print!("{}", format_provider_list(&config));
        Ok(())
    }

    async fn check(&self, args: &CheckArgs) -> Result<()> {
        self.check_with_header_timeout(args, lb::ATTEMPT_HEADER_TIMEOUT)
            .await
    }

    async fn check_with_header_timeout(
        &self,
        args: &CheckArgs,
        lb_header_timeout: Duration,
    ) -> Result<()> {
        let codex_headers =
            codex::auth_path().and_then(|path| codex::read_auth(&path).ok().flatten());
        let config = self.load_config()?.with_codex(codex_headers.is_some());
        if config.providers.is_empty() {
            bail!("no providers configured");
        }
        let (name, provider) = config.provider(args.provider.as_deref())?;
        let (name, provider) = (name.to_owned(), provider.clone());
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("build HTTP client")?;
        let deadline = Instant::now() + Duration::from_secs(args.timeout);
        let probe = if args.model.is_some() {
            "model"
        } else {
            "models"
        };
        match &provider {
            ProviderConfig::Upstream(upstream) => {
                let timeout = remaining_budget(deadline)?;
                let outcome = probe_provider(
                    &client,
                    upstream,
                    codex_headers.as_ref(),
                    args.model.as_deref(),
                    Some(timeout),
                )
                .await
                .with_context(|| format!("check provider {name}"))?;
                print!(
                    "{}",
                    format_check_report(&name, upstream.protocol.as_str(), probe, &outcome)
                );
                if !outcome.status.is_success() {
                    bail!("provider {name} check failed: {}", outcome.status);
                }
            }
            ProviderConfig::Lb(lb) => {
                // One member probe equals one upstream request: no redirects
                // and no implicit protocol-NACK retries.
                let lb_client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .build()
                    .context("build HTTP client")?;
                let started = Instant::now();
                let mut attempts = 0;
                let mut failures = Vec::new();
                let mut final_outcome = None;
                for member in &lb.members {
                    attempts += 1;
                    let Some(upstream) = config
                        .providers
                        .get(member)
                        .and_then(ProviderConfig::as_upstream)
                    else {
                        failures.push(format!("{member}: provider is not configured"));
                        continue;
                    };
                    if upstream.protocol == Protocol::Codex && codex_headers.is_none() {
                        failures.push(format!("{member}: Codex credentials are unavailable"));
                        continue;
                    }
                    let Ok(remaining) = remaining_budget(deadline) else {
                        failures.push(format!("{member}: timeout budget exhausted"));
                        break;
                    };
                    let header_timeout = remaining.min(lb_header_timeout);
                    let probe = match send_probe(
                        &lb_client,
                        upstream,
                        codex_headers.as_ref(),
                        args.model.as_deref(),
                        Some(remaining),
                        Some(header_timeout),
                    )
                    .await
                    {
                        Ok(probe) => probe,
                        Err(err) => {
                            failures.push(format!("{member}: {err}"));
                            continue;
                        }
                    };
                    // Switch on the status code immediately; never wait for a
                    // failing member's error body before trying the next one.
                    if lb::is_retryable_status(probe.response.status()) {
                        failures.push(format!(
                            "{member}: upstream status {}",
                            probe.response.status()
                        ));
                        continue;
                    }
                    // Once headers are accepted, body/stream failures are final
                    // and must not be replayed against another member.
                    let secrets = probe_secrets(upstream, codex_headers.as_ref());
                    let outcome = finish_probe(
                        probe,
                        upstream.protocol,
                        args.model.as_deref(),
                        upstream.protocol == Protocol::Codex,
                        &secrets,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "load balancer {name} check failed on member {member} after {attempts} attempt(s)"
                        )
                    })?;
                    final_outcome = Some((member.clone(), outcome));
                    break;
                }
                let Some((member, mut outcome)) = final_outcome else {
                    bail!(
                        "load balancer {name} check failed after {attempts} attempt(s): {}",
                        if failures.is_empty() {
                            "timeout budget exhausted".to_string()
                        } else {
                            failures.join("; ")
                        }
                    );
                };
                outcome.latency = started.elapsed();
                outcome.detail = format!(
                    "member {} after {} attempt(s): {}",
                    member, attempts, outcome.detail
                );
                print!("{}", format_check_report(&name, "lb", probe, &outcome));
                if !outcome.status.is_success() {
                    bail!(
                        "load balancer {name} check failed: member {member} returned {}",
                        outcome.status
                    );
                }
            }
        }
        Ok(())
    }

    fn add(&self, args: &AddArgs) -> Result<()> {
        let path = self.config_path();
        let mut config = Config::load_for_update(&path)
            .with_context(|| format!("load config {}", path.display()))?;
        let provider = match args.provider_type {
            Provider::Lb => {
                anyhow::ensure!(
                    args.baseurl.is_none() && !args.apikey && !args.apikey_stdin,
                    "lb providers do not accept --baseurl, --apikey or --apikey-stdin"
                );
                let members = args
                    .members
                    .clone()
                    .context("--type lb requires --members")?;
                anyhow::ensure!(!members.is_empty(), "--members cannot be empty");
                ProviderConfig::lb(members)
            }
            provider_type => {
                anyhow::ensure!(args.members.is_none(), "only --type lb accepts --members");
                let baseurl = args
                    .baseurl
                    .clone()
                    .context("non-lb providers require --baseurl")?;
                let api_key = if args.apikey {
                    Some(rpassword::prompt_password("Upstream API key: ")?)
                } else if args.apikey_stdin {
                    anyhow::ensure!(
                        !io::stdin().is_terminal(),
                        "--apikey-stdin requires piped stdin"
                    );
                    let mut key = String::new();
                    io::stdin().read_line(&mut key)?;
                    Some(key.trim_end_matches(['\r', '\n']).to_owned())
                } else {
                    None
                };
                if let Some(key) = &api_key {
                    anyhow::ensure!(!key.is_empty(), "upstream API key cannot be empty");
                }
                ProviderConfig::upstream(
                    provider_type
                        .protocol()
                        .expect("non-lb provider has a protocol"),
                    baseurl,
                    api_key,
                )
            }
        };
        config.add_provider(args.name.clone(), provider)?;
        config
            .save(&path)
            .with_context(|| format!("save config {}", path.display()))?;
        Ok(())
    }

    fn remove(&self, args: &RemoveArgs) -> Result<()> {
        let path = self.config_path();
        let mut config =
            Config::load(&path).with_context(|| format!("load config {}", path.display()))?;
        config.remove_provider(&args.name)?;
        config
            .save(&path)
            .with_context(|| format!("save config {}", path.display()))?;
        Ok(())
    }

    fn set(&self, args: &SetArgs) -> Result<()> {
        let path = self.config_path();
        let mut config = self.load_config()?;
        match &args.command {
            SetCommand::Default { provider } => config.set_default(provider)?,
        }
        config
            .save(&path)
            .with_context(|| format!("save config {}", path.display()))?;
        Ok(())
    }

    fn load_config(&self) -> Result<Config> {
        let path = self.config_path();
        let result = if self.config.is_some() {
            Config::load(&path)
        } else {
            Config::load_for_update(&path)
        };
        result.with_context(|| format!("load config {}", path.display()))
    }

    async fn serve(&self, args: &ServerArgs) -> Result<()> {
        let path = self.config_path();
        let mut config = self.load_config()?;
        args.apply_to(&mut config)?;
        let addr: SocketAddr = config
            .server
            .parse()
            .context("parse server listen address")?;
        server::serve_reloading(
            addr,
            config,
            path,
            args.server.is_some(),
            args.default.clone(),
        )
        .await
    }

    pub async fn execute(&self) -> Result<()> {
        match &self.command {
            Command::Completion { shell } => {
                let shell = shell.or_else(Shell::from_env).unwrap_or(Shell::Bash);
                generate(shell, &mut Self::command(), "llmapi", &mut io::stdout());
                Ok(())
            }
            Command::List => self.list(),
            Command::Check(args) => self.check(args).await,
            Command::Add(args) => self.add(args),
            Command::Remove(args) => self.remove(args),
            Command::Set(args) => self.set(args),
            Command::Server(args) => self.serve(args).await,
            #[cfg(target_os = "macos")]
            Command::Daemon(args) => args.execute(),
        }
    }
}

fn format_provider_list(config: &Config) -> String {
    let mut table = Table::new();
    table.set_header(["NAME", "TYPE", "BASEURL / MEMBERS", "DEFAULT"]);
    for (name, provider) in &config.providers {
        let (provider_type, target) = match provider {
            ProviderConfig::Upstream(upstream) => {
                (upstream.protocol.as_str(), redact::url(&upstream.base_url))
            }
            ProviderConfig::Lb(lb) => ("lb", format!("members: {}", lb.members.join(", "))),
        };
        table.add_row([
            name.as_str(),
            provider_type,
            target.as_str(),
            if name == &config.default { "*" } else { "" },
        ]);
    }
    format!("{table}\n")
}

#[derive(Debug)]
struct CheckOutcome {
    status: reqwest::StatusCode,
    latency: Duration,
    detail: String,
}

struct ProbeResponse {
    response: reqwest::Response,
    latency: Duration,
}

async fn send_probe(
    client: &reqwest::Client,
    upstream: &UpstreamConfig,
    codex_headers: Option<&HeaderMap>,
    model: Option<&str>,
    timeout: Option<Duration>,
    header_timeout: Option<Duration>,
) -> Result<ProbeResponse> {
    if let Some(model) = model {
        anyhow::ensure!(
            upstream.allows_model(model),
            "model is not allowed by this provider"
        );
    }
    let api_key = upstream.api_key()?;
    let mut headers = HeaderMap::new();
    auth::apply_api_key(&mut headers, upstream.protocol, api_key.as_deref(), None);
    auth::apply_protocol_headers(&mut headers, upstream.protocol, &HeaderMap::new());

    let codex = upstream.protocol == Protocol::Codex;
    let (method, path, body) = match (upstream.protocol, model) {
        (_, None) => (Method::GET, "/models", None),
        (Protocol::OpenAiChat, Some(model)) => (
            Method::POST,
            "/chat/completions",
            Some(json!({
                "model": model,
                "messages": [{"role": "user", "content": "ping"}],
                "stream": false
            })),
        ),
        (Protocol::OpenAiResponses, Some(model)) => (
            Method::POST,
            "/responses",
            Some(json!({
                "model": model,
                "input": "ping",
                "stream": false
            })),
        ),
        (Protocol::Anthropic, Some(model)) => (
            Method::POST,
            "/messages",
            Some(json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "ping"}]
            })),
        ),
        (Protocol::Codex, Some(model)) => {
            let mut body = json!({"model": model, "input": "ping"});
            codex::prepare_request(&mut body)?;
            (Method::POST, "/responses", Some(body))
        }
    };
    if codex {
        let auth = codex_headers.context("Codex credentials are unavailable")?;
        codex::apply_headers(&mut headers, auth, model.is_some());
    }
    let body = body
        .map(|body| serde_json::to_vec(&body))
        .transpose()
        .context("serialize probe request")?;
    let mut url = proxy::upstream_url(&upstream.base_url, path, None);
    if codex && model.is_none() {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("client_version=");
        url.push_str(codex::CLIENT_VERSION);
    }

    let request = client
        .request(method, &url)
        .headers(headers)
        .header(header::CONTENT_TYPE, "application/json");
    let request = match body {
        Some(body) => request.body(body),
        None => request,
    };
    let request = match timeout {
        Some(timeout) => request.timeout(timeout),
        None => request,
    };
    let started = Instant::now();
    let send = request.send();
    let response = match header_timeout {
        Some(limit) => match tokio::time::timeout(limit, send).await {
            Ok(result) => result,
            Err(_) => bail!(
                "request upstream {}: response header timeout after {}ms",
                redact::url(&url),
                limit.as_millis()
            ),
        },
        None => send.await,
    };
    let response = response.map_err(|err| {
        anyhow!(
            "request upstream {}: {}",
            redact::url(&url),
            redact::transport_error(err)
        )
    })?;
    Ok(ProbeResponse {
        response,
        latency: started.elapsed(),
    })
}

/// Read the probe result after the response headers have been accepted.
///
/// Errors here happen after a member has been chosen, so callers must not fail
/// over to another member for them.
async fn finish_probe(
    probe: ProbeResponse,
    protocol: Protocol,
    model: Option<&str>,
    codex: bool,
    secrets: &[String],
) -> Result<CheckOutcome> {
    let ProbeResponse {
        mut response,
        latency,
    } = probe;
    let status = response.status();
    if !status.is_success() {
        // Do not require a well-formed body for an error report, but never
        // buffer an unbounded response. Redact before summarizing so a secret
        // crossing the summary boundary cannot survive as a truncated prefix.
        let detail = match proxy::read_upstream_body(response, proxy::MAX_ERROR_BODY_BYTES).await {
            Ok(body) => compact_text(&redact_error_body(&body, secrets)),
            Err(err) => format!("response body unavailable: {err}"),
        };
        return Ok(CheckOutcome {
            status,
            latency,
            detail,
        });
    }

    let detail = match (model, codex) {
        (None, _) => {
            let body = read_probe_body(response).await?;
            serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| model_count(&value))
                .map(|count| format!("{count} models"))
                .unwrap_or_else(|| "reachable".into())
        }
        (Some(_), true) => match response
            .chunk()
            .await
            .map_err(|err| anyhow!("read upstream stream: {}", redact::transport_error(err)))?
        {
            Some(chunk) => format!("stream received ({} bytes)", chunk.len()),
            None => "empty stream".into(),
        },
        (Some(_), false) => {
            let body = read_probe_body(response).await?;
            response_text(protocol, &body)
                .map(|text| {
                    format!(
                        "reply: {}",
                        truncate(&collapse(&redact_known_secrets(&text, secrets)), 80)
                    )
                })
                .unwrap_or_else(|| "reachable".into())
        }
    };
    Ok(CheckOutcome {
        status,
        latency,
        detail,
    })
}

/// Read a probe response body with the same upper bound as proxied responses.
async fn read_probe_body(response: reqwest::Response) -> Result<bytes::Bytes> {
    proxy::read_upstream_body(response, proxy::MAX_JSON_BODY_BYTES)
        .await
        .map_err(|err| anyhow!("read upstream response: {err}"))
}

fn codex_bearer_token(headers: Option<&HeaderMap>) -> Option<String> {
    headers?
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// Credentials that may appear in request diagnostics for this provider.
fn probe_secrets(upstream: &UpstreamConfig, codex_headers: Option<&HeaderMap>) -> Vec<String> {
    let mut secrets = Vec::new();
    if let Ok(Some(key)) = upstream.api_key() {
        secrets.push(key);
    }
    if upstream.protocol == Protocol::Codex {
        secrets.extend(codex_bearer_token(codex_headers));
    }
    secrets.extend(redact::url_secrets(&upstream.base_url));
    // The HTTP client turns URL userinfo into an `Authorization: Basic` value;
    // scrub both the full header value and its reversible payload.
    if let Some(token) = url_basic_token(&upstream.base_url) {
        if let Some(payload) = token.strip_prefix("Basic ") {
            secrets.push(payload.to_string());
        }
        secrets.push(token);
    }

    let mut seen = std::collections::BTreeSet::new();
    secrets.retain(|secret| !secret.is_empty() && seen.insert(secret.clone()));
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets
}

/// The header value `reqwest` generates from URL userinfo.
fn url_basic_token(url: &str) -> Option<String> {
    let (username, password) = redact::url_basic_auth(url)?;
    let payload = format!("{username}:{}", password.unwrap_or_default());
    Some(format!("Basic {}", base64_encode(payload.as_bytes())))
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b111111) as usize] as char
        } else {
            '='
        });
    }
    output
}

/// Replace every known credential before any summarizing/truncation step.
fn redact_known_secrets(text: &str, secrets: &[String]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        redacted = redacted.replace(secret.as_str(), "***");
    }
    redacted
}

/// Redact an error body before summarizing it.
///
/// JSON bodies are decoded first so escaped forms of a secret (quotes,
/// backslashes, unicode escapes) match the decoded values collected by
/// `probe_secrets`; other bodies are redacted as plain text.
fn redact_error_body(body: &[u8], secrets: &[String]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => {
            serde_json::to_string(&redact_json_strings(value, secrets)).unwrap_or_default()
        }
        Err(_) => redact_known_secrets(&String::from_utf8_lossy(body), secrets),
    }
}

fn redact_json_strings(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_known_secrets(&text, secrets)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_json_strings(value, secrets))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, redact_json_strings(value, secrets)))
                .collect(),
        ),
        other => other,
    }
}

async fn probe_provider(
    client: &reqwest::Client,
    upstream: &UpstreamConfig,
    codex_headers: Option<&HeaderMap>,
    model: Option<&str>,
    timeout: Option<Duration>,
) -> Result<CheckOutcome> {
    let probe = send_probe(client, upstream, codex_headers, model, timeout, None).await?;
    let codex = upstream.protocol == Protocol::Codex;
    let secrets = probe_secrets(upstream, codex_headers);
    finish_probe(probe, upstream.protocol, model, codex, &secrets).await
}

fn model_count(value: &Value) -> Option<usize> {
    value
        .get("data")
        .or_else(|| value.get("models"))?
        .as_array()
        .map(Vec::len)
}

fn response_text(protocol: Protocol, body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let text =
        match protocol {
            Protocol::OpenAiChat => json_text(&value["choices"][0]["message"]["content"]),
            Protocol::OpenAiResponses => value
                .get("output_text")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    value.get("output")?.as_array()?.iter().find_map(|item| {
                        item.get("content")?.as_array()?.iter().find_map(json_text)
                    })
                }),
            Protocol::Anthropic => value.get("content")?.as_array()?.iter().find_map(json_text),
            Protocol::Codex => None,
        }?;
    Some(text)
}

fn json_text(value: &Value) -> Option<String> {
    value.as_str().map(ToString::to_string).or_else(|| {
        value
            .as_array()?
            .iter()
            .find_map(|part| part.get("text")?.as_str().map(ToString::to_string))
    })
}

fn compact_text(text: &str) -> String {
    let text = collapse(text);
    if text.is_empty() {
        "empty response".into()
    } else {
        truncate(&text, 240)
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut output: String = text.chars().take(limit).collect();
    output.push_str("...");
    output
}

fn format_check_report(
    name: &str,
    provider_type: &str,
    probe: &str,
    outcome: &CheckOutcome,
) -> String {
    let status = outcome.status.to_string();
    let latency = format!("{} ms", outcome.latency.as_millis());
    let mut table = Table::new();
    table.set_header(["PROVIDER", "TYPE", "PROBE", "STATUS", "LATENCY", "DETAIL"]);
    table.add_row([
        name,
        provider_type,
        probe,
        &status,
        &latency,
        &outcome.detail,
    ]);
    format!("{table}\n")
}

fn remaining_budget(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(!remaining.is_zero(), "check timeout budget exhausted");
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        routing::{get, post},
        Json, Router,
    };
    use std::{collections::BTreeMap, fs};
    use tempfile::tempdir;

    fn upstream_config(
        protocol: Protocol,
        base_url: impl Into<String>,
        api_key: Option<&str>,
    ) -> UpstreamConfig {
        ProviderConfig::upstream(protocol, base_url, api_key.map(Into::into))
            .as_upstream()
            .unwrap()
            .clone()
    }

    #[test]
    fn default_config_path_supports_all_formats() {
        let dir = tempdir().unwrap();
        let directory = dir.path().join(".config/enyo");
        fs::create_dir_all(&directory).unwrap();
        assert_eq!(
            Cmd::default_config_path_in(dir.path()),
            directory.join("llmapi.yaml")
        );
        for name in ["llmapi.json", "llmapi.yml", "llmapi.toml", "llmapi.yaml"] {
            fs::write(directory.join(name), "").unwrap();
            assert_eq!(
                Cmd::default_config_path_in(dir.path()),
                directory.join(name)
            );
        }
    }

    #[test]
    fn config_path_prefers_explicit_then_local_formats_then_home() {
        let dir = tempdir().unwrap();
        let directory = dir.path();
        let cmd = Cmd::parse_from(["llmapi", "list"]);
        assert_eq!(cmd.config_path_in(directory), Cmd::default_config_path());

        for (name, body, expected) in [
            (
                "config.json",
                r#"{"default":"json","providers":{"json":{"type":"chat","baseurl":"https://json.test"}}}"#,
                "json",
            ),
            (
                "config.yml",
                "default: yml\nproviders:\n  yml:\n    type: chat\n    baseurl: https://yml.test\n",
                "yml",
            ),
        ] {
            fs::write(directory.join(name), body).unwrap();
            assert_eq!(cmd.config_path_in(directory), directory.join(name));
            assert_eq!(
                Config::load(cmd.config_path_in(directory)).unwrap().default,
                expected
            );
        }

        fs::write(
            directory.join("config.yaml"),
            "default: yaml\nproviders:\n  yaml:\n    type: chat\n    baseurl: https://yaml.test\n",
        )
        .unwrap();
        assert_eq!(cmd.config_path_in(directory), directory.join("config.yaml"));
        assert_eq!(
            Config::load(cmd.config_path_in(directory)).unwrap().default,
            "yaml"
        );

        fs::write(
            directory.join("config.toml"),
            "default = 'toml'\n[providers.toml]\ntype = 'chat'\nbaseurl = 'https://toml.test'\n",
        )
        .unwrap();
        assert_eq!(cmd.config_path_in(directory), directory.join("config.toml"));
        assert_eq!(
            Config::load(cmd.config_path_in(directory)).unwrap().default,
            "toml"
        );

        let explicit = directory.join("config.yaml");
        let cmd = Cmd::parse_from(["llmapi", "list", "--config", explicit.to_str().unwrap()]);
        assert_eq!(cmd.config_path_in(directory), explicit);
        assert_eq!(
            Config::load(cmd.config_path_in(directory)).unwrap().default,
            "yaml"
        );

        let missing = directory.join("missing.yaml");
        let cmd = Cmd::parse_from(["llmapi", "--config", missing.to_str().unwrap(), "list"]);
        assert_eq!(cmd.config_path_in(directory), missing);
        assert!(Config::load(cmd.config_path_in(directory)).is_err());
    }

    #[test]
    fn parses_required_subcommands() {
        let completion = Cmd::parse_from(["llmapi", "completion", "zsh"]);
        assert!(matches!(
            completion.command,
            Command::Completion {
                shell: Some(Shell::Zsh)
            }
        ));
        assert!(matches!(
            Cmd::parse_from(["llmapi", "comp", "fish"]).command,
            Command::Completion {
                shell: Some(Shell::Fish)
            }
        ));
        for name in ["list", "ls", "l"] {
            assert!(matches!(
                Cmd::try_parse_from(["llmapi", name]).unwrap().command,
                Command::List
            ));
        }
        assert!(matches!(
            Cmd::parse_from(["llmapi", "check"]).command,
            Command::Check(CheckArgs {
                provider: None,
                model: None,
                timeout: 30,
            })
        ));
        assert!(matches!(
            Cmd::parse_from([
                "llmapi",
                "check",
                "deepseek",
                "--model",
                "deepseek-chat",
                "--timeout",
                "5",
            ])
            .command,
            Command::Check(CheckArgs {
                provider: Some(_),
                model: Some(_),
                timeout: 5,
            })
        ));
        let add = Cmd::parse_from([
            "llmapi",
            "add",
            "deepseek",
            "--type",
            "chat",
            "--baseurl",
            "https://api.deepseek.test",
            "--apikey-stdin",
        ]);
        assert!(matches!(
            add.command,
            Command::Add(AddArgs {
                provider_type: Provider::OpenAiChat,
                apikey_stdin: true,
                ..
            })
        ));
        let interactive = Cmd::parse_from([
            "llmapi",
            "add",
            "openai",
            "--type",
            "responses",
            "--baseurl",
            "https://api.openai.test",
            "--apikey",
        ]);
        assert!(matches!(
            interactive.command,
            Command::Add(AddArgs { apikey: true, .. })
        ));
        assert!(Cmd::try_parse_from([
            "llmapi",
            "add",
            "openai",
            "--type",
            "responses",
            "--baseurl",
            "https://api.openai.test",
            "--apikey",
            "plaintext",
        ])
        .is_err());
        let lb = Cmd::parse_from([
            "llmapi",
            "add",
            "pool",
            "--type",
            "lb",
            "--members",
            "primary,secondary",
        ]);
        assert!(matches!(
            lb.command,
            Command::Add(AddArgs {
                provider_type: Provider::Lb,
                members: Some(ref members),
                ..
            }) if members == &vec!["primary".to_string(), "secondary".to_string()]
        ));
        assert!(Cmd::try_parse_from([
            "llmapi",
            "add",
            "pool",
            "--type",
            "lb",
            "--members",
            "primary",
            "--baseurl",
            "https://example.test",
        ])
        .is_err());
        assert!(Cmd::try_parse_from(["llmapi", "add", "pool", "--type", "lb"]).is_err());
        assert!(Cmd::try_parse_from([
            "llmapi",
            "add",
            "plain",
            "--type",
            "chat",
            "--baseurl",
            "https://example.test",
            "--members",
            "a,b",
        ])
        .is_err());
        let remove = Cmd::parse_from(["llmapi", "rm", "deepseek"]);
        assert!(matches!(remove.command, Command::Remove(RemoveArgs { .. })));
        let set = Cmd::parse_from(["llmapi", "set", "default", "deepseek"]);
        assert!(matches!(
            set.command,
            Command::Set(SetArgs {
                command: SetCommand::Default { .. }
            })
        ));
        assert!(matches!(
            Cmd::parse_from(["llmapi", "server"]).command,
            Command::Server(_)
        ));
    }

    #[test]
    fn add_and_set_commands_persist_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let add = Cmd::parse_from([
            "llmapi",
            "--config",
            path.to_str().unwrap(),
            "add",
            "deepseek",
            "--type",
            "chat",
            "--baseurl",
            "https://api.deepseek.test/",
        ]);
        let Command::Add(args) = &add.command else {
            unreachable!()
        };
        add.add(args).unwrap();

        let add_openai = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "openai",
            "--type",
            "responses",
            "--baseurl",
            "https://api.openai.test/v1",
        ]);
        let Command::Add(args) = &add_openai.command else {
            unreachable!()
        };
        add_openai.add(args).unwrap();

        let set = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "set",
            "default",
            "openai",
        ]);
        let Command::Set(args) = &set.command else {
            unreachable!()
        };
        set.set(args).unwrap();

        let config = Config::load(path).unwrap();
        assert_eq!(config.default, "openai");
        assert_eq!(config.providers.len(), 2);
    }

    #[test]
    fn remove_command_deletes_provider_and_reselects_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        config
            .add_provider(
                "first".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, "https://first.test", None),
            )
            .unwrap();
        config
            .add_provider(
                "second".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, "https://second.test", None),
            )
            .unwrap();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from(["llmapi", "-c", path.to_str().unwrap(), "rm", "first"]);
        let Command::Remove(args) = &cmd.command else {
            unreachable!()
        };
        cmd.remove(args).unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.default, "second");
        assert_eq!(config.providers.len(), 1);
    }

    #[test]
    fn provider_list_marks_default_without_api_keys() {
        let config = Config {
            server: "127.0.0.1:8080".into(),
            access_log: false,
            api_key: None,
            default: "deepseek".into(),
            providers: BTreeMap::from([(
                "deepseek".into(),
                ProviderConfig::upstream(
                    Protocol::OpenAiChat,
                    "https://api.deepseek.test",
                    Some("sk-secret".into()),
                ),
            )]),
        };

        let output = format_provider_list(&config);

        assert!(output.contains("deepseek"));
        assert!(output.contains("openai-chat"));
        assert!(output.contains("https://api.deepseek.test"));
        let row = output
            .lines()
            .find(|line| line.contains("| deepseek "))
            .unwrap();
        assert_eq!(row.rsplit('|').nth(1).unwrap().trim(), "*");
        assert!(!output.contains('\t'));
        assert!(!output.contains("sk-secret"));
    }

    #[test]
    fn server_overrides_parse() {
        let cmd = Cmd::parse_from([
            "llmapi",
            "server",
            "--server",
            "127.0.0.1:9090",
            "--default",
            "openai",
        ]);
        let Command::Server(args) = cmd.command else {
            unreachable!()
        };
        assert_eq!(args.server.as_deref(), Some("127.0.0.1:9090"));
        assert_eq!(args.default.as_deref(), Some("openai"));

        let mut config = Config {
            server: "127.0.0.1:8080".into(),
            access_log: false,
            api_key: None,
            default: "deepseek".into(),
            providers: BTreeMap::from([
                (
                    "deepseek".into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, "https://deepseek.test", None),
                ),
                (
                    "openai".into(),
                    ProviderConfig::upstream(
                        Protocol::OpenAiResponses,
                        "https://openai.test",
                        None,
                    ),
                ),
            ]),
        };
        args.apply_to(&mut config).unwrap();
        assert_eq!(config.server, "127.0.0.1:9090");
        assert_eq!(config.default, "openai");

        let missing = Cmd::parse_from(["llmapi", "server", "--default", "missing"]);
        let Command::Server(args) = missing.command else {
            unreachable!()
        };
        assert!(args.apply_to(&mut config).is_err());
        assert_eq!(config.default, "openai");
    }

    #[test]
    fn set_rejects_unknown_provider_without_rewriting_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        fs::write(
            &path,
            "default: deepseek\nproviders:\n  deepseek:\n    type: chat\n    baseurl: https://example.test\n",
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "set",
            "default",
            "missing",
        ]);
        let Command::Set(args) = &cmd.command else {
            unreachable!()
        };

        assert!(cmd.set(args).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[tokio::test]
    async fn check_models_probe_reports_model_count() {
        let upstream = spawn_upstream(Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                assert_eq!(
                    headers[header::AUTHORIZATION].to_str().unwrap(),
                    "Bearer sk-test"
                );
                Json(json!({"data": [{"id": "one"}, {"id": "two"}]}))
            }),
        ))
        .await;
        let provider = upstream_config(
            Protocol::OpenAiChat,
            format!("{upstream}/v1"),
            Some("sk-test"),
        );

        let outcome = probe_provider(&reqwest::Client::new(), &provider, None, None, None)
            .await
            .unwrap();

        assert_eq!(outcome.status.as_u16(), 200);
        assert_eq!(outcome.detail, "2 models");
    }

    #[tokio::test]
    async fn check_model_probe_uses_provider_protocol() {
        let upstream = spawn_upstream(Router::new().route(
            "/v1/chat/completions",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(
                    headers[header::AUTHORIZATION].to_str().unwrap(),
                    "Bearer sk-test"
                );
                assert_eq!(body["model"], "deepseek-chat");
                assert_eq!(body["messages"][0]["content"], "ping");
                assert_eq!(body["stream"], false);
                Json(json!({"choices": [{"message": {"content": "pong"}}]}))
            }),
        ))
        .await;
        let mut provider = upstream_config(
            Protocol::OpenAiChat,
            format!("{upstream}/v1"),
            Some("sk-test"),
        );
        provider.models = vec!["deepseek-chat".into()];
        let client = reqwest::Client::new();

        let outcome = probe_provider(&client, &provider, None, Some("deepseek-chat"), None)
            .await
            .unwrap();

        assert_eq!(outcome.status.as_u16(), 200);
        assert_eq!(outcome.detail, "reply: pong");

        let rejected = probe_provider(&client, &provider, None, Some("other-model"), None).await;
        assert!(matches!(rejected, Err(err) if err.to_string().contains("model is not allowed")));
    }

    #[tokio::test]
    async fn check_failure_reports_status_and_error_body() {
        let upstream = spawn_upstream(Router::new().route(
            "/v1/models",
            get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"message": "bad key"}})),
                )
            }),
        ))
        .await;
        let provider = upstream_config(
            Protocol::OpenAiChat,
            format!("{upstream}/v1"),
            Some("sk-test"),
        );

        let outcome = probe_provider(&reqwest::Client::new(), &provider, None, None, None)
            .await
            .unwrap();

        assert_eq!(outcome.status.as_u16(), 401);
        assert!(outcome.detail.contains("bad key"));
    }

    #[test]
    fn add_lb_command_persists_members_and_list_shows_them() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        for (name, protocol) in [("primary", "chat"), ("secondary", "responses")] {
            let cmd = Cmd::parse_from([
                "llmapi",
                "-c",
                path.to_str().unwrap(),
                "add",
                name,
                "--type",
                protocol,
                "--baseurl",
                &format!("https://{name}.test/v1"),
            ]);
            let Command::Add(args) = &cmd.command else {
                unreachable!()
            };
            cmd.add(args).unwrap();
        }
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "pool",
            "--type",
            "lb",
            "--members",
            "secondary,primary",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        cmd.add(args).unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.providers["pool"].as_lb().unwrap().members,
            ["secondary", "primary"]
        );
        assert!(config.providers["pool"].as_upstream().is_none());

        let output = format_provider_list(&config);
        let row = output
            .lines()
            .find(|line| line.contains("| pool "))
            .unwrap();
        assert!(row.contains("lb"));
        assert!(row.contains("secondary, primary"));
        assert!(!output.contains("sk-secret"));

        // Credentials embedded in a configured URL are redacted for display.
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "secured",
            "--type",
            "chat",
            "--baseurl",
            "https://user:pass@secured.test/v1?api_key=sk-hidden",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        cmd.add(args).unwrap();
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "secured-extra",
            "--type",
            "chat",
            "--baseurl",
            "https:///alice:s3cr3t@secured.test/v1",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        cmd.add(args).unwrap();
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "secured-network",
            "--type",
            "chat",
            "--baseurl",
            "//alice:s3cr3t@secured.test/v1",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        cmd.add(args).unwrap();
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "secured-ftp",
            "--type",
            "chat",
            "--baseurl",
            "ftp://alice:s3cr3t@secured.test/v1",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        cmd.add(args).unwrap();
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "secured-ftp-extra",
            "--type",
            "chat",
            "--baseurl",
            "ftp://alice:s3cr3t@secured.test:bad/v1",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        cmd.add(args).unwrap();
        let output = format_provider_list(&Config::load(&path).unwrap());
        assert!(
            output.contains("https://***@secured.test/v1?api_key=***"),
            "{output}"
        );
        assert!(output.contains("***@secured.test/v1"), "{output}");
        assert!(output.contains("https://***@secured.test/v1"), "{output}");
        assert!(output.contains("ftp://***@secured.test/v1"), "{output}");
        assert!(output.contains("ftp://***@secured.test:bad/v1"), "{output}");
        assert!(!output.contains("user:pass"), "{output}");
        assert!(!output.contains("sk-hidden"), "{output}");
        assert!(!output.contains("alice"), "{output}");
        assert!(!output.contains("s3cr3t"), "{output}");

        let set = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "set",
            "default",
            "pool",
        ]);
        let Command::Set(args) = &set.command else {
            unreachable!()
        };
        set.set(args).unwrap();
        assert_eq!(Config::load(&path).unwrap().default, "pool");
    }

    #[tokio::test]
    async fn probe_detail_redacts_credentials_embedded_in_member_url() {
        let rejected = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid password: s3cr3t-url-pass",
                )
            }),
        ))
        .await;
        let host = rejected.trim_start_matches("http://");
        let upstream = UpstreamConfig {
            protocol: Protocol::OpenAiChat,
            base_url: format!("http://alice:s3cr3t-url-pass@{host}/v1"),
            api_key: None,
            models: Vec::new(),
        };
        let secrets = probe_secrets(&upstream, None);
        assert!(
            secrets.iter().any(|secret| secret == "s3cr3t-url-pass"),
            "{secrets:?}"
        );

        let client = reqwest::Client::new();
        let probe = send_probe(
            &client,
            &upstream,
            None,
            None,
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .unwrap();
        let outcome = finish_probe(probe, upstream.protocol, None, false, &secrets)
            .await
            .unwrap();
        assert_eq!(outcome.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            !outcome.detail.contains("s3cr3t-url-pass"),
            "{}",
            outcome.detail
        );
        assert!(
            outcome.detail.contains("invalid password"),
            "{}",
            outcome.detail
        );
    }

    #[tokio::test]
    async fn probe_detail_redacts_sensitive_query_values_from_member_url() {
        let rejected = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "bad key: sk-query-secret",
                )
            }),
        ))
        .await;
        let upstream = UpstreamConfig {
            protocol: Protocol::OpenAiChat,
            base_url: format!("{rejected}/v1?api_key=sk-query-secret"),
            api_key: None,
            models: Vec::new(),
        };
        let secrets = probe_secrets(&upstream, None);
        assert!(
            secrets.iter().any(|secret| secret == "sk-query-secret"),
            "{secrets:?}"
        );

        let client = reqwest::Client::new();
        let probe = send_probe(
            &client,
            &upstream,
            None,
            None,
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .unwrap();
        let outcome = finish_probe(probe, upstream.protocol, None, false, &secrets)
            .await
            .unwrap();
        assert!(
            !outcome.detail.contains("sk-query-secret"),
            "{}",
            outcome.detail
        );
        assert!(outcome.detail.contains("bad key"), "{}", outcome.detail);
    }

    #[tokio::test]
    async fn probe_detail_redacts_generated_basic_authorization() {
        #[derive(Clone)]
        struct EchoBasic {
            authorization: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        }
        async fn echo_basic(
            axum::extract::State(state): axum::extract::State<EchoBasic>,
            headers: HeaderMap,
        ) -> (axum::http::StatusCode, String) {
            let authorization = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            *state.authorization.lock().unwrap() = Some(authorization.clone());
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("rejected authorization: {authorization}"),
            )
        }

        // The second case has an invalid UTF-8 password: reqwest drops it and
        // sends `alice:` only, so the scrubber must predict the same value.
        for (userinfo, payload, password) in [
            (
                "alice:s3cr3t-url-pass",
                "alice:s3cr3t-url-pass",
                "s3cr3t-url-pass",
            ),
            ("alice:%FF", "alice:", "%FF"),
        ] {
            let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
            let mock = spawn_upstream(
                Router::new()
                    .route("/{*path}", axum::routing::any(echo_basic))
                    .with_state(EchoBasic {
                        authorization: observed.clone(),
                    }),
            )
            .await;
            let host = mock.trim_start_matches("http://");
            let upstream = UpstreamConfig {
                protocol: Protocol::OpenAiChat,
                base_url: format!("http://{userinfo}@{host}/v1"),
                api_key: None,
                models: Vec::new(),
            };
            let expected = format!("Basic {}", base64_encode(payload.as_bytes()));
            let secrets = probe_secrets(&upstream, None);
            assert!(secrets.contains(&expected), "{userinfo}: {secrets:?}");

            let client = reqwest::Client::new();
            let probe = send_probe(
                &client,
                &upstream,
                None,
                None,
                Some(Duration::from_secs(5)),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                observed.lock().unwrap().as_deref(),
                Some(expected.as_str()),
                "{userinfo}"
            );
            let outcome = finish_probe(probe, upstream.protocol, None, false, &secrets)
                .await
                .unwrap();
            assert!(
                !outcome.detail.contains(&expected),
                "{userinfo}: {}",
                outcome.detail
            );
            assert!(
                !outcome.detail.contains(password),
                "{userinfo}: {}",
                outcome.detail
            );
            assert!(
                outcome.detail.contains("rejected authorization"),
                "{userinfo}: {}",
                outcome.detail
            );
        }
    }

    #[tokio::test]
    async fn probe_detail_redacts_raw_encoded_query_secrets() {
        let rejected = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid query: api_key=sk%2Dquery%2Dsecret",
                )
            }),
        ))
        .await;
        let upstream = UpstreamConfig {
            protocol: Protocol::OpenAiChat,
            base_url: format!("{rejected}/v1?api_key=sk%2Dquery%2Dsecret"),
            api_key: None,
            models: Vec::new(),
        };
        let secrets = probe_secrets(&upstream, None);
        for expected in ["sk%2Dquery%2Dsecret", "sk-query-secret"] {
            assert!(
                secrets.iter().any(|secret| secret == expected),
                "{expected}: {secrets:?}"
            );
        }

        let client = reqwest::Client::new();
        let probe = send_probe(
            &client,
            &upstream,
            None,
            None,
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .unwrap();
        let outcome = finish_probe(probe, upstream.protocol, None, false, &secrets)
            .await
            .unwrap();
        assert!(
            !outcome.detail.contains("sk%2Dquery%2Dsecret"),
            "{}",
            outcome.detail
        );
        assert!(
            !outcome.detail.contains("sk-query-secret"),
            "{}",
            outcome.detail
        );
        assert!(
            outcome.detail.contains("invalid query"),
            "{}",
            outcome.detail
        );
    }

    #[tokio::test]
    async fn probe_detail_redacts_json_escaped_credentials() {
        let secret = "p\"ass-word";
        let rejected = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"message": "invalid password: p\"ass-word"}})),
                )
            }),
        ))
        .await;
        let host = rejected.trim_start_matches("http://");
        let upstream = UpstreamConfig {
            protocol: Protocol::OpenAiChat,
            base_url: format!("http://alice:p%22ass-word@{host}/v1"),
            api_key: None,
            models: Vec::new(),
        };
        let secrets = probe_secrets(&upstream, None);
        assert!(secrets.iter().any(|value| value == secret), "{secrets:?}");

        let client = reqwest::Client::new();
        let probe = send_probe(
            &client,
            &upstream,
            None,
            None,
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .unwrap();
        let outcome = finish_probe(probe, upstream.protocol, None, false, &secrets)
            .await
            .unwrap();

        assert!(
            outcome.detail.contains("invalid password"),
            "{}",
            outcome.detail
        );
        assert!(!outcome.detail.contains(secret), "{}", outcome.detail);
        assert!(
            !outcome.detail.contains("p\\\"ass-word"),
            "{}",
            outcome.detail
        );
    }

    #[tokio::test]
    async fn probe_detail_redacts_before_truncating_error_summary() {
        let secret = "sk-boundary-secret-value";
        let body = format!("{}{secret}", "x".repeat(235));
        let upstream = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(move || {
                let body = body.clone();
                async move { (axum::http::StatusCode::BAD_REQUEST, body) }
            }),
        ))
        .await;
        let response = reqwest::get(format!("{upstream}/models")).await.unwrap();
        let secrets = vec![secret.to_string()];
        let outcome = finish_probe(
            ProbeResponse {
                response,
                latency: Duration::ZERO,
            },
            Protocol::OpenAiChat,
            None,
            false,
            &secrets,
        )
        .await
        .unwrap();

        assert!(!outcome.detail.contains(secret), "{}", outcome.detail);
        assert!(!outcome.detail.contains(&secret[..8]), "{}", outcome.detail);
    }

    #[tokio::test]
    async fn probe_detail_redacts_before_truncating_success_reply() {
        let secret = "sk-reply-secret-value";
        let content = format!("{}{secret}", "y".repeat(75));
        let upstream = spawn_upstream(Router::new().route(
            "/{*path}",
            get(move || {
                let content = content.clone();
                async move { Json(json!({"choices": [{"message": {"content": content}}]})) }
            }),
        ))
        .await;
        let response = reqwest::get(format!("{upstream}/models")).await.unwrap();
        let secrets = vec![secret.to_string()];
        let outcome = finish_probe(
            ProbeResponse {
                response,
                latency: Duration::ZERO,
            },
            Protocol::OpenAiChat,
            Some("shared-model"),
            false,
            &secrets,
        )
        .await
        .unwrap();

        assert!(outcome.detail.starts_with("reply: "), "{}", outcome.detail);
        assert!(!outcome.detail.contains(secret), "{}", outcome.detail);
        assert!(!outcome.detail.contains(&secret[..8]), "{}", outcome.detail);
    }

    #[test]
    fn add_rejects_invalid_lb_without_rewriting_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "add",
            "pool",
            "--type",
            "lb",
            "--members",
            "missing",
        ]);
        let Command::Add(args) = &cmd.command else {
            unreachable!()
        };
        assert!(cmd.add(args).is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn check_lb_uses_member_order_and_reports_final_member() {
        let unauthorized = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"message": "nope"}})),
                )
            }),
        ))
        .await;
        let healthy = spawn_upstream(
            Router::new()
                .route(
                    "/v1/models",
                    get(|| async { Json(json!({"data": [{"id": "one"}]})) }),
                )
                .route(
                    "/v1/chat/completions",
                    post(|Json(body): Json<Value>| async move {
                        assert_eq!(body["model"], "shared-model");
                        Json(json!({"choices": [{"message": {"content": "pong"}}]}))
                    }),
                ),
        )
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        config
            .add_provider(
                "first".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, format!("{unauthorized}/v1"), None),
            )
            .unwrap();
        config
            .add_provider(
                "second".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, format!("{healthy}/v1"), None),
            )
            .unwrap();
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        for model in [None, Some("shared-model")] {
            let mut args = vec!["llmapi", "-c", path.to_str().unwrap(), "check", "pool"];
            if let Some(model) = model {
                args.extend(["--model", model]);
            }
            args.extend(["--timeout", "5"]);
            let cmd = Cmd::parse_from(args);
            let Command::Check(args) = &cmd.command else {
                unreachable!()
            };
            cmd.check(args).await.unwrap();
        }
    }

    #[tokio::test]
    async fn check_lb_spends_one_shared_timeout_budget() {
        async fn slow_failure() -> impl axum::response::IntoResponse {
            tokio::time::sleep(Duration::from_millis(700)).await;
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "down"})),
            )
        }
        async fn slow_success() -> Json<Value> {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Json(json!({"data": [{"id": "late"}]}))
        }
        let first = spawn_upstream(Router::new().route("/models", get(slow_failure))).await;
        let second = spawn_upstream(Router::new().route("/models", get(slow_success))).await;
        let third_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = third_calls.clone();
        let third = spawn_upstream(Router::new().route(
            "/models",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({"data": []}))
                }
            }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [("first", &first), ("second", &second), ("third", &third)] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into(), "third".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--timeout",
            "1",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let started = Instant::now();
        let error = cmd.check(args).await.unwrap_err();
        // The second member needs more time than the first left over: a
        // per-member budget would let it succeed instead.
        assert!(
            error.to_string().contains("timeout budget exhausted"),
            "{error}"
        );
        assert_eq!(
            third_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "members after an exhausted budget must not be attempted"
        );
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
    }

    #[tokio::test]
    async fn check_lb_budget_covers_response_body_read_without_replay() {
        let first = spawn_upstream(Router::new().route(
            "/models",
            get(|| async {
                let chunks = async_stream::stream! {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{\"data\":[]}"));
                };
                axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from_stream(chunks))
                    .unwrap()
            }),
        ))
        .await;
        let second_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = second_calls.clone();
        let second = spawn_upstream(Router::new().route(
            "/models",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({"data": [{"id": "ok"}]}))
                }
            }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [("first", &first), ("second", &second)] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--timeout",
            "1",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let started = Instant::now();
        let error = cmd.check(args).await.unwrap_err();
        // The budget covers reading the already-accepted response body.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "body reads must stay inside the total budget: {error}"
        );
        assert_eq!(
            second_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an accepted response must never be replayed"
        );
    }

    #[tokio::test]
    async fn check_lb_switches_on_retryable_status_without_reading_the_body() {
        let first = spawn_upstream(Router::new().route(
            "/models",
            get(|| async {
                let chunks = async_stream::stream! {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{}"));
                };
                axum::response::Response::builder()
                    .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from_stream(chunks))
                    .unwrap()
            }),
        ))
        .await;
        let second = spawn_upstream(Router::new().route(
            "/models",
            get(|| async { Json(json!({"data": [{"id": "ok"}]})) }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [("first", &first), ("second", &second)] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--timeout",
            "1",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let started = Instant::now();
        cmd.check(args).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "check must switch before reading the failing body"
        );
    }

    #[tokio::test]
    async fn check_lb_does_not_replay_after_accepting_headers() {
        let unavailable_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = unavailable_calls.clone();
        let unavailable = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::http::StatusCode::UNAUTHORIZED
                }
            }),
        ))
        .await;
        let first = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(|| async {
                let chunks = async_stream::stream! {
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    yield Err(std::io::Error::other("response broke"));
                };
                axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from_stream(chunks))
                    .unwrap()
            }),
        ))
        .await;
        let second_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = second_calls.clone();
        let second = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({"choices": [{"message": {"content": "ok"}}]}))
                }
            }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [
            ("unavailable", &unavailable),
            ("first", &first),
            ("second", &second),
        ] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["unavailable".into(), "first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--model",
            "shared-model",
            "--timeout",
            "5",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let error = cmd.check(args).await.unwrap_err();
        assert_eq!(
            unavailable_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            error
                .to_string()
                .contains("member first after 2 attempt(s)"),
            "{error}"
        );
        assert_eq!(
            second_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an accepted response must never be replayed"
        );
    }

    #[tokio::test]
    async fn check_lb_caps_each_member_header_wait_but_keeps_total_budget() {
        async fn slow_headers() -> Json<Value> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Json(json!({"data": []}))
        }
        let first = spawn_upstream(Router::new().route("/models", get(slow_headers))).await;
        let second = spawn_upstream(Router::new().route(
            "/models",
            get(|| async { Json(json!({"data": [{"id": "ok"}]})) }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [("first", &first), ("second", &second)] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--timeout",
            "32",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            cmd.check_with_header_timeout(args, Duration::from_millis(200)),
        )
        .await;
        assert!(
            result.is_ok(),
            "each member header wait must be capped below the total budget"
        );
        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn check_lb_bounds_accepted_error_body_without_replay() {
        let first = spawn_upstream(Router::new().route(
            "/models",
            get(|| async {
                axum::response::Response::builder()
                    .status(axum::http::StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(vec![
                        b'x';
                        proxy::MAX_ERROR_BODY_BYTES + 1
                    ]))
                    .unwrap()
            }),
        ))
        .await;
        let second_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = second_calls.clone();
        let second = spawn_upstream(Router::new().route(
            "/models",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({"data": [{"id": "ok"}]}))
                }
            }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [("first", &first), ("second", &second)] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--timeout",
            "5",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let started = Instant::now();
        let error = cmd.check(args).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        assert!(error.to_string().contains("400 Bad Request"), "{error}");
        assert_eq!(
            second_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an accepted response must never be replayed"
        );
    }

    #[tokio::test]
    async fn check_concrete_provider_still_follows_redirects() {
        let upstream = spawn_upstream(
            Router::new()
                .route(
                    "/models",
                    get(|| async {
                        (
                            axum::http::StatusCode::TEMPORARY_REDIRECT,
                            [(axum::http::header::LOCATION, "/models-final")],
                        )
                    }),
                )
                .route(
                    "/models-final",
                    get(|| async { Json(json!({"data": [{"id": "redirected"}]})) }),
                ),
        )
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        config
            .add_provider(
                "first".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, upstream, None),
            )
            .unwrap();
        config.default = "first".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "first",
            "--timeout",
            "5",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        cmd.check(args).await.unwrap();
    }

    #[tokio::test]
    async fn check_does_not_follow_redirects_or_replay_members() {
        let first_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = first_calls.clone();
        let first = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, "/chat/completions")],
                        Json(json!({"error": "redirect"})),
                    )
                }
            }),
        ))
        .await;
        let second_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = second_calls.clone();
        let second = spawn_upstream(Router::new().route(
            "/{*path}",
            axum::routing::any(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({"choices": [{"message": {"content": "ok"}}]}))
                }
            }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        for (name, url) in [("first", &first), ("second", &second)] {
            config
                .add_provider(
                    name.into(),
                    ProviderConfig::upstream(Protocol::OpenAiChat, url.clone(), None),
                )
                .unwrap();
        }
        config
            .add_provider(
                "pool".into(),
                ProviderConfig::lb(vec!["first".into(), "second".into()]),
            )
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--model",
            "shared-model",
            "--timeout",
            "5",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        assert!(cmd.check(args).await.is_err());
        assert_eq!(
            first_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a member must be attempted once"
        );
        assert_eq!(
            second_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "307 is a final response, not a failover"
        );
    }

    #[tokio::test]
    async fn check_lb_reports_failure_when_every_member_is_unavailable() {
        let unauthorized = spawn_upstream(Router::new().route(
            "/models",
            get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
        ))
        .await;
        let dir = tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::default();
        config
            .add_provider(
                "only".into(),
                ProviderConfig::upstream(Protocol::OpenAiChat, unauthorized, None),
            )
            .unwrap();
        config
            .add_provider("pool".into(), ProviderConfig::lb(vec!["only".into()]))
            .unwrap();
        config.default = "pool".into();
        config.save(&path).unwrap();

        let cmd = Cmd::parse_from([
            "llmapi",
            "-c",
            path.to_str().unwrap(),
            "check",
            "pool",
            "--timeout",
            "5",
        ]);
        let Command::Check(args) = &cmd.command else {
            unreachable!()
        };
        let error = cmd.check(args).await.unwrap_err();
        assert!(error.to_string().contains("only"), "{error}");
    }

    async fn spawn_upstream(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }
}
