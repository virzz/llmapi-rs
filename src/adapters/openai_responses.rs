use chrono::Utc;
use serde_json::{json, Map, Value};

use super::super::{
    adapters::{
        annotation_from_openai, annotation_to_openai, content_extensions_for, extensions_without,
        validate_request_extras, value_as_extensions, AdapterError, RequestAdapter,
        ResponseAdapter, SseEvent, StreamAdapter, StreamState,
    },
    model::{
        joined_text, Extensions, LLMChoice, LLMContent, LLMContentKind, LLMFinishReason,
        LLMMediaSource, LLMMessage, LLMProtocol, LLMRequest, LLMResponse, LLMRole, LLMStreamEvent,
        LLMToolChoice, LLMUsage,
    },
};

const RESPONSES_EXTRA_REQUEST_FIELDS: &[&str] = &[
    "background",
    "conversation",
    "include",
    "max_tool_calls",
    "previous_response_id",
    "prompt",
    "prompt_cache_key",
    "prompt_cache_retention",
    "safety_identifier",
    "service_tier",
    "store",
    "stream_options",
    "top_logprobs",
    "truncation",
    "user",
];

pub struct OpenAiResponsesAdapter;

impl RequestAdapter for OpenAiResponsesAdapter {
    fn to_llm_request(value: Value) -> Result<LLMRequest, AdapterError> {
        let model = value["model"]
            .as_str()
            .ok_or(AdapterError::MissingField("model"))?
            .to_string();
        let mut messages = responses_input(&value["input"])?;
        if let Some(instructions) = value.get("instructions").filter(|value| !value.is_null()) {
            let content = if let Some(text) = instructions.as_str() {
                vec![LLMContent::text(text)]
            } else {
                responses_content_from_value(instructions)
            };
            messages.insert(
                0,
                LLMMessage {
                    source: Some(LLMProtocol::OpenAiResponses),
                    role: LLMRole::Developer,
                    content,
                    name: None,
                    metadata: Extensions::new(),
                },
            );
        }

        let mut extra = extensions_without(
            &value,
            &[
                "model",
                "input",
                "instructions",
                "temperature",
                "max_output_tokens",
                "top_p",
                "stop",
                "stream",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "text",
                "reasoning",
                "metadata",
            ],
        );
        if let Some(verbosity) = value["text"].get("verbosity") {
            extra.insert("verbosity".into(), verbosity.clone());
        }
        Ok(LLMRequest {
            source: LLMProtocol::OpenAiResponses,
            model,
            messages,
            temperature: value["temperature"].as_f64(),
            max_tokens: value["max_output_tokens"].as_u64(),
            top_p: value["top_p"].as_f64(),
            stop: value.get("stop").cloned(),
            stream: value["stream"].as_bool().unwrap_or(false),
            tools: super::tools_from_openai_responses(&value["tools"])?,
            tool_choice: parse_responses_tool_choice(value.get("tool_choice")),
            parallel_tool_calls: value["parallel_tool_calls"].as_bool(),
            response_format: value["text"].get("format").cloned(),
            reasoning: value.get("reasoning").cloned(),
            metadata: value_as_extensions(&value["metadata"]),
            extra,
        })
    }

    fn from_llm_request(request: &LLMRequest) -> Result<Value, AdapterError> {
        validate_request_extras(
            request,
            LLMProtocol::OpenAiResponses,
            RESPONSES_EXTRA_REQUEST_FIELDS,
            &["verbosity"],
            &["logprobs"],
        )?;
        let (instructions, input) = responses_input_to_value(&request.messages)?;
        let mut value = Map::new();
        copy_selected(&mut value, &request.extra, RESPONSES_EXTRA_REQUEST_FIELDS);
        if request.source == LLMProtocol::AnthropicMessages
            && request.extra.get("service_tier").and_then(Value::as_str) == Some("standard_only")
        {
            value.insert("service_tier".into(), json!("default"));
        }
        value.insert("model".into(), json!(request.model));
        value.insert("input".into(), Value::Array(input));
        value.insert("stream".into(), json!(request.stream));
        if let Some(instructions) = instructions {
            value.insert("instructions".into(), instructions);
        }
        insert_option(&mut value, "temperature", request.temperature);
        insert_option(&mut value, "max_output_tokens", request.max_tokens);
        insert_option(&mut value, "top_p", request.top_p);
        if let Some(stop) = &request.stop {
            value.insert("stop".into(), stop.clone());
        }
        if !request.tools.is_empty() {
            value.insert(
                "tools".into(),
                Value::Array(super::tools_to_openai_responses(&request.tools)?),
            );
        }
        if let Some(choice) = &request.tool_choice {
            value.insert("tool_choice".into(), responses_tool_choice_to_value(choice));
        }
        if let Some(parallel) = request.parallel_tool_calls {
            value.insert("parallel_tool_calls".into(), json!(parallel));
        }
        let mut text = Map::new();
        if let Some(format) = &request.response_format {
            text.insert("format".into(), format.clone());
        }
        if let Some(verbosity) = request.extra.get("verbosity") {
            text.insert("verbosity".into(), verbosity.clone());
        }
        if !text.is_empty() {
            value.insert("text".into(), Value::Object(text));
        }
        if let Some(reasoning) = &request.reasoning {
            value.insert(
                "reasoning".into(),
                reasoning_to_responses_request(reasoning, request.source),
            );
        }
        if !request.metadata.is_empty() {
            value.insert("metadata".into(), Value::Object(request.metadata.clone()));
        }
        Ok(Value::Object(value))
    }
}

impl ResponseAdapter for OpenAiResponsesAdapter {
    fn to_llm_response(value: Value) -> Result<LLMResponse, AdapterError> {
        let content = if value["output"].is_array() {
            responses_output_from_value(&value["output"])?
        } else if let Some(text) = value["output_text"].as_str() {
            vec![LLMContent::text(text)]
        } else {
            Vec::new()
        };
        let status = value["status"].as_str().unwrap_or("completed");
        let mut metadata = value_as_extensions(&value["metadata"]);
        if let Some(error) = value.get("error").filter(|value| !value.is_null()) {
            metadata.insert("error".into(), error.clone());
        }
        if let Some(details) = value
            .get("incomplete_details")
            .filter(|value| !value.is_null())
        {
            metadata.insert("incomplete_details".into(), details.clone());
        }
        let finish_reason = responses_finish_reason(status, &value["incomplete_details"], &content);
        Ok(LLMResponse {
            source: LLMProtocol::OpenAiResponses,
            id: value["id"].as_str().map(ToString::to_string),
            model: value["model"].as_str().map(ToString::to_string),
            choices: vec![LLMChoice {
                index: 0,
                role: LLMRole::Assistant,
                content,
                finish_reason: Some(finish_reason),
                metadata: Extensions::new(),
            }],
            usage: value.get("usage").and_then(responses_usage_from_value),
            metadata,
            extra: extensions_without(
                &value,
                &[
                    "id",
                    "object",
                    "created_at",
                    "model",
                    "output",
                    "output_text",
                    "status",
                    "usage",
                    "metadata",
                    "error",
                    "incomplete_details",
                ],
            ),
        })
    }

