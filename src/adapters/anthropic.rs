use serde_json::{json, Map, Value};

use super::super::{
    adapters::{
        annotation_from_anthropic, annotation_to_anthropic, content_extensions_for,
        extensions_without, validate_request_extras, value_as_extensions, AdapterError,
        RequestAdapter, ResponseAdapter, SseEvent, StreamAdapter, StreamState,
    },
    model::{
        Extensions, LLMChoice, LLMContent, LLMContentKind, LLMFinishReason, LLMMediaSource,
        LLMMessage, LLMProtocol, LLMRequest, LLMResponse, LLMRole, LLMStreamEvent, LLMToolChoice,
        LLMUsage,
    },
};

const ANTHROPIC_EXTRA_REQUEST_FIELDS: &[&str] = &[
    "container",
    "context_management",
    "inference_geo",
    "mcp_servers",
    "service_tier",
    "speed",
    "top_k",
];

pub struct AnthropicAdapter;

impl RequestAdapter for AnthropicAdapter {
    fn to_llm_request(value: Value) -> Result<LLMRequest, AdapterError> {
        let model = value["model"]
            .as_str()
            .ok_or(AdapterError::MissingField("model"))?
            .to_string();
        let mut messages = Vec::new();
        if let Some(system) = value.get("system").filter(|value| !value.is_null()) {
            messages.push(LLMMessage {
                source: Some(LLMProtocol::AnthropicMessages),
                role: LLMRole::System,
                content: anthropic_content_from_value(system),
                name: None,
                metadata: Extensions::new(),
            });
        }
        for item in value["messages"]
            .as_array()
            .ok_or(AdapterError::MissingField("messages"))?
        {
            messages.push(LLMMessage {
                source: Some(LLMProtocol::AnthropicMessages),
                role: match item["role"].as_str().unwrap_or("user") {
                    "assistant" => LLMRole::Assistant,
                    _ => LLMRole::User,
                },
                content: anthropic_content_from_value(&item["content"]),
                name: None,
                metadata: extensions_without(item, &["role", "content"]),
            });
        }
        let reasoning = match (
            value.get("thinking").cloned(),
            value["output_config"].get("effort").cloned(),
        ) {
            (Some(mut thinking), Some(effort)) => {
                thinking["effort"] = effort;
                Some(thinking)
            }
            (Some(thinking), None) => Some(thinking),
            (None, Some(effort)) => Some(json!({"effort": effort})),
            (None, None) => None,
        };
        let response_format = value["output_config"]
            .get("format")
            .cloned()
            .or_else(|| value.get("output_format").cloned());

        Ok(LLMRequest {
            source: LLMProtocol::AnthropicMessages,
            model,
            messages,
            temperature: value["temperature"].as_f64(),
            max_tokens: value["max_tokens"].as_u64(),
            top_p: value["top_p"].as_f64(),
            stop: value.get("stop_sequences").cloned(),
            stream: value["stream"].as_bool().unwrap_or(false),
            tools: super::tools_from_anthropic(&value["tools"])?,
            tool_choice: parse_anthropic_tool_choice(value.get("tool_choice")),
            parallel_tool_calls: value["tool_choice"]["disable_parallel_tool_use"]
                .as_bool()
                .map(|disabled| !disabled),
            response_format,
            reasoning,
            metadata: value_as_extensions(&value["metadata"]),
            extra: extensions_without(
                &value,
                &[
                    "model",
                    "messages",
                    "system",
                    "temperature",
                    "max_tokens",
                    "top_p",
                    "stop_sequences",
                    "stream",
                    "tools",
                    "tool_choice",
                    "output_config",
                    "output_format",
                    "thinking",
                    "metadata",
                ],
            ),
        })
    }

