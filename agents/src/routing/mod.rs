pub mod context_block;
pub mod discovery;
pub mod integration;
pub mod matchers;
pub mod resolver;
pub mod tokenizer;
pub mod virtual_group;

pub use context_block::{ContextBlock, PeersContextBlock, StackSummary};
pub use discovery::{DiscoveryDispatcher, SessionSummary, StackCategory, StackOutput};
pub use virtual_group::{GroupMember, VirtualGroup};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StructuredQuery {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub preferred_language: Option<String>,
    #[serde(default)]
    pub category: Option<StackCategory>,
    #[serde(default)]
    pub allow_fanout: bool,
    #[serde(default)]
    pub threshold: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteQuery {
    pub prompt: String,
    pub structured: StructuredQuery,
}

#[derive(Debug, Clone)]
pub struct RankedMember {
    pub session_id: String,
    pub score: f64,
    pub matched_tokens: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RouteResult {
    pub ranked: Vec<RankedMember>,
    pub group_relevance: f64,
}

#[derive(Debug, Clone)]
pub struct Router {
    pub group_threshold: f64,
    pub member_threshold: f64,
    pub use_stemming: bool,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            group_threshold: 0.02,
            member_threshold: 0.0,
            use_stemming: true,
        }
    }
}

impl Router {
    pub fn route(&self, query: &RouteQuery, group: &VirtualGroup) -> RouteResult {
        let mut tokens = tokenizer::tokenize(&query.prompt, self.use_stemming);
        for keyword in &query.structured.keywords {
            tokens.extend(tokenizer::tokenize(keyword, self.use_stemming));
        }
        tokens.sort();
        tokens.dedup();

        let group_relevance = matchers::jaccard::score(&tokens, group.topic_signature());
        let mut ranked = group
            .members()
            .iter()
            .map(|member| {
                let mut score = matchers::jaccard::score(&tokens, &member.summary.topic_signature);
                if let Some(lang) = &query.structured.preferred_language {
                    if member.summary.stack.main_language.as_deref() == Some(lang.as_str()) {
                        score += 0.15;
                    } else {
                        score *= 0.75;
                    }
                }
                if let Some(category) = query.structured.category {
                    if member.summary.stack.dominant_categories.contains(&category) {
                        score += 0.10;
                    }
                }
                let matched_tokens = matched_tokens(&tokens, &member.summary.topic_signature);
                RankedMember {
                    session_id: member.session_id.clone(),
                    score,
                    matched_tokens,
                }
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        RouteResult {
            ranked,
            group_relevance,
        }
    }
}

fn matched_tokens(query: &[String], signature: &[String]) -> Vec<String> {
    let signature = signature
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    query
        .iter()
        .filter(|token| signature.contains(token.as_str()))
        .cloned()
        .collect()
}
