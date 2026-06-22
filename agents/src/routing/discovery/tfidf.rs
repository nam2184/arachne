use std::collections::{HashMap, HashSet};
use std::path::Path;

use walkdir::WalkDir;

use crate::routing::tokenizer;

use super::{is_noise_dir, is_text_extension, DiscoveryTechnique, SessionSummary};

pub struct TfIdfDiscovery {
    pub min_df: usize,
    pub max_df_ratio: f64,
    pub top_k: usize,
}

impl Default for TfIdfDiscovery {
    fn default() -> Self {
        Self {
            min_df: 1,
            max_df_ratio: 0.65,
            top_k: 30,
        }
    }
}

impl DiscoveryTechnique for TfIdfDiscovery {
    fn name(&self) -> &'static str {
        "tfidf"
    }

    fn analyze(&self, root: &Path) -> SessionSummary {
        let mut docs = Vec::<Vec<String>>::new();
        let mut df = HashMap::<String, usize>::new();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| !is_noise_dir(&entry.file_name().to_string_lossy()))
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !is_text_extension(ext) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let tokens = tokenizer::tokenize(&text, true);
            if tokens.is_empty() {
                continue;
            }
            let unique = tokens.iter().cloned().collect::<HashSet<_>>();
            for token in unique {
                *df.entry(token).or_default() += 1;
            }
            docs.push(tokens);
        }
        let n = docs.len();
        if n == 0 {
            return SessionSummary::default();
        }
        let max_df = ((n as f64) * self.max_df_ratio).ceil() as usize;
        let mut scores = HashMap::<String, f64>::new();
        for tokens in docs {
            let mut tf = HashMap::<String, usize>::new();
            for token in tokens {
                *tf.entry(token).or_default() += 1;
            }
            let total = tf.values().sum::<usize>().max(1) as f64;
            for (token, count) in tf {
                let doc_count = *df.get(&token).unwrap_or(&1);
                if doc_count < self.min_df || doc_count > max_df {
                    continue;
                }
                let idf = ((n as f64) / (doc_count as f64)).ln() + 1.0;
                *scores.entry(token).or_default() += ((count as f64) / total) * idf;
            }
        }
        let mut ranked = scores.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(self.top_k);
        SessionSummary {
            topic_signature: ranked.into_iter().map(|(token, _)| token).collect(),
            ..Default::default()
        }
    }
}
