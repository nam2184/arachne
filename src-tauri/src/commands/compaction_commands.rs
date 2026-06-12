use std::sync::Arc;
use tauri::State;

use crate::services::agent_service::AgentService;

#[derive(serde::Serialize)]
pub struct CompactOutcomePayload {
    pub status: String,
    pub summary: String,
}

#[tauri::command]
pub async fn compact_now(
    session_id: String,
    agent_service: State<'_, Arc<AgentService>>,
) -> Result<CompactOutcomePayload, String> {
    match agent_service.compact_now(&session_id).await {
        Ok(arachne_agents::CompactionOutcome::Compacted { summary }) => Ok(CompactOutcomePayload {
            status: "compacted".to_string(),
            summary,
        }),
        Ok(arachne_agents::CompactionOutcome::NotNeeded) => Ok(CompactOutcomePayload {
            status: "not_needed".to_string(),
            summary: String::new(),
        }),
        Ok(arachne_agents::CompactionOutcome::Failed { reason }) => Err(reason),
        Err(error) => Err(error),
    }
}
