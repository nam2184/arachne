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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub project_id: String,
    pub provider: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
}

impl AgentSession {
    pub fn new(project_id: String, provider: String, model: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            project_id,
            provider,
            model,
            created_at: Utc::now(),
        }
    }
}