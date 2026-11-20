use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_SERVER: &str = "127.0.0.1:8080";
pub const LB_TYPE: &str = "lb";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub access_log: bool,
    #[serde(default, skip_serializing_if = "Injects::is_default")]
    pub injects: Injects,
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

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Injects {
    /// Inject a provider when local Codex ChatGPT credentials are available.
    pub codex: bool,
}

impl Injects {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// A configured provider: either a real upstream or a load-balancer group.
///
/// The load-balancer variant intentionally has no address or credentials, so an
/// LB can never be sent to as if it were an upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConfig {
    Upstream(UpstreamConfig),
    Lb(LbConfig),
}

/// A real upstream that requests can be sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamConfig {
    /// Protocol spoken to the upstream. Never [`Protocol::Lb`]-like.
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    /// Allowed model IDs. An empty list allows any model.
    pub models: Vec<String>,
}

/// A round-robin load-balancer group over existing providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbConfig {
    pub members: Vec<String>,
}

/// Concrete upstream protocols. There is deliberately no load-balancer variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Llmapi,
    Codex,
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
    /// Relay to another llmapi server using the client's protocol.
    #[serde(rename = "llmapi")]
    #[value(name = "llmapi")]
    Llmapi,
    /// Discovered from the local Codex login; never stored as provider config.
    #[serde(rename = "codex", skip_deserializing)]
    #[value(skip)]
    Codex,
    /// Round-robin group over other providers.
    #[serde(rename = "lb")]
    #[value(name = "lb")]
    Lb,
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
    #[error(
        "Codex provider is unavailable: enable injects.codex or configure a provider named codex"
    )]
    CodexInjectionDisabled,
    #[error("provider already exists: {0}")]
    ProviderAlreadyExists(String),
    #[error("default provider is not configured: {0}")]
    DefaultProviderNotFound(String),
    #[error("invalid provider name: {0}")]
    InvalidProviderName(String),
    #[error("server apikey cannot be empty")]
    EmptyServerApiKey,
    #[error("lb provider `{0}` requires at least one member")]
    EmptyLbMembers(String),
    #[error("lb provider `{lb}` lists member `{member}` more than once")]
    DuplicateLbMember { lb: String, member: String },
    #[error("lb provider `{lb}` references unknown member `{member}`")]
    LbMemberNotFound { lb: String, member: String },
    #[error("lb provider `{0}` cannot reference itself")]
    LbSelfReference(String),
    #[error("lb provider `{lb}` cannot reference another lb `{member}`")]
    NestedLb { lb: String, member: String },
    #[error("provider `{provider}` is used by lb `{lb}`; update or remove that lb first")]
    ProviderInUse { provider: String, lb: String },
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
            access_log: false,
            injects: Injects::default(),
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
            if name != "codex" {
                return Err(ConfigError::ProviderNotFound(name.to_string()));
            }
            if !self.injects.codex {
                return Err(ConfigError::CodexInjectionDisabled);
            }
        }
        self.default = name.to_string();
        Ok(())
    }

    pub fn remove_provider(&mut self, name: &str) -> Result<(), ConfigError> {
        if !self.providers.contains_key(name) {
            return Err(ConfigError::ProviderNotFound(name.to_string()));
        }
        if let Some(lb) = self
            .providers
            .iter()
            .find_map(|(lb_name, provider)| match provider {
                ProviderConfig::Lb(lb) if lb.members.iter().any(|member| member == name) => {
                    Some(lb_name.clone())
                }
                _ => None,
            })
        {
            return Err(ConfigError::ProviderInUse {
                provider: name.to_string(),
                lb,
            });
        }
        self.providers.remove(name);
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

    pub(crate) fn with_codex(mut self, available: bool) -> Self {
        if self.injects.codex && available {
            self.providers.entry("codex".into()).or_insert_with(|| {
                ProviderConfig::upstream(Protocol::Codex, crate::codex::BASE_URL, None)
            });
            if self.default.is_empty() {
                self.default = "codex".into();
            }
        }
        self
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
            && self.default != "codex"
            && !self.providers.contains_key(&self.default)
        {
            return Err(ConfigError::DefaultProviderNotFound(self.default.clone()));
        }
        for (name, provider) in &self.providers {
            provider.validate(name, &self.providers)?;
        }
        Ok(())
    }
}

