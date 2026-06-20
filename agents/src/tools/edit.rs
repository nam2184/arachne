use std::path::Path;

use crate::file_mutation::FileMutationService;
use crate::{ToolCall, ToolResult};

use super::{failure, resolve_session_path, string_arg, success, ToolContext};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let requested = string_arg(call, "path");
    let old = string_arg(call, "old_string");
    let new = string_arg(call, "new_string");
    let path = resolve_session_path(&requested, ctx, "edit");

    if old.is_empty() {
        return failure("edit", "old_string is required".to_string());
    }

    run_with_path(call, &path, &old, &new)
}

pub fn run_with_path(_call: &ToolCall, path: &Path, old: &str, new: &str) -> ToolResult {
    let mutation = FileMutationService::new();
    let target = match mutation.target(path) {
        Ok(target) => target,
        Err(error) => return failure("edit", error.to_string()),
    };
    let original = match std::fs::read(&target.canonical) {
        Ok(content) => content,
        Err(error) => return failure("edit", error.to_string()),
    };
    let content = match String::from_utf8(original.clone()) {
        Ok(content) => content,
        Err(_) => {
            return failure(
                "edit",
                format!("{} is not valid UTF-8", target.canonical.display()),
            )
        }
    };

    if !content.contains(old) {
        return failure("edit", "old_string was not found".to_string());
    }

    match mutation.write_if_unmodified(&target, &original, content.replacen(old, new, 1)) {
        Ok(_) => success("edit", format!("Edited {}", target.canonical.display())),
        Err(error) => failure("edit", error.to_string()),
    }
}
