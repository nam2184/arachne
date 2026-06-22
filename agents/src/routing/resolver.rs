use std::path::PathBuf;
use std::sync::Arc;

use crate::routing::discovery::SessionSummary;
use crate::routing::virtual_group::{GroupMember, VirtualGroup};
use crate::routing::{ContextBlock, PeersContextBlock};
use crate::SessionService;

pub fn build_virtual_group(
    caller_session_id: &str,
    session_service: &Arc<SessionService>,
) -> Result<(Option<String>, VirtualGroup), String> {
    let caller = session_service
        .get_session(caller_session_id)?
        .ok_or_else(|| format!("caller session not found: {caller_session_id}"))?;
    let Some(group_id) = caller.group_id.clone() else {
        return Ok((None, VirtualGroup::default()));
    };
    let sessions = session_service.sessions_in_group(&group_id)?;
    let members = sessions
        .into_iter()
        .filter(|session| session.id != caller.id)
        .filter(|session| session.parent_session_id.is_none())
        .map(|session| {
            let summary = session
                .summary_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<SessionSummary>(json).ok())
                .unwrap_or_default();
            GroupMember {
                session_id: session.id,
                directory: PathBuf::from(session.directory),
                summary,
            }
        })
        .collect();
    Ok((Some(group_id), VirtualGroup::from_summaries(members)))
}

pub fn build_context_block(
    caller_session_id: &str,
    session_service: &Arc<SessionService>,
) -> Result<PeersContextBlock, String> {
    let caller = session_service
        .get_session(caller_session_id)?
        .ok_or_else(|| format!("caller session not found: {caller_session_id}"))?;
    let (group_id, group) = build_virtual_group(caller_session_id, session_service)?;
    let peers = group
        .members()
        .iter()
        .map(|member| {
            let mut session = caller.clone();
            session.id = member.session_id.clone();
            session.directory = member.directory.display().to_string();
            ContextBlock::from_session_and_summary(&session, &member.summary)
        })
        .collect();
    Ok(PeersContextBlock {
        caller_session_id: caller.id,
        group_id,
        peers,
    })
}