    fn from_llm_response(response: &LLMResponse) -> Result<Value, AdapterError> {
        if response.choices.len() > 1 {
            return Err(AdapterError::Unsupported(
                "OpenAI Responses cannot represent multiple Chat choices".into(),
            ));
        }
        let choice = response
            .primary_choice()
            .ok_or(AdapterError::InvalidField("choices"))?;
        let mut output = responses_output_to_value(&choice.content)?;
        if let Some(logprobs) = choice.metadata.get("logprobs") {
            let text = output
                .iter_mut()
                .find(|item| item["type"].as_str() == Some("message"))
                .and_then(|message| message["content"].as_array_mut())
                .and_then(|content| {
                    content
                        .iter_mut()
                        .find(|part| part["type"].as_str() == Some("output_text"))
                });
            if let Some(text) = text {
                text["logprobs"] = logprobs.clone();
            }
        }
        let status = responses_status(choice.finish_reason);
        let mut value = if response.source == LLMProtocol::OpenAiResponses {
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
                .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4()))),
        );
        value.insert("object".into(), json!("response"));
        value.insert("created_at".into(), json!(Utc::now().timestamp()));
        value.insert(
            "model".into(),
            json!(response.model.clone().unwrap_or_default()),
        );
        value.insert("output".into(), Value::Array(output));
        value.insert("output_text".into(), json!(joined_text(&choice.content)));
        value.insert("status".into(), json!(status));
        value.insert("metadata".into(), Value::Object(response.metadata.clone()));
        if status == "incomplete" {
            value.insert(
                "incomplete_details".into(),
                json!({"reason": incomplete_reason(choice.finish_reason)}),
            );
        }
        if let Some(error) = response.metadata.get("error") {
            value.insert("error".into(), error.clone());
        }
        if let Some(usage) = &response.usage {
            value.insert("usage".into(), responses_usage_to_value(usage));
        }
        Ok(Value::Object(value))
    }
}

