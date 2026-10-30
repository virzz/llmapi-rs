//! @about: LLM API protocol conversion proxy

pub mod adapters;
pub mod auth;
mod codex;
pub mod config;
#[cfg(target_os = "macos")]
pub mod daemon;
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

use anyhow::{bail, Context, Result};
use axum::http::{header, HeaderMap, Method};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use comfy_table::Table;
use serde_json::{json, Value};

use config::{Config, Provider, ProviderConfig};

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
    List,
    /// Check that a provider is reachable
    Check(CheckArgs),
    /// Add a provider
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
    /// Provider protocol: openai-chat|chat, openai-responses|responses, anthropic|messages
    #[arg(long = "type")]
    provider_type: Provider,
    /// Upstream API base URL
    #[arg(long)]
    baseurl: String,
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
            .timeout(Duration::from_secs(args.timeout))
            .build()
            .context("build HTTP client")?;
        let probe = if args.model.is_some() {
            "model"
        } else {
            "models"
        };
        let outcome = probe_provider(
            &client,
            &provider,
            codex_headers.as_ref(),
            args.model.as_deref(),
        )
        .await
        .with_context(|| format!("check provider {name}"))?;
        print!("{}", format_check_report(&name, &provider, probe, &outcome));
        if !outcome.status.is_success() {
            bail!("provider {name} check failed: {}", outcome.status);
        }
        Ok(())
    }

    fn add(&self, args: &AddArgs) -> Result<()> {
        let path = self.config_path();
        let mut config = Config::load_for_update(&path)
            .with_context(|| format!("load config {}", path.display()))?;
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
        config.add_provider(
            args.name.clone(),
            ProviderConfig {
                provider_type: args.provider_type,
                base_url: args.baseurl.clone(),
                api_key,
            },
        )?;
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
    table.set_header(["NAME", "TYPE", "BASEURL", "DEFAULT"]);
    for (name, provider) in &config.providers {
        table.add_row([
            name.as_str(),
            provider.provider_type.as_str(),
            provider.base_url.as_str(),
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

async fn probe_provider(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    codex_headers: Option<&HeaderMap>,
    model: Option<&str>,
) -> Result<CheckOutcome> {
    let api_key = provider.api_key()?;
    let mut headers = HeaderMap::new();
    auth::apply_api_key(
        &mut headers,
        provider.provider_type,
        api_key.as_deref(),
        None,
    );
    auth::apply_protocol_headers(&mut headers, provider.provider_type, &HeaderMap::new());

    let codex = provider.provider_type == Provider::Codex;
    let (method, path, body) = match (provider.provider_type, model) {
        (_, None) => (Method::GET, "/models", None),
        (Provider::OpenAiChat, Some(model)) => (
            Method::POST,
            "/chat/completions",
            Some(json!({
                "model": model,
                "messages": [{"role": "user", "content": "ping"}],
                "stream": false
            })),
        ),
        (Provider::OpenAiResponses, Some(model)) => (
            Method::POST,
            "/responses",
            Some(json!({
                "model": model,
                "input": "ping",
                "stream": false
            })),
        ),
        (Provider::Anthropic, Some(model)) => (
            Method::POST,
            "/messages",
            Some(json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "ping"}]
            })),
        ),
        (Provider::Codex, Some(model)) => {
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
    let mut url = proxy::upstream_url(&provider.base_url, path, None);
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
    let started = Instant::now();
    let mut response = request
        .send()
        .await
        .with_context(|| format!("request upstream {url}"))?;
    let latency = started.elapsed();
    let status = response.status();
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default();
        return Ok(CheckOutcome {
            status,
            latency,
            detail: compact_text(&body),
        });
    }

    let detail = match (model, codex) {
        (None, _) => {
            let body = response.bytes().await.context("read upstream response")?;
            serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| model_count(&value))
                .map(|count| format!("{count} models"))
                .unwrap_or_else(|| "reachable".into())
        }
        (Some(_), true) => match response.chunk().await.context("read upstream stream")? {
            Some(chunk) => format!("stream received ({} bytes)", chunk.len()),
            None => "empty stream".into(),
        },
        (Some(_), false) => {
            let body = response.bytes().await.context("read upstream response")?;
            response_text(provider.provider_type, &body)
                .map(|text| format!("reply: {text}"))
                .unwrap_or_else(|| "reachable".into())
        }
    };
    Ok(CheckOutcome {
        status,
        latency,
        detail,
    })
}

