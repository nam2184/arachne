use std::path::{Path, PathBuf};

use crate::{ToolCall, ToolResult};

use super::{
    failure, string_arg, success, usize_arg, wildcard_match, ToolContext, GREP_DEFAULT_LIMIT,
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
    run_with_root(call, &root_path)
}

pub fn run_with_root(call: &ToolCall, root: &Path) -> ToolResult {
    let pattern = string_arg(call, "pattern");
    let include = string_arg(call, "include");
    let limit = usize_arg(call, "limit").unwrap_or(GREP_DEFAULT_LIMIT);

    if pattern.is_empty() {
        return failure("grep", "pattern is required".to_string());
    }

    tracing::info!(
        root = %root.display(),
        pattern = %pattern,
        include = %include,
        limit,
        "grep walk started"
    );
    let started = std::time::Instant::now();
    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(20)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_ignored_search_dir(entry.path()))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if !include.is_empty() && !wildcard_match(&include, &entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if line.contains(&pattern) {
                matches.push(format!(
                    "{}:{}: {}",
                    entry.path().display(),
                    index + 1,
                    line
                ));
                if matches.len() >= limit {
                    break;
                }
            }
        }
        if matches.len() >= limit {
            break;
        }
    }

    tracing::info!(
        root = %root.display(),
        pattern = %pattern,
        include = %include,
        matches = matches.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "grep walk finished"
    );

    if matches.is_empty() {
        return failure("grep", "No matches found".to_string());
    }
    let mut output = matches.join("\n");
    if matches.len() >= limit {
        output.push_str(&format!(
            "\n\n(Results are truncated: showing first {limit} matches. Use a more specific pattern or include filter.)"
        ));
    }
    success("grep", output)
}

fn is_ignored_search_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | "node_modules"
            | "dist"
            | "build"
            | "target"
            | "coverage"
            | ".next"
            | ".nuxt"
            | ".cache"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn call(pattern: &str) -> ToolCall {
        ToolCall {
            name: "grep".to_string(),
            arguments: HashMap::from([("pattern".to_string(), json!(pattern))]),
        }
    }

    #[test]
    fn run_with_root_finds_matching_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\nfoo bar\n").unwrap();
        let result = run_with_root(&call("hello"), dir.path());
        assert!(result.success);
        assert!(result.output.contains("hello world"));
    }

    #[test]
    fn run_with_root_no_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let result = run_with_root(&call("nope"), dir.path());
        assert!(!result.success);
    }

    #[test]
    fn run_with_root_requires_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = call("anything");
        c.arguments.remove("pattern");
        let result = run_with_root(&c, dir.path());
        assert!(!result.success);
    }
}
