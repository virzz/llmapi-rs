use chrono::Utc;
use serde_json::{json, Map, Value};

use super::super::{
    adapters::{
        annotation_from_openai, annotation_to_openai, content_extensions_for, extensions_without,
        message_extensions_for, validate_request_extras, value_as_extensions, AdapterError,
        RequestAdapter, ResponseAdapter, SseEvent, StreamAdapter, StreamState,
    },
    model::{
        joined_text, Extensions, LLMChoice, LLMContent, LLMContentKind, LLMFinishReason,
        LLMMediaSource, LLMMessage, LLMProtocol, LLMRequest, LLMResponse, LLMRole, LLMStreamEvent,
        LLMTool, LLMToolChoice, LLMToolKind, LLMUsage,
    },
};

const CHAT_EXTRA_REQUEST_FIELDS: &[&str] = &[
    "audio",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "modalities",
    "n",
    "prediction",
    "presence_penalty",
    "prompt_cache_key",
    "prompt_cache_retention",
    "safety_identifier",
    "seed",
    "service_tier",
    "store",
    "stream_options",
    "top_logprobs",
    "user",
    "verbosity",
    "web_search_options",
];

pub struct OpenAiChatAdapter;

impl RequestAdapter for OpenAiChatAdapter {
    fn to_llm_request(value: Value) -> Result<LLMRequest, AdapterError> {
        let model = value["model"]
            .as_str()
            .ok_or(AdapterError::MissingField("model"))?
            .to_string();
        let messages = value["messages"]
            .as_array()
            .ok_or(AdapterError::MissingField("messages"))?
            .iter()
            .map(parse_chat_message)
            .collect::<Result<_, _>>()?;
        let reasoning = value.get("reasoning").cloned().or_else(|| {
            value
                .get("reasoning_effort")
                .cloned()
                .map(|effort| json!({"effort": effort}))
        });
        let mut tools = super::tools_from_openai_chat(&value["tools"])?;
        if let Some(options) = value
            .get("web_search_options")
            .filter(|value| value.is_object())
        {
            let mut config = options.clone();
            config["type"] = json!("web_search");
            tools.push(LLMTool {
                source: LLMProtocol::OpenAiChat,
                kind: LLMToolKind::Builtin {
                    protocol: LLMProtocol::OpenAiChat,
                    name: "web_search".into(),
                    config,
                },
                metadata: Extensions::new(),
            });
        }

        Ok(LLMRequest {
            source: LLMProtocol::OpenAiChat,
            model,
            messages,
            temperature: value["temperature"].as_f64(),
            max_tokens: value["max_completion_tokens"]
                .as_u64()
                .or_else(|| value["max_tokens"].as_u64()),
            top_p: value["top_p"].as_f64(),
            stop: value.get("stop").cloned(),
            stream: value["stream"].as_bool().unwrap_or(false),
            tools,
            tool_choice: parse_chat_tool_choice(value.get("tool_choice")),
            parallel_tool_calls: value["parallel_tool_calls"].as_bool(),
            response_format: value
                .get("response_format")
                .map(normalize_chat_response_format),
            reasoning,
            metadata: value_as_extensions(&value["metadata"]),
            extra: extensions_without(
                &value,
                &[
                    "model",
                    "messages",
                    "temperature",
                    "max_completion_tokens",
                    "max_tokens",
                    "top_p",
                    "stop",
                    "stream",
                    "tools",
                    "tool_choice",
                    "parallel_tool_calls",
                    "response_format",
                    "reasoning",
                    "reasoning_effort",
                    "metadata",
                    "web_search_options",
                ],
            ),
        })
    }

    fn from_llm_request(request: &LLMRequest) -> Result<Value, AdapterError> {
        validate_request_extras(
            request,
            LLMProtocol::OpenAiChat,
            CHAT_EXTRA_REQUEST_FIELDS,
            &[],
            &["include"],
        )?;
        let messages = request
            .messages
            .iter()
            .map(chat_message_to_value)
            .collect::<Result<Vec<_>, _>>()?;
        let mut value = Map::new();
        copy_selected(&mut value, &request.extra, CHAT_EXTRA_REQUEST_FIELDS);
        if request.source == LLMProtocol::AnthropicMessages
            && request.extra.get("service_tier").and_then(Value::as_str) == Some("standard_only")
        {
            value.insert("service_tier".into(), json!("default"));
        }
        value.insert("model".into(), json!(request.model));
        value.insert("messages".into(), Value::Array(messages));
        value.insert("stream".into(), json!(request.stream));
        insert_option(&mut value, "temperature", request.temperature);
        insert_option(&mut value, "max_tokens", request.max_tokens);
        insert_option(&mut value, "top_p", request.top_p);
        if let Some(stop) = &request.stop {
            value.insert("stop".into(), stop.clone());
        }
        let mut chat_tools = Vec::new();
        for tool in &request.tools {
            if let LLMToolKind::Builtin { name, config, .. } = &tool.kind {
                if !matches!(name.as_str(), "web_search" | "web_search_preview") {
                    chat_tools.push(tool.clone());
                    continue;
                }
                let mut options = Map::new();
                for field in ["search_context_size", "user_location"] {
                    if let Some(field_value) = config.get(field) {
                        options.insert(field.into(), field_value.clone());
                    }
                }
                value.insert("web_search_options".into(), Value::Object(options));
            } else {
                chat_tools.push(tool.clone());
            }
        }
        if !chat_tools.is_empty() {
            value.insert(
                "tools".into(),
                Value::Array(super::tools_to_openai_chat(&chat_tools)?),
            );
        }
        if let Some(choice) = &request.tool_choice {
            value.insert("tool_choice".into(), chat_tool_choice_to_value(choice));
        }
        if let Some(parallel) = request.parallel_tool_calls {
            value.insert("parallel_tool_calls".into(), json!(parallel));
        }
        if let Some(format) = &request.response_format {
            value.insert(
                "response_format".into(),
                chat_response_format_to_value(format),
            );
        }
        if let Some(reasoning) = &request.reasoning {
            if let Some(effort) = reasoning.get("effort") {
                value.insert("reasoning_effort".into(), effort.clone());
            } else if request.source != LLMProtocol::AnthropicMessages {
                value.insert("reasoning".into(), reasoning.clone());
            } else if reasoning["type"].as_str() != Some("disabled") {
                value.insert("reasoning_effort".into(), json!("high"));
            }
        }
        if !request.metadata.is_empty() {
            value.insert("metadata".into(), Value::Object(request.metadata.clone()));
        }
        Ok(Value::Object(value))
    }
}