impl StreamAdapter for OpenAiResponsesAdapter {
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
            "response.created" | "response.in_progress" => {
                if !state.started {
                    let response = &value["response"];
                    state.id = response["id"].as_str().map(ToString::to_string);
                    state.model = response["model"].as_str().map(ToString::to_string);
                    state.started = true;
                    state.usage = response.get("usage").and_then(responses_usage_from_value);
                    events.push(LLMStreamEvent::MessageStart {
                        id: state.id.clone(),
                        model: state.model.clone(),
                        usage: state.usage.clone(),
                    });
                }
            }
            "response.output_item.added" => {
                ensure_message_started(&mut events, state, &value);
                let index = value["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(content) = responses_output_item_from_value(&value["item"])? {
                    state.open_content.insert(index);
                    events.push(LLMStreamEvent::ContentStart { index, content });
                }
            }
            "response.content_part.added" => {
                ensure_message_started(&mut events, state, &value);
                let index = value["output_index"].as_u64().unwrap_or(0) as usize;
                if !state.open_content.contains(&index) {
                    let content = responses_content_from_value(&json!([value["part"].clone()]))
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| LLMContent::text(""));
                    state.open_content.insert(index);
                    events.push(LLMStreamEvent::ContentStart { index, content });
                }
            }
            "response.output_text.delta" => events.push(LLMStreamEvent::TextDelta {
                index: value["output_index"].as_u64().unwrap_or(0) as usize,
                text: value["delta"].as_str().unwrap_or_default().to_string(),
            }),
            "response.refusal.delta" => events.push(LLMStreamEvent::RefusalDelta {
                index: value["output_index"].as_u64().unwrap_or(0) as usize,
                refusal: value["delta"].as_str().unwrap_or_default().to_string(),
            }),
            "response.output_text.annotation.added" => {
                events.push(LLMStreamEvent::AnnotationAdded {
                    index: value["output_index"].as_u64().unwrap_or(0) as usize,
                    annotation: annotation_from_openai(
                        &value["annotation"],
                        LLMProtocol::OpenAiResponses,
                    ),
                })
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => events
                .push(LLMStreamEvent::ReasoningDelta {
                    index: value["output_index"].as_u64().unwrap_or(0) as usize,
                    text: value["delta"].as_str().map(ToString::to_string),
                    signature: None,
                }),
            "response.function_call_arguments.delta" => {
                events.push(LLMStreamEvent::ToolCallDelta {
                    index: value["output_index"].as_u64().unwrap_or(0) as usize,
                    id: value["item_id"].as_str().map(ToString::to_string),
                    name: value["name"].as_str().map(ToString::to_string),
                    arguments_delta: value["delta"].as_str().unwrap_or_default().to_string(),
                })
            }
            "response.output_item.done" => {
                let index = value["output_index"].as_u64().unwrap_or(0) as usize;
                if state.open_content.remove(&index) {
                    events.push(LLMStreamEvent::ContentEnd { index });
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let response = &value["response"];
                for index in std::mem::take(&mut state.open_content) {
                    events.push(LLMStreamEvent::ContentEnd { index });
                }
                let usage = response.get("usage").and_then(responses_usage_from_value);
                let content = responses_output_from_value(&response["output"])?;
                events.push(LLMStreamEvent::MessageEnd {
                    finish_reason: Some(responses_finish_reason(
                        response["status"].as_str().unwrap_or("completed"),
                        &response["incomplete_details"],
                        &content,
                    )),
                    usage,
                    metadata: extensions_without(response, &["status", "usage", "output"]),
                });
                state.ended = true;
            }
            "error" => events.push(LLMStreamEvent::Error {
                message: value["error"]["message"]
                    .as_str()
                    .or_else(|| value["message"].as_str())
                    .unwrap_or("upstream stream error")
                    .to_string(),
                metadata: value_as_extensions(value.get("error").unwrap_or(&value)),
            }),
            "response.queued"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.refusal.done"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done" => {}
            _ => events.push(LLMStreamEvent::Raw {
                protocol: LLMProtocol::OpenAiResponses,
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
                        .or_else(|| Some(format!("resp_{}", uuid::Uuid::new_v4())));
                    state.model = model.clone();
                    state.usage = usage.clone();
                    state.started = true;
                    output.push(responses_stream_event(state, "response.created", json!({"response": response_stream_object(state, "in_progress", usage.as_ref())})));
                }
                LLMStreamEvent::ContentStart { index, content } => {
                    let target = state.target_content_index(*index);
                    let item_id = content_id(content);
                    state.content_ids.insert(target, item_id.clone());
                    state.content_buffers.insert(target, String::new());
                    if let LLMContentKind::Text { annotations, .. } = &content.kind {
                        state
                            .content_annotations
                            .insert(target, annotations.clone());
                    }
                    if let LLMContentKind::ToolCall { id, name, .. } = &content.kind {
                        state.tool_names.insert(target, name.clone());
                        state.tool_call_ids.insert(
                            target,
                            id.clone()
                                .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                        );
                    }
                    let (kind, item) = response_stream_item(content, &item_id)?;
                    state.content_kinds.insert(target, kind.clone());
                    state.open_content.insert(target);
                    output.push(responses_stream_event(
                        state,
                        "response.output_item.added",
                        json!({"output_index": target, "item": item}),
                    ));
                    if kind == "text" || kind == "refusal" {
                        let part = if kind == "text" {
                            json!({"type": "output_text", "text": "", "annotations": []})
                        } else {
                            json!({"type": "refusal", "refusal": ""})
                        };
                        output.push(responses_stream_event(state, "response.content_part.added", json!({"item_id": item_id, "output_index": target, "content_index": 0, "part": part})));
                    }
                }
                LLMStreamEvent::TextDelta { index, text } => {
                    let target = state.target_content_index(*index);
                    state
                        .content_buffers
                        .entry(target)
                        .or_default()
                        .push_str(text);
                    let item_id = state
                        .content_ids
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4()));
                    output.push(responses_stream_event(state, "response.output_text.delta", json!({"item_id": item_id, "output_index": target, "content_index": 0, "delta": text})));
                }
                LLMStreamEvent::RefusalDelta { index, refusal } => {
                    let target = state.target_content_index(*index);
                    state
                        .content_buffers
                        .entry(target)
                        .or_default()
                        .push_str(refusal);
                    let item_id = state
                        .content_ids
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4()));
                    output.push(responses_stream_event(state, "response.refusal.delta", json!({"item_id": item_id, "output_index": target, "content_index": 0, "delta": refusal})));
                }
                LLMStreamEvent::AnnotationAdded { index, annotation } => {
                    let target = state.target_content_index(*index);
                    let annotation_value =
                        annotation_to_openai(annotation, LLMProtocol::OpenAiResponses)?;
                    let annotations = state.content_annotations.entry(target).or_default();
                    let annotation_index = annotations.len();
                    annotations.push(annotation.clone());
                    let item_id = state
                        .content_ids
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4()));
                    output.push(responses_stream_event(
                        state,
                        "response.output_text.annotation.added",
                        json!({"item_id": item_id, "output_index": target, "content_index": 0, "annotation_index": annotation_index, "annotation": annotation_value}),
                    ));
                }
                LLMStreamEvent::ReasoningDelta {
                    index,
                    text,
                    signature,
                } => {
                    let target = state.target_content_index(*index);
                    if let Some(text) = text {
                        state
                            .content_buffers
                            .entry(target)
                            .or_default()
                            .push_str(text);
                    }
                    let item_id = state
                        .content_ids
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| format!("rs_{}", uuid::Uuid::new_v4()));
                    if let Some(text) = text {
                        output.push(responses_stream_event(state, "response.reasoning_summary_text.delta", json!({"item_id": item_id, "output_index": target, "summary_index": 0, "delta": text})));
                    }
                    if let Some(signature) = signature {
                        output.push(responses_stream_event(
                            state,
                            "response.reasoning_text.delta",
                            json!({"item_id": item_id, "output_index": target, "delta": signature}),
                        ));
                    }
                }
                LLMStreamEvent::ToolCallDelta {
                    index,
                    arguments_delta,
                    ..
                } => {
                    let target = state.target_content_index(*index);
                    state
                        .content_buffers
                        .entry(target)
                        .or_default()
                        .push_str(arguments_delta);
                    let item_id = state
                        .content_ids
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| format!("fc_{}", uuid::Uuid::new_v4()));
                    output.push(responses_stream_event(state, "response.function_call_arguments.delta", json!({"item_id": item_id, "output_index": target, "delta": arguments_delta})));
                }
                LLMStreamEvent::ContentEnd { index } => {
                    let target = state.target_content_index(*index);
                    let item_id = state.content_ids.get(&target).cloned().unwrap_or_default();
                    let kind = state
                        .content_kinds
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| "text".into());
                    let content = state
                        .content_buffers
                        .get(&target)
                        .cloned()
                        .unwrap_or_default();
                    if kind == "text" {
                        let annotations = state
                            .content_annotations
                            .get(&target)
                            .map(|annotations| {
                                annotations
                                    .iter()
                                    .map(|annotation| {
                                        annotation_to_openai(
                                            annotation,
                                            LLMProtocol::OpenAiResponses,
                                        )
                                    })
                                    .collect::<Result<Vec<_>, _>>()
                            })
                            .transpose()?
                            .unwrap_or_default();
                        output.push(responses_stream_event(state, "response.output_text.done", json!({"item_id": item_id, "output_index": target, "content_index": 0, "text": content})));
                        output.push(responses_stream_event(state, "response.content_part.done", json!({"item_id": item_id, "output_index": target, "content_index": 0, "part": {"type": "output_text", "text": content, "annotations": annotations}})));
                    }
                    if kind == "tool_call" {
                        output.push(responses_stream_event(
                            state,
                            "response.function_call_arguments.done",
                            json!({"item_id": item_id, "output_index": target, "arguments": content}),
                        ));
                    }
                    let item = completed_response_item(state, target, &item_id, &kind)?;
                    output.push(responses_stream_event(
                        state,
                        "response.output_item.done",
                        json!({"output_index": target, "item": item}),
                    ));
                    state.completed_items.push(item);
                    state.open_content.remove(&target);
                }
                LLMStreamEvent::Usage { usage } => state.usage = Some(usage.clone()),
                LLMStreamEvent::MessageEnd {
                    finish_reason,
                    usage,
                    metadata,
                } => {
                    if let Some(usage) = usage {
                        state.usage = Some(usage.clone());
                    }
                    let status = responses_status(*finish_reason);
                    let event_type = match status {
                        "failed" => "response.failed",
                        "incomplete" => "response.incomplete",
                        _ => "response.completed",
                    };
                    let mut response = response_stream_object(state, status, state.usage.as_ref());
                    if let Some(object) = response.as_object_mut() {
                        object.extend(metadata.clone());
                        if status == "incomplete" {
                            object.insert(
                                "incomplete_details".into(),
                                json!({"reason": incomplete_reason(*finish_reason)}),
                            );
                        }
                    }
                    output.push(responses_stream_event(
                        state,
                        event_type,
                        json!({"response": response}),
                    ));
                    state.ended = true;
                }
                LLMStreamEvent::Error { message, metadata } => {
                    let mut error = metadata.clone();
                    error.insert("message".into(), json!(message));
                    output.push(responses_stream_event(
                        state,
                        "error",
                        json!({"error": error}),
                    ));
                }
                LLMStreamEvent::Raw { protocol, value } => {
                    if *protocol != LLMProtocol::OpenAiResponses {
                        return Err(AdapterError::Unsupported(format!(
                            "OpenAI Responses cannot represent a {protocol:?} stream event"
                        )));
                    }
                    output.push(SseEvent::json(value["type"].as_str(), value))
                }
            }
        }
        Ok(output)
    }
}