impl ProviderConfig {
    pub fn upstream(
        protocol: Protocol,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self::Upstream(UpstreamConfig {
            protocol,
            base_url: base_url.into(),
            api_key,
            models: Vec::new(),
        })
    }

    pub fn lb(members: Vec<String>) -> Self {
        Self::Lb(LbConfig { members })
    }

    pub fn protocol(&self) -> Option<Protocol> {
        match self {
            Self::Upstream(upstream) => Some(upstream.protocol),
            Self::Lb(_) => None,
        }
    }

    pub fn as_upstream(&self) -> Option<&UpstreamConfig> {
        match self {
            Self::Upstream(upstream) => Some(upstream),
            Self::Lb(_) => None,
        }
    }

    pub fn as_upstream_mut(&mut self) -> Option<&mut UpstreamConfig> {
        match self {
            Self::Upstream(upstream) => Some(upstream),
            Self::Lb(_) => None,
        }
    }

    pub fn as_lb(&self) -> Option<&LbConfig> {
        match self {
            Self::Lb(lb) => Some(lb),
            Self::Upstream(_) => None,
        }
    }

    pub fn api_key(&self) -> Result<Option<String>, ConfigError> {
        match self {
            Self::Upstream(upstream) => upstream.api_key(),
            Self::Lb(_) => Ok(None),
        }
    }

    fn normalize(&mut self) {
        match self {
            Self::Upstream(upstream) => upstream.normalize(),
            Self::Lb(lb) => {
                for member in &mut lb.members {
                    *member = member.trim().to_string();
                }
            }
        }
    }

    fn validate(
        &self,
        name: &str,
        providers: &BTreeMap<String, ProviderConfig>,
    ) -> Result<(), ConfigError> {
        match self {
            Self::Upstream(_) => {}
            Self::Lb(lb) => {
                if lb.members.is_empty() {
                    return Err(ConfigError::EmptyLbMembers(name.to_string()));
                }
                let mut seen = BTreeSet::new();
                for member in &lb.members {
                    validate_provider_name(member)?;
                    if member == name {
                        return Err(ConfigError::LbSelfReference(name.to_string()));
                    }
                    if !seen.insert(member.clone()) {
                        return Err(ConfigError::DuplicateLbMember {
                            lb: name.to_string(),
                            member: member.clone(),
                        });
                    }
                    match providers.get(member) {
                        Some(ProviderConfig::Lb(_)) => {
                            return Err(ConfigError::NestedLb {
                                lb: name.to_string(),
                                member: member.clone(),
                            });
                        }
                        Some(ProviderConfig::Upstream(_)) => {}
                        None if member == "codex" => {}
                        None => {
                            return Err(ConfigError::LbMemberNotFound {
                                lb: name.to_string(),
                                member: member.clone(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl UpstreamConfig {
    pub(crate) fn allows_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|allowed| allowed == model)
    }

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
            Self::Llmapi => "llmapi",
            Self::Codex => "codex",
            Self::Lb => LB_TYPE,
        }
    }

    /// Protocol used to talk to a real upstream; `None` for the lb type.
    pub const fn protocol(self) -> Option<Protocol> {
        match self {
            Self::OpenAiChat => Some(Protocol::OpenAiChat),
            Self::OpenAiResponses => Some(Protocol::OpenAiResponses),
            Self::Anthropic => Some(Protocol::Anthropic),
            Self::Llmapi => Some(Protocol::Llmapi),
            Self::Codex => Some(Protocol::Codex),
            Self::Lb => None,
        }
    }
}

impl Protocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
            Self::Llmapi => "llmapi",
            Self::Codex => "codex",
        }
    }

    const fn from_provider(provider: Provider) -> Option<Self> {
        provider.protocol()
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::fmt::Display for Protocol {
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
            "llmapi" => Ok(Self::Llmapi),
            LB_TYPE => Ok(Self::Lb),
            other => Err(ConfigError::UnknownProviderType(other.to_string())),
        }
    }
}

/// Keep "field is absent" distinct from "field is present with a null value" so
/// type-specific field rules can reject explicitly supplied fields, including
/// `null` in YAML/JSON.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Wire form used while deserializing, then split into upstream/lb.
#[derive(Deserialize)]
struct ProviderConfigRaw {
    #[serde(rename = "type", alias = "upstream")]
    provider_type: Provider,
    #[serde(
        rename = "baseurl",
        alias = "base_url",
        default,
        deserialize_with = "deserialize_present"
    )]
    base_url: Option<Option<String>>,
    #[serde(
        rename = "apikey",
        alias = "api_key",
        default,
        deserialize_with = "deserialize_present"
    )]
    api_key: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    models: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    members: Option<Option<Vec<String>>>,
}

