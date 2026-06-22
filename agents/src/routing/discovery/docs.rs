use std::path::Path;

use walkdir::WalkDir;

use super::{is_noise_dir, DiscoveryTechnique, SessionSummary};

#[derive(Default)]
pub struct DocDiscovery;

impl DiscoveryTechnique for DocDiscovery {
    fn name(&self) -> &'static str {
        "docs"
    }

    fn analyze(&self, root: &Path) -> SessionSummary {
        let mut out = SessionSummary::default();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| !is_noise_dir(&entry.file_name().to_string_lossy()))
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if out.readme_excerpt.is_none() && name.to_ascii_lowercase().starts_with("readme") {
                if let Ok(text) = std::fs::read_to_string(path) {
                    let excerpt = text.lines().take(12).collect::<Vec<_>>().join(" ");
                    out.one_liner = excerpt.chars().take(220).collect::<String>();
                    out.readme_excerpt = Some(excerpt.chars().take(1000).collect());
                }
            }
            if out.module_docs.len() < 20 {
                if let Ok(text) = std::fs::read_to_string(path) {
                    if let Some(doc) = module_doc(&text) {
                        out.module_docs.push((rel, doc));
                    }
                }
            }
        }
        out
    }
}

fn module_doc(text: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in text.lines().take(20) {
        let trimmed = line.trim_start();
        let body = trimmed
            .strip_prefix("//!")
            .or_else(|| trimmed.strip_prefix("///"))
            .or_else(|| trimmed.strip_prefix("#"));
        if let Some(body) = body {
            let body = body.trim();
            if !body.is_empty() {
                lines.push(body.to_string());
            }
        } else if !lines.is_empty() {
            break;
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" ").chars().take(500).collect())
    }
}