fn responses_input(value: &Value) -> Result<Vec<LLMMessage>, AdapterError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(vec![LLMMessage {
            source: Some(LLMProtocol::OpenAiResponses),
            role: LLMRole::User,
            content: vec![LLMContent::text(text)],
            name: None,
            metadata: Extensions::new(),
        }]);
    }
    value
        .as_array()
        .ok_or(AdapterError::MissingField("input"))?
        .iter()
        .map(responses_input_item)
        .collect()
}

fn responses_input_item(item: &Value) -> Result<LLMMessage, AdapterError> {
    let item_type = item["type"].as_str().unwrap_or("message");
    let metadata = extensions_without(
        item,
        &[
            "type",
            "role",
            "content",
            "call_id",
            "name",
            "arguments",
            "output",
            "id",
            "status",
            "summary",
            "encrypted_content",
        ],
    );
    match item_type {
        "message" => Ok(LLMMessage {
            source: Some(LLMProtocol::OpenAiResponses),
            role: parse_responses_role(item["role"].as_str().unwrap_or("user")),
            content: responses_content_from_value(&item["content"]),
            name: None,
            metadata,
        }),
        "function_call" => Ok(LLMMessage {
            source: Some(LLMProtocol::OpenAiResponses),
            role: LLMRole::Assistant,
            content: vec![LLMContent {
                source: Some(LLMProtocol::OpenAiResponses),
                kind: LLMContentKind::ToolCall {
                    id: item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .map(ToString::to_string),
                    name: item["name"]
                        .as_str()
                        .ok_or(AdapterError::MissingField("function_call.name"))?
                        .to_string(),
                    arguments: super::openai_chat::parse_arguments(
                        item["arguments"].as_str().unwrap_or("{}"),
                    ),
                },
                metadata,
            }],
            name: None,
            metadata: Extensions::new(),
        }),
        "function_call_output" => Ok(LLMMessage {
            source: Some(LLMProtocol::OpenAiResponses),
            role: LLMRole::Tool,
            content: vec![LLMContent {
                source: Some(LLMProtocol::OpenAiResponses),
                kind: LLMContentKind::ToolResult {
                    tool_call_id: item["call_id"]
                        .as_str()
                        .ok_or(AdapterError::MissingField("function_call_output.call_id"))?
                        .to_string(),
                    content: responses_tool_output_content(&item["output"]),
                    is_error: item["status"].as_str().map(|status| status == "failed"),
                },
                metadata,
            }],
            name: None,
            metadata: Extensions::new(),
        }),
        "reasoning" => Ok(LLMMessage {
            source: Some(LLMProtocol::OpenAiResponses),
            role: LLMRole::Assistant,
            content: vec![reasoning_from_responses(item)],
            name: None,
            metadata: Extensions::new(),
        }),
        _ => Ok(LLMMessage {
            source: Some(LLMProtocol::OpenAiResponses),
            role: LLMRole::Assistant,
            content: vec![LLMContent {
                source: Some(LLMProtocol::OpenAiResponses),
                kind: LLMContentKind::Raw {
                    protocol: LLMProtocol::OpenAiResponses,
                    value: item.clone(),
                },
                metadata: Extensions::new(),
            }],
            name: None,
            metadata: Extensions::new(),
        }),
    }
}

fn responses_input_to_value(
    messages: &[LLMMessage],
) -> Result<(Option<Value>, Vec<Value>), AdapterError> {
    let mut input = Vec::new();
    for message in messages {
        let mut visible = Vec::new();
        for content in &message.content {
            match &content.kind {
                LLMContentKind::ToolCall { id, name, arguments } => input.push(json!({"type": "function_call", "call_id": id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())), "name": name, "arguments": super::openai_chat::arguments_to_string(arguments)})),
                LLMContentKind::ToolResult { tool_call_id, content, is_error } => input.push(json!({"type": "function_call_output", "call_id": tool_call_id, "output": responses_tool_output_to_value(content)?, "status": if *is_error == Some(true) { "failed" } else { "completed" }})),
                LLMContentKind::Reasoning { .. } => input.push(reasoning_to_responses(content)),
                LLMContentKind::Raw {
                    protocol: LLMProtocol::OpenAiResponses,
                    value,
                } if value["type"].is_string() => input.push(value.clone()),
                LLMContentKind::Raw {
                    protocol,
                    value: raw,
                } => {
                    let item_type = raw["type"].as_str().unwrap_or("unknown");
                    return Err(AdapterError::Unsupported(format!(
                        "OpenAI Responses cannot represent {protocol:?} input item `{item_type}`"
                    )))
                }
                _ => visible.push(content.clone()),
            }
        }
        if !visible.is_empty() {
            input.push(json!({"type": "message", "role": responses_role_name(message.role), "content": responses_message_content_to_value(&visible, message.role)?}));
        }
    }
    Ok((None, input))
}

