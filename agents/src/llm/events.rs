use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}

impl Default for FinishReason {
    fn default() -> Self {
        Self::Unknown
    }
}

impl FromStr for FinishReason {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "stop" => Ok(Self::Stop),
            "length" => Ok(Self::Length),
            "tool_calls" => Ok(Self::ToolCalls),
            "content_filter" => Ok(Self::ContentFilter),
            "error" => Ok(Self::Error),
            _ => Ok(Self::Unknown),
        }
    }
}

impl From<&str> for FinishReason {
    fn from(s: &str) -> Self {
        Self::from_str(s).unwrap_or(Self::Unknown)
    }
}

impl fmt::Display for FinishReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FinishReason::Stop => write!(f, "stop"),
            FinishReason::Length => write!(f, "length"),
            FinishReason::ToolCalls => write!(f, "tool_calls"),
            FinishReason::ContentFilter => write!(f, "content_filter"),
            FinishReason::Error => write!(f, "error"),
            FinishReason::Unknown => write!(f, "unknown"),
        }
    }
}

impl std::error::Error for FinishReason {}

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
}

impl Usage {
    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens
            .unwrap_or(0)
            .saturating_sub(self.reasoning_tokens.unwrap_or(0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolResultValue {
    Json { value: serde_json::Value },
    Text { value: String },
    Error { value: String },
    Content { value: Vec<ToolContentPart> },
}

impl ToolResultValue {
    pub fn text<S: Into<String>>(text: S) -> Self {
        Self::Text { value: text.into() }
    }

    pub fn error<S: Into<String>>(msg: S) -> Self {
        Self::Error { value: msg.into() }
    }

    pub fn json(value: impl serde::Serialize) -> Self {
        Self::Json {
            value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolContentPart {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum LlmEvent {
    StepStart {
        index: u32,
    },
    StepFinish {
        index: u32,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    TextStart {
        id: String,
    },
    TextDelta {
        id: String,
        text: String,
    },
    TextEnd {
        id: String,
    },
    ReasoningStart {
        id: String,
    },
    ReasoningDelta {
        id: String,
        text: String,
    },
    ReasoningEnd {
        id: String,
    },
    ToolInputStart {
        id: String,
        name: String,
    },
    ToolInputDelta {
        id: String,
        name: String,
        text: String,
    },
    ToolInputEnd {
        id: String,
        name: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
        provider_executed: Option<bool>,
    },
    ToolResult {
        id: String,
        name: String,
        result: ToolResultValue,
        output: Option<String>,
    },
    ToolError {
        id: String,
        name: String,
        message: String,
    },
    /// Emitted when the runner detects an XML tool call block inside
    /// streamed text (e.g. `<tool_call>…</tool_call>`, `<tool name="x">…</tool>`,
    /// `<invoke name="x">…</invoke>`, `<function_calls>…</function_calls>`,
    /// `<antml:function_calls>…</antml:function_calls>`, or a self-closing
    /// variant). The model emitted this instead of using the provider's
    /// native tool-calling API. The runner strips the block from the
    /// visible text, appends a `ContentPart::tool_result` describing the
    /// violation to the assistant message, and continues the turn so
    /// the model can self-correct on the next provider call. `raw`
    /// holds the inner body (or the whole block for self-closing tags
    /// and unterminated-at-flush cases) and is intended for diagnostics
    /// rather than execution.
    InvalidToolCall {
        id: String,
        name: String,
        raw: String,
    },
    Finish {
        reason: FinishReason,
        usage: Option<Usage>,
    },
    ProviderError {
        message: String,
    },
    /// Emitted when a parent session spawns child work. The UI can use this
    /// to render a spawned sub-agent indicator under the parent's bubble.
    TaskCall {
        id: String,
        child_session_id: String,
        kind: TaskKind,
        state: TaskState,
    },
    /// Emitted when a child session completes and its result is appended
    /// to the parent's context. `state` is one of `Completed` or `Error`.
    TaskResult {
        id: String,
        child_session_id: String,
        kind: TaskKind,
        state: TaskState,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Completed,
    Error,
}

impl LlmEvent {
    pub fn is_tool_call(&self) -> bool {
        matches!(self, LlmEvent::ToolCall { .. })
    }

    pub fn is_invalid_tool_call(&self) -> bool {
        matches!(self, LlmEvent::InvalidToolCall { .. })
    }

    pub fn is_step_finish(&self) -> bool {
        matches!(self, LlmEvent::StepFinish { .. })
    }

    pub fn is_finish(&self) -> bool {
        matches!(self, LlmEvent::Finish { .. })
    }

    pub fn is_text_delta(&self) -> bool {
        matches!(self, LlmEvent::TextDelta { .. })
    }

    pub fn is_reasoning_delta(&self) -> bool {
        matches!(self, LlmEvent::ReasoningDelta { .. })
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            LlmEvent::ToolCall { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Convenience constructor for `LlmEvent::InvalidToolCall`.
    pub fn invalid_tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        raw: impl Into<String>,
    ) -> Self {
        LlmEvent::InvalidToolCall {
            id: id.into(),
            name: name.into(),
            raw: raw.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn new(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
        }
    }
}
