use std::sync::Arc;
use parking_lot::RwLock;
use crate::domain::{Message, MessageRole};

pub struct MemoryService {
    project_memories: RwLock<std::collections::HashMap<String, ProjectMemory>>,
}

#[derive(Debug, Clone)]
struct ProjectMemory {
    facts: Vec<String>,
    patterns: Vec<String>,
    recent_learns: Vec<String>,
}

impl Default for ProjectMemory {
    fn default() -> Self {
        Self {
            facts: Vec::new(),
            patterns: Vec::new(),
            recent_learns: Vec::new(),
        }
    }
}

impl MemoryService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_fact(&self, project_id: &str, fact: String) {
        let mut memories = self.project_memories.write();
        let memory = memories.entry(project_id.to_string()).or_default();
        if !memory.facts.contains(&fact) {
            memory.facts.push(fact.clone());
        }
    }

    pub fn add_pattern(&self, project_id: &str, pattern: String) {
        let mut memories = self.project_memories.write();
        let memory = memories.entry(project_id.to_string()).or_default();
        if !memory.patterns.contains(&pattern) {
            memory.patterns.push(pattern);
        }
    }

    pub fn add_recent_learn(&self, project_id: &str, learn: String) {
        let mut memories = self.project_memories.write();
        let memory = memories.entry(project_id.to_string()).or_default();
        memory.recent_learns.push(learn);
        if memory.recent_learns.len() > 50 {
            memory.recent_learns.remove(0);
        }
    }

    pub fn get_facts(&self, project_id: &str) -> Vec<String> {
        let memories = self.project_memories.read();
        memories.get(project_id).map(|m| m.facts.clone()).unwrap_or_default()
    }

    pub fn get_patterns(&self, project_id: &str) -> Vec<String> {
        let memories = self.project_memories.read();
        memories.get(project_id).map(|m| m.patterns.clone()).unwrap_or_default()
    }

    pub fn get_recent_learns(&self, project_id: &str) -> Vec<String> {
        let memories = self.project_memories.read();
        memories.get(project_id).map(|m| m.recent_learns.clone()).unwrap_or_default()
    }

    pub fn clear_project_memory(&self, project_id: &str) {
        self.project_memories.write().remove(project_id);
    }

    pub fn save_to_disk(&self, project_id: &str, path: &std::path::Path) -> Result<(), String> {
        let memories = self.project_memories.read();
        let memory = memories.get(project_id).ok_or("Project memory not found")?;

        let json = serde_json::to_string_pretty(memory).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    pub fn load_from_disk(&self, project_id: &str, path: &std::path::Path) -> Result<(), String> {
        let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let memory: ProjectMemory = serde_json::from_str(&content).map_err(|e| e.to_string())?;

        let mut memories = self.project_memories.write();
        memories.insert(project_id.to_string(), memory);
        Ok(())
    }
}

impl Default for MemoryService {
    fn default() -> Self {
        Self {
            project_memories: RwLock::new(std::collections::HashMap::new()),
        }
    }
}
