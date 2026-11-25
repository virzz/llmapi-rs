use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::{
    AdapterError, RequestAdapter, ResponseAdapter, SseEvent, StreamAdapter, StreamState,
    content_extensions_for, extensions_without, message_extensions_for, validate_request_extras,
    value_as_extensions,
};
use crate::model::{
    Extensions, LLMChoice, LLMContent, LLMContentKind, LLMFinishReason, LLMMediaSource, LLMMessage,
    LLMProtocol, LLMRequest, LLMResponse, LLMRole, LLMStreamEvent, LLMTool, LLMToolChoice,
    LLMToolKind, LLMUsage,
};

const GEMINI: LLMProtocol = LLMProtocol::Gemini;
const GENERATION_FIELDS: &[&str] = &[
    "temperature",
    "maxOutputTokens",
    "topP",
    "stopSequences",
    "thinkingConfig",
];

pub struct GeminiAdapter;

impl GeminiAdapter {
    /// The REST endpoint supplies the model and determines whether the response is streamed.
    pub fn to_llm_request_with_model(
        value: Value,
        model: &str,
        stream: bool,
    ) -> Result<LLMRequest, AdapterError> {
        let contents = value["contents"]
            .as_array()
            .ok_or(AdapterError::MissingField("contents"))?;
        let mut messages = Vec::with_capacity(contents.len() + 1);
        if let Some(system) = value.get("systemInstruction") {
            messages.push(message_from_value(system, LLMRole::System)?);
        }
        for content in contents {
            let role = match content["role"].as_str() {
                Some("model") => LLMRole::Assistant,
                Some("function") => LLMRole::Tool,
                _ => LLMRole::User,
            };
            messages.push(message_from_value(content, role)?);
        }

        let generation = &value["generationConfig"];
        let tool_config = &value["toolConfig"]["functionCallingConfig"];
        let mut extra = extensions_without(
            &value,
            &["model", "stream", "systemInstruction", "contents", "tools"],
        );
        if let Some(generation) = extra.get_mut("generationConfig") {
            *generation = Value::Object(extensions_without(generation, GENERATION_FIELDS));
        }
        Ok(LLMRequest {
            source: GEMINI,
            model: model.to_owned(),
            messages,
            temperature: generation["temperature"].as_f64(),
            max_tokens: generation["maxOutputTokens"].as_u64(),
            top_p: generation["topP"].as_f64(),
            stop: generation.get("stopSequences").cloned(),
            stream,
            tools: tools_from_value(&value["tools"])?,
            tool_choice: tool_choice_from_value(tool_config),
            parallel_tool_calls: None,
            response_format: response_format_from_value(generation),
            reasoning: generation.get("thinkingConfig").cloned(),
            metadata: Extensions::new(),
            extra,
        })
    }
}

impl RequestAdapter for GeminiAdapter {
    fn to_llm_request(value: Value) -> Result<LLMRequest, AdapterError> {
        let model = value["model"]
            .as_str()
            .ok_or(AdapterError::MissingField("model"))?
            .to_owned();
        let stream = value["stream"].as_bool().unwrap_or(false);
        Self::to_llm_request_with_model(value, &model, stream)
    }

    fn from_llm_request(request: &LLMRequest) -> Result<Value, AdapterError> {
        if request.source == LLMProtocol::OpenAiResponses {
            for field in ["previous_response_id", "conversation"] {
                if request.extra.get(field).is_some_and(|v| !v.is_null()) {
                    return Err(AdapterError::Unsupported(format!(
                        "Gemini cannot resolve {field}. Send the full conversation in input"
                    )));
                }
            }
        }
        if request.source != GEMINI {
            if request.parallel_tool_calls == Some(false)
                && !request.tools.is_empty()
                && !matches!(request.tool_choice, Some(LLMToolChoice::None))
            {
                return Err(AdapterError::Unsupported(
                    "Gemini cannot preserve parallel_tool_calls: false with tools enabled".into(),
                ));
            }
            // The proxy handles Chat usage delivery without an upstream request field.
            validate_request_extras(request, GEMINI, &[], &[], &["stream_options"])?;
            if request.reasoning.as_ref().is_some_and(|reasoning| {
                !reasoning.is_null() && !reasoning.as_object().is_some_and(Extensions::is_empty)
            }) {
                return Err(AdapterError::Unsupported(format!(
                    "Gemini cannot preserve {:?} reasoning controls",
                    request.source
                )));
            }
        }
        let mut value = if request.source == GEMINI {
            request.extra.clone()
        } else {
            Extensions::new()
        };
        let (system, contents) = messages_to_value(&request.messages)?;
        insert_option(&mut value, "systemInstruction", system);
        value.insert("contents".into(), json!(contents));

        let mut generation = value
            .remove("generationConfig")
            .and_then(|value| value.as_object().cloned());
        let config = generation.get_or_insert_default();
        insert_option(config, "temperature", request.temperature);
        insert_option(config, "maxOutputTokens", request.max_tokens);
        insert_option(config, "topP", request.top_p);
        if let Some(stop) = &request.stop {
            config.insert(
                "stopSequences".into(),
                if stop.is_string() {
                    json!([stop])
                } else {
                    stop.clone()
                },
            );
        }
        if request.source == GEMINI {
            insert_option(config, "thinkingConfig", request.reasoning.as_ref());
        }
        if let Some(format) = &request.response_format {
            match format["type"].as_str() {
                Some("json_object" | "json_schema") => {
                    config.insert("responseMimeType".into(), json!("application/json"));
                    if let Some(schema) = format.get("schema") {
                        let key = if config.contains_key("responseSchema") {
                            "responseSchema"
                        } else {
                            "responseJsonSchema"
                        };
                        config.insert(key.into(), schema.clone());
                    }
                }
                Some("text") if config.contains_key("responseMimeType") => {
                    config.insert("responseMimeType".into(), json!("text/plain"));
                }
                _ => {}
            }
        }
        if !config.is_empty()
            || request.extra.contains_key("generationConfig") && request.source == GEMINI
        {
            value.insert("generationConfig".into(), json!(config));
        }

        if !request.tools.is_empty() {
            value.insert("tools".into(), json!(tools_to_value(&request.tools)?));
        }
        if let Some(choice) = &request.tool_choice {
            let tool_config = value.entry("toolConfig").or_insert_with(|| json!({}));
            tool_config["functionCallingConfig"] =
                tool_choice_to_value(choice, &tool_config["functionCallingConfig"]);
        }
        Ok(Value::Object(value))
    }
}

fn response_format_from_value(generation: &Value) -> Option<Value> {
    match generation["responseMimeType"].as_str()? {
        "application/json" => Some(
            generation
                .get("responseJsonSchema")
                .or_else(|| generation.get("responseSchema"))
                .map_or_else(
                    || json!({"type": "json_object"}),
                    |schema| json!({"type": "json_schema", "schema": schema}),
                ),
        ),
        "text/plain" => Some(json!({"type": "text"})),
        _ => None,
    }
}

fn message_from_value(value: &Value, role: LLMRole) -> Result<LLMMessage, AdapterError> {
    let content = parts_from_value(value)?;
    let role = if role == LLMRole::User
        && !content.is_empty()
        && content
            .iter()
            .all(|part| matches!(part.kind, LLMContentKind::ToolResult { .. }))
    {
        LLMRole::Tool
    } else {
        role
    };
    Ok(LLMMessage {
        source: Some(GEMINI),
        role,
        content,
        name: None,
        metadata: extensions_without(value, &["parts"]),
    })
}

