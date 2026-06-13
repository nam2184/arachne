//! Project-scoped long-term memory store. **Planned future API**.
//!
//! The shape (per-project `ProjectMemory` with `facts`, `patterns`,
//! and `recent_learns`) is the minimum surface the runner will need
//! to inject "what we've learned about this project" into the
//! system prompt on subsequent sessions. Today the runner does not
//! call this — it is a placeholder for the v2 work where the
//! compactor will also extract durable facts from the conversation
//! summary and store them here. Do not delete.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ProjectMemory {
    pub facts: Vec<String>,
    pub patterns: Vec<String>,
    pub recent_learns: Vec<String>,
}

#[derive(Default)]
pub struct MemoryStore {
    project_memories: RwLock<HashMap<String, ProjectMemory>>,
}

impl MemoryStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_fact(&self, project_id: &str, fact: String) {
        let mut memories = self.project_memories.write();
        let memory = memories.entry(project_id.to_string()).or_default();
        if !memory.facts.contains(&fact) {
            memory.facts.push(fact);
        }
    }

    pub fn get_facts(&self, project_id: &str) -> Vec<String> {
        self.project_memories
            .read()
            .get(project_id)
            .map(|memory| memory.facts.clone())
            .unwrap_or_default()
    }
}
