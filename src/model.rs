use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Extensions = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMRequest {
    pub source: LLMProtocol,
    pub model: String,
    #[serde(default)]
    pub messages: Vec<LLMMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<LLMTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<LLMToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default)]
    pub metadata: Extensions,
    #[serde(default)]
    pub extra: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LLMProtocol>,
    pub role: LLMRole,
    #[serde(default)]
    pub content: Vec<LLMContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub metadata: Extensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LLMRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LLMProtocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LLMProtocol>,
    #[serde(flatten)]
    pub kind: LLMContentKind,
    #[serde(default, flatten)]
    pub metadata: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMContentKind {
    Text {
        text: String,
        #[serde(default)]
        annotations: Vec<LLMAnnotation>,
    },
    Image {
        source: LLMMediaSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Audio {
        source: LLMMediaSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    File {
        source: LLMMediaSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        #[serde(default)]
        content: Vec<LLMContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    Refusal {
        refusal: String,
    },
    Raw {
        protocol: LLMProtocol,
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMMediaSource {
    Url {
        url: String,
    },
    Data {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
    FileId {
        file_id: String,
    },
    Raw {
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMTool {
    pub source: LLMProtocol,
    #[serde(flatten)]
    pub kind: LLMToolKind,
    #[serde(default, flatten)]
    pub metadata: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMToolKind {
    Function {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        parameters: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
    Builtin {
        protocol: LLMProtocol,
        name: String,
        config: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMToolChoice {
    Auto,
    None,
    Required,
    Function { name: String },
    Raw { value: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMAnnotation {
    pub source: LLMProtocol,
    pub annotation_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
    #[serde(default)]
    pub metadata: Extensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LLMFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    EndTurn,
    StopSequence,
    MaxTokens,
    ToolUse,
    PauseTurn,
    Refusal,
    ContextLimit,
    Completed,
    Incomplete,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LLMUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LLMProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_audio_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_audio_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u64>,
    #[serde(default)]
    pub extra: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMChoice {
    pub index: usize,
    pub role: LLMRole,
    #[serde(default)]
    pub content: Vec<LLMContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<LLMFinishReason>,
    #[serde(default)]
    pub metadata: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMResponse {
    pub source: LLMProtocol,
    pub id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<LLMChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LLMUsage>,
    #[serde(default)]
    pub metadata: Extensions,
    #[serde(default)]
    pub extra: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMStreamEvent {
    MessageStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<LLMUsage>,
    },
    ContentStart {
        index: usize,
        content: LLMContent,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ReasoningDelta {
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RefusalDelta {
        index: usize,
        refusal: String,
    },
    AnnotationAdded {
        index: usize,
        annotation: LLMAnnotation,
    },
    ToolCallDelta {
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    ContentEnd {
        index: usize,
    },
    Usage {
        usage: LLMUsage,
    },
    MessageEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<LLMFinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<LLMUsage>,
        #[serde(default)]
        metadata: Extensions,
    },
    Error {
        message: String,
        #[serde(default)]
        metadata: Extensions,
    },
    Raw {
        protocol: LLMProtocol,
        value: Value,
    },
}

impl LLMContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            source: None,
            kind: LLMContentKind::Text {
                text: text.into(),
                annotations: Vec::new(),
            },
            metadata: Extensions::new(),
        }
    }

    pub fn text_value(&self) -> Option<&str> {
        match &self.kind {
            LLMContentKind::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

impl LLMResponse {
    pub fn primary_choice(&self) -> Option<&LLMChoice> {
        self.choices.first()
    }
}

pub fn text_content(value: impl Into<String>) -> Vec<LLMContent> {
    vec![LLMContent::text(value)]
}

pub fn joined_text(content: &[LLMContent]) -> String {
    content
        .iter()
        .filter_map(LLMContent::text_value)
        .collect::<Vec<_>>()
        .join("")
}