fn messages_to_value(messages: &[LLMMessage]) -> Result<(Option<Value>, Vec<Value>), AdapterError> {
    let mut system: Option<Extensions> = None;
    let mut system_parts = Vec::new();
    let mut contents = Vec::<Value>::new();
    let mut calls = BTreeMap::new();
    let mut call_order = BTreeMap::new();
    let mut next_call = 0;
    let mut previous_responses_call = false;
    let mut messages = messages.iter().peekable();
    while let Some(message) = messages.next() {
        if is_openai_tool_result(message) {
            let mut parts = Vec::new();
            let mut current = message;
            loop {
                for content in &current.content {
                    if let LLMContentKind::ToolResult { tool_call_id, .. } = &content.kind {
                        let part = part_to_value(content, &calls)?;
                        parts.push((call_order.get(tool_call_id.as_str()).copied(), part));
                    }
                }
                match messages.next_if(|next| is_openai_tool_result(next)) {
                    Some(next) => current = next,
                    None => break,
                }
            }
            // Same-name calls without provider IDs are paired positionally.
            parts.sort_by_key(|(index, _)| *index);
            let parts = parts.into_iter().map(|(_, part)| part).collect::<Vec<_>>();
            contents.push(json!({"role": "user", "parts": parts}));
            previous_responses_call = false;
            continue;
        }

        let mut parts = Vec::with_capacity(message.content.len());
        for content in &message.content {
            if let LLMContentKind::ToolCall {
                id: Some(id), name, ..
            } = &content.kind
            {
                calls.insert(id.as_str(), name.as_str());
                call_order.insert(id.as_str(), next_call);
                next_call += 1;
            }
            parts.push(part_to_value(content, &calls)?);
        }
        let mut value = message_extensions_for(message, GEMINI);
        let responses_call = message.source == Some(LLMProtocol::OpenAiResponses)
            && message.role == LLMRole::Assistant
            && message
                .content
                .iter()
                .any(|part| matches!(part.kind, LLMContentKind::ToolCall { .. }));
        if matches!(message.role, LLMRole::System | LLMRole::Developer) {
            if !parts.iter().all(|part| part["text"].is_string()) {
                return Err(AdapterError::Unsupported(
                    "Gemini system instructions require text".into(),
                ));
            }
            system.get_or_insert(value);
            system_parts.extend(parts);
        } else if responses_call
            && previous_responses_call
            && let Some(previous) = contents.last_mut()
            && let Some(previous_parts) = previous["parts"].as_array_mut()
        {
            // Responses represents each call in the same assistant turn as its own input item.
            previous_parts.extend(parts);
        } else {
            if message.source != Some(GEMINI) {
                let role = if message.role == LLMRole::Assistant {
                    "model"
                } else {
                    "user"
                };
                value.insert("role".into(), json!(role));
            }
            value.insert("parts".into(), Value::Array(parts));
            contents.push(Value::Object(value));
        }
        previous_responses_call = responses_call;
    }
    Ok((
        system.map(|mut value| {
            value.insert("parts".into(), Value::Array(system_parts));
            Value::Object(value)
        }),
        contents,
    ))
}

fn is_openai_tool_result(message: &LLMMessage) -> bool {
    matches!(
        message.source,
        Some(LLMProtocol::OpenAiChat | LLMProtocol::OpenAiResponses)
    ) && message.role == LLMRole::Tool
        && !message.content.is_empty()
        && message
            .content
            .iter()
            .all(|part| matches!(part.kind, LLMContentKind::ToolResult { .. }))
}

