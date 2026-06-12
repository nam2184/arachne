use std::path::{Path, PathBuf};

use crate::{ToolCall, ToolResult};

use super::{
    failure, string_arg, success, usize_arg, wildcard_match, ToolContext, GLOB_DEFAULT_LIMIT,
};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let root = string_arg(call, "path");
    let root_path = if root.is_empty() {
        if ctx.project_root.as_os_str().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            ctx.project_root.clone()
        }
    } else {
        PathBuf::from(&root)
    };
    tracing::info!(
        tool = "glob",
        requested_path = %root,
        resolved_root = %root_path.display(),
        project_root = %ctx.project_root.display(),
        project_root_empty = ctx.project_root.as_os_str().is_empty(),
        cwd = %std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        pattern = %string_arg(call, "pattern"),
        "glob dispatch"
    );
    tracing::debug!(
        tool = "glob",
        context_project_root = %ctx.project_root.display(),
        context_mode = ?ctx.mode,
        "glob ToolContext"
    );
    run_with_root(call, &root_path)
}

pub fn run_with_root(call: &ToolCall, root: &Path) -> ToolResult {
    let pattern = string_arg(call, "pattern");
    let pattern = if pattern.is_empty() {
        "*".to_string()
    } else {
        pattern
    };
    let limit = usize_arg(call, "limit").unwrap_or(GLOB_DEFAULT_LIMIT);

    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(20)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy();
        if wildcard_match(&pattern, &relative)
            || wildcard_match(&pattern, &entry.file_name().to_string_lossy())
        {
            matches.push(entry.path().to_string_lossy().to_string());
            if matches.len() >= limit {
                break;
            }
        }
    }

    if matches.is_empty() {
        return failure("glob", "No files found".to_string());
    }
    let mut output = matches.join("\n");
    if matches.len() >= limit {
        output.push_str(&format!(
            "\n\n(Results are truncated: showing first {limit} results. Consider using a more specific path or pattern.)"
        ));
    }
    success("glob", output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn call(pattern: &str) -> ToolCall {
        ToolCall {
            name: "glob".to_string(),
            arguments: HashMap::from([("pattern".to_string(), json!(pattern))]),
        }
    }

    #[test]
    fn run_with_root_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("b.rs"), "b").unwrap();
        let result = run_with_root(&call("*.txt"), dir.path());
        assert!(result.success);
        assert!(result.output.contains("a.txt"));
        assert!(!result.output.contains("b.rs"));
    }

    #[test]
    fn run_with_root_reports_empty() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_with_root(&call("*.zzz"), dir.path());
        assert!(!result.success);
    }
}
