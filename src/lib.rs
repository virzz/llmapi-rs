//! @about: LLM API protocol conversion proxy

pub mod adapters;
pub mod auth;
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
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

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
    /// List configured providers
    List,
    /// Add a provider
    Add(AddArgs),
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
    /// Upstream API key
    #[arg(long)]
    apikey: Option<String>,
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
        let config = self.load_config()?;
        print!("{}", format_provider_list(&config));
        Ok(())
    }

    fn add(&self, args: &AddArgs) -> Result<()> {
        let path = self.config_path();
        let mut config = Config::load_for_update(&path)
            .with_context(|| format!("load config {}", path.display()))?;
        config.add_provider(
            args.name.clone(),
            ProviderConfig {
                provider_type: args.provider_type,
                base_url: args.baseurl.clone(),
                api_key: args.apikey.clone(),
            },
        )?;
        config
            .save(&path)
            .with_context(|| format!("save config {}", path.display()))?;
        Ok(())
    }

    fn set(&self, args: &SetArgs) -> Result<()> {
        let path = self.config_path();
        let mut config =
            Config::load(&path).with_context(|| format!("load config {}", path.display()))?;
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
        Config::load(&path).with_context(|| format!("load config {}", path.display()))
    }

    async fn serve(&self, args: &ServerArgs) -> Result<()> {
        let path = self.config_path();
        let mut config =
            Config::load(&path).with_context(|| format!("load config {}", path.display()))?;
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
            Command::List => self.list(),
            Command::Add(args) => self.add(args),
            Command::Set(args) => self.set(args),
            Command::Server(args) => self.serve(args).await,
            #[cfg(target_os = "macos")]
            Command::Daemon(args) => args.execute(),
        }
    }
}

fn format_provider_list(config: &Config) -> String {
    let mut output = String::from("NAME\tTYPE\tBASEURL\tDEFAULT\n");
    for (name, provider) in &config.providers {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            name,
            provider.provider_type,
            provider.base_url,
            if name == &config.default { "*" } else { "" }
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(matches!(
            Cmd::parse_from(["llmapi", "list"]).command,
            Command::List
        ));
        let add = Cmd::parse_from([
            "llmapi",
            "add",
            "deepseek",
            "--type",
            "chat",
            "--baseurl",
            "https://api.deepseek.test",
            "--apikey",
            "sk-test",
        ]);
        assert!(matches!(
            add.command,
            Command::Add(AddArgs {
                provider_type: Provider::OpenAiChat,
                ..
            })
        ));
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
    fn provider_list_marks_default_without_api_keys() {
        let config = Config {
            server: "127.0.0.1:8080".into(),
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

        assert!(output.contains("deepseek\topenai-chat\thttps://api.deepseek.test\t*"));
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
}