fn responses_content_from_value(value: &Value) -> Vec<LLMContent> {
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
                    "file_id",
                    "file_data",
                    "file_url",
                    "filename",
                    "detail",
                    "refusal",
                    "input_audio",
                    "annotations",
                ],
            );
            let kind = match part["type"].as_str().unwrap_or("input_text") {
                "input_text" | "output_text" | "text" => LLMContentKind::Text {
                    text: part["text"].as_str().unwrap_or_default().to_string(),
                    annotations: part["annotations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|annotation| {
                            annotation_from_openai(annotation, LLMProtocol::OpenAiResponses)
                        })
                        .collect(),
                },
                "input_image" => LLMContentKind::Image {
                    source: if let Some(file_id) = part["file_id"].as_str() {
                        LLMMediaSource::FileId {
                            file_id: file_id.to_string(),
                        }
                    } else {
                        LLMMediaSource::Url {
                            url: part["image_url"].as_str().unwrap_or_default().to_string(),
                        }
                    },
                    detail: part["detail"].as_str().map(ToString::to_string),
                },
                "input_file" => LLMContentKind::File {
                    source: if let Some(file_id) = part["file_id"].as_str() {
                        LLMMediaSource::FileId {
                            file_id: file_id.to_string(),
                        }
                    } else if let Some(data) = part["file_data"].as_str() {
                        LLMMediaSource::Data {
                            data: data.to_string(),
                            media_type: None,
                        }
                    } else {
                        LLMMediaSource::Url {
                            url: part["file_url"].as_str().unwrap_or_default().to_string(),
                        }
                    },
                    filename: part["filename"].as_str().map(ToString::to_string),
                    media_type: None,
                },
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
                "refusal" => LLMContentKind::Refusal {
                    refusal: part["refusal"].as_str().unwrap_or_default().to_string(),
                },
                _ => LLMContentKind::Raw {
                    protocol: LLMProtocol::OpenAiResponses,
                    value: part.clone(),
                },
            };
            LLMContent {
                source: Some(LLMProtocol::OpenAiResponses),
                kind,
                metadata,
            }
        })
        .collect()
}

fn responses_content_to_value(
    content: &[LLMContent],
    role: LLMRole,
) -> Result<Vec<Value>, AdapterError> {
    content
        .iter()
        .map(|content| {
            let mut value = content_extensions_for(content, LLMProtocol::OpenAiResponses);
            match &content.kind {
                LLMContentKind::Text { text, annotations } => {
                    value.insert(
                        "type".into(),
                        json!(if role == LLMRole::Assistant {
                            "output_text"
                        } else {
                            "input_text"
                        }),
                    );
                    value.insert("text".into(), json!(text));
                    if !annotations.is_empty() {
                        value.insert(
                            "annotations".into(),
                            Value::Array(
                                annotations
                                    .iter()
                                    .map(|annotation| {
                                        annotation_to_openai(
                                            annotation,
                                            LLMProtocol::OpenAiResponses,
                                        )
                                    })
                                    .collect::<Result<Vec<_>, _>>()?,
                            ),
                        );
                    }
                }
                LLMContentKind::Image { source, detail } => {
                    value.insert("type".into(), json!("input_image"));
                    match source {
                        LLMMediaSource::FileId { file_id } => {
                            value.insert("file_id".into(), json!(file_id));
                        }
                        _ => {
                            value.insert("image_url".into(), json!(media_source_to_url(source)));
                        }
                    }
                    if let Some(detail) = detail {
                        value.insert("detail".into(), json!(detail));
                    }
                }
                LLMContentKind::File {
                    source, filename, ..
                } => {
                    value.insert("type".into(), json!("input_file"));
                    match source {
                        LLMMediaSource::FileId { file_id } => {
                            value.insert("file_id".into(), json!(file_id));
                        }
                        LLMMediaSource::Data { data, .. } => {
                            value.insert("file_data".into(), json!(data));
                        }
                        _ => {
                            value.insert("file_url".into(), json!(media_source_to_url(source)));
                        }
                    }
                    if let Some(filename) = filename {
                        value.insert("filename".into(), json!(filename));
                    }
                }
                LLMContentKind::Audio { source, format } => {
                    value.insert("type".into(), json!("input_audio"));
                    let data = match source {
                        LLMMediaSource::Data { data, .. } => data.clone(),
                        _ => media_source_to_url(source),
                    };
                    value.insert(
                        "input_audio".into(),
                        json!({"data": data, "format": format}),
                    );
                }
                LLMContentKind::Refusal { refusal } => {
                    value.insert("type".into(), json!("refusal"));
                    value.insert("refusal".into(), json!(refusal));
                }
                LLMContentKind::Raw {
                    protocol: LLMProtocol::OpenAiResponses,
                    value: raw,
                } => return Ok(raw.clone()),
                LLMContentKind::Raw {
                    protocol,
                    value: raw,
                } => {
                    let item_type = raw["type"].as_str().unwrap_or("unknown");
                    return Err(AdapterError::Unsupported(format!(
                        "OpenAI Responses cannot represent {protocol:?} content block `{item_type}`"
                    )));
                }
                _ => return Err(AdapterError::InvalidField("responses.content")),
            }
            Ok(Value::Object(value))
        })
        .collect()
}

fn responses_message_content_to_value(
    content: &[LLMContent],
    role: LLMRole,
) -> Result<Value, AdapterError> {
    if content
        .iter()
        .all(|part| matches!(part.kind, LLMContentKind::Text { .. }))
    {
        Ok(json!(joined_text(content)))
    } else {
        Ok(Value::Array(responses_content_to_value(content, role)?))
    }
}

fn responses_output_from_value(value: &Value) -> Result<Vec<LLMContent>, AdapterError> {
    let mut content = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("message") {
            "message" => content.extend(responses_content_from_value(&item["content"])),
            "function_call" => content.push(LLMContent {
                source: Some(LLMProtocol::OpenAiResponses),
                kind: LLMContentKind::ToolCall {
                    id: item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .map(ToString::to_string),
                    name: item["name"]
                        .as_str()
                        .ok_or(AdapterError::MissingField("output.function_call.name"))?
                        .to_string(),
                    arguments: super::openai_chat::parse_arguments(
                        item["arguments"].as_str().unwrap_or("{}"),
                    ),
                },
                metadata: extensions_without(
                    item,
                    &["type", "id", "call_id", "name", "arguments", "status"],
                ),
            }),
            "reasoning" => content.push(reasoning_from_responses(item)),
            _ => content.push(LLMContent {
                source: Some(LLMProtocol::OpenAiResponses),
                kind: LLMContentKind::Raw {
                    protocol: LLMProtocol::OpenAiResponses,
                    value: item.clone(),
                },
                metadata: Extensions::new(),
            }),
        }
    }
    Ok(content)
}

