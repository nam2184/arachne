use std::path::Path;

use walkdir::WalkDir;

use super::{is_noise_dir, is_text_extension, DiscoveryTechnique, SessionSummary, SymbolSummary};

#[derive(Default)]
pub struct SymbolDiscovery;

impl DiscoveryTechnique for SymbolDiscovery {
    fn name(&self) -> &'static str {
        "symbols"
    }

    fn analyze(&self, root: &Path) -> SessionSummary {
        let mut out = SessionSummary::default();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| !is_noise_dir(&entry.file_name().to_string_lossy()))
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() || out.top_symbols.len() >= 50 {
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
            let symbols = extract_symbols(&text);
            if !symbols.is_empty() {
                out.top_symbols.push(SymbolSummary {
                    file: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
                    exports: symbols,
                });
            }
        }
        out
    }
}

fn extract_symbols(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().take(300) {
        let trimmed = line.trim_start();
        for marker in [
            "pub fn ",
            "fn ",
            "pub struct ",
            "struct ",
            "pub enum ",
            "enum ",
            "class ",
            "def ",
            "export function ",
            "export const ",
            "interface ",
            "type ",
        ] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                if let Some(name) = rest
                    .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .find(|part| !part.is_empty())
                {
                    let name = name.to_string();
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
        if out.len() >= 20 {
            break;
        }
    }
    out
}
