use serde_json::{json, Value};

use super::super::{
    adapters::{AdapterError, RequestAdapter, ResponseAdapter, StreamAdapter},
    model::{text_content, LLMMessage, LLMRequest, LLMResponse, LLMRole, LLMStreamEvent},
};

pub struct GeminiAdapter;

impl GeminiAdapter {
    pub fn to_llm_request_with_model(
        value: Value,
        model: &str,
        stream: bool,
    ) -> Result<LLMRequest, AdapterError> {
        let mut messages = Vec::new();
        for item in value["contents"]
            .as_array()
            .ok_or(AdapterError::MissingField("contents"))?
        {
            let role = match item["role"].as_str().unwrap_or("user") {
                "model" => LLMRole::Assistant,
                _ => LLMRole::User,
            };
            let text = item["parts"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            messages.push(LLMMessage {
                role,
                content: text_content(text),
                name: None,
                tool_call_id: None,
                metadata: json!({}),
            });
        }
        Ok(LLMRequest {
            model: model.to_string(),
            system: value["systemInstruction"]["parts"][0]["text"]
                .as_str()
                .map(ToString::to_string),
            messages,
            temperature: value["generationConfig"]["temperature"].as_f64(),
            max_tokens: value["generationConfig"]["maxOutputTokens"].as_u64(),
            top_p: value["generationConfig"]["topP"].as_f64(),
            stop: value.get("safetySettings").cloned(),
            stream,
            tools: value["tools"].as_array().cloned().unwrap_or_default(),
            metadata: json!({ "source": "gemini" }),
        })
    }
}

impl RequestAdapter for GeminiAdapter {
    fn to_llm_request(value: Value) -> Result<LLMRequest, AdapterError> {
        Self::to_llm_request_with_model(value, "gemini-pro", false)
    }

    fn from_llm_request(request: &LLMRequest) -> Result<Value, AdapterError> {
        let contents: Vec<Value> = request
            .messages
            .iter()
            .map(|message| {
                json!({
                    "role": match message.role {
                        LLMRole::Assistant => "model",
                        _ => "user",
                    },
                    "parts": [{"text": super::openai_chat::join_text(&message.content)}]
                })
            })
            .collect();
        let mut value = json!({
            "contents": contents,
            "generationConfig": {}
        });
        if let Some(system) = &request.system {
            value["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if let Some(temperature) = request.temperature {
            value["generationConfig"]["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            value["generationConfig"]["maxOutputTokens"] = json!(max_tokens);
        }
        if let Some(top_p) = request.top_p {
            value["generationConfig"]["topP"] = json!(top_p);
        }
        Ok(value)
    }
}

impl ResponseAdapter for GeminiAdapter {
    fn to_llm_response(value: Value) -> Result<LLMResponse, AdapterError> {
        let text = value["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        Ok(LLMResponse {
            id: None,
            model: None,
            content: text_content(text),
            finish_reason: value["candidates"][0]["finishReason"]
                .as_str()
                .map(ToString::to_string),
            usage: value.get("usageMetadata").cloned(),
            metadata: json!({ "source": "gemini" }),
        })
    }

    fn from_llm_response(response: &LLMResponse) -> Result<Value, AdapterError> {
        Ok(json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": super::openai_chat::join_text(&response.content)}]
                },
                "finishReason": response.finish_reason.clone().unwrap_or_else(|| "STOP".into())
            }],
            "usageMetadata": response.usage.clone().unwrap_or_else(|| json!({}))
        }))
    }
}

impl StreamAdapter for GeminiAdapter {
    fn parse_stream_event(event: &str) -> Result<Option<LLMStreamEvent>, AdapterError> {
        let Some(data) = super::sse_data(event) else {
            return Ok(None);
        };
        let value: Value =
            serde_json::from_str(data).map_err(|_| AdapterError::InvalidField("stream_json"))?;
        let text = value["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or_default();
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(LLMStreamEvent::TextDelta {
            text: text.to_string(),
        }))
    }

    fn format_stream_event(event: &LLMStreamEvent) -> Result<Option<String>, AdapterError> {
        match event {
            LLMStreamEvent::TextDelta { text } => Ok(Some(format!(
                "data: {}\n\n",
                json!({"candidates":[{"content":{"role":"model","parts":[{"text":text}]}}]})
            ))),
            LLMStreamEvent::MessageEnd { .. } => Ok(Some("data: {}\n\n".to_string())),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gemini_request_to_llm_request() {
        let value = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"temperature": 0.2, "maxOutputTokens": 128}
        });
        let request =
            GeminiAdapter::to_llm_request_with_model(value, "gemini-2.5-pro", false).unwrap();
        assert_eq!(request.model, "gemini-2.5-pro");
        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(request.max_tokens, Some(128));
    }
}
