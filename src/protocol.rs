use super::config::Protocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    Models,
    GeminiInteractions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFormat {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    Gemini,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub provider: Option<String>,
    pub input: InputFormat,
}

pub fn detect_route(path: &str) -> Option<Route> {
    if let Some(input) = detect_endpoint(path) {
        return Some(Route {
            provider: None,
            input,
        });
    }

    let named = path
        .strip_prefix('/')?
        .split_once('/')
        .filter(|(provider, _)| !provider.is_empty());
    if let Some((provider, endpoint)) = named
        && let Some(input) = detect_endpoint(&format!("/{endpoint}"))
    {
        return Some(Route {
            provider: Some(provider.to_string()),
            input,
        });
    }
    if interactions_endpoint(path).is_some() {
        return Some(Route {
            provider: None,
            input: InputFormat::GeminiInteractions,
        });
    }
    let (provider, _) = named?;
    interactions_endpoint(&path[provider.len() + 1..]).map(|_| Route {
        provider: Some(provider.to_string()),
        input: InputFormat::GeminiInteractions,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionsEndpoint {
    Create,
    Item,
    Cancel,
}

pub(crate) fn interactions_endpoint(path: &str) -> Option<(&str, InteractionsEndpoint)> {
    let path = path
        .strip_prefix("/v1beta")
        .or_else(|| path.strip_prefix("/v1"))
        .filter(|path| path.starts_with('/'))
        .unwrap_or(path);
    if path == "/interactions" {
        return Some((path, InteractionsEndpoint::Create));
    }
    let resource = path.strip_prefix("/interactions/")?;
    let (id, endpoint) = match resource.split_once('/') {
        None => (resource, InteractionsEndpoint::Item),
        Some((id, "cancel")) => (id, InteractionsEndpoint::Cancel),
        _ => return None,
    };
    if id.is_empty()
        || matches!(id, "." | "..")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
    {
        return None;
    }
    Some((path, endpoint))
}

fn detect_endpoint(path: &str) -> Option<InputFormat> {
    match path {
        "/chat/completions" | "/v1/chat/completions" => Some(InputFormat::OpenAiChat),
        "/responses" | "/v1/responses" => Some(InputFormat::OpenAiResponses),
        "/messages" | "/v1/messages" => Some(InputFormat::AnthropicMessages),
        "/models" | "/v1/models" => Some(InputFormat::Models),
        _ => None,
    }
}

/// Canonical upstream endpoint, or the resource prefix for Gemini's model-specific endpoint.
/// Chat is the default for a relay without a client format.
pub fn upstream_for(protocol: Protocol) -> (UpstreamFormat, &'static str) {
    match protocol {
        Protocol::OpenAiChat | Protocol::Llmapi => {
            (UpstreamFormat::OpenAiChat, "/chat/completions")
        }
        Protocol::OpenAiResponses | Protocol::Codex => {
            (UpstreamFormat::OpenAiResponses, "/responses")
        }
        Protocol::Anthropic => (UpstreamFormat::AnthropicMessages, "/messages"),
        Protocol::Gemini => (UpstreamFormat::Gemini, "/models"),
    }
}

pub(crate) fn upstream_for_input(
    protocol: Protocol,
    input: InputFormat,
) -> (UpstreamFormat, &'static str) {
    let protocol = match (protocol, input) {
        (Protocol::Llmapi, InputFormat::OpenAiResponses) => Protocol::OpenAiResponses,
        (Protocol::Llmapi, InputFormat::AnthropicMessages) => Protocol::Anthropic,
        _ => protocol,
    };
    upstream_for(protocol)
}

pub fn is_transparent(input: InputFormat, upstream: UpstreamFormat) -> bool {
    matches!(
        (input, upstream),
        (InputFormat::OpenAiChat, UpstreamFormat::OpenAiChat)
            | (
                InputFormat::OpenAiResponses,
                UpstreamFormat::OpenAiResponses
            )
            | (
                InputFormat::AnthropicMessages,
                UpstreamFormat::AnthropicMessages
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_default_provider_routes() {
        assert_eq!(
            detect_route("/chat/completions"),
            Some(Route {
                provider: None,
                input: InputFormat::OpenAiChat,
            })
        );
        assert_eq!(
            detect_route("/responses").unwrap().input,
            InputFormat::OpenAiResponses
        );
        assert_eq!(
            detect_route("/messages").unwrap().input,
            InputFormat::AnthropicMessages
        );
    }

    #[test]
    fn detects_named_provider_routes() {
        assert_eq!(
            detect_route("/anthropic/chat/completions"),
            Some(Route {
                provider: Some("anthropic".into()),
                input: InputFormat::OpenAiChat,
            })
        );
        assert_eq!(
            detect_route("/openai/responses")
                .unwrap()
                .provider
                .as_deref(),
            Some("openai")
        );
        assert_eq!(
            detect_route("/deepseek/messages").unwrap().input,
            InputFormat::AnthropicMessages
        );
    }

    #[test]
    fn rejects_unknown_routes() {
        assert_eq!(detect_route("/too/many/responses"), None);
    }

    #[test]
    fn detects_model_routes() {
        for path in ["/models", "/v1/models"] {
            assert_eq!(
                detect_route(path),
                Some(Route {
                    provider: None,
                    input: InputFormat::Models,
                })
            );
        }
        for path in ["/openai/models", "/openai/v1/models"] {
            assert_eq!(
                detect_route(path),
                Some(Route {
                    provider: Some("openai".into()),
                    input: InputFormat::Models,
                })
            );
        }
    }

    #[test]
    fn maps_protocol_to_canonical_upstream_endpoint() {
        assert_eq!(
            upstream_for(Protocol::OpenAiChat),
            (UpstreamFormat::OpenAiChat, "/chat/completions")
        );
        assert_eq!(
            upstream_for(Protocol::OpenAiResponses),
            (UpstreamFormat::OpenAiResponses, "/responses")
        );
        assert_eq!(
            upstream_for(Protocol::Anthropic),
            (UpstreamFormat::AnthropicMessages, "/messages")
        );
    }
}
