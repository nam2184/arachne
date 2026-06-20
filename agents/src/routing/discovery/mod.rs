pub mod docs;
pub mod stack;
pub mod symbols;
pub mod tfidf;

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StackCategory {
    WebFrontend,
    ApiBackend,
    CliTool,
    Library,
    DataPipeline,
    Database,
    Messaging,
    BuildSystem,
    Docs,
    Tests,
    Config,
    Unknown,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SymbolSummary {
    pub file: PathBuf,
    pub exports: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StackOutput {
    pub main_language: Option<String>,
    pub all_languages: Vec<String>,
    pub libs: Vec<String>,
    pub build_scripts: Vec<PathBuf>,
    pub manifests: Vec<PathBuf>,
    pub ci_configs: Vec<PathBuf>,
    pub entry_points: Vec<PathBuf>,
    pub test_dirs: Vec<PathBuf>,
    pub doc_dirs: Vec<PathBuf>,
    pub config_files: Vec<PathBuf>,
    pub total_files: usize,
    pub total_loc: usize,
    pub dominant_categories: Vec<StackCategory>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub stack: StackOutput,
    pub one_liner: String,
    pub topic_signature: Vec<String>,
    pub readme_excerpt: Option<String>,
    pub module_docs: Vec<(PathBuf, String)>,
    pub top_symbols: Vec<SymbolSummary>,
}

pub trait DiscoveryTechnique: Send + Sync {
    fn name(&self) -> &'static str;
    fn analyze(&self, root: &Path) -> SessionSummary;
}

pub struct DiscoveryDispatcher {
    techniques: Vec<Box<dyn DiscoveryTechnique>>,
}

impl Default for DiscoveryDispatcher {
    fn default() -> Self {
        Self {
            techniques: vec![
                Box::<stack::StackDiscovery>::default(),
                Box::<docs::DocDiscovery>::default(),
                Box::<symbols::SymbolDiscovery>::default(),
                Box::<tfidf::TfIdfDiscovery>::default(),
            ],
        }
    }
}

impl DiscoveryDispatcher {
    pub fn discover(&self, root: &Path) -> SessionSummary {
        let mut merged = SessionSummary::default();
        for technique in &self.techniques {
            merge_summary(&mut merged, technique.analyze(root));
        }
        if merged.one_liner.trim().is_empty() {
            merged.one_liner = compose_one_liner(&merged);
        }
        merged
    }
}

fn merge_summary(dst: &mut SessionSummary, src: SessionSummary) {
    merge_stack(&mut dst.stack, src.stack);
    if dst.one_liner.is_empty() && !src.one_liner.is_empty() {
        dst.one_liner = src.one_liner;
    }
    if dst.readme_excerpt.is_none() {
        dst.readme_excerpt = src.readme_excerpt;
    }
    extend_unique(&mut dst.topic_signature, src.topic_signature);
    dst.module_docs.extend(src.module_docs);
    dst.top_symbols.extend(src.top_symbols);
}

fn merge_stack(dst: &mut StackOutput, src: StackOutput) {
    if dst.main_language.is_none() {
        dst.main_language = src.main_language;
    }
    extend_unique(&mut dst.all_languages, src.all_languages);
    extend_unique(&mut dst.libs, src.libs);
    extend_unique(&mut dst.build_scripts, src.build_scripts);
    extend_unique(&mut dst.manifests, src.manifests);
    extend_unique(&mut dst.ci_configs, src.ci_configs);
    extend_unique(&mut dst.entry_points, src.entry_points);
    extend_unique(&mut dst.test_dirs, src.test_dirs);
    extend_unique(&mut dst.doc_dirs, src.doc_dirs);
    extend_unique(&mut dst.config_files, src.config_files);
    extend_unique(&mut dst.dominant_categories, src.dominant_categories);
    dst.total_files = dst.total_files.max(src.total_files);
    dst.total_loc = dst.total_loc.max(src.total_loc);
}

fn extend_unique<T: PartialEq>(dst: &mut Vec<T>, values: Vec<T>) {
    for value in values {
        if !dst.contains(&value) {
            dst.push(value);
        }
    }
}

fn compose_one_liner(summary: &SessionSummary) -> String {
    let lang = summary
        .stack
        .main_language
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let topics = summary
        .topic_signature
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if topics.is_empty() {
        format!("{lang} project")
    } else {
        format!("{lang} project covering {topics}")
    }
}

pub(crate) fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "__pycache__"
            | ".venv"
            | "venv"
            | ".next"
            | ".turbo"
            | "vendor"
            | "Pods"
            | ".gradle"
            | ".idea"
            | ".vscode"
    )
}

pub(crate) fn is_text_extension(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "go"
            | "rb"
            | "java"
            | "kt"
            | "swift"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "hpp"
            | "cs"
            | "scala"
            | "sh"
            | "bash"
            | "sql"
            | "html"
            | "css"
            | "scss"
            | "md"
            | "mdx"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "proto"
            | "graphql"
            | "gql"
            | "txt"
    )
}