fn parts_from_value(value: &Value) -> Result<Vec<LLMContent>, AdapterError> {
    value["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(part_from_value)
        .collect()
}

fn part_from_value(value: &Value) -> Result<LLMContent, AdapterError> {
    let (kind, fields): (_, &[&str]) = if let Some(text) = value["text"].as_str() {
        if value["thought"].as_bool() == Some(true) {
            (
                LLMContentKind::Reasoning {
                    text: Some(text.to_owned()),
                    summary: Vec::new(),
                    signature: string(&value["thoughtSignature"]),
                    encrypted_content: None,
                },
                &["text", "thought", "thoughtSignature"],
            )
        } else {
            (
                LLMContentKind::Text {
                    text: text.to_owned(),
                    annotations: Vec::new(),
                },
                &["text"],
            )
        }
    } else if let Some(call) = value.get("functionCall") {
        (
            LLMContentKind::ToolCall {
                id: string(&call["id"]),
                name: required_string(call, "name")?,
                arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
            },
            &[],
        )
    } else if let Some(result) = value.get("functionResponse") {
        let name = required_string(result, "name")?;
        (
            LLMContentKind::ToolResult {
                tool_call_id: string(&result["id"]).unwrap_or(name),
                content: result
                    .get("response")
                    .map(|response| LLMContent::text(response.to_string()))
                    .into_iter()
                    .collect(),
                is_error: result["response"].get("error").map(|_| true),
            },
            &[],
        )
    } else if let Some(data) = value.get("inlineData") {
        let mime_type = string(&data["mimeType"]);
        let source = LLMMediaSource::Data {
            data: required_string(data, "data")?,
            media_type: mime_type.clone(),
        };
        (
            media_kind(source, mime_type, string(&data["displayName"])),
            &[],
        )
    } else if let Some(data) = value.get("fileData") {
        let mime_type = string(&data["mimeType"]);
        let source = LLMMediaSource::Url {
            url: required_string(data, "fileUri")?,
        };
        (
            media_kind(source, mime_type, string(&data["displayName"])),
            &[],
        )
    } else {
        (
            LLMContentKind::Raw {
                protocol: GEMINI,
                value: value.clone(),
            },
            &[],
        )
    };
    let mut metadata = extensions_without(value, fields);
    // The normalized media source already owns the payload.
    for (key, payload) in [("inlineData", "data"), ("fileData", "fileUri")] {
        if let Some(blob) = metadata.get_mut(key).and_then(Value::as_object_mut) {
            blob.remove(payload);
        }
    }
    if matches!(kind, LLMContentKind::Raw { .. }) {
        metadata.clear();
    }
    Ok(LLMContent {
        source: Some(GEMINI),
        kind,
        metadata,
    })
}

fn media_kind(
    source: LLMMediaSource,
    mime_type: Option<String>,
    filename: Option<String>,
) -> LLMContentKind {
    match mime_type.as_deref().and_then(|mime| mime.split_once('/')) {
        Some(("image", _)) => LLMContentKind::Image {
            source,
            detail: None,
        },
        Some(("audio", format)) => LLMContentKind::Audio {
            source,
            format: Some(format.to_owned()),
        },
        _ => LLMContentKind::File {
            source,
            filename,
            media_type: mime_type,
        },
    }
}

fn part_to_value(
    content: &LLMContent,
    calls: &BTreeMap<&str, &str>,
) -> Result<Value, AdapterError> {
    let mut part = Value::Object(content_extensions_for(content, GEMINI));
    match &content.kind {
        LLMContentKind::Text { text, .. } => part["text"] = json!(text),
        LLMContentKind::Reasoning {
            text,
            summary,
            signature,
            encrypted_content,
        } => {
            if encrypted_content.is_some() {
                return Err(AdapterError::Unsupported(
                    "Gemini cannot represent encrypted reasoning from another API".into(),
                ));
            }
            part["text"] = json!(text.clone().unwrap_or_else(|| summary.join("")));
            part["thought"] = json!(true);
            if content.source == Some(GEMINI)
                && let Some(signature) = signature
            {
                part["thoughtSignature"] = json!(signature);
            }
        }
        LLMContentKind::ToolCall {
            id,
            name,
            arguments,
        } => {
            let mut call = value_as_extensions(&part["functionCall"]);
            call.insert("name".into(), json!(name));
            insert_option(&mut call, "id", id.as_ref());
            if content.source != Some(GEMINI)
                || call.contains_key("args")
                || arguments != &json!({})
            {
                let arguments = match arguments {
                    Value::String(text) if text.is_empty() => json!({}),
                    Value::String(text) => serde_json::from_str(text)
                        .map_err(|_| AdapterError::InvalidField("functionCall.args"))?,
                    other => other.clone(),
                };
                if !arguments.is_object() {
                    return Err(AdapterError::InvalidField("functionCall.args"));
                }
                call.insert("args".into(), arguments);
            }
            part["functionCall"] = json!(call);
            if matches!(
                content.source,
                Some(
                    LLMProtocol::OpenAiChat
                        | LLMProtocol::OpenAiResponses
                        | LLMProtocol::AnthropicMessages
                )
            ) && let Some(signature) = content
                .metadata
                .get("extra_content")
                .and_then(|extra| extra.pointer("/google/thought_signature"))
                .and_then(Value::as_str)
            {
                part["thoughtSignature"] = json!(signature);
            }
        }
        LLMContentKind::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            let mut result = value_as_extensions(&part["functionResponse"]);
            let native = result.contains_key("name");
            let name = result
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| calls.get(tool_call_id.as_str()).copied())
                .ok_or(AdapterError::MissingField("functionResponse.name"))?
                .to_owned();
            result.insert("name".into(), json!(name));
            if !native || result.contains_key("id") {
                result.insert("id".into(), json!(tool_call_id));
            }

            let mut text = String::new();
            let mut media = Vec::new();
            for item in content {
                if let Some(fragment) = item.text_value() {
                    text.push_str(fragment);
                } else {
                    let part = part_to_value(item, calls)?;
                    if part.get("inlineData").is_none() && part.get("fileData").is_none() {
                        return Err(AdapterError::Unsupported(
                            "Gemini function responses support text and media".into(),
                        ));
                    }
                    media.push(part);
                }
            }
            let response = serde_json::from_str::<Value>(&text)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| {
                    if *is_error == Some(true) {
                        json!({"error": text})
                    } else {
                        json!({"output": text})
                    }
                });
            if !native || !text.is_empty() {
                result.insert("response".into(), response);
            }
            if !media.is_empty() {
                result.insert("parts".into(), json!(media));
            }
            part["functionResponse"] = json!(result);
        }
        LLMContentKind::Image { source, .. } => media_to_part(&mut part, source, None, None)?,
        LLMContentKind::Audio { source, format } => {
            let mime = format.as_ref().map(|format| format!("audio/{format}"));
            media_to_part(&mut part, source, mime.as_deref(), None)?;
        }
        LLMContentKind::File {
            source,
            filename,
            media_type,
        } => {
            media_to_part(
                &mut part,
                source,
                media_type.as_deref(),
                filename.as_deref(),
            )?;
        }
        LLMContentKind::Refusal { refusal } => part["text"] = json!(refusal),
        LLMContentKind::Raw {
            protocol: GEMINI,
            value,
        } => return Ok(value.clone()),
        LLMContentKind::Raw { protocol, .. } => {
            return Err(AdapterError::Unsupported(format!(
                "Gemini cannot represent {protocol:?} raw content"
            )));
        }
    }
    Ok(part)
}

fn media_to_part(
    part: &mut Value,
    source: &LLMMediaSource,
    mime: Option<&str>,
    filename: Option<&str>,
) -> Result<(), AdapterError> {
    let (key, field, data, mime) = match source {
        LLMMediaSource::Data { data, media_type } => (
            "inlineData",
            "data",
            data.as_str(),
            media_type.as_deref().or(mime),
        ),
        LLMMediaSource::Url { url } => {
            if let Some((mime, data)) = url
                .strip_prefix("data:")
                .and_then(|data| data.split_once(";base64,"))
            {
                ("inlineData", "data", data, Some(mime))
            } else {
                ("fileData", "fileUri", url.as_str(), mime)
            }
        }
        _ => {
            return Err(AdapterError::Unsupported(
                "Gemini media requires inline base64 data or a file URI".into(),
            ));
        }
    };
    let mut blob = value_as_extensions(&part[key]);
    blob.insert(field.into(), json!(data));
    insert_option(&mut blob, "mimeType", mime);
    if blob.contains_key("displayName") {
        insert_option(&mut blob, "displayName", filename);
    }
    part[key] = Value::Object(blob);
    Ok(())
}