impl ProviderConfigRaw {
    fn into_config(self) -> Result<ProviderConfig, String> {
        match self.provider_type {
            Provider::Lb => {
                if self.base_url.is_some() {
                    return Err("lb provider does not accept `baseurl`".into());
                }
                if self.api_key.is_some() {
                    return Err("lb provider does not accept `apikey`".into());
                }
                if self.models.is_some() {
                    return Err("lb provider does not accept `models`".into());
                }
                let members = match self.members {
                    Some(Some(members)) => members,
                    _ => return Err("lb provider requires `members`".into()),
                };
                Ok(ProviderConfig::lb(members))
            }
            provider_type => {
                if self.members.is_some() {
                    return Err(format!(
                        "provider type `{provider_type}` does not accept `members`"
                    ));
                }
                let base_url = match self.base_url {
                    Some(Some(base_url)) => base_url,
                    _ => {
                        return Err(format!(
                            "provider type `{provider_type}` requires `baseurl`"
                        ))
                    }
                };
                let protocol = Protocol::from_provider(provider_type)
                    .ok_or_else(|| format!("unknown provider type: {provider_type}"))?;
                Ok(ProviderConfig::Upstream(UpstreamConfig {
                    protocol,
                    base_url,
                    api_key: self.api_key.flatten(),
                    models: self.models.flatten().unwrap_or_default(),
                }))
            }
        }
    }
}

impl<'de> Deserialize<'de> for ProviderConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ProviderConfigRaw::deserialize(deserializer)?
            .into_config()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize)]
struct UpstreamWire<'a> {
    #[serde(rename = "type")]
    provider_type: &'a str,
    baseurl: &'a str,
    #[serde(rename = "apikey", skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    models: &'a [String],
}

#[derive(Serialize)]
struct LbWire<'a> {
    #[serde(rename = "type")]
    provider_type: &'a str,
    members: &'a [String],
}

impl Serialize for ProviderConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Upstream(upstream) => UpstreamWire {
                provider_type: upstream.protocol.as_str(),
                baseurl: &upstream.base_url,
                api_key: upstream.api_key.as_deref(),
                models: &upstream.models,
            }
            .serialize(serializer),
            Self::Lb(lb) => LbWire {
                provider_type: LB_TYPE,
                members: &lb.members,
            }
            .serialize(serializer),
        }
    }
}

