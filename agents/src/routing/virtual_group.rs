use std::collections::HashMap;
use std::path::PathBuf;

use crate::routing::discovery::SessionSummary;

#[derive(Debug, Clone)]
pub struct GroupMember {
    pub session_id: String,
    pub directory: PathBuf,
    pub summary: SessionSummary,
}

#[derive(Debug, Clone, Default)]
pub struct VirtualGroup {
    members: Vec<GroupMember>,
    topic_signature: Vec<String>,
}

impl VirtualGroup {
    pub fn from_summaries(members: Vec<GroupMember>) -> Self {
        let mut group = Self {
            members,
            topic_signature: Vec::new(),
        };
        group.recompute_signature();
        group
    }

    pub fn members(&self) -> &[GroupMember] {
        &self.members
    }

    pub fn topic_signature(&self) -> &[String] {
        &self.topic_signature
    }

    fn recompute_signature(&mut self) {
        let mut scores = HashMap::<String, f64>::new();
        for member in &self.members {
            for (idx, token) in member.summary.topic_signature.iter().enumerate() {
                *scores.entry(token.clone()).or_default() += 1.0 / ((idx + 1) as f64);
            }
        }
        let mut ranked = scores.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(50);
        self.topic_signature = ranked.into_iter().map(|(token, _)| token).collect();
    }
}