fn tools_from_value(value: &Value) -> Result<Vec<LLMTool>, AdapterError> {
    let mut tools = Vec::new();
    for tool in value.as_array().into_iter().flatten() {
        for declaration in tool["functionDeclarations"]
            .as_array()
            .into_iter()
            .flatten()
        {
            tools.push(LLMTool {
                source: GEMINI,
                kind: LLMToolKind::Function {
                    name: required_string(declaration, "name")?,
                    description: string(&declaration["description"]),
                    parameters: declaration
                        .get("parametersJsonSchema")
                        .or_else(|| declaration.get("parameters"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    strict: None,
                },
                metadata: extensions_without(
                    declaration,
                    &["name", "description", "parametersJsonSchema"],
                ),
            });
        }
        if let Some(config) = tool
            .as_object()
            .map(|_| extensions_without(tool, &["functionDeclarations"]))
            .filter(|config| !config.is_empty())
        {
            let name = config.keys().next().map(String::as_str).unwrap_or_default();
            let name = match name {
                "googleSearch" | "googleSearchRetrieval" => "web_search",
                "codeExecution" => "code_interpreter",
                "computerUse" => "computer",
                _ => name,
            }
            .to_owned();
            tools.push(LLMTool {
                source: GEMINI,
                kind: LLMToolKind::Builtin {
                    protocol: GEMINI,
                    name,
                    config: Value::Object(config),
                },
                metadata: Extensions::new(),
            });
        }
    }
    Ok(tools)
}

fn tools_to_value(tools: &[LLMTool]) -> Result<Vec<Value>, AdapterError> {
    let mut declarations = Vec::new();
    let mut builtins = Vec::new();
    for tool in tools {
        match &tool.kind {
            LLMToolKind::Function {
                name,
                description,
                parameters,
                ..
            } => {
                let mut declaration = if tool.source == GEMINI {
                    tool.metadata.clone()
                } else {
                    Extensions::new()
                };
                declaration.insert("name".into(), json!(name));
                insert_option(&mut declaration, "description", description.as_ref());
                if !parameters.is_null() {
                    let key = if declaration.contains_key("parameters") {
                        "parameters"
                    } else {
                        "parametersJsonSchema"
                    };
                    declaration.insert(key.into(), parameters.clone());
                }
                declarations.push(Value::Object(declaration));
            }
            LLMToolKind::Builtin {
                protocol: GEMINI,
                config,
                ..
            } => builtins.push(config.clone()),
            LLMToolKind::Builtin { protocol, name, .. } => {
                return Err(AdapterError::Unsupported(format!(
                    "Gemini cannot represent {protocol:?} built-in tool `{name}`"
                )));
            }
        }
    }
    let mut output = Vec::new();
    if !declarations.is_empty() {
        output.push(json!({"functionDeclarations": declarations}));
    }
    output.extend(builtins);
    Ok(output)
}

fn tool_choice_from_value(value: &Value) -> Option<LLMToolChoice> {
    Some(match value["mode"].as_str()? {
        "AUTO" => LLMToolChoice::Auto,
        "NONE" => LLMToolChoice::None,
        "ANY" => match value["allowedFunctionNames"].as_array().map(Vec::as_slice) {
            Some([name]) => LLMToolChoice::Function {
                name: name.as_str()?.to_owned(),
            },
            Some(names) if !names.is_empty() => LLMToolChoice::Raw {
                value: value.clone(),
            },
            _ => LLMToolChoice::Required,
        },
        _ => LLMToolChoice::Raw {
            value: value.clone(),
        },
    })
}

fn tool_choice_to_value(choice: &LLMToolChoice, previous: &Value) -> Value {
    let mut config = value_as_extensions(previous);
    let mode = match choice {
        LLMToolChoice::Auto => "AUTO",
        LLMToolChoice::None => "NONE",
        LLMToolChoice::Required => "ANY",
        LLMToolChoice::Function { name } => {
            config.insert("allowedFunctionNames".into(), json!([name]));
            "ANY"
        }
        LLMToolChoice::Raw { value } => return value.clone(),
    };
    config.insert("mode".into(), json!(mode));
    Value::Object(config)
}

impl ResponseAdapter for GeminiAdapter {
    fn to_llm_response(value: Value) -> Result<LLMResponse, AdapterError> {
        let mut choices: Vec<_> = value["candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, candidate)| {
                let content = parts_from_value(&candidate["content"])?;
                let tool_calls = content
                    .iter()
                    .any(|part| matches!(part.kind, LLMContentKind::ToolCall { .. }));
                Ok(LLMChoice {
                    index: candidate["index"]
                        .as_u64()
                        .map_or(index, |index| index as usize),
                    role: LLMRole::Assistant,
                    finish_reason: finish_from_value(&candidate["finishReason"], tool_calls),
                    content,
                    metadata: candidate_metadata(candidate),
                })
            })
            .collect::<Result<_, AdapterError>>()?;
        if choices.is_empty() && prompt_blocked(value.get("promptFeedback")) {
            choices.push(LLMChoice {
                index: 0,
                role: LLMRole::Assistant,
                finish_reason: Some(LLMFinishReason::ContentFilter),
                content: Vec::new(),
                metadata: Extensions::new(),
            });
        }
        Ok(LLMResponse {
            source: GEMINI,
            id: string(&value["responseId"]),
            model: string(&value["modelVersion"]),
            choices,
            usage: usage_from_value(&value["usageMetadata"]),
            metadata: Extensions::new(),
            extra: extensions_without(
                &value,
                &["candidates", "responseId", "modelVersion", "usageMetadata"],
            ),
        })
    }

    fn from_llm_response(response: &LLMResponse) -> Result<Value, AdapterError> {
        let mut value = if response.source == GEMINI {
            response.extra.clone()
        } else {
            Extensions::new()
        };
        insert_option(&mut value, "responseId", response.id.as_ref());
        insert_option(&mut value, "modelVersion", response.model.as_ref());
        insert_option(
            &mut value,
            "usageMetadata",
            response.usage.as_ref().map(usage_to_value),
        );
        if !response.choices.is_empty() && !prompt_blocked(value.get("promptFeedback")) {
            let candidates = response
                .choices
                .iter()
                .enumerate()
                .map(|(position, choice)| {
                    let mut candidate = if response.source == GEMINI {
                        choice.metadata.clone()
                    } else {
                        Extensions::new()
                    };
                    if candidate.contains_key("index")
                        || position != choice.index
                        || response.source != GEMINI
                    {
                        candidate.insert("index".into(), json!(choice.index));
                    }
                    if !choice.content.is_empty() || candidate.contains_key("content") {
                        let mut content = candidate
                            .get("content")
                            .map_or_else(Extensions::new, value_as_extensions);
                        if response.source != GEMINI {
                            content.insert("role".into(), json!("model"));
                        }
                        let parts = choice
                            .content
                            .iter()
                            .map(|part| part_to_value(part, &BTreeMap::new()))
                            .collect::<Result<Vec<_>, _>>()?;
                        content.insert("parts".into(), Value::Array(parts));
                        candidate.insert("content".into(), Value::Object(content));
                    }
                    set_finish_reason(&mut candidate, choice.finish_reason);
                    Ok(Value::Object(candidate))
                })
                .collect::<Result<Vec<_>, AdapterError>>()?;
            value.insert("candidates".into(), json!(candidates));
        }
        Ok(Value::Object(value))
    }
}

fn prompt_blocked(feedback: Option<&Value>) -> bool {
    feedback
        .and_then(|feedback| feedback["blockReason"].as_str())
        .is_some_and(|reason| reason != "BLOCK_REASON_UNSPECIFIED")
}

fn candidate_metadata(candidate: &Value) -> Extensions {
    let mut metadata = value_as_extensions(candidate);
    if let Some(content) = metadata.get_mut("content") {
        *content = json!(extensions_without(content, &["parts"]));
    }
    metadata
}

fn finish_from_value(value: &Value, tool_calls: bool) -> Option<LLMFinishReason> {
    Some(match value.as_str()? {
        "FINISH_REASON_UNSPECIFIED" => return None,
        "STOP" if tool_calls => LLMFinishReason::ToolCalls,
        "STOP" => LLMFinishReason::Stop,
        "MAX_TOKENS" => LLMFinishReason::MaxTokens,
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION" => LLMFinishReason::ContentFilter,
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "TOO_MANY_TOOL_CALLS" => {
            LLMFinishReason::Failed
        }
        _ => LLMFinishReason::Unknown,
    })
}

fn set_finish_reason(candidate: &mut Extensions, reason: Option<LLMFinishReason>) {
    let Some(reason) = reason else { return };
    let tool_calls = matches!(
        reason,
        LLMFinishReason::ToolCalls | LLMFinishReason::ToolUse | LLMFinishReason::FunctionCall
    );
    if candidate
        .get("finishReason")
        .and_then(|value| finish_from_value(value, tool_calls))
        == Some(reason)
    {
        return;
    }
    let reason = match reason {
        LLMFinishReason::Length | LLMFinishReason::MaxTokens | LLMFinishReason::ContextLimit => {
            "MAX_TOKENS"
        }
        LLMFinishReason::ContentFilter | LLMFinishReason::Refusal => "SAFETY",
        LLMFinishReason::Stop
        | LLMFinishReason::EndTurn
        | LLMFinishReason::StopSequence
        | LLMFinishReason::ToolCalls
        | LLMFinishReason::ToolUse
        | LLMFinishReason::FunctionCall
        | LLMFinishReason::Completed => "STOP",
        _ => "OTHER",
    };
    candidate.insert("finishReason".into(), json!(reason));
}

const USAGE_FIELDS: &[&str] = &[
    "promptTokenCount",
    "candidatesTokenCount",
    "totalTokenCount",
    "cachedContentTokenCount",
    "thoughtsTokenCount",
];

fn usage_from_value(value: &Value) -> Option<LLMUsage> {
    value.as_object()?;
    Some(LLMUsage {
        source: Some(GEMINI),
        input_tokens: value["promptTokenCount"].as_u64(),
        output_tokens: value["candidatesTokenCount"].as_u64(),
        total_tokens: value["totalTokenCount"].as_u64(),
        cache_read_tokens: value["cachedContentTokenCount"].as_u64(),
        reasoning_tokens: value["thoughtsTokenCount"].as_u64(),
        extra: extensions_without(value, USAGE_FIELDS),
        ..LLMUsage::default()
    })
}

fn usage_to_value(usage: &LLMUsage) -> Value {
    let mut value = if usage.source == Some(GEMINI) {
        usage.extra.clone()
    } else {
        Extensions::new()
    };
    insert_option(&mut value, "promptTokenCount", usage.input_tokens);
    insert_option(&mut value, "candidatesTokenCount", usage.output_tokens);
    insert_option(&mut value, "totalTokenCount", usage.total_tokens);
    insert_option(
        &mut value,
        "cachedContentTokenCount",
        usage.cache_read_tokens,
    );
    insert_option(&mut value, "thoughtsTokenCount", usage.reasoning_tokens);
    Value::Object(value)
}

impl StreamAdapter for GeminiAdapter {
    fn parse_stream_event(
        event: &SseEvent,
        state: &mut StreamState,
    ) -> Result<Vec<LLMStreamEvent>, AdapterError> {
        if event.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| AdapterError::InvalidField("stream_json"))?;
        if let Some(error) = value.get("error") {
            return Ok(vec![LLMStreamEvent::Error {
                message: string(&error["message"])
                    .unwrap_or_else(|| "upstream stream error".into()),
                metadata: value_as_extensions(error),
            }]);
        }

        let mut events = Vec::new();
        if !state.started {
            state.started = true;
            state.id = string(&value["responseId"]);
            state.model = string(&value["modelVersion"]);
            events.push(LLMStreamEvent::MessageStart {
                id: state.id.clone(),
                model: state.model.clone(),
                usage: None,
            });
        }
        if let Some(usage) = usage_from_value(&value["usageMetadata"]) {
            state.usage = Some(usage.clone());
            events.push(LLMStreamEvent::Usage { usage });
        }
        if state.ended {
            return Ok(events);
        }

        // The normalized stream carries one message, so only candidate zero has content events.
        let candidate = value["candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .find(|(position, candidate)| {
                candidate["index"]
                    .as_u64()
                    .map_or(*position == 0, |index| index == 0)
            })
            .map(|(_, candidate)| candidate);
        if let Some(candidate) = candidate {
            for part in parts_from_value(&candidate["content"])? {
                let index = state.next_content_index;
                state.next_content_index += 1;
                stream_part(part, index, state, &mut events);
            }
        }
        let reason = if prompt_blocked(value.get("promptFeedback")) {
            Some(LLMFinishReason::ContentFilter)
        } else {
            candidate.and_then(|candidate| {
                finish_from_value(&candidate["finishReason"], !state.tool_names.is_empty())
            })
        };
        if let Some(reason) = reason {
            state.ended = true;
            state.finish_reason = Some(reason);
            let mut metadata = extensions_without(
                &value,
                &["candidates", "responseId", "modelVersion", "usageMetadata"],
            );
            if let Some(candidate) = candidate {
                let candidate = extensions_without(candidate, &["content"]);
                metadata.insert("candidates".into(), json!([candidate]));
            }
            events.push(LLMStreamEvent::MessageEnd {
                finish_reason: Some(reason),
                usage: state.usage.clone(),
                metadata,
            });
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
                    state.started = true;
                    state.id = id.clone();
                    state.model = model.clone();
                    state.usage = usage.clone();
                }
                LLMStreamEvent::ContentStart { index, content } => match &content.kind {
                    LLMContentKind::Text { text, .. } if text.is_empty() => {
                        state.pending_content.insert(*index, content.clone());
                    }
                    LLMContentKind::Reasoning {
                        text: None,
                        signature: None,
                        summary,
                        encrypted_content: None,
                    } if summary.is_empty() => {
                        state.pending_content.insert(*index, content.clone());
                    }
                    LLMContentKind::ToolCall { .. } => {
                        state.pending_content.insert(*index, content.clone());
                    }
                    _ => output.push(stream_chunk(
                        state,
                        part_to_value(content, &BTreeMap::new())?,
                    )),
                },
                LLMStreamEvent::TextDelta { index, text } => {
                    let content = state.pending_content.remove(index);
                    let mut part = content.as_ref().map_or_else(Extensions::new, |content| {
                        content_extensions_for(content, GEMINI)
                    });
                    part.insert("text".into(), json!(text));
                    output.push(stream_chunk(state, Value::Object(part)));
                }
                LLMStreamEvent::ReasoningDelta {
                    index,
                    text,
                    signature,
                } => {
                    let content = state.pending_content.remove(index);
                    let mut part = content.as_ref().map_or_else(Extensions::new, |content| {
                        content_extensions_for(content, GEMINI)
                    });
                    if content
                        .as_ref()
                        .is_some_and(|content| content.source == Some(GEMINI))
                    {
                        insert_option(&mut part, "thoughtSignature", signature.as_ref());
                    }
                    if text.is_some() || part.contains_key("thoughtSignature") {
                        insert_option(&mut part, "text", text.as_ref());
                        part.insert("thought".into(), json!(true));
                        output.push(stream_chunk(state, Value::Object(part)));
                    }
                }
                LLMStreamEvent::RefusalDelta { refusal, .. } => {
                    output.push(stream_chunk(state, json!({"text": refusal})))
                }
                LLMStreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    let pending =
                        state
                            .pending_content
                            .entry(*index)
                            .or_insert_with(|| LLMContent {
                                source: None,
                                kind: LLMContentKind::ToolCall {
                                    id: None,
                                    name: String::new(),
                                    arguments: json!({}),
                                },
                                metadata: Extensions::new(),
                            });
                    if let LLMContentKind::ToolCall {
                        id: call_id,
                        name: call_name,
                        ..
                    } = &mut pending.kind
                    {
                        if let Some(id) = id {
                            *call_id = Some(id.clone());
                        }
                        if let Some(name) = name {
                            *call_name = name.clone();
                        }
                        state
                            .content_buffers
                            .entry(*index)
                            .or_default()
                            .push_str(arguments_delta);
                    }
                }
                LLMStreamEvent::ContentEnd { index } => {
                    if let Some(mut content) = state.pending_content.remove(index) {
                        if let LLMContentKind::ToolCall { arguments, .. } = &mut content.kind
                            && let Some(buffer) = state
                                .content_buffers
                                .remove(index)
                                .filter(|buffer| !buffer.is_empty())
                        {
                            *arguments = serde_json::from_str(&buffer)
                                .map_err(|_| AdapterError::InvalidField("functionCall.args"))?;
                        }
                        output.push(stream_chunk(
                            state,
                            part_to_value(&content, &BTreeMap::new())?,
                        ));
                    }
                }
                LLMStreamEvent::Usage { usage } => {
                    state.usage = Some(usage.clone());
                    if state.ended {
                        output.push(SseEvent::json(
                            None,
                            &json!({"usageMetadata": usage_to_value(usage)}),
                        ));
                    }
                }
                LLMStreamEvent::MessageEnd {
                    finish_reason,
                    usage,
                    metadata,
                } => {
                    let mut value = metadata.clone();
                    if !value.contains_key("promptFeedback") || value.contains_key("candidates") {
                        let mut candidate = value
                            .get("candidates")
                            .and_then(|candidates| candidates.get(0))
                            .map_or_else(Extensions::new, value_as_extensions);
                        set_finish_reason(
                            &mut candidate,
                            finish_reason.or(Some(LLMFinishReason::Stop)),
                        );
                        value.insert("candidates".into(), json!([candidate]));
                    }
                    insert_option(
                        &mut value,
                        "usageMetadata",
                        usage.as_ref().or(state.usage.as_ref()).map(usage_to_value),
                    );
                    insert_option(&mut value, "responseId", state.id.as_ref());
                    insert_option(&mut value, "modelVersion", state.model.as_ref());
                    output.push(SseEvent::json(None, &Value::Object(value)));
                    state.ended = true;
                }
                LLMStreamEvent::Error { message, metadata } => {
                    let mut error = metadata.clone();
                    error.insert("message".into(), json!(message));
                    output.push(SseEvent::json(None, &json!({"error": error})));
                }
                LLMStreamEvent::Raw {
                    protocol: GEMINI,
                    value,
                } => output.push(SseEvent::json(None, value)),
                LLMStreamEvent::Raw { protocol, .. } => {
                    return Err(AdapterError::Unsupported(format!(
                        "Gemini cannot represent {protocol:?} raw stream events"
                    )));
                }
                LLMStreamEvent::AnnotationAdded { .. } => {}
            }
        }
        Ok(output)
    }
}

