use std::sync::Arc;

pub use openman_agents::runtime::AgentRuntime;
use openman_agents::runtime::CodeContextProvider;

use crate::services::tree_sitter::TreeSitterService;

impl CodeContextProvider for TreeSitterService {
    fn query_functions(&self, content: &str, language: &str) -> Result<Vec<String>, String> {
        TreeSitterService::query_functions(self, content, language)
    }
}

pub fn create_agent_runtime(tree_sitter: Arc<TreeSitterService>) -> Arc<AgentRuntime> {
    AgentRuntime::with_code_context(tree_sitter)
}