    fn from_llm_request(request: &LLMRequest) -> Result<Value, AdapterError> {
        validate_request_extras(
            request,
            LLMProtocol::AnthropicMessages,
            ANTHROPIC_EXTRA_REQUEST_FIELDS,
            &["user", "safety_identifier"],
            &[
                "include",
                "prompt_cache_key",
                "prompt_cache_retention",
                "stream_options",
                "verbosity",
            ],
        )?;
        let (system, messages) = anthropic_messages_to_value(&request.messages)?;
        let mut value = Map::new();
        copy_selected(&mut value, &request.extra, ANTHROPIC_EXTRA_REQUEST_FIELDS);
        if let Some(service_tier) = request.extra.get("service_tier").and_then(Value::as_str) {
            value.insert(
                "service_tier".into(),
                json!(if service_tier == "standard_only" {
                    "standard_only"
                } else {
                    "auto"
                }),
            );
        }
        value.insert("model".into(), json!(request.model));
        value.insert("messages".into(), Value::Array(messages));
        value.insert(
            "max_tokens".into(),
            json!(request.max_tokens.unwrap_or(1024)),
        );
        value.insert("stream".into(), json!(request.stream));
        if !system.is_empty() {
            value.insert("system".into(), Value::Array(system));
        }
        insert_option(&mut value, "temperature", request.temperature);
        insert_option(&mut value, "top_p", request.top_p);
        if let Some(stop) = &request.stop {
            value.insert("stop_sequences".into(), normalize_stop_sequences(stop));
        }
        if !request.tools.is_empty() {
            value.insert(
                "tools".into(),
                Value::Array(super::tools_to_anthropic(&request.tools)?),
            );
        }
        if let Some(choice) = &request.tool_choice {
            let mut choice = anthropic_tool_choice_to_value(choice);
            if let Some(parallel) = request.parallel_tool_calls {
                choice["disable_parallel_tool_use"] = json!(!parallel);
            }
            value.insert("tool_choice".into(), choice);
        }
        let mut output_config = Map::new();
        if let Some(format) = &request.response_format {
            output_config.insert("format".into(), format.clone());
        }
        if let Some(reasoning) = &request.reasoning {
            if let Some(effort) = reasoning.get("effort") {
                output_config.insert("effort".into(), effort.clone());
            }
            let thinking = if request.source == LLMProtocol::AnthropicMessages {
                let mut thinking = reasoning.clone();
                thinking
                    .as_object_mut()
                    .map(|object| object.remove("effort"));
                thinking
            } else if reasoning["effort"].as_str() == Some("none") {
                json!({"type": "disabled"})
            } else {
                json!({"type": "adaptive"})
            };
            if thinking.is_object() && !thinking.as_object().is_some_and(Map::is_empty) {
                value.insert("thinking".into(), thinking);
            }
        }
        if !output_config.is_empty() {
            value.insert("output_config".into(), Value::Object(output_config));
        }
        if let Some(metadata) = anthropic_request_metadata(request) {
            value.insert("metadata".into(), metadata);
        }
        Ok(Value::Object(value))
    }
}

impl ResponseAdapter for AnthropicAdapter {
    fn to_llm_response(value: Value) -> Result<LLMResponse, AdapterError> {
        let raw_reason = value["stop_reason"].as_str();
        let mut choice_metadata = Extensions::new();
        if let Some(sequence) = value.get("stop_sequence").filter(|value| !value.is_null()) {
            choice_metadata.insert("stop_sequence".into(), sequence.clone());
        }
        if raw_reason
            .is_some_and(|reason| parse_anthropic_finish_reason(reason) == LLMFinishReason::Unknown)
        {
            choice_metadata.insert("raw_finish_reason".into(), json!(raw_reason));
        }
        Ok(LLMResponse {
            source: LLMProtocol::AnthropicMessages,
            id: value["id"].as_str().map(ToString::to_string),
            model: value["model"].as_str().map(ToString::to_string),
            choices: vec![LLMChoice {
                index: 0,
                role: LLMRole::Assistant,
                content: anthropic_content_from_value(&value["content"]),
                finish_reason: raw_reason.map(parse_anthropic_finish_reason),
                metadata: choice_metadata,
            }],
            usage: value.get("usage").and_then(anthropic_usage_from_value),
            metadata: Extensions::new(),
            extra: extensions_without(
                &value,
                &[
                    "id",
                    "type",
                    "role",
                    "model",
                    "content",
                    "stop_reason",
                    "stop_sequence",
                    "usage",
                ],
            ),
        })
    }

    fn from_llm_response(response: &LLMResponse) -> Result<Value, AdapterError> {
        if response.choices.len() > 1 {
            return Err(AdapterError::Unsupported(
                "Anthropic Messages cannot represent multiple Chat choices".into(),
            ));
        }
        let choice = response
            .primary_choice()
            .ok_or(AdapterError::InvalidField("choices"))?;
        let mut value = if response.source == LLMProtocol::AnthropicMessages {
            response.extra.clone()
        } else {
            Extensions::new()
        };
        value.insert(
            "id".into(),
            json!(response
                .id
                .clone()
                .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4()))),
        );
        value.insert("type".into(), json!("message"));
        value.insert("role".into(), json!("assistant"));
        value.insert(
            "model".into(),
            json!(response.model.clone().unwrap_or_default()),
        );
        value.insert(
            "content".into(),
            Value::Array(
                choice
                    .content
                    .iter()
                    .map(anthropic_content_to_value)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        );
        value.insert(
            "stop_reason".into(),
            choice
                .finish_reason
                .map(anthropic_finish_reason_to_value)
                .unwrap_or(Value::Null),
        );
        value.insert(
            "stop_sequence".into(),
            choice
                .metadata
                .get("stop_sequence")
                .cloned()
                .unwrap_or(Value::Null),
        );
        if let Some(usage) = &response.usage {
            value.insert("usage".into(), anthropic_usage_to_value(usage));
        }
        Ok(Value::Object(value))
    }
}

