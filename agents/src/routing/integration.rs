use std::sync::Arc;

use crate::routing::resolver::build_virtual_group;
use crate::SessionService;

pub fn validate_connected_peer(
    caller_session_id: &str,
    peer_id: &str,
    session_service: &Arc<SessionService>,
) -> Result<(), String> {
    let (group_id, group) = build_virtual_group(caller_session_id, session_service)?;
    if group.members().is_empty() {
        return Err(
            "peer_session_id requires the caller to be connected to a session group".to_string(),
        );
    }
    if group
        .members()
        .iter()
        .any(|member| member.session_id == peer_id)
    {
        return Ok(());
    }
    Err(format!(
        "peer_session_id denied: '{peer_id}' is not connected to this session{}",
        group_id
            .as_deref()
            .map(|id| format!(" in group {id}"))
            .unwrap_or_default()
    ))
}