fn model_count(value: &Value) -> Option<usize> {
    value
        .get("data")
        .or_else(|| value.get("models"))?
        .as_array()
        .map(Vec::len)
}

fn response_text(provider: Provider, body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let text =
        match provider {
            Provider::OpenAiChat => json_text(&value["choices"][0]["message"]["content"]),
            Provider::OpenAiResponses => value
                .get("output_text")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    value.get("output")?.as_array()?.iter().find_map(|item| {
                        item.get("content")?.as_array()?.iter().find_map(json_text)
                    })
                }),
            Provider::Anthropic => value.get("content")?.as_array()?.iter().find_map(json_text),
            Provider::Codex => None,
        }?;
    Some(truncate(&collapse(&text), 80))
}

fn json_text(value: &Value) -> Option<String> {
    value.as_str().map(ToString::to_string).or_else(|| {
        value
            .as_array()?
            .iter()
            .find_map(|part| part.get("text")?.as_str().map(ToString::to_string))
    })
}

fn compact_text(body: &[u8]) -> String {
    let text = collapse(&String::from_utf8_lossy(body));
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
    provider: &ProviderConfig,
    probe: &str,
    outcome: &CheckOutcome,
) -> String {
    let status = outcome.status.to_string();
    let latency = format!("{} ms", outcome.latency.as_millis());
    let mut table = Table::new();
    table.set_header(["PROVIDER", "TYPE", "PROBE", "STATUS", "LATENCY", "DETAIL"]);
    table.add_row([
        name,
        provider.provider_type.as_str(),
        probe,
        &status,
        &latency,
        &outcome.detail,
    ]);
    format!("{table}\n")
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
        assert!(matches!(
            Cmd::parse_from(["llmapi", "list"]).command,
            Command::List
        ));
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
                ProviderConfig {
                    provider_type: Provider::OpenAiChat,
                    base_url: "https://first.test".into(),
                    api_key: None,
                },
            )
            .unwrap();
        config
            .add_provider(
                "second".into(),
                ProviderConfig {
                    provider_type: Provider::OpenAiChat,
                    base_url: "https://second.test".into(),
                    api_key: None,
                },
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
                ProviderConfig {
                    provider_type: Provider::OpenAiChat,
                    base_url: "https://api.deepseek.test".into(),
                    api_key: Some("sk-secret".into()),
                },
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
                    ProviderConfig {
                        provider_type: Provider::OpenAiChat,
                        base_url: "https://deepseek.test".into(),
                        api_key: None,
                    },
                ),
                (
                    "openai".into(),
                    ProviderConfig {
                        provider_type: Provider::OpenAiResponses,
                        base_url: "https://openai.test".into(),
                        api_key: None,
                    },
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
        let provider = ProviderConfig {
            provider_type: Provider::OpenAiChat,
            base_url: format!("{upstream}/v1"),
            api_key: Some("sk-test".into()),
        };

        let outcome = probe_provider(&reqwest::Client::new(), &provider, None, None)
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
        let provider = ProviderConfig {
            provider_type: Provider::OpenAiChat,
            base_url: format!("{upstream}/v1"),
            api_key: Some("sk-test".into()),
        };

        let outcome = probe_provider(
            &reqwest::Client::new(),
            &provider,
            None,
            Some("deepseek-chat"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.status.as_u16(), 200);
        assert_eq!(outcome.detail, "reply: pong");
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
        let provider = ProviderConfig {
            provider_type: Provider::OpenAiChat,
            base_url: format!("{upstream}/v1"),
            api_key: Some("sk-test".into()),
        };

        let outcome = probe_provider(&reqwest::Client::new(), &provider, None, None)
            .await
            .unwrap();

        assert_eq!(outcome.status.as_u16(), 401);
        assert!(outcome.detail.contains("bad key"));
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