impl ResponseAdapter for OpenAiChatAdapter {
    fn to_llm_response(value: Value) -> Result<LLMResponse, AdapterError> {
        let choices = value["choices"]
            .as_array()
            .ok_or(AdapterError::InvalidField("choices"))?
            .iter()
            .enumerate()
            .map(|(fallback_index, choice)| {
                let mut message = choice["message"].clone();
                if message["role"].is_null() {
                    message["role"] = json!("assistant");
                }
                let parsed = parse_chat_message(&message)?;
                let raw_reason = choice["finish_reason"].as_str();
                let mut metadata =
                    extensions_without(choice, &["index", "message", "finish_reason", "logprobs"]);
                if let Some(logprobs) = choice.get("logprobs") {
                    metadata.insert("logprobs".into(), logprobs.clone());
                }
                if raw_reason.is_some_and(|reason| {
                    parse_chat_finish_reason(reason) == LLMFinishReason::Unknown
                }) {
                    metadata.insert("raw_finish_reason".into(), json!(raw_reason));
                }
                Ok(LLMChoice {
                    index: choice["index"]
                        .as_u64()
                        .map_or(fallback_index, |index| index as usize),
                    role: parsed.role,
                    content: parsed.content,
                    finish_reason: raw_reason.map(parse_chat_finish_reason),
                    metadata,
                })
            })
            .collect::<Result<_, AdapterError>>()?;

        Ok(LLMResponse {
            source: LLMProtocol::OpenAiChat,
            id: value["id"].as_str().map(ToString::to_string),
            model: value["model"].as_str().map(ToString::to_string),
            choices,
            usage: value.get("usage").and_then(chat_usage_from_value),
            metadata: Extensions::new(),
            extra: extensions_without(&value, &["id", "object", "model", "choices", "usage"]),
        })
    }

    fn from_llm_response(response: &LLMResponse) -> Result<Value, AdapterError> {
        let choices = response
            .choices
            .iter()
            .map(|choice| {
                let message = LLMMessage {
                    source: Some(response.source),
                    role: choice.role,
                    content: choice.content.clone(),
                    name: None,
                    metadata: Extensions::new(),
                };
                let mut value = Map::new();
                value.insert("index".into(), json!(choice.index));
                value.insert("message".into(), chat_message_to_value(&message)?);
                value.insert(
                    "finish_reason".into(),
                    choice
                        .finish_reason
                        .map(chat_finish_reason_to_value)
                        .unwrap_or(Value::Null),
                );
                if let Some(logprobs) = choice.metadata.get("logprobs") {
                    value.insert("logprobs".into(), logprobs.clone());
                }
                Ok(Value::Object(value))
            })
            .collect::<Result<Vec<_>, AdapterError>>()?;
        let mut value = if response.source == LLMProtocol::OpenAiChat {
            response.extra.clone()
        } else {
            Extensions::new()
        };
        if let Some(service_tier) = response.extra.get("service_tier") {
            value.insert("service_tier".into(), service_tier.clone());
        }
        value.insert(
            "id".into(),
            json!(response
                .id
                .clone()
                .unwrap_or_else(|| format!("chatcmpl_{}", uuid::Uuid::new_v4()))),
        );
        value.insert("object".into(), json!("chat.completion"));
        value.insert("created".into(), json!(Utc::now().timestamp()));
        value.insert(
            "model".into(),
            json!(response.model.clone().unwrap_or_default()),
        );
        value.insert("choices".into(), Value::Array(choices));
        if let Some(usage) = &response.usage {
            value.insert("usage".into(), chat_usage_to_value(usage));
        }
        Ok(Value::Object(value))
    }
}

impl StreamAdapter for OpenAiChatAdapter {
    fn parse_stream_event(
        event: &SseEvent,
        state: &mut StreamState,
    ) -> Result<Vec<LLMStreamEvent>, AdapterError> {
        if event.data.trim() == "[DONE]" {
            if state.ended {
                return Ok(Vec::new());
            }
            state.ended = true;
            let mut events = state
                .open_content
                .iter()
                .copied()
                .map(|index| LLMStreamEvent::ContentEnd { index })
                .collect::<Vec<_>>();
            state.open_content.clear();
            events.push(LLMStreamEvent::MessageEnd {
                finish_reason: state.finish_reason,
                usage: state.usage.clone(),
                metadata: Extensions::new(),
            });
            return Ok(events);
        }

        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| AdapterError::InvalidField("stream_json"))?;
        if let Some(error) = value.get("error") {
            return Ok(vec![LLMStreamEvent::Error {
                message: error["message"]
                    .as_str()
                    .unwrap_or("upstream stream error")
                    .to_string(),
                metadata: value_as_extensions(error),
            }]);
        }