impl StreamAdapter for AnthropicAdapter {
    fn parse_stream_event(
        event: &SseEvent,
        state: &mut StreamState,
    ) -> Result<Vec<LLMStreamEvent>, AdapterError> {
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| AdapterError::InvalidField("stream_json"))?;
        let event_type = value["type"]
            .as_str()
            .or(event.event.as_deref())
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            "message_start" => {
                let message = &value["message"];
                state.id = message["id"].as_str().map(ToString::to_string);
                state.model = message["model"].as_str().map(ToString::to_string);
                state.usage = message.get("usage").and_then(anthropic_usage_from_value);
                state.started = true;
                events.push(LLMStreamEvent::MessageStart {
                    id: state.id.clone(),
                    model: state.model.clone(),
                    usage: state.usage.clone(),
                });
            }
            "content_block_start" => {
                ensure_message_started(&mut events, state);
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let content =
                    anthropic_content_from_value(&json!([value["content_block"].clone()]))
                        .into_iter()
                        .next()
                        .ok_or(AdapterError::InvalidField("content_block"))?;
                state.open_content.insert(index);
                events.push(LLMStreamEvent::ContentStart { index, content });
            }
            "content_block_delta" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                match value["delta"]["type"].as_str().unwrap_or_default() {
                    "text_delta" => events.push(LLMStreamEvent::TextDelta {
                        index,
                        text: value["delta"]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    "input_json_delta" => events.push(LLMStreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: value["delta"]["partial_json"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    "thinking_delta" => events.push(LLMStreamEvent::ReasoningDelta {
                        index,
                        text: value["delta"]["thinking"].as_str().map(ToString::to_string),
                        signature: None,
                    }),
                    "signature_delta" => events.push(LLMStreamEvent::ReasoningDelta {
                        index,
                        text: None,
                        signature: value["delta"]["signature"]
                            .as_str()
                            .map(ToString::to_string),
                    }),
                    "citations_delta" => events.push(LLMStreamEvent::AnnotationAdded {
                        index,
                        annotation: annotation_from_anthropic(&value["delta"]["citation"]),
                    }),
                    _ => events.push(LLMStreamEvent::Raw {
                        protocol: LLMProtocol::AnthropicMessages,
                        value,
                    }),
                }
            }
            "content_block_stop" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                state.open_content.remove(&index);
                events.push(LLMStreamEvent::ContentEnd { index });
            }
            "message_delta" => {
                state.finish_reason = value["delta"]["stop_reason"]
                    .as_str()
                    .map(parse_anthropic_finish_reason);
                if let Some(usage) = value.get("usage").and_then(anthropic_usage_from_value) {
                    merge_usage(&mut state.usage, &usage);
                    events.push(LLMStreamEvent::Usage {
                        usage: state.usage.clone().unwrap_or(usage),
                    });
                }
            }
            "message_stop" => {
                for index in std::mem::take(&mut state.open_content) {
                    events.push(LLMStreamEvent::ContentEnd { index });
                }
                events.push(LLMStreamEvent::MessageEnd {
                    finish_reason: state.finish_reason,
                    usage: state.usage.clone(),
                    metadata: Extensions::new(),
                });
                state.ended = true;
            }
            "error" => events.push(LLMStreamEvent::Error {
                message: value["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream stream error")
                    .to_string(),
                metadata: value_as_extensions(&value["error"]),
            }),
            "ping" => {}
            _ => events.push(LLMStreamEvent::Raw {
                protocol: LLMProtocol::AnthropicMessages,
                value,
            }),
        }
        Ok(events)
    }

