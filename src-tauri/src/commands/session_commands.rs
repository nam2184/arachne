use arachne_agents::{AgentSession, ConversationService, SessionGroup, SessionService};
use std::sync::Arc;
use tauri::State;

#[derive(serde::Serialize)]
pub struct SessionInitPayload {
    sessions: Vec<AgentSession>,
    groups: Vec<SessionGroup>,
}

#[tauri::command]
pub async fn init_sessions(
    session_service: State<'_, Arc<SessionService>>,
) -> Result<SessionInitPayload, String> {
    Ok(SessionInitPayload {
        sessions: session_service.get_all_sessions()?,
        groups: session_service.get_all_groups()?,
    })
}

#[tauri::command]
pub async fn create_session(
    project_id: String,
    directory: String,
    provider: String,
    model: String,
    session_service: State<'_, Arc<SessionService>>,
    conversation_service: State<'_, Arc<ConversationService>>,
) -> Result<String, String> {
    let (id, created) =
        session_service.create_top_level_session(project_id, directory, provider, model)?;
    if created {
        if let Err(error) = conversation_service.create_conversation(&id) {
            let _ = session_service.delete_session(&id);
            return Err(error);
        }
    }
    Ok(id)
}

#[tauri::command]
pub async fn get_session(
    id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<Option<AgentSession>, String> {
    session_service.get_session(&id)
}

#[tauri::command]
pub async fn get_all_sessions(
    session_service: State<'_, Arc<SessionService>>,
) -> Result<Vec<AgentSession>, String> {
    session_service.get_all_sessions()
}

#[tauri::command]
pub async fn delete_session(
    id: String,
    session_service: State<'_, Arc<SessionService>>,
    conversation_service: State<'_, Arc<ConversationService>>,
) -> Result<(), String> {
    conversation_service.delete_conversation(&id)?;
    session_service.delete_session(&id)
}

#[tauri::command]
pub async fn create_session_group(
    session_ids: Vec<String>,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<String, String> {
    session_service.create_group(session_ids)
}

#[tauri::command]
pub async fn get_all_session_groups(
    session_service: State<'_, Arc<SessionService>>,
) -> Result<Vec<SessionGroup>, String> {
    session_service.get_all_groups()
}

#[tauri::command]
pub async fn delete_session_group(
    id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.delete_group(&id)
}

#[tauri::command]
pub async fn rename_session_group(
    id: String,
    name: Option<String>,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.rename_group(&id, name)
}

#[tauri::command]
pub async fn add_session_to_group(
    session_id: String,
    group_id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.add_session_to_group(&session_id, &group_id)
}

#[tauri::command]
pub async fn remove_session_from_group(
    session_id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.remove_session_from_group(&session_id)
}
