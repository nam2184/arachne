use std::path::Path;

use crate::file_mutation::FileMutationService;
use crate::{ToolCall, ToolResult};

use super::{
    failure, resolve_session_path, string_arg, success_with_metadata, unified_diff, ToolContext,
};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let requested = string_arg(call, "path");
    let path = resolve_session_path(&requested, ctx, "write");
    run_with_path(call, &path)
}

pub fn run_with_path(call: &ToolCall, path: &Path) -> ToolResult {
    let content = string_arg(call, "content");
    let mutation = FileMutationService::new();
    let target = match mutation.target(path) {
        Ok(target) => target,
        Err(error) => return failure("write", error.to_string()),
    };
    let before = std::fs::read_to_string(&target.canonical).ok();

    match mutation.write_text_preserving_bom(&target, &content) {
        Ok(_) => {
            let diff = unified_diff(&target.resource, before.as_deref(), Some(&content));
            success_with_metadata(
                "write",
                format!("Wrote {}", target.canonical.display()),
                serde_json::json!({
                    "file": target.resource,
                    "diff": diff.diff,
                    "additions": diff.additions,
                    "deletions": diff.deletions,
                }),
            )
        }
        Err(error) => failure("write", error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn call(path: &str, content: &str) -> ToolCall {
        ToolCall {
            name: "write".to_string(),
            arguments: HashMap::from([
                ("path".to_string(), json!(path)),
                ("content".to_string(), json!(content)),
            ]),
        }
    }

    #[test]
    fn run_with_path_writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("out.txt");
        let result = run_with_path(&call(file.to_str().unwrap(), "hi"), &file);
        assert!(result.success);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");
    }
}
