use std::{collections::BTreeMap, env, fs, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_SERVER: &str = "127.0.0.1:8080";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(
        default,
        rename = "apikey",
        alias = "api_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key: Option<String>,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: Provider,
    #[serde(rename = "baseurl", alias = "base_url")]
    pub base_url: String,
    #[serde(
        default,
        rename = "apikey",
        alias = "api_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum Provider {
    #[serde(rename = "openai-chat", alias = "chat")]
    #[value(name = "openai-chat", alias = "chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses", alias = "responses")]
    #[value(name = "openai-responses", alias = "responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic", alias = "messages")]
    #[value(name = "anthropic", alias = "messages")]
    Anthropic,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse yaml config: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("parse toml config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("json config: {0}")]
    Json(#[from] serde_json::Error),
    #[error("serialize toml config: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("unsupported config extension: {0}")]
    UnsupportedExtension(String),
    #[error("unknown provider type: {0}")]
    UnknownProviderType(String),
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("provider already exists: {0}")]
    ProviderAlreadyExists(String),
    #[error("default provider is not configured: {0}")]
    DefaultProviderNotFound(String),
    #[error("invalid provider name: {0}")]
    InvalidProviderName(String),
    #[error("server apikey cannot be empty")]
    EmptyServerApiKey,
    #[error("expand environment variable {name}: {source}")]
    EnvVar {
        name: String,
        #[source]
        source: env::VarError,
    },
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: default_server(),
            api_key: None,
            default: String::new(),
            providers: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn api_key(&self) -> Result<Option<String>, ConfigError> {
        let Some(configured) = &self.api_key else {
            return Ok(None);
        };
        let key = expand_env(configured.clone())?;
        if key.is_empty() {
            return Err(ConfigError::EmptyServerApiKey);
        }
        Ok(Some(key))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let body = fs::read_to_string(path)?;
        Self::parse(path, &body)
    }

    pub(crate) fn parse(path: &Path, body: &str) -> Result<Self, ConfigError> {
        let mut config = match extension(path).as_deref() {
            Some("yaml" | "yml") => serde_yaml::from_str::<Self>(body)?,
            Some("toml") => toml::from_str::<Self>(body)?,
            Some("json") => serde_json::from_str::<Self>(body)?,
            Some(ext) => return Err(ConfigError::UnsupportedExtension(ext.to_string())),
            None => return Err(ConfigError::UnsupportedExtension(String::new())),
        };
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    pub fn load_for_update(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        match Self::load(path.as_ref()) {
            Ok(config) => Ok(config),
            Err(ConfigError::Read(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(err) => Err(err),
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let path = path.as_ref();
        let body = match extension(path).as_deref() {
            Some("yaml" | "yml") => serde_yaml::to_string(self)?,
            Some("toml") => toml::to_string_pretty(self)?,
            Some("json") => serde_json::to_string_pretty(self)?,
            Some(ext) => return Err(ConfigError::UnsupportedExtension(ext.to_string())),
            None => return Err(ConfigError::UnsupportedExtension(String::new())),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, body)?;
        Ok(())
    }

    pub fn add_provider(
        &mut self,
        name: String,
        mut provider: ProviderConfig,
    ) -> Result<(), ConfigError> {
        validate_provider_name(&name)?;
        if self.providers.contains_key(&name) {
            return Err(ConfigError::ProviderAlreadyExists(name));
        }
        provider.normalize();
        self.providers.insert(name.clone(), provider);
        if self.default.is_empty() {
            self.default = name;
        }
        Ok(())
    }

    pub fn set_default(&mut self, name: &str) -> Result<(), ConfigError> {
        if !self.providers.contains_key(name) {
            return Err(ConfigError::ProviderNotFound(name.to_string()));
        }
        self.default = name.to_string();
        Ok(())
    }

    pub fn remove_provider(&mut self, name: &str) -> Result<(), ConfigError> {
        if self.providers.remove(name).is_none() {
            return Err(ConfigError::ProviderNotFound(name.to_string()));
        }
        if self.default == name {
            self.default = self.providers.keys().next().cloned().unwrap_or_default();
        }
        Ok(())
    }

    pub fn provider(&self, name: Option<&str>) -> Result<(&str, &ProviderConfig), ConfigError> {
        let name = name.unwrap_or(&self.default);
        self.providers
            .get_key_value(name)
            .map(|(name, provider)| (name.as_str(), provider))
            .ok_or_else(|| ConfigError::ProviderNotFound(name.to_string()))
    }

    fn normalize(&mut self) {
        self.server = self.server.trim().to_string();
        self.default = self.default.trim().to_string();
        for provider in self.providers.values_mut() {
            provider.normalize();
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.api_key.as_deref() == Some("") {
            return Err(ConfigError::EmptyServerApiKey);
        }
        for name in self.providers.keys() {
            validate_provider_name(name)?;
        }
        if !(self.providers.is_empty() && self.default.is_empty())
            && !self.providers.contains_key(&self.default)
        {
            return Err(ConfigError::DefaultProviderNotFound(self.default.clone()));
        }
        Ok(())
    }
}

impl ProviderConfig {
    pub fn api_key(&self) -> Result<Option<String>, ConfigError> {
        self.api_key.clone().map(expand_env).transpose()
    }

    fn normalize(&mut self) {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
    }
}

impl Provider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai-chat" | "chat" => Ok(Self::OpenAiChat),
            "openai-responses" | "responses" => Ok(Self::OpenAiResponses),
            "anthropic" | "messages" => Ok(Self::Anthropic),
            other => Err(ConfigError::UnknownProviderType(other.to_string())),
        }
    }
}

fn default_server() -> String {
    DEFAULT_SERVER.to_string()
}

fn validate_provider_name(name: &str) -> Result<(), ConfigError> {
    if !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }
    Err(ConfigError::InvalidProviderName(name.to_string()))
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

fn expand_env(value: String) -> Result<String, ConfigError> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value.as_str();
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let name = &after_start[..end];
        let replacement = env::var(name).map_err(|source| ConfigError::EnvVar {
            name: name.to_string(),
            source,
        })?;
        output.push_str(&replacement);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG_YAML: &str = r#"
default: deepseek
providers:
  deepseek:
    type: chat
    baseurl: https://xxxxxxx
    apikey: sk-xxxxxxx
  openai:
    type: responses
    baseurl: https://yyyyyyy
    apikey: sk-yyyyyyy
  anthropic:
    type: messages
    baseurl: https://zzzzzzz
    apikey: sk-zzzzzzz
"#;

    #[test]
    fn loads_multi_provider_yaml_and_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        fs::write(&path, CONFIG_YAML).unwrap();

        let config = Config::load(path).unwrap();

        assert_eq!(config.server, DEFAULT_SERVER);
        assert_eq!(config.default, "deepseek");
        assert_eq!(config.providers.len(), 3);
        assert_eq!(
            config.providers["deepseek"].provider_type,
            Provider::OpenAiChat
        );
        assert_eq!(
            config.providers["openai"].provider_type,
            Provider::OpenAiResponses
        );
        assert_eq!(
            config.providers["anthropic"].provider_type,
            Provider::Anthropic
        );
    }

    #[test]
    fn add_and_set_default_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        for extension in ["yaml", "yml", "toml", "json"] {
            let path = dir.path().join(format!("llmapi.{extension}"));
            let mut config = Config::load_for_update(&path).unwrap();
            config.api_key = Some("${LLMAPI_API_KEY}".into());
            config
                .add_provider(
                    "deepseek".into(),
                    ProviderConfig {
                        provider_type: Provider::OpenAiChat,
                        base_url: "https://api.deepseek.test/".into(),
                        api_key: Some("sk-test".into()),
                    },
                )
                .unwrap();
            config
                .add_provider(
                    "openai".into(),
                    ProviderConfig {
                        provider_type: Provider::OpenAiResponses,
                        base_url: "https://api.openai.test".into(),
                        api_key: None,
                    },
                )
                .unwrap();
            config.set_default("openai").unwrap();
            config.save(&path).unwrap();

            let loaded = Config::load(path).unwrap();
            assert_eq!(loaded.api_key.as_deref(), Some("${LLMAPI_API_KEY}"));
            assert_eq!(loaded.default, "openai");
            assert_eq!(
                loaded.providers["deepseek"].base_url,
                "https://api.deepseek.test"
            );
        }
    }

    #[test]
    fn rejects_missing_default_provider() {
        let error = serde_yaml::from_str::<Config>(CONFIG_YAML)
            .map(|mut config| {
                config.default = "missing".into();
                config.validate().unwrap_err()
            })
            .unwrap();
        assert!(matches!(error, ConfigError::DefaultProviderNotFound(_)));
    }

    #[test]
    fn rejects_empty_server_api_key() {
        let body = CONFIG_YAML.replacen("default:", "apikey: ''\ndefault:", 1);
        let error = Config::parse(Path::new("llmapi.yaml"), &body).unwrap_err();
        assert!(matches!(error, ConfigError::EmptyServerApiKey));
    }

    #[test]
    fn removing_last_provider_keeps_config_loadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmapi.yaml");
        let mut config = Config::parse(&path, CONFIG_YAML).unwrap();
        assert!(matches!(
            config.remove_provider("missing"),
            Err(ConfigError::ProviderNotFound(_))
        ));
        config.remove_provider("deepseek").unwrap();
        assert_eq!(config.default, "anthropic");
        config.remove_provider("anthropic").unwrap();
        config.remove_provider("openai").unwrap();
        assert!(config.default.is_empty());
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn provider_type_accepts_canonical_and_short_names() {
        assert_eq!(Provider::from_str("chat").unwrap(), Provider::OpenAiChat);
        assert_eq!(
            Provider::from_str("openai-responses").unwrap(),
            Provider::OpenAiResponses
        );
        assert_eq!(Provider::from_str("messages").unwrap(), Provider::Anthropic);
    }
}
