use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWebSearchConfig {
    #[serde(default)]
    pub searxng_base_url: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

impl RuntimeWebSearchConfig {
    pub fn merge(&mut self, next: Self) {
        if next.searxng_base_url.is_some() {
            self.searxng_base_url = next.searxng_base_url;
        }
        if next.max_results.is_some() {
            self.max_results = next.max_results;
        }
    }
}