    fn format_stream_events(
        events: &[LLMStreamEvent],
        state: &mut StreamState,
    ) -> Result<Vec<SseEvent>, AdapterError> {
        let mut output = Vec::new();
        for event in events {
            match event {
                LLMStreamEvent::MessageStart { id, model, usage } => {
                    state.id = id
                        .clone()
                        .or_else(|| Some(format!("msg_{}", uuid::Uuid::new_v4())));
                    state.model = model.clone();
                    state.usage = usage.clone();
                    state.started = true;
                    output.push(anthropic_stream_event(
                        "message_start",
                        json!({"type": "message_start", "message": {
                            "id": state.id, "type": "message", "role": "assistant",
                            "model": state.model, "content": [], "stop_reason": null,
                            "stop_sequence": null,
                            "usage": usage.as_ref().map(anthropic_usage_to_value).unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0}))
                        }}),
                    ));
                }
                LLMStreamEvent::ContentStart { index, content } => {
                    let target = state.target_content_index(*index);
                    state.open_content.insert(target);
                    state
                        .content_kinds
                        .insert(target, anthropic_content_kind(content).into());
                    output.push(anthropic_stream_event(
                        "content_block_start",
                        json!({"type": "content_block_start", "index": target, "content_block": anthropic_stream_content_start(content)?}),
                    ));
                }
                LLMStreamEvent::TextDelta { index, text } => {
                    let target = state.target_content_index(*index);
                    output.push(anthropic_stream_event("content_block_delta", json!({"type": "content_block_delta", "index": target, "delta": {"type": "text_delta", "text": text}})));
                }
                LLMStreamEvent::ReasoningDelta {
                    index,
                    text,
                    signature,
                } => {
                    let target = state.target_content_index(*index);
                    if let Some(text) = text {
                        output.push(anthropic_stream_event("content_block_delta", json!({"type": "content_block_delta", "index": target, "delta": {"type": "thinking_delta", "thinking": text}})));
                    }
                    if let Some(signature) = signature {
                        output.push(anthropic_stream_event("content_block_delta", json!({"type": "content_block_delta", "index": target, "delta": {"type": "signature_delta", "signature": signature}})));
                    }
                }
                LLMStreamEvent::RefusalDelta { index, refusal } => {
                    let target = state.target_content_index(*index);
                    output.push(anthropic_stream_event("content_block_delta", json!({"type": "content_block_delta", "index": target, "delta": {"type": "text_delta", "text": refusal}})));
                }
                LLMStreamEvent::AnnotationAdded { index, annotation } => {
                    let target = state.target_content_index(*index);
                    let citation = annotation_to_anthropic(annotation)?;
                    output.push(anthropic_stream_event(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": target, "delta": {"type": "citations_delta", "citation": citation}}),
                    ));
                }
                LLMStreamEvent::ToolCallDelta {
                    index,
                    arguments_delta,
                    ..
                } => {
                    let target = state.target_content_index(*index);
                    output.push(anthropic_stream_event("content_block_delta", json!({"type": "content_block_delta", "index": target, "delta": {"type": "input_json_delta", "partial_json": arguments_delta}})));
                }
                LLMStreamEvent::ContentEnd { index } => {
                    let target = state.target_content_index(*index);
                    if state.open_content.remove(&target) {
                        output.push(anthropic_stream_event(
                            "content_block_stop",
                            json!({"type": "content_block_stop", "index": target}),
                        ));
                    }
                }
                LLMStreamEvent::Usage { usage } => merge_usage(&mut state.usage, usage),
                LLMStreamEvent::MessageEnd {
                    finish_reason,
                    usage,
                    metadata,
                } => {
                    if let Some(usage) = usage {
                        merge_usage(&mut state.usage, usage);
                    }
                    for index in std::mem::take(&mut state.open_content) {
                        output.push(anthropic_stream_event(
                            "content_block_stop",
                            json!({"type": "content_block_stop", "index": index}),
                        ));
                    }
                    let mut delta = metadata.clone();
                    delta.insert(
                        "stop_reason".into(),
                        finish_reason
                            .map(anthropic_finish_reason_to_value)
                            .unwrap_or_else(|| json!("end_turn")),
                    );
                    delta.insert("stop_sequence".into(), Value::Null);
                    let usage = state
                        .usage
                        .as_ref()
                        .map(anthropic_usage_to_value)
                        .unwrap_or_else(|| json!({"output_tokens": 0}));
                    output.push(anthropic_stream_event(
                        "message_delta",
                        json!({"type": "message_delta", "delta": delta, "usage": usage}),
                    ));
                    output.push(anthropic_stream_event(
                        "message_stop",
                        json!({"type": "message_stop"}),
                    ));
                    state.ended = true;
                }
                LLMStreamEvent::Error { message, metadata } => {
                    let mut error = metadata.clone();
                    error.entry("type").or_insert_with(|| json!("api_error"));
                    error.insert("message".into(), json!(message));
                    output.push(anthropic_stream_event(
                        "error",
                        json!({"type": "error", "error": error}),
                    ));
                }
                LLMStreamEvent::Raw { protocol, value } => {
                    if *protocol != LLMProtocol::AnthropicMessages {
                        return Err(AdapterError::Unsupported(format!(
                            "Anthropic Messages cannot represent a {protocol:?} stream event"
                        )));
                    }
                    output.push(anthropic_stream_event(
                        value["type"].as_str().unwrap_or("message"),
                        value.clone(),
                    ));
                }
            }
        }
        Ok(output)
    }
}

