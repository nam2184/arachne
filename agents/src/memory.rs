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