fn stream_part(
    mut content: LLMContent,
    index: usize,
    state: &mut StreamState,
    events: &mut Vec<LLMStreamEvent>,
) {
    let delta = match &mut content.kind {
        LLMContentKind::Text { text, .. } => Some(LLMStreamEvent::TextDelta {
            index,
            text: std::mem::take(text),
        }),
        LLMContentKind::Reasoning {
            text, signature, ..
        } => Some(LLMStreamEvent::ReasoningDelta {
            index,
            text: text.take(),
            signature: signature.take(),
        }),
        LLMContentKind::ToolCall {
            name, arguments, ..
        } => {
            state.tool_names.insert(index, name.clone());
            Some(LLMStreamEvent::ToolCallDelta {
                index,
                id: None,
                name: None,
                arguments_delta: arguments.to_string(),
            })
        }
        _ => None,
    };
    events.push(LLMStreamEvent::ContentStart { index, content });
    events.extend(delta);
    events.push(LLMStreamEvent::ContentEnd { index });
}

fn stream_chunk(state: &StreamState, part: Value) -> SseEvent {
    let mut value =
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [part]}}]});
    if let Some(id) = &state.id {
        value["responseId"] = json!(id);
    }
    if let Some(model) = &state.model {
        value["modelVersion"] = json!(model);
    }
    SseEvent::json(None, &value)
}

