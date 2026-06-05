use tauri::State;
use crate::domain::{AgentSession, Message, MessageRole};
use crate::services::agent_runtime::AgentRuntime;
use crate::services::memory_service::MemoryService;
use std::sync::Arc;

#[tauri::command]
pub async fn create_agent_session(
    project_id: String,
    provider: String,
    model: String,
    agent_runtime: State<'_, Arc<AgentRuntime>>,
) -> Result<String, String> {
    let session_id = agent_runtime.create_agent(project_id, provider, model);
    Ok(session_id)
}

#[tauri::command]
pub async fn send_message(
    session_id: String,
    message: String,
    agent_runtime: State<'_, Arc<AgentRuntime>>,
) -> Result<String, String> {
    agent_runtime.send_message(&session_id, &message)
}

#[tauri::command]
pub async fn update_agent_context(
    session_id: String,
    files: Vec<String>,
    agent_runtime: State<'_, Arc<AgentRuntime>>,
) -> Result<(), String> {
    agent_runtime.update_context(&session_id, files);
    Ok(())
}

#[tauri::command]
pub async fn add_memory_fact(
    session_id: String,
    fact: String,
    agent_runtime: State<'_, Arc<AgentRuntime>>,
) -> Result<(), String> {
    agent_runtime.add_memory_fact(&session_id, fact);
    Ok(())
}

#[tauri::command]
pub async fn parse_code_context(
    session_id: String,
    content: String,
    language: String,
    agent_runtime: State<'_, Arc<AgentRuntime>>,
) -> Result<String, String> {
    agent_runtime.parse_code_for_context(&session_id, &content, &language)
}