fn default_server() -> String {
    DEFAULT_SERVER.to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
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

    const LB_YAML: &str = r#"
default: pool
providers:
  primary:
    type: chat
    baseurl: https://primary.test/v1
    apikey: sk-primary
  secondary:
    type: responses
    baseurl: https://secondary.test/v1
    apikey: sk-secondary
  pool:
    type: lb
    members: [primary, secondary]
"#;

    #[test]
    fn parses_llmapi_relay_from_upstream_and_serializes_as_type() {
        let provider: ProviderConfig =
            toml::from_str("upstream = \"llmapi\"\nbaseurl = \"https://relay.test/pool/v1\"")
                .unwrap();
        assert_eq!(provider.protocol(), Some(Protocol::Llmapi));
        let serialized = serde_json::to_value(&provider).unwrap();
        assert_eq!(serialized["type"], "llmapi");
        assert_eq!(
            serde_json::from_value::<ProviderConfig>(serialized).unwrap(),
            provider
        );
    }

    #[test]
    fn upstream_models_round_trip_with_an_optional_allowlist() {
        for (configured, expected) in [
            ("", vec![]),
            ("\nmodels: []", vec![]),
            ("\nmodels: [model-a, model-b]", vec!["model-a", "model-b"]),
        ] {
            let provider: ProviderConfig = serde_yaml::from_str(&format!(
                "type: chat\nbaseurl: https://example.test{configured}"
            ))
            .unwrap();
            assert_eq!(provider.as_upstream().unwrap().models, expected);
            let serialized = serde_json::to_value(&provider).unwrap();
            assert_eq!(serialized.get("models").is_some(), !expected.is_empty());
            assert_eq!(
                serde_json::from_value::<ProviderConfig>(serialized).unwrap(),
                provider
            );
        }
    }

    #[test]
    fn codex_injection_requires_explicit_opt_in() {
        for body in ["", "injects = {}", "injects = { codex = false }"] {
            let disabled = Config::parse(Path::new("config.toml"), body).unwrap();
            assert!(!disabled.injects.codex);
            assert_eq!(disabled.clone().with_codex(true), disabled);
        }

        let enabled = Config::parse(Path::new("config.yaml"), "injects: {codex: true}").unwrap();
        assert!(enabled.injects.codex);
        assert_eq!(enabled.clone().with_codex(true).default, "codex");
        let serialized = toml::to_string(&enabled).unwrap();
        let loaded = Config::parse(Path::new("config.toml"), &serialized).unwrap();
        assert_eq!(loaded, enabled);

        let mut manual = Config::parse(Path::new("config.yaml"), CONFIG_YAML).unwrap();
        let provider = manual.providers["openai"].clone();
        manual.providers.insert("codex".into(), provider.clone());
        manual.set_default("codex").unwrap();
        assert_eq!(manual.with_codex(true).provider(None).unwrap().1, &provider);
    }

    #[test]
    fn automatic_codex_preserves_configured_providers_and_default() {
        let mut config = Config::parse(Path::new("config.yaml"), CONFIG_YAML).unwrap();
        config.injects.codex = true;
        let effective = config.clone().with_codex(true);
        assert_eq!(effective.default, "deepseek");
        assert_eq!(
            effective.providers["codex"].protocol(),
            Some(Protocol::Codex)
        );
        assert!(!config.providers.contains_key("codex"));
        assert_eq!(config.clone().with_codex(false), config);
        let mut collision = config;
        let manual = collision.providers["openai"].clone();
        collision.providers.insert("codex".into(), manual.clone());
        assert_eq!(collision.with_codex(true).providers["codex"], manual);
    }

    #[test]
    fn selecting_codex_persists_only_the_default_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut config = Config::default();
        config.injects.codex = true;
        config.set_default("codex").unwrap();
        config.save(&path).unwrap();
        let saved = Config::load(path).unwrap();
        assert_eq!(saved.default, "codex");
        assert!(saved.providers.is_empty());
        assert!(saved.with_codex(true).provider(None).is_ok());
    }

    #[test]
    fn access_log_defaults_off_and_reads_true() {
        assert!(!Config::default().access_log);
        assert!(
            !Config::parse(Path::new("llmapi.yaml"), CONFIG_YAML)
                .unwrap()
                .access_log
        );

        let yaml = format!("access_log: true\n{CONFIG_YAML}");
        assert!(
            Config::parse(Path::new("llmapi.yaml"), &yaml)
                .unwrap()
                .access_log
        );

        let toml = "access_log = true\n";
        assert!(
            Config::parse(Path::new("llmapi.toml"), toml)
                .unwrap()
                .access_log
        );
    }

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
            config.providers["deepseek"].protocol(),
            Some(Protocol::OpenAiChat)
        );
        assert_eq!(
            config.providers["openai"].protocol(),
            Some(Protocol::OpenAiResponses)
        );
        assert_eq!(
            config.providers["anthropic"].protocol(),
            Some(Protocol::Anthropic)
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
                    ProviderConfig::upstream(
                        Protocol::OpenAiChat,
                        "https://api.deepseek.test/",
                        Some("sk-test".into()),
                    ),
                )
                .unwrap();
            config
                .add_provider(
                    "openai".into(),
                    ProviderConfig::upstream(
                        Protocol::OpenAiResponses,
                        "https://api.openai.test",
                        None,
                    ),
                )
                .unwrap();
            config.set_default("openai").unwrap();
            config.save(&path).unwrap();

            let loaded = Config::load(path).unwrap();
            assert_eq!(loaded.api_key.as_deref(), Some("${LLMAPI_API_KEY}"));
            assert_eq!(loaded.default, "openai");
            assert_eq!(
                loaded.providers["deepseek"].as_upstream().unwrap().base_url,
                "https://api.deepseek.test"
            );
        }
    }

    #[test]
    fn lb_config_round_trips_in_every_format_and_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut base = Config::parse(Path::new("llmapi.yaml"), LB_YAML).unwrap();
        base.add_provider(
            "third".into(),
            ProviderConfig::upstream(Protocol::Anthropic, "https://third.test", None),
        )
        .unwrap();
        let members = vec![
            "secondary".to_string(),
            "third".to_string(),
            "primary".to_string(),
        ];
        base.providers
            .insert("pool".into(), ProviderConfig::lb(members.clone()));
        for extension in ["yaml", "yml", "toml", "json"] {
            let path = dir.path().join(format!("llmapi.{extension}"));
            let config = base.clone();
            config.save(&path).unwrap();

            let loaded = Config::load(&path).unwrap();
            assert_eq!(
                loaded.providers["pool"].as_lb().unwrap().members,
                members,
                "{extension}"
            );
            assert_eq!(
                loaded.providers["primary"].as_upstream().unwrap().protocol,
                Protocol::OpenAiChat
            );
            assert_eq!(loaded, config, "{extension}");
        }
    }

    #[test]
    fn lb_serialization_does_not_fake_address_or_key() {
        let config = Config::parse(Path::new("llmapi.yaml"), LB_YAML).unwrap();

        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&serde_yaml::to_string(&config).unwrap()).unwrap();
        let pool = &yaml["providers"]["pool"];
        assert_eq!(pool["type"], "lb");
        assert_eq!(pool["members"].as_sequence().unwrap().len(), 2);
        assert!(pool.get("baseurl").is_none());
        assert!(pool.get("apikey").is_none());

        let toml: toml::Value = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        let pool = &toml["providers"]["pool"];
        assert_eq!(pool["type"].as_str(), Some("lb"));
        assert!(pool.get("baseurl").is_none());
        assert!(pool.get("apikey").is_none());
    }

    #[test]
    fn rejects_invalid_lb_definitions() {
        let cases = [
            ("members: []", ConfigErrorKind::Empty),
            ("members: [primary, primary]", ConfigErrorKind::Duplicate),
            ("members: [missing]", ConfigErrorKind::Unknown),
            ("members: [pool]", ConfigErrorKind::SelfReference),
            ("members: [other]", ConfigErrorKind::Nested),
        ];
        for (members, kind) in cases {
            let mut body = LB_YAML.replace("members: [primary, secondary]", members);
            if kind == ConfigErrorKind::Nested {
                body = format!("{body}  other:\n    type: lb\n    members: [primary]\n");
            }
            let result = Config::parse(Path::new("llmapi.yaml"), &body);
            let error = result.unwrap_err();
            match kind {
                ConfigErrorKind::Empty => {
                    assert!(matches!(error, ConfigError::EmptyLbMembers(_)), "{members}")
                }
                ConfigErrorKind::Duplicate => {
                    assert!(matches!(error, ConfigError::DuplicateLbMember { .. }))
                }
                ConfigErrorKind::Unknown => {
                    assert!(matches!(error, ConfigError::LbMemberNotFound { .. }))
                }
                ConfigErrorKind::SelfReference => {
                    assert!(matches!(error, ConfigError::LbSelfReference(_)))
                }
                ConfigErrorKind::Nested => {
                    assert!(matches!(error, ConfigError::NestedLb { .. }))
                }
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ConfigErrorKind {
        Empty,
        Duplicate,
        Unknown,
        SelfReference,
        Nested,
    }

    #[test]
    fn rejects_mixed_lb_and_upstream_fields() {
        for body in [
            "default: pool\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n  pool:\n    type: lb\n    baseurl: https://b.test\n    members: [primary]\n",
            "default: pool\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n  pool:\n    type: lb\n    apikey: sk-secret\n    members: [primary]\n",
            "default: primary\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n    members: [other]\n",
        ] {
            let error = Config::parse(Path::new("llmapi.yaml"), body).unwrap_err();
            assert!(matches!(error, ConfigError::Yaml(_)), "{error}");
        }
    }

    #[test]
    fn rejects_forbidden_fields_even_when_explicitly_null() {
        let yaml_lb_baseurl = "default: pool\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n  pool:\n    type: lb\n    members: [primary]\n    base_url: null\n";
        let yaml_lb_apikey = "default: pool\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n  pool:\n    type: lb\n    members: [primary]\n    apikey: null\n";
        let json_lb_baseurl = r#"{"default":"pool","providers":{"primary":{"type":"chat","baseurl":"https://a.test"},"pool":{"type":"lb","members":["primary"],"baseurl":null}}}"#;
        let json_lb_apikey = r#"{"default":"pool","providers":{"primary":{"type":"chat","baseurl":"https://a.test"},"pool":{"type":"lb","members":["primary"],"api_key":null}}}"#;
        let yaml_upstream_members = "default: primary\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n    members: null\n";
        let json_upstream_members = r#"{"default":"primary","providers":{"primary":{"type":"chat","baseurl":"https://a.test","members":null}}}"#;

        for (name, body, forbidden) in [
            ("llmapi.yaml", yaml_lb_baseurl, "baseurl"),
            ("llmapi.yaml", yaml_lb_apikey, "apikey"),
            ("llmapi.json", json_lb_baseurl, "baseurl"),
            ("llmapi.json", json_lb_apikey, "apikey"),
            ("llmapi.yaml", yaml_upstream_members, "members"),
            ("llmapi.json", json_upstream_members, "members"),
        ] {
            let error = Config::parse(Path::new(name), body).unwrap_err();
            assert!(
                error.to_string().contains(forbidden),
                "{name}: {body} => {error}"
            );
        }
    }

    #[test]
    fn explicit_null_upstream_key_is_still_accepted() {
        let body = "default: primary\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n    apikey: null\n";
        let config = Config::parse(Path::new("llmapi.yaml"), body).unwrap();
        assert_eq!(
            config.providers["primary"].as_upstream().unwrap().api_key,
            None
        );
    }

    #[test]
    fn single_member_lb_and_automatic_codex_member_are_valid() {
        let body = "default: pool\nproviders:\n  primary:\n    type: chat\n    baseurl: https://a.test\n  pool:\n    type: lb\n    members: [primary]\n  solo:\n    type: lb\n    members: [codex]\n";
        let mut config = Config::parse(Path::new("llmapi.yaml"), body).unwrap();
        config.injects.codex = true;
        assert_eq!(
            config.providers["pool"].as_lb().unwrap().members,
            ["primary"]
        );
        assert_eq!(config.providers["solo"].as_lb().unwrap().members, ["codex"]);
        assert!(config.with_codex(true).provider(Some("codex")).is_ok());
    }

    #[test]
    fn configured_codex_provider_takes_priority_over_automatic_one() {
        let body = "default: pool\nproviders:\n  codex:\n    type: chat\n    baseurl: https://codex.test\n  pool:\n    type: lb\n    members: [codex]\n";
        let mut config = Config::parse(Path::new("llmapi.yaml"), body).unwrap();
        config.injects.codex = true;
        let effective = config.with_codex(true);
        assert_eq!(
            effective.providers["codex"].protocol(),
            Some(Protocol::OpenAiChat)
        );
        assert_eq!(effective.providers.len(), 2);
    }

    #[test]
    fn removing_referenced_provider_fails_without_changes() {
        let mut config = Config::parse(Path::new("llmapi.yaml"), LB_YAML).unwrap();
        let before = config.clone();
        let error = config.remove_provider("primary").unwrap_err();
        assert!(matches!(
            &error,
            ConfigError::ProviderInUse { lb, .. } if lb == "pool"
        ));
        assert!(error.to_string().contains("pool"));
        assert_eq!(config, before);

        config.remove_provider("pool").unwrap();
        assert!(config.providers.contains_key("primary"));
        config.remove_provider("primary").unwrap();
        assert!(!config.providers.contains_key("primary"));
    }

    #[test]
    fn default_can_target_lb_and_removal_reselects() {
        let mut config = Config::parse(Path::new("llmapi.yaml"), LB_YAML).unwrap();
        assert_eq!(config.default, "pool");
        config.remove_provider("pool").unwrap();
        assert_eq!(config.default, "primary");
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
        assert_eq!(Provider::from_str("lb").unwrap(), Provider::Lb);
        assert_eq!(Provider::Lb.as_str(), "lb");
        assert_eq!(Provider::Lb.protocol(), None);
    }
}
