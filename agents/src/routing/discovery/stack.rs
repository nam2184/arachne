use std::collections::HashMap;
use std::path::Path;

use walkdir::WalkDir;

use super::{is_noise_dir, DiscoveryTechnique, SessionSummary, StackCategory, StackOutput};

#[derive(Default)]
pub struct StackDiscovery;

impl DiscoveryTechnique for StackDiscovery {
    fn name(&self) -> &'static str {
        "stack"
    }

    fn analyze(&self, root: &Path) -> SessionSummary {
        let mut stack = StackOutput::default();
        let mut lang_counts: HashMap<String, usize> = HashMap::new();
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
            stack.total_files += 1;
            if let Ok(text) = std::fs::read_to_string(path) {
                stack.total_loc += text.lines().count();
            }
            let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let ext = rel.extension().and_then(|s| s.to_str()).unwrap_or("");
            classify_name(name, &rel, &mut stack);
            if let Some(lang) = ext_to_language(ext) {
                *lang_counts.entry(lang.to_string()).or_default() += 1;
                if !stack.all_languages.iter().any(|item| item == lang) {
                    stack.all_languages.push(lang.to_string());
                }
            }
            classify_dirs(&rel, &mut stack);
        }
        stack.main_language = lang_counts
            .into_iter()
            .filter(|(lang, _)| lang != "config" && lang != "markdown")
            .max_by_key(|(_, count)| *count)
            .map(|(lang, _)| lang);
        infer_categories(&mut stack);
        SessionSummary {
            stack,
            ..Default::default()
        }
    }
}

fn classify_name(name: &str, rel: &Path, stack: &mut StackOutput) {
    match name {
        "Cargo.toml" | "Cargo.lock" | "package.json" | "package-lock.json" | "pnpm-lock.yaml"
        | "yarn.lock" | "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt"
        | "Pipfile" | "go.mod" | "go.sum" | "Gemfile" | "Gemfile.lock" | "pom.xml"
        | "build.gradle" | "build.gradle.kts" => stack.manifests.push(rel.to_path_buf()),
        "Makefile" | "justfile" | "Taskfile.yml" | "Rakefile" => {
            stack.build_scripts.push(rel.to_path_buf())
        }
        ".gitlab-ci.yml" | ".travis.yml" | "circle.yml" => stack.ci_configs.push(rel.to_path_buf()),
        "Dockerfile" | "docker-compose.yml" | "docker-compose.yaml" => {
            stack.config_files.push(rel.to_path_buf())
        }
        _ => {}
    }
    if name.eq_ignore_ascii_case("readme.md") || name.eq_ignore_ascii_case("readme.rst") {
        stack.doc_dirs.push(rel.to_path_buf());
    }
    if rel.starts_with(".github/workflows") || rel.starts_with(".circleci") {
        stack.ci_configs.push(rel.to_path_buf());
    }
    if matches!(
        rel.to_string_lossy().as_ref(),
        "src/main.rs"
            | "src/lib.rs"
            | "src/main.py"
            | "src/main.go"
            | "src/main.ts"
            | "src/index.ts"
            | "src/index.js"
            | "src/main.js"
    ) {
        stack.entry_points.push(rel.to_path_buf());
    }
}

fn classify_dirs(rel: &Path, stack: &mut StackOutput) {
    let parts = rel
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect::<Vec<_>>();
    if parts
        .iter()
        .any(|part| matches!(*part, "tests" | "test" | "__tests__" | "spec"))
    {
        stack.test_dirs.push(rel.to_path_buf());
    }
    if parts.iter().any(|part| matches!(*part, "docs" | "doc")) {
        stack.doc_dirs.push(rel.to_path_buf());
    }
    if parts
        .iter()
        .any(|part| matches!(*part, "migrations" | "schema"))
    {
        push_category(stack, StackCategory::Database);
    }
}

fn infer_categories(stack: &mut StackOutput) {
    if stack
        .manifests
        .iter()
        .any(|path| path.ends_with("package.json"))
    {
        stack.libs.push("node".to_string());
        push_category(stack, StackCategory::WebFrontend);
    }
    if stack
        .manifests
        .iter()
        .any(|path| path.ends_with("Cargo.toml"))
    {
        stack.libs.push("rust".to_string());
        push_category(stack, StackCategory::ApiBackend);
    }
    if !stack.test_dirs.is_empty() {
        push_category(stack, StackCategory::Tests);
    }
    if !stack.build_scripts.is_empty() {
        push_category(stack, StackCategory::BuildSystem);
    }
    if !stack.doc_dirs.is_empty() {
        push_category(stack, StackCategory::Docs);
    }
    if stack.dominant_categories.is_empty() {
        push_category(stack, StackCategory::Unknown);
    }
}

fn push_category(stack: &mut StackOutput, category: StackCategory) {
    if !stack.dominant_categories.contains(&category) {
        stack.dominant_categories.push(category);
    }
}

fn ext_to_language(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" => Some("javascript"),
        "go" => Some("go"),
        "rb" => Some("ruby"),
        "java" | "kt" => Some("jvm"),
        "swift" => Some("swift"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" => Some("cpp"),
        "cs" => Some("csharp"),
        "sql" => Some("sql"),
        "md" | "mdx" => Some("markdown"),
        "json" | "yaml" | "yml" | "toml" => Some("config"),
        _ => None,
    }
}
