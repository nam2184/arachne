use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("File operation failed: {0}")]
    FileError(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("LLM error: {0}")]
    LlmError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Parse error: {0}")]
    ParseError(String),
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}