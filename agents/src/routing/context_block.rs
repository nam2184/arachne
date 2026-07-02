use crate::routing::discovery::{SessionSummary, StackOutput};
use crate::AgentSession;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StackSummary {
    pub main_language: Option<String>,
    pub all_languages: Vec<String>,
    pub libs: Vec<String>,
    pub manifests: Vec<String>,
    pub entry_points: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextBlock {
    pub session_id: String,
    pub directory: String,
    pub one_liner: String,
    pub topic_signature: Vec<String>,
    pub stack: StackSummary,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeersContextBlock {
    pub caller_session_id: String,
    pub group_id: Option<String>,
    pub peers: Vec<ContextBlock>,
}

impl ContextBlock {
    pub fn from_session_and_summary(session: &AgentSession, summary: &SessionSummary) -> Self {
        Self {
            session_id: session.id.clone(),
            directory: session.directory.clone(),
            one_liner: summary.one_liner.clone(),
            topic_signature: summary.topic_signature.clone(),
            stack: StackSummary::from_stack(&summary.stack),
            capabilities: vec![
                "read".into(),
                "glob".into(),
                "grep".into(),
                "webfetch".into(),
            ],
        }
    }
}

impl StackSummary {
    fn from_stack(stack: &StackOutput) -> Self {
        Self {
            main_language: stack.main_language.clone(),
            all_languages: stack.all_languages.clone(),
            libs: stack.libs.clone(),
            manifests: stack
                .manifests
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            entry_points: stack
                .entry_points
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        }
    }
}

impl PeersContextBlock {
    pub fn render(&self) -> Option<String> {
        if self.peers.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str("<peers>\n");
        out.push_str(&format!(
            "Main session: {}. You are currently operating in this main session; peer sessions below are separate connected sessions.\n",
            self.caller_session_id
        ));
        if let Some(group_id) = &self.group_id {
            out.push_str(&format!("Connected group: {group_id}\n"));
        }
        out.push_str("The following peer sessions are real sessions connected to the main session. In plan mode, you may pass `peer_session_id` to read, glob, or grep to inspect one of these different connected sessions. Omit `peer_session_id` for main-session/local/current-repo work. Do not target child sessions spawned by `task`.\n\n");
        for peer in &self.peers {
            out.push_str(&format!("- peer_session_id=\"{}\"\n", peer.session_id));
            out.push_str(&format!("  directory: {}\n", peer.directory));
            if !peer.one_liner.trim().is_empty() {
                out.push_str(&format!("  summary: {}\n", peer.one_liner.trim()));
            }
            if let Some(lang) = &peer.stack.main_language {
                out.push_str(&format!("  main_language: {lang}\n"));
            }
            if !peer.stack.libs.is_empty() {
                out.push_str(&format!("  libs: {}\n", peer.stack.libs.join(", ")));
            }
            if !peer.topic_signature.is_empty() {
                out.push_str(&format!(
                    "  topic_signature: [{}]\n",
                    peer.topic_signature
                        .iter()
                        .take(15)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            out.push_str(&format!(
                "  capabilities: {}\n\n",
                peer.capabilities.join(", ")
            ));
        }
        out.push_str("</peers>");
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_context_names_main_session_and_peers() {
        let block = PeersContextBlock {
            caller_session_id: "main-session".to_string(),
            group_id: Some("group-1".to_string()),
            peers: vec![ContextBlock {
                session_id: "peer-session".to_string(),
                directory: "/work/peer".to_string(),
                one_liner: "Peer project".to_string(),
                topic_signature: vec!["rust".to_string()],
                stack: StackSummary {
                    main_language: Some("Rust".to_string()),
                    all_languages: vec!["Rust".to_string()],
                    libs: vec!["tokio".to_string()],
                    manifests: Vec::new(),
                    entry_points: Vec::new(),
                },
                capabilities: vec!["read".to_string(), "glob".to_string()],
            }],
        };

        let rendered = block.render().expect("peer context should render");

        assert!(rendered.contains("Main session: main-session"));
        assert!(rendered.contains("peer sessions are real sessions connected to the main session"));
        assert!(rendered.contains("peer_session_id=\"peer-session\""));
        assert!(
            rendered.contains("Omit `peer_session_id` for main-session/local/current-repo work")
        );
    }
}
