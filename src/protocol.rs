use super::config::Provider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFormat {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
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

    let (provider, endpoint) = path.strip_prefix('/')?.split_once('/')?;
    if provider.is_empty() || provider.contains('/') {
        return None;
    }
    detect_endpoint(&format!("/{endpoint}")).map(|input| Route {
        provider: Some(provider.to_string()),
        input,
    })
}

fn detect_endpoint(path: &str) -> Option<InputFormat> {
    match path {
        "/chat/completions" | "/v1/chat/completions" => Some(InputFormat::OpenAiChat),
        "/responses" | "/v1/responses" => Some(InputFormat::OpenAiResponses),
        "/messages" | "/v1/messages" => Some(InputFormat::AnthropicMessages),
        _ => None,
    }
}

pub fn upstream_for(provider: Provider) -> (UpstreamFormat, &'static str) {
    match provider {
        Provider::OpenAiChat => (UpstreamFormat::OpenAiChat, "/chat/completions"),
        Provider::OpenAiResponses => (UpstreamFormat::OpenAiResponses, "/responses"),
        Provider::Anthropic => (UpstreamFormat::AnthropicMessages, "/messages"),
    }
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
        assert_eq!(detect_route("/models"), None);
        assert_eq!(detect_route("/openai/models"), None);
        assert_eq!(detect_route("/too/many/responses"), None);
    }

    #[test]
    fn maps_provider_to_canonical_upstream_endpoint() {
        assert_eq!(
            upstream_for(Provider::OpenAiChat),
            (UpstreamFormat::OpenAiChat, "/chat/completions")
        );
        assert_eq!(
            upstream_for(Provider::OpenAiResponses),
            (UpstreamFormat::OpenAiResponses, "/responses")
        );
        assert_eq!(
            upstream_for(Provider::Anthropic),
            (UpstreamFormat::AnthropicMessages, "/messages")
        );
    }
}