fn anthropic_content_from_value(value: &Value) -> Vec<LLMContent> {
    if let Some(text) = value.as_str() {
        return vec![LLMContent::text(text)];
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| {
            let metadata = extensions_without(
                block,
                &[
                    "type",
                    "text",
                    "source",
                    "id",
                    "name",
                    "input",
                    "tool_use_id",
                    "content",
                    "is_error",
                    "thinking",
                    "signature",
                    "data",
                    "citations",
                ],
            );
            let kind = match block["type"].as_str().unwrap_or("text") {
                "text" => LLMContentKind::Text {
                    text: block["text"].as_str().unwrap_or_default().to_string(),
                    annotations: block["citations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(annotation_from_anthropic)
                        .collect(),
                },
                "image" => LLMContentKind::Image {
                    source: anthropic_source_from_value(&block["source"]),
                    detail: None,
                },
                "document" => LLMContentKind::File {
                    source: anthropic_source_from_value(&block["source"]),
                    filename: block["title"].as_str().map(ToString::to_string),
                    media_type: block["source"]["media_type"]
                        .as_str()
                        .map(ToString::to_string),
                },
                "tool_use" | "server_tool_use" => LLMContentKind::ToolCall {
                    id: block["id"].as_str().map(ToString::to_string),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                },
                "tool_result" => LLMContentKind::ToolResult {
                    tool_call_id: block["tool_use_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    content: anthropic_content_from_value(&block["content"]),
                    is_error: block["is_error"].as_bool(),
                },
                "thinking" => LLMContentKind::Reasoning {
                    text: block["thinking"].as_str().map(ToString::to_string),
                    summary: Vec::new(),
                    signature: block["signature"].as_str().map(ToString::to_string),
                    encrypted_content: None,
                },
                "redacted_thinking" => LLMContentKind::Reasoning {
                    text: None,
                    summary: Vec::new(),
                    signature: None,
                    encrypted_content: block["data"].as_str().map(ToString::to_string),
                },
                "refusal" => LLMContentKind::Refusal {
                    refusal: block["text"]
                        .as_str()
                        .or_else(|| block["refusal"].as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                _ => LLMContentKind::Raw {
                    protocol: LLMProtocol::AnthropicMessages,
                    value: block.clone(),
                },
            };
            LLMContent {
                source: Some(LLMProtocol::AnthropicMessages),
                kind,
                metadata,
            }
        })
        .collect()
}

fn anthropic_content_to_value(content: &LLMContent) -> Result<Value, AdapterError> {
    let mut value = content_extensions_for(content, LLMProtocol::AnthropicMessages);
    match &content.kind {
        LLMContentKind::Text { text, annotations } => {
            value.insert("type".into(), json!("text"));
            value.insert("text".into(), json!(text));
            if !annotations.is_empty() {
                value.insert(
                    "citations".into(),
                    Value::Array(
                        annotations
                            .iter()
                            .map(annotation_to_anthropic)
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                );
            }
        }
        LLMContentKind::Image { source, .. } => {
            value.insert("type".into(), json!("image"));
            value.insert("source".into(), anthropic_source_to_value(source));
        }
        LLMContentKind::File {
            source,
            filename,
            media_type,
        } => {
            value.insert("type".into(), json!("document"));
            let mut source = anthropic_source_to_value(source);
            if let Some(media_type) = media_type {
                source["media_type"] = json!(media_type);
            }
            value.insert("source".into(), source);
            if let Some(filename) = filename {
                value.insert("title".into(), json!(filename));
            }
        }
        LLMContentKind::ToolCall {
            id,
            name,
            arguments,
        } => {
            value.insert("type".into(), json!("tool_use"));
            value.insert(
                "id".into(),
                json!(id
                    .clone()
                    .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4()))),
            );
            value.insert("name".into(), json!(name));
            value.insert("input".into(), normalize_tool_arguments(arguments));
        }
        LLMContentKind::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            value.insert("type".into(), json!("tool_result"));
            value.insert("tool_use_id".into(), json!(tool_call_id));
            if content
                .iter()
                .all(|part| matches!(part.kind, LLMContentKind::Text { .. }))
            {
                value.insert(
                    "content".into(),
                    json!(content
                        .iter()
                        .filter_map(LLMContent::text_value)
                        .collect::<Vec<_>>()
                        .join("")),
                );
            } else {
                value.insert(
                    "content".into(),
                    Value::Array(
                        content
                            .iter()
                            .map(anthropic_content_to_value)
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                );
            }
            if let Some(is_error) = is_error {
                value.insert("is_error".into(), json!(is_error));
            }
        }
        LLMContentKind::Reasoning {
            text,
            signature,
            encrypted_content,
            ..
        } => {
            if let Some(encrypted) = encrypted_content {
                value.insert("type".into(), json!("redacted_thinking"));
                value.insert("data".into(), json!(encrypted));
            } else {
                value.insert("type".into(), json!("thinking"));
                value.insert("thinking".into(), json!(text.clone().unwrap_or_default()));
                value.insert(
                    "signature".into(),
                    json!(signature.clone().unwrap_or_default()),
                );
            }
        }
        LLMContentKind::Refusal { refusal } => {
            value.insert("type".into(), json!("text"));
            value.insert("text".into(), json!(refusal));
        }
        LLMContentKind::Audio { .. } => {
            return Err(AdapterError::Unsupported(
                "Anthropic Messages cannot represent audio content".into(),
            ));
        }
        LLMContentKind::Raw {
            protocol: LLMProtocol::AnthropicMessages,
            value,
        } => return Ok(value.clone()),
        LLMContentKind::Raw {
            protocol,
            value: raw,
        } => {
            let item_type = raw["type"].as_str().unwrap_or("unknown");
            return Err(AdapterError::Unsupported(format!(
                "Anthropic Messages cannot represent {protocol:?} content block `{item_type}`"
            )));
        }
    }
    Ok(Value::Object(value))
}

fn anthropic_messages_to_value(
    messages: &[LLMMessage],
) -> Result<(Vec<Value>, Vec<Value>), AdapterError> {
    let mut system = Vec::new();
    let mut output: Vec<Value> = Vec::new();
    for message in messages {
        if matches!(message.role, LLMRole::System | LLMRole::Developer) {
            system.extend(
                message
                    .content
                    .iter()
                    .map(anthropic_content_to_value)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            continue;
        }
        let role = if message.role == LLMRole::Assistant {
            "assistant"
        } else {
            "user"
        };
        let content = message
            .content
            .iter()
            .map(anthropic_content_to_value)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(previous) = output.last_mut().filter(|value| value["role"] == role) {
            if let Some(previous_content) = previous["content"].as_array_mut() {
                previous_content.extend(content);
            }
        } else {
            output.push(json!({"role": role, "content": content}));
        }
    }
    Ok((system, output))
}

fn anthropic_source_from_value(value: &Value) -> LLMMediaSource {
    match value["type"].as_str().unwrap_or_default() {
        "base64" => LLMMediaSource::Data {
            data: value["data"].as_str().unwrap_or_default().to_string(),
            media_type: value["media_type"].as_str().map(ToString::to_string),
        },
        "url" => LLMMediaSource::Url {
            url: value["url"].as_str().unwrap_or_default().to_string(),
        },
        "file" => LLMMediaSource::FileId {
            file_id: value["file_id"].as_str().unwrap_or_default().to_string(),
        },
        "text" => LLMMediaSource::Data {
            data: value["data"].as_str().unwrap_or_default().to_string(),
            media_type: Some("text/plain".into()),
        },
        _ => LLMMediaSource::Raw {
            value: value.clone(),
        },
    }
}

fn anthropic_source_to_value(source: &LLMMediaSource) -> Value {
    match source {
        LLMMediaSource::Url { url } => {
            if let Some((media_type, data)) = parse_data_url(url) {
                json!({"type": "base64", "media_type": media_type, "data": data})
            } else {
                json!({"type": "url", "url": url})
            }
        }
        LLMMediaSource::Data { data, media_type } => json!({
            "type": "base64",
            "media_type": media_type.as_deref().unwrap_or("application/octet-stream"),
            "data": data
        }),
        LLMMediaSource::FileId { file_id } => json!({"type": "file", "file_id": file_id}),
        LLMMediaSource::Raw { value } => value.clone(),
    }
}

fn parse_anthropic_tool_choice(value: Option<&Value>) -> Option<LLMToolChoice> {
    let value = value?;
    match value["type"].as_str() {
        Some("auto") => Some(LLMToolChoice::Auto),
        Some("none") => Some(LLMToolChoice::None),
        Some("any") => Some(LLMToolChoice::Required),
        Some("tool") => value["name"].as_str().map(|name| LLMToolChoice::Function {
            name: name.to_string(),
        }),
        _ => Some(LLMToolChoice::Raw {
            value: value.clone(),
        }),
    }
}

fn anthropic_tool_choice_to_value(choice: &LLMToolChoice) -> Value {
    match choice {
        LLMToolChoice::Auto => json!({"type": "auto"}),
        LLMToolChoice::None => json!({"type": "none"}),
        LLMToolChoice::Required => json!({"type": "any"}),
        LLMToolChoice::Function { name } => json!({"type": "tool", "name": name}),
        LLMToolChoice::Raw { value } => value.clone(),
    }
}

pub(super) fn parse_anthropic_finish_reason(reason: &str) -> LLMFinishReason {
    match reason {
        "end_turn" => LLMFinishReason::EndTurn,
        "max_tokens" => LLMFinishReason::MaxTokens,
        "stop_sequence" => LLMFinishReason::StopSequence,
        "tool_use" => LLMFinishReason::ToolUse,
        "pause_turn" => LLMFinishReason::PauseTurn,
        "refusal" => LLMFinishReason::Refusal,
        "model_context_window_exceeded" => LLMFinishReason::ContextLimit,
        _ => LLMFinishReason::Unknown,
    }
}

pub(super) fn anthropic_finish_reason_to_value(reason: LLMFinishReason) -> Value {
    json!(match reason {
        LLMFinishReason::Length | LLMFinishReason::MaxTokens | LLMFinishReason::Incomplete => {
            "max_tokens"
        }
        LLMFinishReason::ToolCalls | LLMFinishReason::ToolUse | LLMFinishReason::FunctionCall => {
            "tool_use"
        }
        LLMFinishReason::StopSequence => "stop_sequence",
        LLMFinishReason::PauseTurn => "pause_turn",
        LLMFinishReason::Refusal | LLMFinishReason::ContentFilter => "refusal",
        LLMFinishReason::ContextLimit => "model_context_window_exceeded",
        _ => "end_turn",
    })
}

pub(super) fn anthropic_usage_from_value(value: &Value) -> Option<LLMUsage> {
    let object = value.as_object()?;
    let input = value["input_tokens"].as_u64();
    let output = value["output_tokens"].as_u64();
    Some(LLMUsage {
        source: Some(LLMProtocol::AnthropicMessages),
        input_tokens: input,
        output_tokens: output,
        total_tokens: input.zip(output).map(|(input, output)| input + output),
        cache_read_tokens: value["cache_read_input_tokens"].as_u64(),
        cache_creation_tokens: value["cache_creation_input_tokens"].as_u64(),
        reasoning_tokens: None,
        input_audio_tokens: None,
        output_audio_tokens: None,
        accepted_prediction_tokens: None,
        rejected_prediction_tokens: None,
        extra: object
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "input_tokens"
                        | "output_tokens"
                        | "cache_read_input_tokens"
                        | "cache_creation_input_tokens"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

pub(super) fn anthropic_usage_to_value(usage: &LLMUsage) -> Value {
    let mut value = if usage.source == Some(LLMProtocol::AnthropicMessages) {
        usage.extra.clone()
    } else {
        Extensions::new()
    };
    value.insert(
        "input_tokens".into(),
        json!(usage.input_tokens.unwrap_or(0)),
    );
    value.insert(
        "output_tokens".into(),
        json!(usage.output_tokens.unwrap_or(0)),
    );
    if let Some(tokens) = usage.cache_read_tokens {
        value.insert("cache_read_input_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.cache_creation_tokens {
        value.insert("cache_creation_input_tokens".into(), json!(tokens));
    }
    Value::Object(value)
}

fn anthropic_stream_content_start(content: &LLMContent) -> Result<Value, AdapterError> {
    Ok(match &content.kind {
        LLMContentKind::Text { .. } | LLMContentKind::Refusal { .. } => {
            json!({"type": "text", "text": ""})
        }
        LLMContentKind::ToolCall { id, name, .. } => json!({
            "type": "tool_use",
            "id": id.clone().unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4())),
            "name": name,
            "input": {}
        }),
        LLMContentKind::Reasoning {
            encrypted_content: Some(data),
            ..
        } => json!({"type": "redacted_thinking", "data": data}),
        LLMContentKind::Reasoning { .. } => {
            json!({"type": "thinking", "thinking": "", "signature": ""})
        }
        _ => anthropic_content_to_value(content)?,
    })
}

fn anthropic_content_kind(content: &LLMContent) -> &'static str {
    match content.kind {
        LLMContentKind::ToolCall { .. } => "tool_call",
        LLMContentKind::Reasoning { .. } => "reasoning",
        _ => "text",
    }
}

fn anthropic_stream_event(event: &str, value: Value) -> SseEvent {
    SseEvent::json(Some(event), &value)
}

fn ensure_message_started(events: &mut Vec<LLMStreamEvent>, state: &mut StreamState) {
    if !state.started {
        state.started = true;
        events.push(LLMStreamEvent::MessageStart {
            id: None,
            model: None,
            usage: None,
        });
    }
}

fn merge_usage(current: &mut Option<LLMUsage>, update: &LLMUsage) {
    let target = current.get_or_insert_with(LLMUsage::default);
    if update.source.is_some() {
        target.source = update.source;
    }
    if update.input_tokens.is_some() {
        target.input_tokens = update.input_tokens;
    }
    if update.output_tokens.is_some() {
        target.output_tokens = update.output_tokens;
    }
    if update.total_tokens.is_some() {
        target.total_tokens = update.total_tokens;
    }
    if update.cache_read_tokens.is_some() {
        target.cache_read_tokens = update.cache_read_tokens;
    }
    if update.cache_creation_tokens.is_some() {
        target.cache_creation_tokens = update.cache_creation_tokens;
    }
    if update.reasoning_tokens.is_some() {
        target.reasoning_tokens = update.reasoning_tokens;
    }
    if update.accepted_prediction_tokens.is_some() {
        target.accepted_prediction_tokens = update.accepted_prediction_tokens;
    }
    if update.rejected_prediction_tokens.is_some() {
        target.rejected_prediction_tokens = update.rejected_prediction_tokens;
    }
    target.extra.extend(update.extra.clone());
}

fn normalize_stop_sequences(value: &Value) -> Value {
    if value.is_array() {
        value.clone()
    } else if let Some(value) = value.as_str() {
        json!([value])
    } else {
        json!([])
    }
}

fn anthropic_request_metadata(request: &LLMRequest) -> Option<Value> {
    let user_id = request
        .metadata
        .get("user_id")
        .and_then(Value::as_str)
        .or_else(|| request.extra.get("user").and_then(Value::as_str))
        .or_else(|| {
            request
                .extra
                .get("safety_identifier")
                .and_then(Value::as_str)
        })?;
    Some(json!({"user_id": user_id}))
}

fn normalize_tool_arguments(value: &Value) -> Value {
    match value {
        Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| json!({"value": text}))
        }
        Value::Object(_) => value.clone(),
        _ => json!({"value": value}),
    }
}

fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let value = url.strip_prefix("data:")?;
    let (media_type, data) = value.split_once(";base64,")?;
    Some((media_type, data))
}

fn copy_selected(target: &mut Map<String, Value>, source: &Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = source.get(*field) {
            target.insert((*field).into(), value.clone());
        }
    }
}

fn insert_option<T: serde::Serialize>(
    target: &mut Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value {
        target.insert(key.into(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_request_preserves_tools_results_thinking_and_cache_control() {
        let request = AnthropicAdapter::to_llm_request(json!({
            "model": "claude-sonnet-4-5", "max_tokens": 1024,
            "system": [{"type": "text", "text": "be concise", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}]},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "check", "signature": "sig"}, {"type": "tool_use", "id": "toolu_1", "name": "weather", "input": {"city": "Paris"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"}]}
            ]
        })).unwrap();
        assert_eq!(
            request.messages[0].content[0].metadata["cache_control"]["type"],
            "ephemeral"
        );
        assert!(matches!(
            request.messages[1].content[0].kind,
            LLMContentKind::Image { .. }
        ));
        assert!(matches!(
            request.messages[2].content[0].kind,
            LLMContentKind::Reasoning { .. }
        ));
        assert!(matches!(
            request.messages[2].content[1].kind,
            LLMContentKind::ToolCall { .. }
        ));
        assert!(matches!(
            request.messages[3].content[0].kind,
            LLMContentKind::ToolResult { .. }
        ));
    }

    #[test]
    fn anthropic_usage_and_finish_reason_are_normalized() {
        let response = AnthropicAdapter::to_llm_response(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "max_tokens", "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 4}
        })).unwrap();
        assert_eq!(
            response.choices[0].finish_reason,
            Some(LLMFinishReason::MaxTokens)
        );
        assert_eq!(response.usage.as_ref().unwrap().cache_read_tokens, Some(4));
    }

    #[test]
    fn output_effort_survives_without_explicit_thinking_config() {
        let request = AnthropicAdapter::to_llm_request(json!({
            "model": "claude-opus",
            "max_tokens": 128,
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();

        assert_eq!(request.reasoning.as_ref().unwrap()["effort"], "high");
        let output = AnthropicAdapter::from_llm_request(&request).unwrap();
        assert_eq!(output["output_config"]["effort"], "high");
        assert!(output.get("thinking").is_none());
    }
}
