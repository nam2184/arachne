use std::sync::Arc;
use tauri::{AppHandle, State};

use crate::services::agent_service::AgentService;

#[tauri::command]
pub async fn send_message(
    session_id: String,
    message: String,
    mode: Option<String>,
    app: AppHandle,
    agent_service: State<'_, Arc<AgentService>>,
) -> Result<String, String> {
    let mode = mode
        .as_deref()
        .map(parse_mode)
        .transpose()?
        .unwrap_or_default();
    agent_service
        .send_message(&session_id, message, mode, app)
        .await
}

fn parse_mode(value: &str) -> Result<arachne_agents::permission::PermissionMode, String> {
    use std::str::FromStr;
    arachne_agents::permission::PermissionMode::from_str(value)
        .map_err(|_| format!("Invalid mode: {value}"))
}

#[tauri::command]
pub async fn update_session_provider(
    session_id: String,
    provider: String,
    model: String,
    agent_service: State<'_, Arc<AgentService>>,
) -> Result<(), String> {
    agent_service
        .update_session_provider(&session_id, provider, model)
        .await
}

#[tauri::command]
pub async fn stop_session_stream(
    session_id: String,
    agent_service: State<'_, Arc<AgentService>>,
) -> Result<bool, String> {
    Ok(agent_service.send_cancel(&session_id))
}