fn responses_output_to_value(content: &[LLMContent]) -> Result<Vec<Value>, AdapterError> {
    let mut output = Vec::new();
    let mut visible = Vec::new();
    for content in content {
        match &content.kind {
            LLMContentKind::ToolCall { id, name, arguments } => output.push(json!({"id": format!("fc_{}", uuid::Uuid::new_v4()), "type": "function_call", "status": "completed", "call_id": id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())), "name": name, "arguments": super::openai_chat::arguments_to_string(arguments)})),
            LLMContentKind::Reasoning { .. } => output.push(reasoning_to_responses(content)),
            LLMContentKind::Raw {
                protocol: LLMProtocol::OpenAiResponses,
                value,
            } if value["type"].is_string() => output.push(value.clone()),
            LLMContentKind::Raw { protocol, value } => {
                let item_type = value["type"].as_str().unwrap_or("unknown");
                return Err(AdapterError::Unsupported(format!("OpenAI Responses cannot represent {protocol:?} output item `{item_type}`")));
            }
            LLMContentKind::ToolResult { .. } => return Err(AdapterError::Unsupported("Responses output cannot contain a tool result".into())),
            _ => visible.push(content.clone()),
        }
    }
    if !visible.is_empty() {
        output.insert(0, json!({"id": format!("msg_{}", uuid::Uuid::new_v4()), "type": "message", "status": "completed", "role": "assistant", "content": responses_content_to_value(&visible, LLMRole::Assistant)?}));
    }
    Ok(output)
}

fn responses_output_item_from_value(item: &Value) -> Result<Option<LLMContent>, AdapterError> {
    Ok(match item["type"].as_str().unwrap_or_default() {
        "message" => Some(LLMContent::text("")),
        "function_call" => Some(LLMContent {
            source: Some(LLMProtocol::OpenAiResponses),
            kind: LLMContentKind::ToolCall {
                id: item["call_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .map(ToString::to_string),
                name: item["name"].as_str().unwrap_or_default().to_string(),
                arguments: json!({}),
            },
            metadata: Extensions::new(),
        }),
        "reasoning" => Some(reasoning_from_responses(item)),
        "refusal" => Some(LLMContent {
            source: Some(LLMProtocol::OpenAiResponses),
            kind: LLMContentKind::Refusal {
                refusal: String::new(),
            },
            metadata: Extensions::new(),
        }),
        _ => None,
    })
}

fn reasoning_from_responses(value: &Value) -> LLMContent {
    let summary = value["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .map(ToString::to_string)
        .collect();
    LLMContent {
        source: Some(LLMProtocol::OpenAiResponses),
        kind: LLMContentKind::Reasoning {
            text: value["content"]
                .as_array()
                .and_then(|parts| parts.first())
                .and_then(|part| part["text"].as_str())
                .map(ToString::to_string),
            summary,
            signature: None,
            encrypted_content: value["encrypted_content"].as_str().map(ToString::to_string),
        },
        metadata: extensions_without(
            value,
            &[
                "type",
                "id",
                "status",
                "summary",
                "content",
                "encrypted_content",
            ],
        ),
    }
}

fn reasoning_to_responses(content: &LLMContent) -> Value {
    let LLMContentKind::Reasoning {
        text,
        summary,
        encrypted_content,
        ..
    } = &content.kind
    else {
        return Value::Null;
    };
    let mut value = content_extensions_for(content, LLMProtocol::OpenAiResponses);
    value.insert("id".into(), json!(format!("rs_{}", uuid::Uuid::new_v4())));
    value.insert("type".into(), json!("reasoning"));
    value.insert("status".into(), json!("completed"));
    value.insert(
        "summary".into(),
        json!(summary
            .iter()
            .map(|text| json!({"type": "summary_text", "text": text}))
            .collect::<Vec<_>>()),
    );
    if let Some(text) = text {
        value.insert(
            "content".into(),
            json!([{"type": "reasoning_text", "text": text}]),
        );
    }
    if let Some(encrypted) = encrypted_content {
        value.insert("encrypted_content".into(), json!(encrypted));
    }
    Value::Object(value)
}

fn responses_tool_output_content(value: &Value) -> Vec<LLMContent> {
    if let Some(text) = value.as_str() {
        vec![LLMContent::text(text)]
    } else {
        responses_content_from_value(value)
    }
}
fn responses_tool_output_to_value(content: &[LLMContent]) -> Result<Value, AdapterError> {
    if content
        .iter()
        .all(|part| matches!(part.kind, LLMContentKind::Text { .. }))
    {
        Ok(json!(joined_text(content)))
    } else {
        Ok(Value::Array(responses_content_to_value(
            content,
            LLMRole::Tool,
        )?))
    }
}
fn parse_responses_role(role: &str) -> LLMRole {
    match role {
        "system" => LLMRole::System,
        "developer" => LLMRole::Developer,
        "assistant" => LLMRole::Assistant,
        "tool" => LLMRole::Tool,
        _ => LLMRole::User,
    }
}
fn responses_role_name(role: LLMRole) -> &'static str {
    match role {
        LLMRole::System => "system",
        LLMRole::Developer => "developer",
        LLMRole::Assistant => "assistant",
        LLMRole::Tool => "tool",
        LLMRole::User => "user",
    }
}

fn parse_responses_tool_choice(value: Option<&Value>) -> Option<LLMToolChoice> {
    let value = value?;
    match value.as_str() {
        Some("auto") => Some(LLMToolChoice::Auto),
        Some("none") => Some(LLMToolChoice::None),
        Some("required") => Some(LLMToolChoice::Required),
        Some(_) => Some(LLMToolChoice::Raw {
            value: value.clone(),
        }),
        None if value["type"].as_str() == Some("function") => {
            value["name"].as_str().map(|name| LLMToolChoice::Function {
                name: name.to_string(),
            })
        }
        None => Some(LLMToolChoice::Raw {
            value: value.clone(),
        }),
    }
}

fn reasoning_to_responses_request(reasoning: &Value, source: LLMProtocol) -> Value {
    if source != LLMProtocol::AnthropicMessages {
        return reasoning.clone();
    }
    let reasoning_type = reasoning["type"].as_str();
    if reasoning_type == Some("disabled") {
        return json!({"effort": "none"});
    }
    let effort = reasoning["effort"].as_str().unwrap_or_else(|| {
        match reasoning["budget_tokens"].as_u64().unwrap_or_default() {
            0..=2048 => "low",
            2049..=8192 => "medium",
            _ => "high",
        }
    });
    json!({"effort": effort, "summary": "auto"})
}
fn responses_tool_choice_to_value(choice: &LLMToolChoice) -> Value {
    match choice {
        LLMToolChoice::Auto => json!("auto"),
        LLMToolChoice::None => json!("none"),
        LLMToolChoice::Required => json!("required"),
        LLMToolChoice::Function { name } => json!({"type": "function", "name": name}),
        LLMToolChoice::Raw { value } => value.clone(),
    }
}

fn responses_finish_reason(
    status: &str,
    details: &Value,
    content: &[LLMContent],
) -> LLMFinishReason {
    if content
        .iter()
        .any(|content| matches!(content.kind, LLMContentKind::ToolCall { .. }))
        && status == "completed"
    {
        return LLMFinishReason::ToolCalls;
    }
    match status {
        "completed" => LLMFinishReason::Completed,
        "failed" => LLMFinishReason::Failed,
        "cancelled" => LLMFinishReason::Cancelled,
        "incomplete" => match details["reason"].as_str() {
            Some("max_output_tokens") => LLMFinishReason::MaxTokens,
            Some("content_filter") => LLMFinishReason::ContentFilter,
            Some("model_context_window_exceeded") => LLMFinishReason::ContextLimit,
            _ => LLMFinishReason::Incomplete,
        },
        _ => LLMFinishReason::Unknown,
    }
}
fn responses_status(reason: Option<LLMFinishReason>) -> &'static str {
    match reason {
        Some(LLMFinishReason::Failed) => "failed",
        Some(LLMFinishReason::Cancelled) => "cancelled",
        Some(
            LLMFinishReason::Length
            | LLMFinishReason::MaxTokens
            | LLMFinishReason::ContextLimit
            | LLMFinishReason::Incomplete
            | LLMFinishReason::ContentFilter,
        ) => "incomplete",
        _ => "completed",
    }
}
fn incomplete_reason(reason: Option<LLMFinishReason>) -> &'static str {
    match reason {
        Some(LLMFinishReason::ContentFilter | LLMFinishReason::Refusal) => "content_filter",
        Some(LLMFinishReason::ContextLimit) => "model_context_window_exceeded",
        _ => "max_output_tokens",
    }
}

