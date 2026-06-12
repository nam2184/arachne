use std::path::{Path, PathBuf};

use crate::{ToolCall, ToolResult};

use super::{failure, string_arg, success, usize_arg, ToolContext, READ_DEFAULT_LIMIT};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let path = string_arg(call, "path");
    tracing::info!(
        tool = "read",
        requested_path = %path,
        project_root = %ctx.project_root.display(),
        "read dispatch"
    );
    run_with_path(call, Path::new(&path))
}

pub fn run_with_path(call: &ToolCall, path: &Path) -> ToolResult {
    let offset = usize_arg(call, "offset").unwrap_or(1).max(1);
    let limit = usize_arg(call, "limit").or(Some(READ_DEFAULT_LIMIT));

    match std::fs::read_to_string(path) {
        Ok(content) => success("read", format_lines(&content, offset, limit)),
        Err(error) => failure("read", error.to_string()),
    }
}

pub fn run_with_pathbuf(call: &ToolCall, path: PathBuf) -> ToolResult {
    run_with_path(call, &path)
}

fn format_lines(content: &str, offset: usize, limit: Option<usize>) -> String {
    content
        .lines()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(limit.unwrap_or(usize::MAX))
        .map(|(index, line)| format!("{}: {}", index + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn call(path: &str) -> ToolCall {
        ToolCall {
            name: "read".to_string(),
            arguments: HashMap::from([("path".to_string(), json!(path))]),
        }
    }

    #[test]
    fn run_with_path_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let result = run_with_path(&call(file.to_str().unwrap()), &file);
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[test]
    fn run_with_path_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("missing.txt");
        let result = run_with_path(&call(file.to_str().unwrap()), &file);
        assert!(!result.success);
    }
}