        let mut events = Vec::new();
        if !state.started {
            state.id = value["id"].as_str().map(ToString::to_string);
            state.model = value["model"].as_str().map(ToString::to_string);
            state.started = true;
            events.push(LLMStreamEvent::MessageStart {
                id: state.id.clone(),
                model: state.model.clone(),
                usage: None,
            });
        }

        if let Some(usage) = value.get("usage").and_then(chat_usage_from_value) {
            state.usage = Some(usage.clone());
            events.push(LLMStreamEvent::Usage { usage });
        }
        for choice in value["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str() {
                ensure_content_started(&mut events, state, 0, LLMContent::text(""));
                events.push(LLMStreamEvent::TextDelta {
                    index: 0,
                    text: text.to_string(),
                });
            }
            if let Some(text) = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str())
            {
                ensure_content_started(&mut events, state, 1, reasoning_content());
                events.push(LLMStreamEvent::ReasoningDelta {
                    index: 1,
                    text: Some(text.to_string()),
                    signature: None,
                });
            }
            if let Some(refusal) = delta["refusal"].as_str() {
                ensure_content_started(&mut events, state, 2, refusal_content());
                events.push(LLMStreamEvent::RefusalDelta {
                    index: 2,
                    refusal: refusal.to_string(),
                });
            }
            for annotation in delta["annotations"].as_array().into_iter().flatten() {
                events.push(LLMStreamEvent::AnnotationAdded {
                    index: 0,
                    annotation: annotation_from_openai(annotation, LLMProtocol::OpenAiChat),
                });
            }
            for tool in delta["tool_calls"].as_array().into_iter().flatten() {
                let source_tool_index = tool["index"].as_u64().unwrap_or(0) as usize;
                let content_index = source_tool_index + 3;
                ensure_content_started(
                    &mut events,
                    state,
                    content_index,
                    LLMContent {
                        source: Some(LLMProtocol::OpenAiChat),
                        kind: LLMContentKind::ToolCall {
                            id: tool["id"].as_str().map(ToString::to_string),
                            name: tool["function"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            arguments: json!({}),
                        },
                        metadata: Extensions::new(),
                    },
                );
                events.push(LLMStreamEvent::ToolCallDelta {
                    index: content_index,
                    id: tool["id"].as_str().map(ToString::to_string),
                    name: tool["function"]["name"].as_str().map(ToString::to_string),
                    arguments_delta: tool["function"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            if let Some(function) = delta.get("function_call").filter(|value| value.is_object()) {
                let content_index = 3;
                ensure_content_started(
                    &mut events,
                    state,
                    content_index,
                    LLMContent {
                        source: Some(LLMProtocol::OpenAiChat),
                        kind: LLMContentKind::ToolCall {
                            id: None,
                            name: function["name"].as_str().unwrap_or_default().to_string(),
                            arguments: json!({}),
                        },
                        metadata: {
                            let mut metadata = Extensions::new();
                            metadata.insert("legacy_function_call".into(), json!(true));
                            metadata
                        },
                    },
                );
                events.push(LLMStreamEvent::ToolCallDelta {
                    index: content_index,
                    id: None,
                    name: function["name"].as_str().map(ToString::to_string),
                    arguments_delta: function["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            if delta.as_object().is_some_and(|object| {
                object.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "role"
                            | "content"
                            | "reasoning_content"
                            | "reasoning"
                            | "refusal"
                            | "annotations"
                            | "tool_calls"
                            | "function_call"
                    )
                })
            }) {
                events.push(LLMStreamEvent::Raw {
                    protocol: LLMProtocol::OpenAiChat,
                    value: value.clone(),
                });
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                state.finish_reason = Some(parse_chat_finish_reason(reason));
            }
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
                LLMStreamEvent::MessageStart { id, model, .. } => {
                    state.id = id
                        .clone()
                        .or_else(|| Some(format!("chatcmpl_{}", uuid::Uuid::new_v4())));
                    state.model = model.clone();
                    state.started = true;
                    output.push(chat_stream_event(
                        state,
                        json!({"role": "assistant", "content": ""}),
                        None,
                        None,
                    ));
                }
                LLMStreamEvent::ContentStart { index, content } => {
                    let target_index = state.target_content_index(*index);
                    state.open_content.insert(target_index);
                    if let LLMContentKind::ToolCall { id, name, .. } = &content.kind {
                        output.push(chat_stream_event(
                            state,
                            json!({"tool_calls": [{"index": target_index, "id": id, "type": "function", "function": {"name": name, "arguments": ""}}]}),
                            None,
                            None,
                        ));
                    }
                }
                LLMStreamEvent::TextDelta { text, .. } => output.push(chat_stream_event(
                    state,
                    json!({"content": text}),
                    None,
                    None,
                )),
                LLMStreamEvent::ReasoningDelta {
                    text, signature, ..
                } => {
                    let mut delta = Map::new();
                    if let Some(text) = text {
                        delta.insert("reasoning_content".into(), json!(text));
                    }
                    if let Some(signature) = signature {
                        delta.insert("reasoning_signature".into(), json!(signature));
                    }
                    output.push(chat_stream_event(state, Value::Object(delta), None, None));
                }
                LLMStreamEvent::RefusalDelta { refusal, .. } => output.push(chat_stream_event(
                    state,
                    json!({"refusal": refusal}),
                    None,
                    None,
                )),
                LLMStreamEvent::AnnotationAdded { annotation, .. } => {
                    let annotation = annotation_to_openai(annotation, LLMProtocol::OpenAiChat)?;
                    output.push(chat_stream_event(
                        state,
                        json!({"annotations": [annotation]}),
                        None,
                        None,
                    ));
                }
                LLMStreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    let target_index = state.target_content_index(*index);
                    output.push(chat_stream_event(
                        state,
                        json!({"tool_calls": [{"index": target_index, "id": id, "type": "function", "function": {"name": name, "arguments": arguments_delta}}]}),
                        None,
                        None,
                    ));
                }
                LLMStreamEvent::ContentEnd { index } => {
                    if let Some(target) = state.content_indices.get(index) {
                        state.open_content.remove(target);
                    }
                }
                LLMStreamEvent::Usage { usage } => state.usage = Some(usage.clone()),
                LLMStreamEvent::MessageEnd {
                    finish_reason,
                    usage,
                    ..
                } => {
                    if let Some(usage) = usage {
                        state.usage = Some(usage.clone());
                    }
                    output.push(chat_stream_event(
                        state,
                        json!({}),
                        finish_reason.map(chat_finish_reason_to_value),
                        None,
                    ));
                    if let Some(usage) = &state.usage {
                        output.push(chat_stream_event(
                            state,
                            json!({}),
                            None,
                            Some(chat_usage_to_value(usage)),
                        ));
                    }
                    output.push(SseEvent {
                        data: "[DONE]".into(),
                        ..SseEvent::default()
                    });
                    state.ended = true;
                }
                LLMStreamEvent::Error { message, metadata } => {
                    let mut error = metadata.clone();
                    error.insert("message".into(), json!(message));
                    output.push(SseEvent::json(None, &json!({"error": error})));
                }
                LLMStreamEvent::Raw { protocol, value } => {
                    if *protocol != LLMProtocol::OpenAiChat {
                        return Err(AdapterError::Unsupported(format!(
                            "OpenAI Chat cannot represent a {protocol:?} stream event"
                        )));
                    }
                    output.push(SseEvent::json(None, value));
                }
            }
        }
        Ok(output)
    }
}