pub(super) fn responses_usage_from_value(value: &Value) -> Option<LLMUsage> {
    let object = value.as_object()?;
    let input_details = &value["input_tokens_details"];
    let output_details = &value["output_tokens_details"];
    Some(LLMUsage {
        source: Some(LLMProtocol::OpenAiResponses),
        input_tokens: value["input_tokens"].as_u64(),
        output_tokens: value["output_tokens"].as_u64(),
        total_tokens: value["total_tokens"].as_u64(),
        cache_read_tokens: input_details["cached_tokens"].as_u64(),
        cache_creation_tokens: None,
        reasoning_tokens: output_details["reasoning_tokens"].as_u64(),
        input_audio_tokens: None,
        output_audio_tokens: None,
        accepted_prediction_tokens: output_details["accepted_prediction_tokens"].as_u64(),
        rejected_prediction_tokens: output_details["rejected_prediction_tokens"].as_u64(),
        extra: object
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "input_tokens"
                        | "output_tokens"
                        | "total_tokens"
                        | "input_tokens_details"
                        | "output_tokens_details"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}
pub(super) fn responses_usage_to_value(usage: &LLMUsage) -> Value {
    let mut value = if usage.source == Some(LLMProtocol::OpenAiResponses) {
        usage.extra.clone()
    } else {
        Extensions::new()
    };
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    value.insert("input_tokens".into(), json!(input));
    value.insert("output_tokens".into(), json!(output));
    value.insert(
        "total_tokens".into(),
        json!(usage.total_tokens.unwrap_or(input + output)),
    );
    let mut input_details = Map::new();
    if let Some(tokens) = usage.cache_read_tokens {
        input_details.insert("cached_tokens".into(), json!(tokens));
    }
    if !input_details.is_empty() {
        value.insert("input_tokens_details".into(), Value::Object(input_details));
    }
    let mut output_details = Map::new();
    if let Some(tokens) = usage.reasoning_tokens {
        output_details.insert("reasoning_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.accepted_prediction_tokens {
        output_details.insert("accepted_prediction_tokens".into(), json!(tokens));
    }
    if let Some(tokens) = usage.rejected_prediction_tokens {
        output_details.insert("rejected_prediction_tokens".into(), json!(tokens));
    }
    if !output_details.is_empty() {
        value.insert(
            "output_tokens_details".into(),
            Value::Object(output_details),
        );
    }
    Value::Object(value)
}

fn ensure_message_started(
    events: &mut Vec<LLMStreamEvent>,
    state: &mut StreamState,
    value: &Value,
) {
    if !state.started {
        state.started = true;
        events.push(LLMStreamEvent::MessageStart {
            id: value["response_id"].as_str().map(ToString::to_string),
            model: None,
            usage: None,
        });
    }
}
fn responses_stream_event(state: &mut StreamState, event_type: &str, fields: Value) -> SseEvent {
    let mut value = fields;
    value["type"] = json!(event_type);
    value["sequence_number"] = json!(state.sequence_number);
    state.sequence_number += 1;
    SseEvent::json(Some(event_type), &value)
}
fn response_stream_object(state: &StreamState, status: &str, usage: Option<&LLMUsage>) -> Value {
    json!({"id": state.id, "object": "response", "created_at": Utc::now().timestamp(), "status": status, "model": state.model, "output": state.completed_items, "usage": usage.map(responses_usage_to_value)})
}
fn content_id(content: &LLMContent) -> String {
    match &content.kind {
        LLMContentKind::ToolCall { .. } => format!("fc_{}", uuid::Uuid::new_v4()),
        LLMContentKind::Reasoning { .. } => format!("rs_{}", uuid::Uuid::new_v4()),
        _ => format!("msg_{}", uuid::Uuid::new_v4()),
    }
}
fn response_stream_item(content: &LLMContent, id: &str) -> Result<(String, Value), AdapterError> {
    Ok(match &content.kind {
        LLMContentKind::ToolCall {
            id: call_id, name, ..
        } => (
            "tool_call".into(),
            json!({"id": id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": ""}),
        ),
        LLMContentKind::Reasoning { .. } => (
            "reasoning".into(),
            json!({"id": id, "type": "reasoning", "status": "in_progress", "summary": []}),
        ),
        LLMContentKind::Refusal { .. } => (
            "refusal".into(),
            json!({"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}),
        ),
        LLMContentKind::ToolResult { .. } => {
            return Err(AdapterError::Unsupported(
                "Responses output stream cannot contain tool results".into(),
            ))
        }
        _ => (
            "text".into(),
            json!({"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}),
        ),
    })
}
fn completed_response_item(
    state: &StreamState,
    index: usize,
    item_id: &str,
    kind: &str,
) -> Result<Value, AdapterError> {
    let content = state
        .content_buffers
        .get(&index)
        .cloned()
        .unwrap_or_default();
    Ok(match kind {
        "tool_call" => json!({
            "id": item_id,
            "type": "function_call",
            "status": "completed",
            "call_id": state.tool_call_ids.get(&index),
            "name": state.tool_names.get(&index),
            "arguments": content,
        }),
        "reasoning" => json!({
            "id": item_id,
            "type": "reasoning",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": content}],
        }),
        "refusal" => json!({
            "id": item_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "refusal", "refusal": content}],
        }),
        _ => json!({
            "id": item_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": content,
                "annotations": state
                    .content_annotations
                    .get(&index)
                    .map(|annotations| annotations
                        .iter()
                        .map(|annotation| annotation_to_openai(annotation, LLMProtocol::OpenAiResponses))
                        .collect::<Result<Vec<_>, _>>())
                    .transpose()?
                    .unwrap_or_default(),
            }],
        }),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{anthropic::AnthropicAdapter, openai_chat::OpenAiChatAdapter};

    #[test]
    fn parses_function_calls_outputs_reasoning_and_multimodal_input() {
        let request = OpenAiResponsesAdapter::to_llm_request(json!({
            "model": "gpt-5", "instructions": "be concise", "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "inspect"}, {"type": "input_image", "image_url": "https://example.test/a.png"}]},
                {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "sunny"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "checked"}], "encrypted_content": "enc"}
            ]
        })).unwrap();
        assert_eq!(request.messages[0].role, LLMRole::Developer);
        assert!(matches!(
            request.messages[1].content[1].kind,
            LLMContentKind::Image { .. }
        ));
        assert!(matches!(
            request.messages[2].content[0].kind,
            LLMContentKind::ToolCall { .. }
        ));
        assert!(matches!(
            request.messages[3].content[0].kind,
            LLMContentKind::ToolResult { .. }
        ));
        assert!(matches!(
            request.messages[4].content[0].kind,
            LLMContentKind::Reasoning { .. }
        ));
    }

    #[test]
    fn responses_output_keeps_tool_call_and_usage_details() {
        let response = OpenAiResponsesAdapter::to_llm_response(json!({
            "id": "resp_1", "model": "gpt-5", "status": "completed",
            "output": [{"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{}"}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "input_tokens_details": {"cached_tokens": 4}, "output_tokens_details": {"reasoning_tokens": 2}}
        })).unwrap();
        assert_eq!(
            response.choices[0].finish_reason,
            Some(LLMFinishReason::ToolCalls)
        );
        assert!(matches!(
            response.choices[0].content[0].kind,
            LLMContentKind::ToolCall { .. }
        ));
        assert_eq!(response.usage.as_ref().unwrap().reasoning_tokens, Some(2));
    }

    #[test]
    fn response_stream_accumulates_annotations_in_terminal_items() {
        let annotation = crate::model::LLMAnnotation {
            source: LLMProtocol::AnthropicMessages,
            annotation_type: "web_search_result_location".into(),
            start_index: Some(0),
            end_index: Some(5),
            url: Some("https://example.test".into()),
            title: Some("Example".into()),
            file_id: None,
            filename: None,
            cited_text: Some("hello".into()),
            metadata: Extensions::new(),
        };
        let events = vec![
            LLMStreamEvent::MessageStart {
                id: Some("resp_test".into()),
                model: Some("gpt-5".into()),
                usage: None,
            },
            LLMStreamEvent::ContentStart {
                index: 0,
                content: LLMContent::text(""),
            },
            LLMStreamEvent::TextDelta {
                index: 0,
                text: "hello".into(),
            },
            LLMStreamEvent::AnnotationAdded {
                index: 0,
                annotation,
            },
            LLMStreamEvent::ContentEnd { index: 0 },
            LLMStreamEvent::MessageEnd {
                finish_reason: Some(LLMFinishReason::Completed),
                usage: None,
                metadata: Extensions::new(),
            },
        ];
        let output =
            OpenAiResponsesAdapter::format_stream_events(&events, &mut StreamState::default())
                .unwrap();
        let values = output
            .iter()
            .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
            .collect::<Vec<_>>();
        let item_done = values
            .iter()
            .find(|value| value["type"] == "response.output_item.done")
            .unwrap();
        let completed = values
            .iter()
            .find(|value| value["type"] == "response.completed")
            .unwrap();

        assert_eq!(
            item_done["item"]["content"][0]["annotations"][0]["url"],
            "https://example.test"
        );
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["annotations"][0]["url"],
            "https://example.test"
        );
    }

    #[test]
    fn anthropic_system_cache_metadata_does_not_leak_into_responses() {
        let request = AnthropicAdapter::to_llm_request(json!({
            "model": "claude-sonnet",
            "max_tokens": 256,
            "system": [{
                "type": "text",
                "text": "Be concise",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "hello",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .unwrap();

        let output = OpenAiResponsesAdapter::from_llm_request(&request).unwrap();
        assert_eq!(output["input"][0]["role"], "system");
        assert_eq!(output["input"][0]["content"], "Be concise");
        assert_eq!(output["input"][1]["content"], "hello");
        assert!(!output.to_string().contains("cache_control"));
    }

    #[test]
    fn advanced_response_items_fail_explicitly_when_target_cannot_represent_them() {
        for item_type in [
            "custom_tool_call",
            "mcp_call",
            "web_search_call",
            "file_search_call",
            "computer_call",
            "code_interpreter_call",
            "local_shell_call",
            "shell_call",
            "apply_patch_call",
        ] {
            let request = OpenAiResponsesAdapter::to_llm_request(json!({
                "model": "gpt-5",
                "input": [{"type": item_type, "id": "item_1"}]
            }))
            .unwrap();

            let chat_error = OpenAiChatAdapter::from_llm_request(&request).unwrap_err();
            let anthropic_error = AnthropicAdapter::from_llm_request(&request).unwrap_err();
            assert!(chat_error.to_string().contains(item_type));
            assert!(anthropic_error.to_string().contains(item_type));
        }
    }
}
