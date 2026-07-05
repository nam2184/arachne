use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub path: String,
    pub name: String,
    pub tech_stack: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl Project {
    pub fn new(path: String, name: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            path,
            name,
            tech_stack: Vec::new(),
            created_at: Utc::now(),
        }
    }

    pub fn container(name: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            path: String::new(),
            name,
            tech_stack: Vec::new(),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TechStack {
    pub languages: Vec<String>,
}

impl TechStack {
    pub fn new() -> Self {
        Self {
            languages: Vec::new(),
        }
    }

    pub fn add_language(&mut self, language: String) {
        if !self.languages.contains(&language) {
            self.languages.push(language);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub project_id: String,
    pub provider: Provider,
    pub context: AgentContext,
    pub memory: AgentMemory,
}

impl Agent {
    pub fn new(project_id: String, provider: Provider) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            project_id,
            provider,
            context: AgentContext::default(),
            memory: AgentMemory::default(),
        }
    }

    pub fn build_messages(&self, user_input: &str) -> Vec<LlmMessage> {
        let mut messages = vec![LlmMessage {
            role: "system".to_string(),
            content: format!(
                "You are an AI coding assistant. Languages detected in this project: {:?}.",
                self.context.languages
            ),
        }];

        for fact in &self.memory.project_facts {
            messages.push(LlmMessage {
                role: "system".to_string(),
                content: format!("Project fact: {fact}"),
            });
        }

        messages.push(LlmMessage {
            role: "user".to_string(),
            content: user_input.to_string(),
        });

        messages
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentContext {
    pub current_files: Vec<String>,
    pub languages: Vec<String>,
    pub recent_searches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentMemory {
    pub project_facts: Vec<String>,
    pub learned_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub project_id: String,
    pub directory: String,
    pub provider: String,
    pub model: String,
    pub title: Option<String>,
    pub group_id: Option<String>,
    pub summary_json: Option<String>,
    pub parent_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl AgentSession {
    pub fn new(project_id: String, directory: String, provider: String, model: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            project_id,
            directory,
            provider,
            model,
            title: None,
            group_id: None,
            summary_json: None,
            parent_session_id: None,
            created_at: Utc::now(),
        }
    }

    /// Construct a child session. Children inherit the parent's project, but
    /// get a fresh id and a `parent_session_id` link. The directory can be
    /// either the same as the parent (for subagent calls into the parent's own
    /// codebase) or a different one (for subagent worktrees, though we don't
    /// enforce worktrees today).
    pub fn child_of(
        parent: &AgentSession,
        directory: String,
        provider: String,
        model: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            project_id: parent.project_id.clone(),
            directory,
            provider,
            model,
            title: None,
            group_id: None,
            summary_json: None,
            parent_session_id: Some(parent.id.clone()),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub id: String,
    pub name: Option<String>,
    pub session_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl SessionGroup {
    pub fn new(session_ids: Vec<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: None,
            session_ids,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

impl Message {
    pub fn new(session_id: String, role: MessageRole, content: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id,
            role,
            content,
            timestamp: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

impl Provider {
    pub fn new(name: String, model: String) -> Self {
        Self {
            name,
            model,
            api_key: None,
            base_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub protocol: ProviderProtocol,
    pub enabled: bool,
    #[serde(skip)]
    pub auth_account_id: Option<String>,
    #[serde(skip)]
    pub auth_field_type: ProviderAuthFieldType,
}

impl ProviderConfig {
    pub fn new(name: String, model: String, protocol: ProviderProtocol) -> Self {
        Self {
            name,
            model,
            api_key: None,
            base_url: None,
            protocol,
            enabled: true,
            auth_account_id: None,
            auth_field_type: ProviderAuthFieldType::ApiKey,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProviderAuthFieldType {
    #[serde(rename = "API_KEY")]
    ApiKey,
    #[serde(rename = "OAUTH")]
    OAuth,
}

impl Default for ProviderAuthFieldType {
    fn default() -> Self {
        Self::ApiKey
    }
}

impl ProviderAuthFieldType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ApiKey => "API_KEY",
            Self::OAuth => "OAUTH",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "OAUTH" => Self::OAuth,
            _ => Self::ApiKey,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAuthState {
    pub provider_name: String,
    pub field_type: ProviderAuthFieldType,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    pub api_key: Option<String>,
}

impl ProviderAuthState {
    pub fn new(provider_name: String) -> Self {
        Self {
            provider_name,
            field_type: ProviderAuthFieldType::ApiKey,
            access_token: None,
            refresh_token: None,
            account_id: None,
            api_key: None,
        }
    }

    pub fn selected_token(&self) -> Option<String> {
        match self.field_type {
            ProviderAuthFieldType::ApiKey => self.api_key.clone(),
            ProviderAuthFieldType::OAuth => self.access_token.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderOAuthProfile {
    pub id: String,
    pub provider_name: String,
    pub label: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

impl ProviderOAuthProfile {
    pub fn new(
        provider_name: String,
        label: String,
        access_token: String,
        refresh_token: Option<String>,
        account_id: Option<String>,
        is_active: bool,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            provider_name,
            label,
            access_token,
            refresh_token,
            account_id,
            created_at: now,
            last_used_at: if is_active { Some(now) } else { None },
            is_active,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderProtocol {
    OpenAI,
    Anthropic,
}

impl ProviderProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "anthropic" => Self::Anthropic,
            _ => Self::OpenAI,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProviderType {
    Anthropic,
    OpenAI,
    OpenRouter,
    Ollama,
}

impl ProviderType {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAI),
            "openrouter" => Some(Self::OpenRouter),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    pub model: String,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

impl Tool {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: String,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}