fn parse_chat_message(value: &Value) -> Result<LLMMessage, AdapterError> {
    let role = parse_role(
        value["role"]
            .as_str()
            .ok_or(AdapterError::MissingField("role"))?,
    )?;
    let mut content = parse_chat_content(&value["content"]);
    let message_annotations = value["annotations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|annotation| annotation_from_openai(annotation, LLMProtocol::OpenAiChat))
        .collect::<Vec<_>>();
    if !message_annotations.is_empty() {
        if let Some(LLMContent {
            kind: LLMContentKind::Text { annotations, .. },
            ..
        }) = content
            .iter_mut()
            .find(|content| matches!(content.kind, LLMContentKind::Text { .. }))
        {
            annotations.extend(message_annotations);
        }
    }
    if let Some(reasoning) = value["reasoning_content"]
        .as_str()
        .or_else(|| value["reasoning"].as_str())
    {
        content.push(LLMContent {
            source: Some(LLMProtocol::OpenAiChat),
            kind: LLMContentKind::Reasoning {
                text: Some(reasoning.to_string()),
                summary: Vec::new(),
                signature: value["reasoning_signature"]
                    .as_str()
                    .map(ToString::to_string),
                encrypted_content: None,
            },
            metadata: Extensions::new(),
        });
    }
    if let Some(refusal) = value["refusal"].as_str() {
        content.push(LLMContent {
            source: Some(LLMProtocol::OpenAiChat),
            kind: LLMContentKind::Refusal {
                refusal: refusal.to_string(),
            },
            metadata: Extensions::new(),
        });
    }
    if let Some(audio) = value.get("audio").filter(|audio| audio.is_object()) {
        let source = if let Some(data) = audio["data"].as_str() {
            LLMMediaSource::Data {
                data: data.to_string(),
                media_type: audio["format"].as_str().map(ToString::to_string),
            }
        } else if let Some(id) = audio["id"].as_str() {
            LLMMediaSource::FileId {
                file_id: id.to_string(),
            }
        } else {
            LLMMediaSource::Raw {
                value: audio.clone(),
            }
        };
        let mut metadata = extensions_without(audio, &["id", "data", "format"]);
        metadata.insert("chat_audio_response".into(), json!(true));
        content.push(LLMContent {
            source: Some(LLMProtocol::OpenAiChat),
            kind: LLMContentKind::Audio {
                source,
                format: audio["format"].as_str().map(ToString::to_string),
            },
            metadata,
        });
    }
    for tool_call in value["tool_calls"].as_array().into_iter().flatten() {
        let function = &tool_call["function"];
        content.push(LLMContent {
            source: Some(LLMProtocol::OpenAiChat),
            kind: LLMContentKind::ToolCall {
                id: tool_call["id"].as_str().map(ToString::to_string),
                name: function["name"]
                    .as_str()
                    .ok_or(AdapterError::MissingField("tool_calls.function.name"))?
                    .to_string(),
                arguments: parse_arguments(function["arguments"].as_str().unwrap_or("{}")),
            },
            metadata: extensions_without(tool_call, &["id", "type", "function", "index"]),
        });
    }
    if let Some(function) = value.get("function_call").filter(|value| value.is_object()) {
        let mut metadata = Extensions::new();
        metadata.insert("legacy_function_call".into(), json!(true));
        content.push(LLMContent {
            source: Some(LLMProtocol::OpenAiChat),
            kind: LLMContentKind::ToolCall {
                id: None,
                name: function["name"]
                    .as_str()
                    .ok_or(AdapterError::MissingField("function_call.name"))?
                    .to_string(),
                arguments: parse_arguments(function["arguments"].as_str().unwrap_or("{}")),
            },
            metadata,
        });
    }
    if role == LLMRole::Tool {
        let tool_call_id = value["tool_call_id"]
            .as_str()
            .or_else(|| value["name"].as_str())
            .ok_or(AdapterError::MissingField("tool_call_id"))?
            .to_string();
        let mut result_metadata = Extensions::new();
        if value["tool_call_id"].is_null() {
            result_metadata.insert("legacy_function_result".into(), json!(true));
        }
        content = vec![LLMContent {
            source: Some(LLMProtocol::OpenAiChat),
            kind: LLMContentKind::ToolResult {
                tool_call_id,
                content,
                is_error: None,
            },
            metadata: result_metadata,
        }];
    }
    Ok(LLMMessage {
        source: Some(LLMProtocol::OpenAiChat),
        role,
        content,
        name: value["name"].as_str().map(ToString::to_string),
        metadata: extensions_without(
            value,
            &[
                "role",
                "content",
                "name",
                "tool_call_id",
                "tool_calls",
                "function_call",
                "refusal",
                "reasoning",
                "reasoning_content",
                "reasoning_signature",
                "annotations",
                "audio",
            ],
        ),
    })
}

fn chat_message_to_value(message: &LLMMessage) -> Result<Value, AdapterError> {
    if message.role == LLMRole::Tool {
        let result = message
            .content
            .iter()
            .find_map(|content| match &content.kind {
                LLMContentKind::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => Some((tool_call_id, content)),
                _ => None,
            })
            .ok_or(AdapterError::InvalidField("tool_message.content"))?;
        let mut value = message_extensions_for(message, LLMProtocol::OpenAiChat);
        value.insert("role".into(), json!("tool"));
        value.insert("tool_call_id".into(), json!(result.0));
        value.insert("content".into(), chat_visible_content(result.1)?);
        if let Some(name) = &message.name {
            value.insert("name".into(), json!(name));
        }
        return Ok(Value::Object(value));
    }

    let mut value = message_extensions_for(message, LLMProtocol::OpenAiChat);
    value.insert("role".into(), json!(role_name(message.role)));
    if let Some(name) = &message.name {
        value.insert("name".into(), json!(name));
    }
    let visible = message
        .content
        .iter()
        .filter(|content| match content.kind {
            LLMContentKind::Audio { .. } => {
                content.metadata.get("chat_audio_response") != Some(&json!(true))
            }
            LLMContentKind::Text { .. }
            | LLMContentKind::Image { .. }
            | LLMContentKind::File { .. }
            | LLMContentKind::Raw { .. } => true,
            _ => false,
        })
        .cloned()
        .collect::<Vec<_>>();
    value.insert(
        "content".into(),
        if visible.is_empty() {
            Value::Null
        } else {
            chat_visible_content(&visible)?
        },
    );

    let tool_calls = message
        .content
        .iter()
        .filter_map(|content| match &content.kind {
            LLMContentKind::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "id": id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                "type": "function",
                "function": {"name": name, "arguments": arguments_to_string(arguments)}
            })),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !tool_calls.is_empty() {
        value.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    let reasoning = message
        .content
        .iter()
        .filter_map(|content| match &content.kind {
            LLMContentKind::Reasoning { text, summary, .. } => text
                .clone()
                .or_else(|| (!summary.is_empty()).then(|| summary.join("\n"))),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if !reasoning.is_empty() {
        value.insert("reasoning_content".into(), json!(reasoning));
    }
    let refusal = message
        .content
        .iter()
        .filter_map(|content| match &content.kind {
            LLMContentKind::Refusal { refusal } => Some(refusal.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if !refusal.is_empty() {
        value.insert("refusal".into(), json!(refusal));
    }
    let annotations = message
        .content
        .iter()
        .filter_map(|content| match &content.kind {
            LLMContentKind::Text { annotations, .. } => Some(annotations),
            _ => None,
        })
        .flatten()
        .map(|annotation| annotation_to_openai(annotation, LLMProtocol::OpenAiChat))
        .collect::<Result<Vec<_>, _>>()?;
    if !annotations.is_empty() {
        value.insert("annotations".into(), Value::Array(annotations));
    }
    if let Some(audio) = message.content.iter().find(|content| {
        matches!(content.kind, LLMContentKind::Audio { .. })
            && content.metadata.get("chat_audio_response") == Some(&json!(true))
    }) {
        let LLMContentKind::Audio { source, format } = &audio.kind else {
            unreachable!()
        };
        let mut audio_value = content_extensions_for(audio, LLMProtocol::OpenAiChat);
        audio_value.remove("chat_audio_response");
        match source {
            LLMMediaSource::FileId { file_id } => {
                audio_value.insert("id".into(), json!(file_id));
            }
            LLMMediaSource::Data { data, .. } => {
                audio_value.insert("data".into(), json!(data));
            }
            _ => {
                audio_value.insert("data".into(), json!(media_source_to_url(source)));
            }
        }
        if let Some(format) = format {
            audio_value.insert("format".into(), json!(format));
        }
        value.insert("audio".into(), Value::Object(audio_value));
    }
    Ok(Value::Object(value))
}

fn parse_chat_content(value: &Value) -> Vec<LLMContent> {
    if let Some(text) = value.as_str() {
        return vec![LLMContent::text(text)];
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|part| {
            let metadata = extensions_without(
                part,
                &[
                    "type",
                    "text",
                    "image_url",
                    "input_audio",
                    "file",
                    "refusal",
                    "annotations",
                ],
            );
            let kind = match part["type"].as_str().unwrap_or("text") {
                "text" | "input_text" | "output_text" => LLMContentKind::Text {
                    text: part["text"].as_str().unwrap_or_default().to_string(),
                    annotations: part["annotations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|annotation| {
                            annotation_from_openai(annotation, LLMProtocol::OpenAiChat)
                        })
                        .collect(),
                },
                "image_url" => {
                    let image = &part["image_url"];
                    let url = image
                        .as_str()
                        .or_else(|| image["url"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    LLMContentKind::Image {
                        source: LLMMediaSource::Url { url },
                        detail: image["detail"].as_str().map(ToString::to_string),
                    }
                }
                "input_audio" => LLMContentKind::Audio {
                    source: LLMMediaSource::Data {
                        data: part["input_audio"]["data"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        media_type: None,
                    },
                    format: part["input_audio"]["format"]
                        .as_str()
                        .map(ToString::to_string),
                },
                "file" => parse_file_source(&part["file"]),
                "refusal" => LLMContentKind::Refusal {
                    refusal: part["refusal"].as_str().unwrap_or_default().to_string(),
                },
                _ => LLMContentKind::Raw {
                    protocol: LLMProtocol::OpenAiChat,
                    value: part.clone(),
                },
            };
            LLMContent {
                source: Some(LLMProtocol::OpenAiChat),
                kind,
                metadata,
            }
        })
        .collect()
}

fn chat_visible_content(content: &[LLMContent]) -> Result<Value, AdapterError> {
    if content
        .iter()
        .all(|part| matches!(part.kind, LLMContentKind::Text { .. }))
    {
        return Ok(json!(joined_text(content)));
    }
    Ok(Value::Array(
        content
            .iter()
            .map(chat_content_to_value)
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn chat_content_to_value(content: &LLMContent) -> Result<Value, AdapterError> {
    let mut value = content_extensions_for(content, LLMProtocol::OpenAiChat);
    match &content.kind {
        LLMContentKind::Text { text, .. } => {
            value.insert("type".into(), json!("text"));
            value.insert("text".into(), json!(text));
        }
        LLMContentKind::Image { source, detail } => {
            value.insert("type".into(), json!("image_url"));
            let mut image = Map::new();
            image.insert("url".into(), json!(media_source_to_url(source)));
            if let Some(detail) = detail {
                image.insert("detail".into(), json!(detail));
            }
            value.insert("image_url".into(), Value::Object(image));
        }
        LLMContentKind::Audio { source, format } => {
            let LLMMediaSource::Data { data, .. } = source else {
                return Ok(
                    json!({"type": "input_audio", "input_audio": {"data": media_source_to_url(source), "format": format}}),
                );
            };
            value.insert("type".into(), json!("input_audio"));
            value.insert(
                "input_audio".into(),
                json!({"data": data, "format": format}),
            );
        }
        LLMContentKind::File {
            source, filename, ..
        } => {
            value.insert("type".into(), json!("file"));
            let mut file = Map::new();
            match source {
                LLMMediaSource::FileId { file_id } => {
                    file.insert("file_id".into(), json!(file_id));
                }
                LLMMediaSource::Data { data, .. } => {
                    file.insert("file_data".into(), json!(data));
                }
                _ => {
                    file.insert("file_data".into(), json!(media_source_to_url(source)));
                }
            }
            if let Some(filename) = filename {
                file.insert("filename".into(), json!(filename));
            }
            value.insert("file".into(), Value::Object(file));
        }
        LLMContentKind::Raw {
            protocol: LLMProtocol::OpenAiChat,
            value: raw,
        } => return Ok(raw.clone()),
        LLMContentKind::Raw {
            protocol,
            value: raw,
        } => {
            let item_type = raw["type"].as_str().unwrap_or("unknown");
            return Err(AdapterError::Unsupported(format!(
                "OpenAI Chat cannot represent {protocol:?} content block `{item_type}`"
            )));
        }
        _ => return Err(AdapterError::InvalidField("chat.content")),
    }
    Ok(Value::Object(value))
}

fn parse_file_source(value: &Value) -> LLMContentKind {
    let source = if let Some(file_id) = value["file_id"].as_str() {
        LLMMediaSource::FileId {
            file_id: file_id.to_string(),
        }
    } else if let Some(data) = value["file_data"].as_str() {
        LLMMediaSource::Data {
            data: data.to_string(),
            media_type: None,
        }
    } else {
        LLMMediaSource::Raw {
            value: value.clone(),
        }
    };
    LLMContentKind::File {
        source,
        filename: value["filename"].as_str().map(ToString::to_string),
        media_type: None,
    }
}

fn parse_role(role: &str) -> Result<LLMRole, AdapterError> {
    match role {
        "system" => Ok(LLMRole::System),
        "developer" => Ok(LLMRole::Developer),
        "user" => Ok(LLMRole::User),
        "assistant" => Ok(LLMRole::Assistant),
        "tool" | "function" => Ok(LLMRole::Tool),
        _ => Err(AdapterError::InvalidField("role")),
    }
}

pub(super) fn role_name(role: LLMRole) -> &'static str {
    match role {
        LLMRole::System => "system",
        LLMRole::Developer => "developer",
        LLMRole::User => "user",
        LLMRole::Assistant => "assistant",
        LLMRole::Tool => "tool",
    }
}

pub(super) fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

pub(super) fn arguments_to_string(arguments: &Value) -> String {
    match arguments {
        Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

fn parse_chat_tool_choice(value: Option<&Value>) -> Option<LLMToolChoice> {
    let value = value?;
    match value.as_str() {
        Some("auto") => Some(LLMToolChoice::Auto),
        Some("none") => Some(LLMToolChoice::None),
        Some("required") => Some(LLMToolChoice::Required),
        Some(_) => Some(LLMToolChoice::Raw {
            value: value.clone(),
        }),
        None if value["type"].as_str() == Some("function") => value["function"]["name"]
            .as_str()
            .map(|name| LLMToolChoice::Function {
                name: name.to_string(),
            }),
        None => Some(LLMToolChoice::Raw {
            value: value.clone(),
        }),
    }
}

fn normalize_chat_response_format(value: &Value) -> Value {
    if value["type"].as_str() != Some("json_schema") {
        return value.clone();
    }
    let mut format = value["json_schema"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    format.insert("type".into(), json!("json_schema"));
    Value::Object(format)
}

fn chat_response_format_to_value(value: &Value) -> Value {
    if value["type"].as_str() != Some("json_schema") {
        return value.clone();
    }
    let mut schema = value.clone();
    schema.as_object_mut().map(|object| object.remove("type"));
    json!({"type": "json_schema", "json_schema": schema})
}

fn chat_tool_choice_to_value(choice: &LLMToolChoice) -> Value {
    match choice {
        LLMToolChoice::Auto => json!("auto"),
        LLMToolChoice::None => json!("none"),
        LLMToolChoice::Required => json!("required"),
        LLMToolChoice::Function { name } => json!({"type": "function", "function": {"name": name}}),
        LLMToolChoice::Raw { value } => value.clone(),
    }
}

pub(super) fn parse_chat_finish_reason(reason: &str) -> LLMFinishReason {
    match reason {
        "stop" => LLMFinishReason::Stop,
        "length" => LLMFinishReason::Length,
        "tool_calls" => LLMFinishReason::ToolCalls,
        "content_filter" => LLMFinishReason::ContentFilter,
        "function_call" => LLMFinishReason::FunctionCall,
        _ => LLMFinishReason::Unknown,
    }
}

pub(super) fn chat_finish_reason_to_value(reason: LLMFinishReason) -> Value {
    json!(match reason {
        LLMFinishReason::Length
        | LLMFinishReason::MaxTokens
        | LLMFinishReason::ContextLimit
        | LLMFinishReason::Incomplete => "length",
        LLMFinishReason::ToolCalls | LLMFinishReason::ToolUse | LLMFinishReason::FunctionCall =>
            "tool_calls",
        LLMFinishReason::ContentFilter | LLMFinishReason::Refusal => "content_filter",
        _ => "stop",
    })
}

pub(super) fn chat_usage_from_value(value: &Value) -> Option<LLMUsage> {
    let object = value.as_object()?;
    let prompt_details = &value["prompt_tokens_details"];
    let completion_details = &value["completion_tokens_details"];
    Some(LLMUsage {
        source: Some(LLMProtocol::OpenAiChat),
        input_tokens: value["prompt_tokens"].as_u64(),
        output_tokens: value["completion_tokens"].as_u64(),
        total_tokens: value["total_tokens"].as_u64(),
        cache_read_tokens: prompt_details["cached_tokens"].as_u64(),
        cache_creation_tokens: None,
        reasoning_tokens: completion_details["reasoning_tokens"].as_u64(),
        input_audio_tokens: prompt_details["audio_tokens"].as_u64(),
        output_audio_tokens: completion_details["audio_tokens"].as_u64(),
        accepted_prediction_tokens: completion_details["accepted_prediction_tokens"].as_u64(),
        rejected_prediction_tokens: completion_details["rejected_prediction_tokens"].as_u64(),
        extra: object
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "prompt_tokens"
                        | "completion_tokens"
                        | "total_tokens"
                        | "prompt_tokens_details"
                        | "completion_tokens_details"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

pub(super) fn chat_usage_to_value(usage: &LLMUsage) -> Value {
    let mut value = if usage.source == Some(LLMProtocol::OpenAiChat) {
        usage.extra.clone()
    } else {
        Extensions::new()
    };
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    value.insert("prompt_tokens".into(), json!(input));
    value.insert("completion_tokens".into(), json!(output));
    value.insert(
        "total_tokens".into(),
        json!(usage.total_tokens.unwrap_or(input + output)),
    );
    let mut prompt = Map::new();
    if let Some(tokens) = usage.cache_read_tokens {
        prompt.insert("cached_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.input_audio_tokens {
        prompt.insert("audio_tokens".into(), json!(tokens));
    }
    if !prompt.is_empty() {
        value.insert("prompt_tokens_details".into(), Value::Object(prompt));
    }
    let mut completion = Map::new();
    if let Some(tokens) = usage.reasoning_tokens {
        completion.insert("reasoning_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.output_audio_tokens {
        completion.insert("audio_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.accepted_prediction_tokens {
        completion.insert("accepted_prediction_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.rejected_prediction_tokens {
        completion.insert("rejected_prediction_tokens".into(), json!(tokens));
    }
    if !completion.is_empty() {
        value.insert(
            "completion_tokens_details".into(),
            Value::Object(completion),
        );
    }
    Value::Object(value)
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
fn media_source_to_url(source: &LLMMediaSource) -> String {
    match source {
        LLMMediaSource::Url { url } => url.clone(),
        LLMMediaSource::Data { data, media_type } => format!(
            "data:{};base64,{data}",
            media_type.as_deref().unwrap_or("application/octet-stream")
        ),
        LLMMediaSource::FileId { file_id } => file_id.clone(),
        LLMMediaSource::Raw { value } => value.as_str().unwrap_or_default().to_string(),
    }
}
fn reasoning_content() -> LLMContent {
    LLMContent {
        source: Some(LLMProtocol::OpenAiChat),
        kind: LLMContentKind::Reasoning {
            text: None,
            summary: Vec::new(),
            signature: None,
            encrypted_content: None,
        },
        metadata: Extensions::new(),
    }
}
fn refusal_content() -> LLMContent {
    LLMContent {
        source: Some(LLMProtocol::OpenAiChat),
        kind: LLMContentKind::Refusal {
            refusal: String::new(),
        },
        metadata: Extensions::new(),
    }
}
fn ensure_content_started(
    events: &mut Vec<LLMStreamEvent>,
    state: &mut StreamState,
    index: usize,
    content: LLMContent,
) {
    if state.open_content.insert(index) {
        events.push(LLMStreamEvent::ContentStart { index, content });
    }
}

fn chat_stream_event(
    state: &StreamState,
    delta: Value,
    finish_reason: Option<Value>,
    usage: Option<Value>,
) -> SseEvent {
    let choices = if usage.is_some()
        && delta.as_object().is_some_and(Map::is_empty)
        && finish_reason.is_none()
    {
        Vec::new()
    } else {
        vec![json!({"index": 0, "delta": delta, "finish_reason": finish_reason})]
    };
    let mut value = json!({"id": state.id, "object": "chat.completion.chunk", "created": Utc::now().timestamp(), "model": state.model, "choices": choices});
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    SseEvent::json(None, &value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{anthropic::AnthropicAdapter, openai_responses::OpenAiResponsesAdapter};

    #[test]
    fn chat_request_preserves_tool_calls_results_and_images() {
        let request = OpenAiChatAdapter::to_llm_request(json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image_url", "image_url": {"url": "https://example.test/a.png", "detail": "high"}}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "tools": [{"type": "function", "function": {"name": "weather", "parameters": {"type": "object"}}}],
            "tool_choice": {"type": "function", "function": {"name": "weather"}}
        })).unwrap();

        assert!(matches!(
            request.messages[0].content[1].kind,
            LLMContentKind::Image { .. }
        ));
        assert!(matches!(
            request.messages[1].content[0].kind,
            LLMContentKind::ToolCall { .. }
        ));
        assert!(matches!(
            request.messages[2].content[0].kind,
            LLMContentKind::ToolResult { .. }
        ));
        let round_trip = OpenAiChatAdapter::from_llm_request(&request).unwrap();
        assert_eq!(
            round_trip["messages"][1]["tool_calls"][0]["function"]["name"],
            "weather"
        );
        assert_eq!(round_trip["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn chat_usage_and_finish_reason_are_normalized() {
        let response = OpenAiChatAdapter::to_llm_response(json!({
            "id": "chatcmpl_1", "model": "gpt", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14, "prompt_tokens_details": {"cached_tokens": 3}, "completion_tokens_details": {"reasoning_tokens": 2}}
        })).unwrap();
        assert_eq!(
            response.choices[0].finish_reason,
            Some(LLMFinishReason::ToolCalls)
        );
        assert_eq!(response.usage.as_ref().unwrap().cache_read_tokens, Some(3));
        assert_eq!(response.usage.as_ref().unwrap().reasoning_tokens, Some(2));
    }

    #[test]
    fn chat_audio_response_round_trips_without_becoming_input_audio() {
        let response = OpenAiChatAdapter::to_llm_response(json!({
            "id": "chatcmpl_audio",
            "model": "gpt-audio",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "audio": {"id": "audio_1", "expires_at": 123, "transcript": "hello"}
                },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        assert!(matches!(
            response.choices[0].content[0].kind,
            LLMContentKind::Audio { .. }
        ));

        let output = OpenAiChatAdapter::from_llm_response(&response).unwrap();
        assert_eq!(output["choices"][0]["message"]["audio"]["id"], "audio_1");
        assert_eq!(
            output["choices"][0]["message"]["audio"]["transcript"],
            "hello"
        );
        assert!(output["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn chat_audio_stream_fails_explicitly_for_incompatible_targets() {
        let source = SseEvent::json(
            None,
            &json!({
                "id": "chatcmpl_audio",
                "model": "gpt-audio",
                "choices": [{
                    "index": 0,
                    "delta": {"audio": {"id": "audio_1", "data": "AAAA"}},
                    "finish_reason": null
                }]
            }),
        );
        let normalized =
            OpenAiChatAdapter::parse_stream_event(&source, &mut StreamState::default()).unwrap();
        assert!(normalized
            .iter()
            .any(|event| matches!(event, LLMStreamEvent::Raw { .. })));

        assert!(matches!(
            AnthropicAdapter::format_stream_events(&normalized, &mut StreamState::default()),
            Err(AdapterError::Unsupported(_))
        ));
        assert!(matches!(
            OpenAiResponsesAdapter::format_stream_events(&normalized, &mut StreamState::default()),
            Err(AdapterError::Unsupported(_))
        ));
    }
}
