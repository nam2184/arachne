use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::domain::{Agent, Provider};
use crate::llm::LlmProvider;

pub trait CodeContextProvider: Send + Sync {
    fn query_functions(&self, content: &str, language: &str) -> Result<Vec<String>, String>;
}

#[derive(Default)]
pub struct AgentRuntime {
    agents: RwLock<HashMap<String, Agent>>,
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    code_context: RwLock<Option<Arc<dyn CodeContextProvider>>>,
}

impl AgentRuntime {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_code_context(code_context: Arc<dyn CodeContextProvider>) -> Arc<Self> {
        Arc::new(Self {
            code_context: RwLock::new(Some(code_context)),
            ..Self::default()
        })
    }

    pub fn register_provider(&self, name: String, provider: Arc<dyn LlmProvider>) {
        self.providers.write().insert(name, provider);
    }

    pub fn create_agent(&self, project_id: String, provider_name: String, model: String) -> String {
        let agent = Agent::new(project_id, Provider::new(provider_name, model));
        let id = agent.id.clone();
        self.agents.write().insert(id.clone(), agent);
        id
    }

    pub fn get_agent(&self, id: &str) -> Option<Agent> {
        self.agents.read().get(id).cloned()
    }

    pub fn send_message(&self, agent_id: &str, content: &str) -> Result<String, String> {
        let mut agents = self.agents.write();
        let agent = agents.get_mut(agent_id).ok_or("Agent not found")?;
        let messages = agent.build_messages(content);
        let provider = self
            .providers
            .read()
            .get(&agent.provider.name)
            .cloned()
            .ok_or("Provider not found")?;

        let response = provider.complete_sync(&agent.provider.model, &messages)?;
        agent.context.recent_searches.push(content.to_string());
        Ok(response)
    }

    pub fn update_context(&self, agent_id: &str, files: Vec<String>) {
        if let Some(agent) = self.agents.write().get_mut(agent_id) {
            agent.context.current_files = files;
        }
    }

    pub fn update_languages(&self, agent_id: &str, languages: Vec<String>) {
        if let Some(agent) = self.agents.write().get_mut(agent_id) {
            agent.context.languages = languages;
        }
    }

    pub fn add_memory_fact(&self, agent_id: &str, fact: String) {
        if let Some(agent) = self.agents.write().get_mut(agent_id) {
            agent.memory.project_facts.push(fact);
        }
    }

    pub fn parse_code_for_context(
        &self,
        agent_id: &str,
        content: &str,
        language: &str,
    ) -> Result<String, String> {
        if !self.agents.read().contains_key(agent_id) {
            return Err("Agent not found".to_string());
        }

        let code_context = self
            .code_context
            .read()
            .clone()
            .ok_or("Code context provider not configured")?;

        Ok(code_context.query_functions(content, language)?.join(", "))
    }
}