fn insert_option(target: &mut Extensions, key: &str, value: Option<impl Serialize>) {
    if let Some(value) = value {
        target.insert(key.into(), json!(value));
    }
}

fn string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn required_string(value: &Value, field: &'static str) -> Result<String, AdapterError> {
    string(&value[field]).ok_or(AdapterError::MissingField(field))
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::adapters::{
        anthropic::AnthropicAdapter, openai_chat::OpenAiChatAdapter,
        openai_responses::OpenAiResponsesAdapter,
    };

    #[test]
    fn rejects_nondefault_foreign_request_options() {
        for (field, value) in [
            ("n", json!(2)),
            ("seed", json!(42)),
            ("logprobs", json!(true)),
        ] {
            let mut input = json!({"model": "gemini-2.5-flash", "messages": [{"role": "user", "content": "ping"}]});
            input[field] = value;
            let request = OpenAiChatAdapter::to_llm_request(input).unwrap();
            assert!(matches!(GeminiAdapter::from_llm_request(&request),
                Err(AdapterError::Unsupported(message)) if message.contains(field)));
        }
        let request = OpenAiChatAdapter::to_llm_request(json!({
            "model": "gemini-2.5-flash", "messages": [{"role": "user", "content": "ping"}],
            "n": 1, "logprobs": false, "store": false,
            "stream": true, "stream_options": {"include_usage": true}
        }))
        .unwrap();
        assert!(GeminiAdapter::from_llm_request(&request).is_ok());
    }

    #[test]
    fn rejects_foreign_reasoning_controls_instead_of_using_model_defaults() {
        let requests = [
            OpenAiChatAdapter::to_llm_request(json!({
                "model": "gemini-2.5-flash", "messages": [{"role": "user", "content": "ping"}],
                "reasoning_effort": "none"
            }))
            .unwrap(),
            OpenAiResponsesAdapter::to_llm_request(json!({
                "model": "gemini-2.5-flash", "input": "ping", "reasoning": {"effort": "high"}
            }))
            .unwrap(),
            AnthropicAdapter::to_llm_request(json!({
                "model": "gemini-2.5-flash", "messages": [{"role": "user", "content": "ping"}],
                "max_tokens": 4096, "thinking": {"type": "enabled", "budget_tokens": 1024}
            }))
            .unwrap(),
        ];
        for mut request in requests {
            assert!(matches!(GeminiAdapter::from_llm_request(&request),
                Err(AdapterError::Unsupported(message)) if message.contains("reasoning")));
            for empty in [Value::Null, json!({})] {
                request.reasoning = Some(empty);
                assert!(GeminiAdapter::from_llm_request(&request).is_ok());
            }
        }
    }

    #[test]
    fn converts_chat_text_response_to_a_gemini_candidate() {
        let response = OpenAiChatAdapter::to_llm_response(json!({
            "choices": [{"message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}]
        })).unwrap();
        let output = GeminiAdapter::from_llm_response(&response).unwrap();
        assert_eq!(
            output["candidates"][0]["content"],
            json!({"role": "model", "parts": [{"text": "pong"}]})
        );
    }

    #[test]
    fn blocked_prompt_preserves_native_feedback_and_client_finish_reasons() {
        let native = json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 3}});
        let response = GeminiAdapter::to_llm_response(native.clone()).unwrap();
        assert_eq!(GeminiAdapter::from_llm_response(&response).unwrap(), native);
        let chat = OpenAiChatAdapter::from_llm_response(&response).unwrap();
        assert_eq!(chat["choices"][0]["finish_reason"], "content_filter");
        let responses = OpenAiResponsesAdapter::from_llm_response(&response).unwrap();
        assert_eq!(responses["incomplete_details"]["reason"], "content_filter");
        let messages = AnthropicAdapter::from_llm_response(&response).unwrap();
        assert_eq!(messages["stop_reason"], "refusal");
    }

    #[test]
    fn chat_tool_history_preserves_gemini_signature() {
        let part = json!({"thoughtSignature": "c2ln", "functionCall": {"id": "call_1", "name": "lookup", "args": {"id": 2}}});
        let native = json!({"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP"}]});
        let response = GeminiAdapter::to_llm_response(native.clone()).unwrap();
        let chat = OpenAiChatAdapter::from_llm_response(&response).unwrap();
        let assistant = &chat["choices"][0]["message"];
        assert_eq!(
            assistant["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "c2ln"
        );
        let request = OpenAiChatAdapter::to_llm_request(json!({"model": "gemini", "messages": [
            assistant, {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
        ]}))
        .unwrap();
        assert_eq!(
            GeminiAdapter::from_llm_request(&request).unwrap()["contents"][0]["parts"][0],
            part
        );

        let events = GeminiAdapter::parse_stream_event(
            &SseEvent::json(None, &native),
            &mut StreamState::default(),
        )
        .unwrap();
        let chunks =
            OpenAiChatAdapter::format_stream_events(&events, &mut StreamState::default()).unwrap();
        let calls: Vec<_> = chunks
            .iter()
            .filter_map(|chunk| {
                let body: Value = serde_json::from_str(&chunk.data).ok()?;
                body["choices"][0]["delta"]["tool_calls"].get(0).cloned()
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "lookup");
        assert_eq!(
            calls[0]["extra_content"]["google"]["thought_signature"],
            "c2ln"
        );
        assert!(calls[1].get("id").is_none());
        assert!(calls[1]["function"].get("name").is_none());
        assert_eq!(calls[1]["function"]["arguments"], r#"{"id":2}"#);
    }

    #[test]
    fn streams_omitted_tool_arguments_as_an_empty_object() {
        let native = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"ping"}}]},"finishReason":"STOP"}]});
        let events = GeminiAdapter::parse_stream_event(
            &SseEvent::json(None, &native),
            &mut StreamState::default(),
        )
        .unwrap();
        for chunks in [
            OpenAiChatAdapter::format_stream_events(&events, &mut StreamState::default()).unwrap(),
            OpenAiResponsesAdapter::format_stream_events(&events, &mut StreamState::default())
                .unwrap(),
        ] {
            let args = chunks
                .iter()
                .filter_map(|chunk| {
                    let value: Value = serde_json::from_str(&chunk.data).ok()?;
                    if value["type"] == "response.function_call_arguments.delta" {
                        value["delta"].as_str().map(str::to_owned)
                    } else {
                        value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                            .as_str()
                            .map(str::to_owned)
                    }
                })
                .collect::<String>();
            assert_eq!(serde_json::from_str::<Value>(&args).unwrap(), json!({}));
        }
        let chunks =
            GeminiAdapter::format_stream_events(&events, &mut StreamState::default()).unwrap();
        let first: Value = serde_json::from_str(&chunks[0].data).unwrap();
        assert!(
            first["candidates"][0]["content"]["parts"][0]["functionCall"]
                .get("args")
                .is_none()
        );
    }

    #[test]
    fn tool_signatures_round_trip_through_responses_and_messages() {
        let native = json!({"candidates":[{"content":{"parts":[{
            "functionCall":{"name":"lookup","args":{"q":"hi"}}, "thoughtSignature":"c2ln"
        }]},"finishReason":"STOP"}]});
        let response = GeminiAdapter::to_llm_response(native.clone()).unwrap();
        let responses = OpenAiResponsesAdapter::from_llm_response(&response).unwrap();
        let messages = AnthropicAdapter::from_llm_response(&response).unwrap();
        let requests = [
            OpenAiResponsesAdapter::to_llm_request(json!({"model":"gemini","input":responses["output"]})).unwrap(),
            AnthropicAdapter::to_llm_request(json!({"model":"gemini","max_tokens":10,"messages":[{"role":"assistant","content":messages["content"]}]})).unwrap(),
        ];
        for request in requests {
            let value = GeminiAdapter::from_llm_request(&request).unwrap();
            assert_eq!(value["contents"][0]["parts"][0]["thoughtSignature"], "c2ln");
        }

        let events = GeminiAdapter::parse_stream_event(
            &SseEvent::json(None, &native),
            &mut StreamState::default(),
        )
        .unwrap();
        let chunks =
            OpenAiResponsesAdapter::format_stream_events(&events, &mut StreamState::default())
                .unwrap();
        let items = chunks
            .iter()
            .filter_map(|chunk| {
                let value: Value = serde_json::from_str(&chunk.data).ok()?;
                matches!(
                    value["type"].as_str(),
                    Some("response.output_item.added" | "response.output_item.done")
                )
                .then(|| value["item"].clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 2);
        assert!(items[0]["call_id"].as_str().is_some());
        assert_eq!(items[0]["call_id"], items[1]["call_id"]);
        for item in &items {
            assert_eq!(item["extra_content"]["google"]["thought_signature"], "c2ln");
        }
        let request =
            OpenAiResponsesAdapter::to_llm_request(json!({"model":"gemini","input":[items[1]]}))
                .unwrap();
        assert_eq!(
            GeminiAdapter::from_llm_request(&request).unwrap()["contents"][0]["parts"][0]["thoughtSignature"],
            "c2ln"
        );

        let completed: Value = chunks
            .iter()
            .filter_map(|chunk| serde_json::from_str::<Value>(&chunk.data).ok())
            .find(|value| value["type"] == "response.completed")
            .unwrap();
        assert_eq!(
            completed["response"]["output"][0]["extra_content"]["google"]["thought_signature"],
            "c2ln"
        );
        let mut parsed = StreamState::default();
        let mut formatted = StreamState::default();
        let output = chunks
            .iter()
            .flat_map(|chunk| {
                let events =
                    OpenAiResponsesAdapter::parse_stream_event(chunk, &mut parsed).unwrap();
                GeminiAdapter::format_stream_events(&events, &mut formatted).unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            output
                .iter()
                .filter_map(|chunk| serde_json::from_str::<Value>(&chunk.data).ok())
                .any(
                    |value| value["candidates"][0]["content"]["parts"][0]["thoughtSignature"]
                        == "c2ln"
                )
        );

        let chunks =
            AnthropicAdapter::format_stream_events(&events, &mut StreamState::default()).unwrap();
        let block = chunks
            .iter()
            .find_map(|chunk| {
                let value: Value = serde_json::from_str(&chunk.data).ok()?;
                (value["type"] == "content_block_start").then(|| value["content_block"].clone())
            })
            .unwrap();
        assert_eq!(
            block["extra_content"]["google"]["thought_signature"],
            "c2ln"
        );
        let mut parsed = StreamState::default();
        let mut formatted = StreamState::default();
        let output = chunks
            .iter()
            .flat_map(|chunk| {
                let events = AnthropicAdapter::parse_stream_event(chunk, &mut parsed).unwrap();
                GeminiAdapter::format_stream_events(&events, &mut formatted).unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            output
                .iter()
                .filter_map(|chunk| serde_json::from_str::<Value>(&chunk.data).ok())
                .any(
                    |value| value["candidates"][0]["content"]["parts"][0]["thoughtSignature"]
                        == "c2ln"
                )
        );
    }

    #[test]
    fn converts_native_request_and_preserves_provider_fields() {
        let input = json!({
            "systemInstruction": {"parts": [{"text": "Be brief"}, {"text": "Use metric units"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "Read this"}, {"inlineData": {"mimeType": "image/png", "data": "YWJj"}}]},
                {"role": "model", "parts": [{"functionCall": {"id": "call_1", "name": "weather", "args": {"city": "Paris"}}, "thoughtSignature": "c2ln"}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "call_1", "name": "weather", "response": {"output": "sunny"}}}]}
            ],
            "generationConfig": {"temperature": 0.2, "maxOutputTokens": 128, "stopSequences": ["END"], "thinkingConfig": {"thinkingBudget": 32}, "topK": 40},
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_MEDIUM_AND_ABOVE"}],
            "cachedContent": "cachedContents/abc",
            "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["weather"]}},
            "tools": [{"functionDeclarations": [{"name": "weather", "description": "Weather", "parameters": {"type": "OBJECT"}}]}, {"googleSearch": {}}]
        });
        let request =
            GeminiAdapter::to_llm_request_with_model(input.clone(), "gemini-2.5-pro", true)
                .unwrap();
        assert_eq!(request.model, "gemini-2.5-pro");
        assert!(request.stream);
        assert_eq!(request.stop, Some(json!(["END"])));
        assert_eq!(request.messages[0].role, LLMRole::System);
        assert_eq!(request.messages[0].content.len(), 2);
        assert!(matches!(
            request.messages[1].content[1].kind,
            LLMContentKind::Image { .. }
        ));
        assert!(
            matches!(&request.messages[3].content[0].kind, LLMContentKind::ToolResult { tool_call_id, .. } if tool_call_id == "call_1")
        );
        assert_eq!(
            request.tool_choice,
            Some(LLMToolChoice::Function {
                name: "weather".into()
            })
        );
        assert_eq!(GeminiAdapter::from_llm_request(&request).unwrap(), input);
    }

    #[test]
    fn encodes_openai_tool_history_and_structured_output() {
        use crate::adapters::openai_chat::OpenAiChatAdapter;
        let request = OpenAiChatAdapter::to_llm_request(json!({
            "model": "gemini-2.5-pro",
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "response_format": {"type": "json_schema", "json_schema": {"name": "weather_result", "schema": {"type": "object"}}}
        })).unwrap();
        let output = GeminiAdapter::from_llm_request(&request).unwrap();
        assert_eq!(
            output["contents"][0]["parts"][0]["functionCall"]["args"],
            json!({"city": "Paris"})
        );
        assert_eq!(
            output["contents"][1]["parts"][0]["functionResponse"],
            json!({"id": "call_1", "name": "weather", "response": {"output": "sunny"}})
        );
        assert_eq!(
            output["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            output["generationConfig"]["responseJsonSchema"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn converts_candidates_usage_and_finish_reasons() {
        let input = json!({
            "responseId": "response_1", "modelVersion": "gemini-2.5-pro",
            "candidates": [
                {"index": 0, "content": {"role": "model", "parts": [{"functionCall": {"name": "weather", "args": {}}}]}, "finishReason": "STOP"},
                {"index": 1, "finishReason": "BLOCKLIST", "safetyRatings": []}
            ],
            "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 4, "totalTokenCount": 19, "thoughtsTokenCount": 3, "cachedContentTokenCount": 7}
        });
        let response = GeminiAdapter::to_llm_response(input.clone()).unwrap();
        assert_eq!(
            response.choices[0].finish_reason,
            Some(LLMFinishReason::ToolCalls)
        );
        assert_eq!(
            response.choices[1].finish_reason,
            Some(LLMFinishReason::ContentFilter)
        );
        assert_eq!(response.usage.as_ref().unwrap().reasoning_tokens, Some(3));
        assert_eq!(GeminiAdapter::from_llm_response(&response).unwrap(), input);
    }

    #[test]
    fn streams_text_thoughts_and_complete_function_calls() {
        let chunks = [
            json!({"responseId": "res", "modelVersion": "gemini", "candidates": [{"content": {"parts": [{"text": "check", "thought": true}, {"text": "hi"}]}}]}),
            json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "lookup", "args": {"id": 2}}, "thoughtSignature": "c2ln"}]}}]}),
            json!({"candidates": [{"finishReason": "STOP"}], "usageMetadata": {"candidatesTokenCount": 5}}),
        ];
        let mut input_state = StreamState::default();
        let mut output_state = StreamState::default();
        let events = chunks
            .iter()
            .flat_map(|chunk| {
                GeminiAdapter::parse_stream_event(&SseEvent::json(None, chunk), &mut input_state)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(&events[0], LLMStreamEvent::MessageStart { id: Some(id), .. } if id == "res")
        );
        assert!(events.iter().any(|event| matches!(event, LLMStreamEvent::ReasoningDelta { text: Some(text), .. } if text == "check")));
        assert!(
            events.iter().any(
                |event| matches!(event, LLMStreamEvent::TextDelta { text, .. } if text == "hi")
            )
        );
        assert!(
            matches!(events.last(), Some(LLMStreamEvent::MessageEnd { finish_reason: Some(LLMFinishReason::ToolCalls), usage: Some(usage), .. }) if usage.output_tokens == Some(5))
        );
        let output = events
            .iter()
            .flat_map(|event| {
                GeminiAdapter::format_stream_events(std::slice::from_ref(event), &mut output_state)
                    .unwrap()
            })
            .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
            .collect::<Vec<_>>();
        assert!(
            output
                .iter()
                .any(|value| value["candidates"][0]["content"]["parts"][0]
                    == chunks[1]["candidates"][0]["content"]["parts"][0])
        );
        assert_eq!(
            output.last().unwrap()["candidates"][0]["finishReason"],
            "STOP"
        );
    }

    #[test]
    fn buffers_openai_tool_arguments_until_the_call_ends() {
        let input = [
            SseEvent::json(
                None,
                &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "extra_content": {"google": {"thought_signature": "c2ln"}}, "function": {"name": "lookup", "arguments": "{\"id\":"}}]}}]}),
            ),
            SseEvent::json(
                None,
                &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "2}"}}]}, "finish_reason": "tool_calls"}]}),
            ),
            SseEvent {
                data: "[DONE]".into(),
                ..SseEvent::default()
            },
        ];
        let mut source = StreamState::default();
        let mut target = StreamState::default();
        let output = input
            .iter()
            .map(|chunk| {
                let events = OpenAiChatAdapter::parse_stream_event(chunk, &mut source).unwrap();
                GeminiAdapter::format_stream_events(&events, &mut target).unwrap()
            })
            .collect::<Vec<_>>();
        assert!(output[0].is_empty());
        assert!(output[1].is_empty());
        let value: Value = serde_json::from_str(&output[2][0].data).unwrap();
        let part = &value["candidates"][0]["content"]["parts"][0];
        assert_eq!(part["thoughtSignature"], "c2ln");
        let call = &part["functionCall"];
        assert_eq!(
            call,
            &json!({"id": "call_1", "name": "lookup", "args": {"id": 2}})
        );
        let end: Value = serde_json::from_str(&output[2][1].data).unwrap();
        assert_eq!(end["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn blocked_stream_retains_feedback_and_ends_without_candidate() {
        let value = json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 8}});
        let events = GeminiAdapter::parse_stream_event(
            &SseEvent::json(None, &value),
            &mut StreamState::default(),
        )
        .unwrap();
        assert!(matches!(
            events.last(),
            Some(LLMStreamEvent::MessageEnd {
                finish_reason: Some(LLMFinishReason::ContentFilter),
                ..
            })
        ));
        let output =
            GeminiAdapter::format_stream_events(&events, &mut StreamState::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&output.last().unwrap().data).unwrap(),
            value
        );
    }
}
