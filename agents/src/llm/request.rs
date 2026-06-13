use serde::{Deserialize, Serialize};

use super::events::{FinishReason, LlmEvent, ToolDefinition, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: Vec<ContentPart>,
}

impl LlmMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![ContentPart::Text {
                text: content.into(),
            }],
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: vec![ContentPart::Text {
                text: content.into(),
            }],
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: vec![ContentPart::Text {
                text: content.into(),
            }],
        }
    }

    pub fn tool(id: &str, name: &str, result: serde_json::Value) -> Self {
        Self {
            role: "tool".to_string(),
            content: vec![ContentPart::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                result,
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        name: String,
        result: serde_json::Value,
    },
    Reasoning {
        text: String,
    },
}

impl ContentPart {
    pub fn text<S: Into<String>>(text: S) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn tool_call(id: &str, name: &str, input: serde_json::Value) -> Self {
        Self::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    pub fn tool_result(id: &str, name: &str, result: serde_json::Value) -> Self {
        Self::ToolResult {
            id: id.to_string(),
            name: name.to_string(),
            result,
        }
    }

    pub fn reasoning<S: Into<String>>(text: S) -> Self {
        Self::Reasoning { text: text.into() }
    }

    pub fn as_prompt_text(&self) -> Option<String> {
        match self {
            Self::Text { text } => {
                // Drop empty text blocks before serializing: they
                // contribute nothing to the model but still cost
                // tokens in array form (`{"type":"text","text":""}`)
                // and waste cache prefix bytes.
                if text.is_empty() {
                    None
                } else {
                    Some(text.clone())
                }
            }
            Self::ToolCall { id, name, input } => Some(format!(
                "<tool_call id=\"{id}\" name=\"{name}\">\n<input>{}</input>\n</tool_call>",
                serde_json::to_string(input).unwrap_or_else(|_| "null".to_string())
            )),
            Self::ToolResult { id, name, result } => Some(format!(
                "<tool_result tool_call_id=\"{id}\" name=\"{name}\">\n{}\n</tool_result>",
                serde_json::to_string(result).unwrap_or_else(|_| "null".to_string())
            )),
            Self::Reasoning { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub provider: String,
    pub system: Vec<String>,
    pub messages: Vec<LlmMessage>,
    /// Tool definitions advertised to the model via the request body.
    /// Each provider lowers this list into its native wire format
    /// (`tools: [...]` for OpenAI-Chat / Anthropic-Messages, etc.).
    /// The model is expected to return tool calls as structured
    /// events (`delta.tool_calls` for OpenAI-Chat, `tool_use` content
    /// blocks for Anthropic), which the providers translate into
    /// `LlmEvent::ToolInput*` / `LlmEvent::ToolCall` events.
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub stop: Option<Vec<String>>,
    /// Stable session identifier. Providers use this to set
    /// `prompt_cache_key` (OpenAI), `prompt_cache_key` /
    /// `prompt_cache_key` (OpenRouter), or `x-session-affinity`
    /// (Anthropic). Same value across all turns of a session
    /// pins the request to the same backend shard and keeps the
    /// automatic prompt cache warm.
    pub session_id: Option<String>,
}

impl LlmRequest {
    pub fn new(model: &str, provider: &str) -> Self {
        Self {
            model: model.to_string(),
            provider: provider.to_string(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            session_id: None,
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system.push(system.into());
        self
    }

    pub fn with_message(mut self, message: LlmMessage) -> Self {
        self.messages.push(message);
        self
    }

    pub fn with_messages(mut self, messages: impl IntoIterator<Item = LlmMessage>) -> Self {
        self.messages.extend(messages);
        self
    }

    /// No-op. Tools are no longer advertised through the request body;
    /// the runner injects them into the system prompt instead. Kept so
    /// external callers don't need to be updated; new code should pass
    /// `tools` via the provider's request builder.
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub reasoning: Option<String>,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
    pub tool_calls: Vec<ToolCallEntry>,
}

impl Default for LlmResponse {
    fn default() -> Self {
        Self {
            content: String::new(),
            reasoning: None,
            finish_reason: FinishReason::Unknown,
            usage: None,
            tool_calls: Vec::new(),
        }
    }
}

impl LlmResponse {
    pub fn from_events(events: Vec<LlmEvent>) -> Self {
        let mut content = String::new();
        let mut reasoning = None;
        let mut finish_reason = FinishReason::Unknown;
        let mut usage = None;
        let mut tool_calls = Vec::new();

        for event in events {
            match event {
                LlmEvent::TextDelta { text, .. } => {
                    content.push_str(&text);
                }
                LlmEvent::ReasoningDelta { text, .. } => {
                    if reasoning.is_none() {
                        reasoning = Some(String::new());
                    }
                    reasoning.as_mut().unwrap().push_str(&text);
                }
                LlmEvent::ToolCall {
                    id, name, input, ..
                } => {
                    tool_calls.push(ToolCallEntry { id, name, input });
                }
                LlmEvent::StepFinish {
                    reason, usage: u, ..
                } => {
                    finish_reason = reason;
                    usage = u;
                }
                LlmEvent::Finish { reason, usage: u } => {
                    finish_reason = reason;
                    usage = u;
                }
                _ => {}
            }
        }

        Self {
            content,
            reasoning,
            finish_reason,
            usage,
            tool_calls,
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ToolCallEntry {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct LlmError {
    pub code: String,
    pub message: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl LlmError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            provider: None,
            model: None,
        }
    }

    pub fn provider(mut self, provider: &str) -> Self {
        self.provider = Some(provider.to_string());
        self
    }

    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for LlmError {}

impl From<reqwest::Error> for LlmError {
    fn from(err: reqwest::Error) -> Self {
        Self {
            code: "network".to_string(),
            message: err.to_string(),
            provider: None,
            model: None,
        }
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(err: serde_json::Error) -> Self {
        Self {
            code: "parse".to_string(),
            message: err.to_string(),
            provider: None,
            model: None,
        }
    }
}
