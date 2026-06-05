use std::path::Path;
use std::sync::Arc;
use ignore::WalkBuilder;
use parking_lot::RwLock;
use crate::domain::TechStack;
use tracing::{debug, info, trace, warn};

pub struct StackDetector {
    cache: RwLock<std::collections::HashMap<String, TechStack>>,
}

impl StackDetector {
    pub fn detect(&self, project_path: &Path) -> TechStack {
        let cache_key = project_path.to_string_lossy().to_string();

        info!(project = %cache_key, "starting language detection");

        if let Some(cached) = self.cache.read().get(&cache_key) {
            debug!(
                project = %cache_key,
                languages = ?cached.languages,
                "language detection cache hit"
            );
            return cached.clone();
        }

        let mut stack = TechStack::new();
        let mut scanned_files = 0usize;
        let mut hyperpolyglot_detections = 0usize;
        let mut fallback_detections = 0usize;
        let mut undetected_files = 0usize;

        let walker = WalkBuilder::new(project_path)
            .hidden(false)
            .ignore(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|file_type| file_type.is_file()) {
                continue;
            }

            scanned_files += 1;
            let path = entry.path();

            match self.detect_with_hyperpolyglot(path) {
                Some(language) => {
                    hyperpolyglot_detections += 1;
                    trace!(file = %path.display(), language = %language, "hyperpolyglot detected language");
                    stack.add_language(language);
                }
                None => {
                    if let Some(language) = self.detect_with_fallback(path) {
                        fallback_detections += 1;
                        trace!(file = %path.display(), language = %language, "fallback detected language");
                        stack.add_language(language);
                    } else {
                        undetected_files += 1;
                        trace!(file = %path.display(), "language detection missed file");
                    }
                }
            }
        }

        info!(
            project = %cache_key,
            scanned_files,
            hyperpolyglot_detections,
            fallback_detections,
            undetected_files,
            languages = ?stack.languages,
            "finished language detection"
        );

        let mut cache = self.cache.write();
        cache.insert(cache_key, stack.clone());

        stack
    }

    fn detect_with_hyperpolyglot(&self, path: &Path) -> Option<String> {
        match hyperpolyglot::detect(path) {
            Ok(Some(detection)) => {
                debug!(
                    file = %path.display(),
                    language = detection.language(),
                    strategy = detection.variant(),
                    "hyperpolyglot detection succeeded"
                );
                Some(detection.language().to_string())
            }
            Ok(None) => {
                debug!(file = %path.display(), "hyperpolyglot could not detect language");
                None
            }
            Err(error) => {
                warn!(
                    file = %path.display(),
                    error = %error,
                    "hyperpolyglot detection failed; falling back"
                );
                None
            }
        }
    }

    fn detect_with_fallback(&self, path: &Path) -> Option<String> {
        if let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
            if let Some(language) = self.detect_by_filename(file_name) {
                return Some(language.to_string());
            }
        }

        let extension = path.extension().and_then(|ext| ext.to_str())?;
        self.detect_by_extension(extension).map(str::to_string)
    }

    fn detect_by_filename(&self, file_name: &str) -> Option<&'static str> {
        match file_name {
            "Cargo.toml" | "Cargo.lock" => Some("Rust"),
            "package.json" | "pnpm-lock.yaml" | "yarn.lock" | "package-lock.json" => Some("JavaScript"),
            "tsconfig.json" => Some("TypeScript"),
            "pyproject.toml" | "requirements.txt" | "setup.py" | "Pipfile" => Some("Python"),
            "go.mod" | "go.sum" => Some("Go"),
            "pom.xml" | "build.gradle" | "build.gradle.kts" => Some("Java"),
            "Gemfile" | "Gemfile.lock" => Some("Ruby"),
            "composer.json" => Some("PHP"),
            "Dockerfile" => Some("Dockerfile"),
            _ => None,
        }
    }

    fn detect_by_extension(&self, extension: &str) -> Option<&'static str> {
        match extension.to_lowercase().as_str() {
            "rs" => Some("Rust"),
            "ts" | "tsx" | "mts" | "cts" => Some("TypeScript"),
            "js" | "jsx" | "mjs" | "cjs" => Some("JavaScript"),
            "py" | "pyw" => Some("Python"),
            "go" => Some("Go"),
            "java" => Some("Java"),
            "rb" => Some("Ruby"),
            "php" => Some("PHP"),
            "cs" => Some("C#"),
            "c" | "h" => Some("C"),
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some("C++"),
            "swift" => Some("Swift"),
            "kt" | "kts" => Some("Kotlin"),
            "scala" => Some("Scala"),
            "dart" => Some("Dart"),
            "ex" | "exs" => Some("Elixir"),
            "erl" | "hrl" => Some("Erlang"),
            "lua" => Some("Lua"),
            "r" => Some("R"),
            "html" | "htm" => Some("HTML"),
            "css" => Some("CSS"),
            "scss" | "sass" => Some("Sass"),
            "json" => Some("JSON"),
            "toml" => Some("TOML"),
            "yaml" | "yml" => Some("YAML"),
            "md" | "mdx" => Some("Markdown"),
            "sql" => Some("SQL"),
            "sh" | "bash" | "zsh" => Some("Shell"),
            "ps1" => Some("PowerShell"),
            _ => None,
        }
    }

    pub fn clear_cache(&self) {
        debug!("clearing language detection cache");
        self.cache.write().clear();
    }
}

impl Default for StackDetector {
    fn default() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }
}

impl StackDetector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}