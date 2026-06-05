use std::path::Path;

use crate::domain::{Tool, ToolCall, ToolResult};

pub fn default_tools() -> Vec<Tool> {
    vec![
        Tool::new("read_file", "Read a file from disk"),
        Tool::new("write_file", "Write content to a file"),
        Tool::new("search_files", "Find files by name"),
        Tool::new("list_directory", "List directory entries"),
    ]
}

pub fn run_tool(call: &ToolCall) -> ToolResult {
    match call.name.as_str() {
        "read_file" => read_file(call),
        "write_file" => write_file(call),
        "search_files" => search_files(call),
        "list_directory" => list_directory(call),
        _ => ToolResult {
            tool: call.name.clone(),
            success: false,
            output: String::new(),
            error: Some("Unknown tool".to_string()),
        },
    }
}

fn read_file(call: &ToolCall) -> ToolResult {
    let path = string_arg(call, "path");
    match std::fs::read_to_string(Path::new(&path)) {
        Ok(output) => success("read_file", output),
        Err(error) => failure("read_file", error.to_string()),
    }
}

fn write_file(call: &ToolCall) -> ToolResult {
    let path = string_arg(call, "path");
    let content = string_arg(call, "content");
    match std::fs::write(Path::new(&path), content) {
        Ok(()) => success("write_file", format!("Wrote {path}")),
        Err(error) => failure("write_file", error.to_string()),
    }
}

fn search_files(call: &ToolCall) -> ToolResult {
    let root = string_arg(call, "path");
    let pattern = string_arg(call, "pattern").to_lowercase();
    let mut matches = Vec::new();

    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.contains(&pattern) {
                matches.push(entry.path().to_string_lossy().to_string());
            }
        }
    }

    success("search_files", matches.join("\n"))
}

fn list_directory(call: &ToolCall) -> ToolResult {
    let path = string_arg(call, "path");
    let entries = match std::fs::read_dir(Path::new(&path)) {
        Ok(entries) => entries,
        Err(error) => return failure("list_directory", error.to_string()),
    };

    let output = entries
        .flatten()
        .map(|entry| entry.path().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    success("list_directory", output)
}

fn string_arg(call: &ToolCall, key: &str) -> String {
    call.arguments
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn success(tool: &str, output: String) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: true,
        output,
        error: None,
    }
}

fn failure(tool: &str, error: String) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: false,
        output: String::new(),
        error: Some(error),
    }
}
