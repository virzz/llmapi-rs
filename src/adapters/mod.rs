pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};
use thiserror::Error;

use super::model::{
    Extensions, LLMAnnotation, LLMContent, LLMFinishReason, LLMMessage, LLMProtocol, LLMRequest,
    LLMResponse, LLMStreamEvent, LLMTool, LLMToolKind, LLMUsage,
};

const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("missing field: {0}")]
    MissingField(&'static str),
    #[error("invalid field: {0}")]
    InvalidField(&'static str),
    #[error("unsupported protocol feature: {0}")]
    Unsupported(String),
    #[error("invalid stream utf-8")]
    InvalidStreamUtf8,
    #[error("stream event exceeds {MAX_SSE_EVENT_BYTES} bytes")]
    StreamEventTooLarge,
}

pub trait RequestAdapter {
    fn to_llm_request(value: Value) -> Result<LLMRequest, AdapterError>;
    fn from_llm_request(request: &LLMRequest) -> Result<Value, AdapterError>;
}

pub trait ResponseAdapter {
    fn to_llm_response(value: Value) -> Result<LLMResponse, AdapterError>;
    fn from_llm_response(response: &LLMResponse) -> Result<Value, AdapterError>;
}

pub trait StreamAdapter {
    fn parse_stream_event(
        event: &SseEvent,
        state: &mut StreamState,
    ) -> Result<Vec<LLMStreamEvent>, AdapterError>;

    fn format_stream_events(
        events: &[LLMStreamEvent],
        state: &mut StreamState,
    ) -> Result<Vec<SseEvent>, AdapterError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

impl SseEvent {
    pub fn json(event: Option<&str>, value: &Value) -> Self {
        Self {
            event: event.map(ToString::to_string),
            data: value.to_string(),
            id: None,
            retry: None,
        }
    }

    pub fn encode(&self) -> String {
        let mut output = String::new();
        if let Some(event) = &self.event {
            output.push_str("event: ");
            output.push_str(event);
            output.push('\n');
        }
        if let Some(id) = &self.id {
            output.push_str("id: ");
            output.push_str(id);
            output.push('\n');
        }
        if let Some(retry) = self.retry {
            output.push_str(&format!("retry: {retry}\n"));
        }
        for line in self.data.split('\n') {
            output.push_str("data: ");
            output.push_str(line);
            output.push('\n');
        }
        output.push('\n');
        output
    }

    fn parse(bytes: &[u8]) -> Result<Option<Self>, AdapterError> {
        let text = std::str::from_utf8(bytes).map_err(|_| AdapterError::InvalidStreamUtf8)?;
        let mut event = Self::default();
        let mut data = Vec::new();

        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event.event = Some(value.to_string()),
                "data" => data.push(value),
                "id" => event.id = Some(value.to_string()),
                "retry" => event.retry = value.parse().ok(),
                _ => {}
            }
        }

        if data.is_empty() && event.event.is_none() && event.id.is_none() && event.retry.is_none() {
            return Ok(None);
        }
        event.data = data.join("\n");
        Ok(Some(event))
    }
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, AdapterError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((end, delimiter_len)) = find_sse_delimiter(&self.buffer) {
            let event = self.buffer[..end].to_vec();
            self.buffer.drain(..end + delimiter_len);
            if let Some(event) = SseEvent::parse(&event)? {
                events.push(event);
            }
        }
        if self.buffer.len() > MAX_SSE_EVENT_BYTES {
            return Err(AdapterError::StreamEventTooLarge);
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<SseEvent>, AdapterError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = std::mem::take(&mut self.buffer);
        Ok(SseEvent::parse(&bytes)?.into_iter().collect())
    }
}

fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|bytes| bytes == b"\n\n");
    let crlf = buffer.windows(4).position(|bytes| bytes == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

#[derive(Debug, Default)]
pub struct StreamState {
    pub id: Option<String>,
    pub model: Option<String>,
    pub started: bool,
    pub ended: bool,
    pub next_content_index: usize,
    pub open_content: BTreeSet<usize>,
    pub content_indices: BTreeMap<usize, usize>,
    pub content_ids: BTreeMap<usize, String>,
    pub content_kinds: BTreeMap<usize, String>,
    pub content_buffers: BTreeMap<usize, String>,
    pub content_annotations: BTreeMap<usize, Vec<LLMAnnotation>>,
    pub tool_names: BTreeMap<usize, String>,
    pub tool_call_ids: BTreeMap<usize, String>,
    pub completed_items: Vec<Value>,
    pub tool_indices: BTreeMap<usize, usize>,
    pub sequence_number: u64,
    pub finish_reason: Option<LLMFinishReason>,
    pub usage: Option<LLMUsage>,
}

impl StreamState {
    pub fn target_content_index(&mut self, source_index: usize) -> usize {
        if let Some(index) = self.content_indices.get(&source_index) {
            return *index;
        }
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.content_indices.insert(source_index, index);
        index
    }
}

pub(super) fn tools_from_openai_chat(value: &Value) -> Result<Vec<LLMTool>, AdapterError> {
    tools_from_value(value, ToolDialect::OpenAiChat)
}

pub(super) fn tools_from_openai_responses(value: &Value) -> Result<Vec<LLMTool>, AdapterError> {
    tools_from_value(value, ToolDialect::OpenAiResponses)
}

pub(super) fn tools_from_anthropic(value: &Value) -> Result<Vec<LLMTool>, AdapterError> {
    tools_from_value(value, ToolDialect::Anthropic)
}

pub(super) fn tools_to_openai_chat(tools: &[LLMTool]) -> Result<Vec<Value>, AdapterError> {
    tools_to_value(tools, ToolDialect::OpenAiChat)
}

pub(super) fn tools_to_openai_responses(tools: &[LLMTool]) -> Result<Vec<Value>, AdapterError> {
    tools_to_value(tools, ToolDialect::OpenAiResponses)
}

pub(super) fn tools_to_anthropic(tools: &[LLMTool]) -> Result<Vec<Value>, AdapterError> {
    tools_to_value(tools, ToolDialect::Anthropic)
}

#[derive(Clone, Copy)]
enum ToolDialect {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

impl ToolDialect {
    fn protocol(self) -> LLMProtocol {
        match self {
            Self::OpenAiChat => LLMProtocol::OpenAiChat,
            Self::OpenAiResponses => LLMProtocol::OpenAiResponses,
            Self::Anthropic => LLMProtocol::AnthropicMessages,
        }
    }
}

fn tools_from_value(value: &Value, dialect: ToolDialect) -> Result<Vec<LLMTool>, AdapterError> {
    let Some(tools) = value.as_array() else {
        return Ok(Vec::new());
    };
    tools
        .iter()
        .map(|tool| {
            let tool_type = tool["type"].as_str().unwrap_or("function");
            let function = match dialect {
                ToolDialect::OpenAiChat if tool_type == "function" => &tool["function"],
                _ => tool,
            };
            if tool_type == "function"
                || matches!(dialect, ToolDialect::Anthropic)
                    && tool["name"].is_string()
                    && tool.get("input_schema").is_some()
            {
                let name = function["name"]
                    .as_str()
                    .ok_or(AdapterError::MissingField("tools.name"))?
                    .to_string();
                let schema_field = if matches!(dialect, ToolDialect::Anthropic) {
                    "input_schema"
                } else {
                    "parameters"
                };
                let mut metadata = extensions_without(
                    function,
                    &["name", "description", schema_field, "strict", "type"],
                );
                if matches!(dialect, ToolDialect::OpenAiChat) {
                    metadata.extend(extensions_without(tool, &["type", "function"]));
                }
                Ok(LLMTool {
                    source: dialect.protocol(),
                    kind: LLMToolKind::Function {
                        name,
                        description: function["description"].as_str().map(ToString::to_string),
                        parameters: function
                            .get(schema_field)
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"})),
                        strict: function["strict"].as_bool(),
                    },
                    metadata,
                })
            } else {
                let name = normalize_builtin_tool_name(tool_type, dialect).to_string();
                Ok(LLMTool {
                    source: dialect.protocol(),
                    kind: LLMToolKind::Builtin {
                        protocol: dialect.protocol(),
                        name,
                        config: tool.clone(),
                    },
                    metadata: Extensions::new(),
                })
            }
        })
        .collect()
}

fn tools_to_value(tools: &[LLMTool], dialect: ToolDialect) -> Result<Vec<Value>, AdapterError> {
    tools
        .iter()
        .map(|tool| {
            Ok(match &tool.kind {
                LLMToolKind::Function {
                    name,
                    description,
                    parameters,
                    strict,
                } => {
                    let mut function = Map::new();
                    function.insert("name".into(), json!(name));
                    if let Some(description) = description {
                        function.insert("description".into(), json!(description));
                    }
                    function.insert(
                        if matches!(dialect, ToolDialect::Anthropic) {
                            "input_schema"
                        } else {
                            "parameters"
                        }
                        .into(),
                        parameters.clone(),
                    );
                    if let Some(strict) = strict {
                        function.insert("strict".into(), json!(strict));
                    }
                    if tool.source == dialect.protocol() {
                        function.extend(tool.metadata.clone());
                    } else {
                        function.extend(
                            tool.metadata
                                .iter()
                                .filter(|(name, _)| name.starts_with("x-"))
                                .map(|(name, value)| (name.clone(), value.clone())),
                        );
                    }
                    if matches!(dialect, ToolDialect::OpenAiChat) {
                        json!({"type": "function", "function": function})
                    } else {
                        if matches!(dialect, ToolDialect::OpenAiResponses) {
                            function.insert("type".into(), json!("function"));
                        }
                        Value::Object(function)
                    }
                }
                LLMToolKind::Builtin {
                    protocol,
                    name,
                    config,
                } => builtin_tool_to_value(*protocol, name, config, dialect)?,
            })
        })
        .collect()
}

fn normalize_builtin_tool_name(name: &str, dialect: ToolDialect) -> &str {
    match (dialect, name) {
        (ToolDialect::Anthropic, "web_search_20250305") => "web_search",
        (ToolDialect::Anthropic, name) if name.starts_with("computer_") => "computer",
        (ToolDialect::Anthropic, name) if name.starts_with("code_execution_") => "code_interpreter",
        (ToolDialect::Anthropic, name) if name.starts_with("bash_") => "local_shell",
        (ToolDialect::OpenAiResponses, "computer_use_preview") => "computer",
        (_, name) => name,
    }
}

fn builtin_tool_to_value(
    source: LLMProtocol,
    name: &str,
    config: &Value,
    dialect: ToolDialect,
) -> Result<Value, AdapterError> {
    let target = dialect.protocol();
    if source == target {
        return Ok(config.clone());
    }
    match (name, dialect) {
        ("web_search" | "web_search_preview", ToolDialect::OpenAiResponses) => {
            let mut value = json!({"type": "web_search"});
            for field in ["search_context_size", "user_location"] {
                if let Some(field_value) = config.get(field) {
                    value[field] = field_value.clone();
                }
            }
            if let Some(domains) = config["allowed_domains"]
                .as_array()
                .or_else(|| config["filters"]["allowed_domains"].as_array())
            {
                value["filters"] = json!({"allowed_domains": domains});
            }
            Ok(value)
        }
        ("web_search" | "web_search_preview", ToolDialect::Anthropic) => {
            let mut value = json!({"type": "web_search_20250305", "name": "web_search"});
            for field in [
                "max_uses",
                "allowed_domains",
                "blocked_domains",
                "user_location",
            ] {
                if let Some(field_value) = config.get(field) {
                    value[field] = field_value.clone();
                }
            }
            if let Some(domains) = config["filters"].get("allowed_domains") {
                value["allowed_domains"] = domains.clone();
            }
            Ok(value)
        }
        ("computer", ToolDialect::OpenAiResponses) => Ok(json!({
            "type": "computer_use_preview",
            "display_width": config["display_width"].as_u64().or_else(|| config["display_width_px"].as_u64()).unwrap_or(1024),
            "display_height": config["display_height"].as_u64().or_else(|| config["display_height_px"].as_u64()).unwrap_or(768),
            "environment": config["environment"].as_str().unwrap_or("browser")
        })),
        ("computer", ToolDialect::Anthropic) => Ok(json!({
            "type": "computer_20250124",
            "name": "computer",
            "display_width_px": config["display_width_px"].as_u64().or_else(|| config["display_width"].as_u64()).unwrap_or(1024),
            "display_height_px": config["display_height_px"].as_u64().or_else(|| config["display_height"].as_u64()).unwrap_or(768)
        })),
        ("code_interpreter", ToolDialect::OpenAiResponses) => {
            Ok(json!({"type": "code_interpreter", "container": {"type": "auto"}}))
        }
        ("code_interpreter", ToolDialect::Anthropic) => {
            Ok(json!({"type": "code_execution_20250522", "name": "code_execution"}))
        }
        ("local_shell", ToolDialect::OpenAiResponses) => Ok(json!({"type": "local_shell"})),
        ("local_shell", ToolDialect::Anthropic) => {
            Ok(json!({"type": "bash_20250124", "name": "bash"}))
        }
        _ => Err(AdapterError::Unsupported(format!(
            "{target:?} cannot represent {source:?} built-in tool `{name}`"
        ))),
    }
}

pub(super) fn extensions_without(value: &Value, known: &[&str]) -> Extensions {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !known.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn value_as_extensions(value: &Value) -> Extensions {
    value.as_object().cloned().unwrap_or_default()
}

pub(super) fn message_extensions_for(message: &LLMMessage, target: LLMProtocol) -> Extensions {
    if message.source == Some(target) {
        message.metadata.clone()
    } else {
        Extensions::new()
    }
}

pub(super) fn content_extensions_for(content: &LLMContent, target: LLMProtocol) -> Extensions {
    if content.source == Some(target) {
        content.metadata.clone()
    } else {
        Extensions::new()
    }
}

pub(super) fn annotation_from_openai(value: &Value, source: LLMProtocol) -> LLMAnnotation {
    let details = value.get("url_citation").unwrap_or(value);
    LLMAnnotation {
        source,
        annotation_type: value["type"].as_str().unwrap_or("unknown").to_string(),
        start_index: details["start_index"].as_u64(),
        end_index: details["end_index"].as_u64(),
        url: details["url"].as_str().map(ToString::to_string),
        title: details["title"].as_str().map(ToString::to_string),
        file_id: details["file_id"].as_str().map(ToString::to_string),
        filename: details["filename"].as_str().map(ToString::to_string),
        cited_text: details["cited_text"].as_str().map(ToString::to_string),
        metadata: extensions_without(
            details,
            &[
                "type",
                "start_index",
                "end_index",
                "url",
                "title",
                "file_id",
                "filename",
                "cited_text",
            ],
        ),
    }
}

pub(super) fn annotation_to_openai(
    annotation: &LLMAnnotation,
    target: LLMProtocol,
) -> Result<Value, AdapterError> {
    let annotation_type = if annotation.url.is_some() {
        "url_citation"
    } else if annotation.file_id.is_some() {
        if target == LLMProtocol::OpenAiChat && annotation.source != target {
            return Err(AdapterError::Unsupported(
                "OpenAI Chat cannot represent a foreign file citation".into(),
            ));
        }
        "file_citation"
    } else if annotation.source == target {
        annotation.annotation_type.as_str()
    } else {
        return Err(AdapterError::Unsupported(format!(
            "{target:?} cannot represent {source:?} citation `{annotation_type}`",
            source = annotation.source,
            annotation_type = annotation.annotation_type,
        )));
    };
    let mut details = if annotation.source == target {
        annotation.metadata.clone()
    } else {
        Extensions::new()
    };
    details.insert("type".into(), json!(annotation_type));
    insert_annotation_field(&mut details, "start_index", annotation.start_index);
    insert_annotation_field(&mut details, "end_index", annotation.end_index);
    insert_annotation_field(&mut details, "url", annotation.url.as_deref());
    insert_annotation_field(&mut details, "title", annotation.title.as_deref());
    insert_annotation_field(&mut details, "file_id", annotation.file_id.as_deref());
    insert_annotation_field(&mut details, "filename", annotation.filename.as_deref());
    if target == LLMProtocol::OpenAiChat && annotation_type == "url_citation" {
        details.remove("type");
        Ok(json!({"type": "url_citation", "url_citation": details}))
    } else {
        Ok(Value::Object(details))
    }
}

pub(super) fn annotation_from_anthropic(value: &Value) -> LLMAnnotation {
    LLMAnnotation {
        source: LLMProtocol::AnthropicMessages,
        annotation_type: value["type"].as_str().unwrap_or("unknown").to_string(),
        start_index: value["start_char_index"]
            .as_u64()
            .or_else(|| value["start_index"].as_u64()),
        end_index: value["end_char_index"]
            .as_u64()
            .or_else(|| value["end_index"].as_u64()),
        url: value["url"].as_str().map(ToString::to_string),
        title: value["title"].as_str().map(ToString::to_string),
        file_id: value["file_id"].as_str().map(ToString::to_string),
        filename: value["filename"].as_str().map(ToString::to_string),
        cited_text: value["cited_text"].as_str().map(ToString::to_string),
        metadata: extensions_without(
            value,
            &[
                "type",
                "start_char_index",
                "end_char_index",
                "start_index",
                "end_index",
                "url",
                "title",
                "file_id",
                "filename",
                "cited_text",
            ],
        ),
    }
}

pub(super) fn annotation_to_anthropic(annotation: &LLMAnnotation) -> Result<Value, AdapterError> {
    let same_protocol = annotation.source == LLMProtocol::AnthropicMessages;
    let annotation_type = if same_protocol {
        annotation.annotation_type.as_str()
    } else if annotation.url.is_some() && annotation.cited_text.is_some() {
        "web_search_result_location"
    } else {
        return Err(AdapterError::Unsupported(format!(
            "Anthropic Messages cannot represent {source:?} citation `{annotation_type}`",
            source = annotation.source,
            annotation_type = annotation.annotation_type,
        )));
    };
    let mut value = if same_protocol {
        annotation.metadata.clone()
    } else {
        Extensions::new()
    };
    value.insert("type".into(), json!(annotation_type));
    insert_annotation_field(&mut value, "start_char_index", annotation.start_index);
    insert_annotation_field(&mut value, "end_char_index", annotation.end_index);
    insert_annotation_field(&mut value, "url", annotation.url.as_deref());
    insert_annotation_field(&mut value, "title", annotation.title.as_deref());
    insert_annotation_field(&mut value, "file_id", annotation.file_id.as_deref());
    insert_annotation_field(&mut value, "filename", annotation.filename.as_deref());
    insert_annotation_field(&mut value, "cited_text", annotation.cited_text.as_deref());
    Ok(Value::Object(value))
}

fn insert_annotation_field<T: SerializeAnnotationField>(
    target: &mut Map<String, Value>,
    name: &str,
    value: Option<T>,
) {
    if let Some(value) = value {
        target.insert(name.into(), value.to_json());
    }
}

trait SerializeAnnotationField {
    fn to_json(self) -> Value;
}

impl SerializeAnnotationField for u64 {
    fn to_json(self) -> Value {
        json!(self)
    }
}

impl SerializeAnnotationField for &str {
    fn to_json(self) -> Value {
        json!(self)
    }
}

pub(super) fn validate_request_extras(
    request: &LLMRequest,
    target: LLMProtocol,
    passthrough: &[&str],
    mapped: &[&str],
    intentionally_ignored: &[&str],
) -> Result<(), AdapterError> {
    for (name, value) in &request.extra {
        if passthrough.contains(&name.as_str())
            || mapped.contains(&name.as_str())
            || intentionally_ignored.contains(&name.as_str())
            || is_effectively_default(name, value)
        {
            continue;
        }
        return Err(AdapterError::Unsupported(format!(
            "{target:?} cannot preserve {source:?} request field `{name}`",
            source = request.source
        )));
    }
    Ok(())
}

fn is_effectively_default(name: &str, value: &Value) -> bool {
    if value.is_null()
        || value.as_str() == Some("")
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(Map::is_empty)
    {
        return true;
    }
    match name {
        "n" => value.as_u64() == Some(1),
        "background" | "logprobs" | "store" => value.as_bool() == Some(false),
        "frequency_penalty" | "presence_penalty" | "top_logprobs" => value.as_f64() == Some(0.0),
        "modalities" => value
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value.as_str() == Some("text"))),
        "truncation" => value.as_str() == Some("disabled"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_function_tool_schemas_without_losing_extensions() {
        let chat = json!([{
            "type": "function",
            "function": {
                "name": "weather",
                "description": "Get weather",
                "parameters": {"type": "object"},
                "strict": true,
                "x-extension": "kept"
            }
        }]);
        let normalized = tools_from_openai_chat(&chat).unwrap();
        let anthropic = tools_to_anthropic(&normalized).unwrap();
        let responses = tools_to_openai_responses(&normalized).unwrap();

        assert_eq!(anthropic[0]["name"], "weather");
        assert_eq!(anthropic[0]["input_schema"]["type"], "object");
        assert_eq!(anthropic[0]["x-extension"], "kept");
        assert_eq!(responses[0]["type"], "function");
    }

    #[test]
    fn translates_compatible_builtin_tools_and_rejects_unrepresentable_tools() {
        let responses = tools_from_openai_responses(&json!([{
            "type": "web_search",
            "search_context_size": "high",
            "filters": {"allowed_domains": ["example.test"]}
        }]))
        .unwrap();
        let anthropic = tools_to_anthropic(&responses).unwrap();
        assert_eq!(anthropic[0]["type"], "web_search_20250305");
        assert_eq!(anthropic[0]["name"], "web_search");
        assert_eq!(anthropic[0]["allowed_domains"][0], "example.test");

        let computer = tools_from_anthropic(&json!([{
            "type": "computer_20250124",
            "name": "computer",
            "display_width_px": 1280,
            "display_height_px": 720
        }]))
        .unwrap();
        let responses = tools_to_openai_responses(&computer).unwrap();
        assert_eq!(responses[0]["type"], "computer_use_preview");
        assert_eq!(responses[0]["display_width"], 1280);

        let file_search = tools_from_openai_responses(&json!([{"type": "file_search"}])).unwrap();
        assert!(matches!(
            tools_to_anthropic(&file_search),
            Err(AdapterError::Unsupported(_))
        ));
    }

    #[test]
    fn citation_shapes_round_trip_only_in_representable_protocols() {
        for citation in [
            json!({
                "type": "char_location",
                "cited_text": "quoted",
                "document_index": 1,
                "document_title": "Guide",
                "start_char_index": 4,
                "end_char_index": 10
            }),
            json!({
                "type": "page_location",
                "cited_text": "quoted",
                "document_index": 1,
                "document_title": "Guide",
                "start_page_number": 2,
                "end_page_number": 3
            }),
            json!({
                "type": "content_block_location",
                "cited_text": "quoted",
                "document_index": 1,
                "document_title": "Guide",
                "start_block_index": 2,
                "end_block_index": 3
            }),
        ] {
            let normalized = annotation_from_anthropic(&citation);
            assert_eq!(annotation_to_anthropic(&normalized).unwrap(), citation);
            assert!(matches!(
                annotation_to_openai(&normalized, LLMProtocol::OpenAiResponses),
                Err(AdapterError::Unsupported(_))
            ));
        }

        let file = json!({
            "type": "file_citation",
            "file_id": "file_1",
            "filename": "guide.pdf",
            "index": 12
        });
        let normalized = annotation_from_openai(&file, LLMProtocol::OpenAiResponses);
        assert_eq!(
            annotation_to_openai(&normalized, LLMProtocol::OpenAiResponses).unwrap(),
            file
        );
        assert!(matches!(
            annotation_to_anthropic(&normalized),
            Err(AdapterError::Unsupported(_))
        ));
    }

    #[test]
    fn semantic_zero_fields_are_not_silently_treated_as_defaults() {
        let with_seed = openai_chat::OpenAiChatAdapter::to_llm_request(json!({
            "model": "gpt-4.1",
            "messages": [{"role": "user", "content": "hello"}],
            "seed": 0
        }))
        .unwrap();
        assert!(matches!(
            anthropic::AnthropicAdapter::from_llm_request(&with_seed),
            Err(AdapterError::Unsupported(message)) if message.contains("field `seed`")
        ));

        let default_penalties = openai_chat::OpenAiChatAdapter::to_llm_request(json!({
            "model": "gpt-4.1",
            "messages": [{"role": "user", "content": "hello"}],
            "frequency_penalty": 0,
            "presence_penalty": 0
        }))
        .unwrap();
        assert!(anthropic::AnthropicAdapter::from_llm_request(&default_penalties).is_ok());
    }

    #[test]
    fn decoder_handles_crlf_multiline_data_and_split_utf8() {
        let mut decoder = SseDecoder::default();
        let bytes = "event: delta\r\ndata: {\"text\":\"你\"}\r\n\r\n".as_bytes();
        let split = bytes.iter().position(|byte| *byte >= 0x80).unwrap() + 1;

        assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
        let events = decoder.push(&bytes[split..]).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data, "{\"text\":\"你\"}");
    }

    #[test]
    fn decoder_preserves_multiple_data_lines() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(b"data: first\ndata: second\n\n").unwrap();
        assert_eq!(events[0].data, "first\nsecond");
    }
}
