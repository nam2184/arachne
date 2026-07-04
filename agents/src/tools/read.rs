use std::path::Path;

use crate::{ToolCall, ToolResult};

use super::{
    failure, resolve_session_path, string_arg, success_with_metadata, usize_arg, ToolContext,
    READ_DEFAULT_LIMIT,
};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let requested = string_arg(call, "path");
    let path = resolve_session_path(&requested, ctx, "read");
    run_with_path(call, &path)
}

pub fn run_with_path(call: &ToolCall, path: &Path) -> ToolResult {
    let offset = usize_arg(call, "offset").unwrap_or(1).max(1);
    let limit = usize_arg(call, "limit").or(Some(READ_DEFAULT_LIMIT));

    match std::fs::read_to_string(path) {
        Ok(content) => {
            let formatted = format_lines(&content, offset, limit);
            success_with_metadata(
                "read",
                formatted.output,
                serde_json::json!({
                    "file": path.display().to_string(),
                    "offset": offset,
                    "limit": limit,
                    "start_line": if formatted.returned_lines > 0 { offset } else { 0 },
                    "end_line": formatted.end_line,
                    "returned_lines": formatted.returned_lines,
                    "total_lines": formatted.total_lines,
                    "truncated": formatted.returned_lines > 0 && formatted.end_line < formatted.total_lines,
                }),
            )
        }
        Err(error) => failure("read", error.to_string()),
    }
}

struct FormattedRead {
    output: String,
    returned_lines: usize,
    end_line: usize,
    total_lines: usize,
}

fn format_lines(content: &str, offset: usize, limit: Option<usize>) -> FormattedRead {
    let lines = content.lines().collect::<Vec<_>>();
    let rows = lines
        .iter()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(limit.unwrap_or(usize::MAX))
        .map(|(index, line)| format!("{}: {}", index + 1, line))
        .collect::<Vec<_>>();
    let returned_lines = rows.len();
    let end_line = if returned_lines > 0 {
        offset + returned_lines - 1
    } else {
        0
    };

    FormattedRead {
        output: rows.join("\n"),
        returned_lines,
        end_line,
        total_lines: lines.len(),
    }
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
    fn run_with_path_reports_read_range_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour").unwrap();
        let mut call = call(file.to_str().unwrap());
        call.arguments.insert("offset".to_string(), json!(2));
        call.arguments.insert("limit".to_string(), json!(2));

        let result = run_with_path(&call, &file);
        assert!(result.success);
        assert_eq!(result.output, "2: two\n3: three");
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["offset"], json!(2));
        assert_eq!(metadata["limit"], json!(2));
        assert_eq!(metadata["start_line"], json!(2));
        assert_eq!(metadata["end_line"], json!(3));
        assert_eq!(metadata["returned_lines"], json!(2));
        assert_eq!(metadata["total_lines"], json!(4));
        assert_eq!(metadata["truncated"], json!(true));
    }

    #[test]
    fn run_with_path_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("missing.txt");
        let result = run_with_path(&call(file.to_str().unwrap()), &file);
        assert!(!result.success);
    }
}
