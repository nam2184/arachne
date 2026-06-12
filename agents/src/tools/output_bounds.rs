/// Hard cap on the number of lines a single tool result may emit
/// before being truncated for model consumption.
pub const MAX_TOOL_OUTPUT_LINES: usize = 2_000;
/// Hard cap on the number of bytes a single tool result may emit
/// before being truncated for model consumption.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 50 * 1024;
/// Approximate chars per token used for budget estimation. Matches
/// opencode's `CHARS_PER_TOKEN` heuristic.
pub const CHARS_PER_TOKEN: usize = 4;
/// Default glob result cap (opencode parity). Tool is allowed to
/// accept its own `limit` argument but defaults to this constant.
pub const GLOB_DEFAULT_LIMIT: usize = 100;
/// Default grep match cap. Tunable via constant.
pub const GREP_DEFAULT_LIMIT: usize = 2_000;
/// Default read line cap when the caller does not specify `limit`.
pub const READ_DEFAULT_LIMIT: usize = 2_000;

const MARKER: &str = "... output truncated; full content suppressed to fit context ...";

/// Outcome of bounding a tool result for model consumption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    pub text: String,
    pub truncated: bool,
}

/// Bound a tool output string to the configured line and byte caps.
/// `MAX_TOOL_OUTPUT_LINES = 0` or `MAX_TOOL_OUTPUT_BYTES = 0` disables
/// that dimension of the check.
pub fn bound_tool_output(text: &str) -> BoundedOutput {
    let line_cap = MAX_TOOL_OUTPUT_LINES;
    let byte_cap = MAX_TOOL_OUTPUT_BYTES;

    if line_cap == 0 && byte_cap == 0 {
        return BoundedOutput {
            text: text.to_string(),
            truncated: false,
        };
    }

    let mut truncated_lines = false;
    let mut truncated_bytes = false;
    let mut kept: String;

    if line_cap > 0 {
        let mut lines: Vec<&str> = text.split('\n').collect();
        if lines.len() > line_cap {
            lines.truncate(line_cap);
            truncated_lines = true;
        }
        kept = lines.join("\n");
    } else {
        kept = text.to_string();
    }

    if byte_cap > 0 && kept.len() > byte_cap {
        kept.truncate(byte_cap);
        truncated_bytes = true;
    }

    if truncated_lines || truncated_bytes {
        if !kept.ends_with('\n') {
            kept.push('\n');
        }
        kept.push_str("\n");
        kept.push_str(MARKER);
        BoundedOutput {
            text: kept,
            truncated: true,
        }
    } else {
        BoundedOutput {
            text: text.to_string(),
            truncated: false,
        }
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

/// Rough token estimate used by request-budget pre-checks.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(CHARS_PER_TOKEN)
}

/// Process-wide counter of tool outputs the runner has truncated.
/// Exposed for diagnostics / tests.
pub fn truncated_count() -> usize {
    TRUNCATED_TOTAL.load(Ordering::Relaxed)
}

static TRUNCATED_TOTAL: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn note_truncation() {
    TRUNCATED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

#[allow(dead_code)]
pub(crate) fn reset_truncation_counter_for_tests() {
    TRUNCATED_TOTAL.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_passes_through_untouched() {
        let out = bound_tool_output("hello\nworld");
        assert!(!out.truncated);
        assert_eq!(out.text, "hello\nworld");
    }

    #[test]
    fn truncates_when_over_line_limit() {
        let text: String = (0..(MAX_TOOL_OUTPUT_LINES + 50))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_tool_output(&text);
        assert!(out.truncated);
        assert!(out.text.contains(MARKER));
    }

    #[test]
    fn truncates_when_over_byte_limit() {
        let text = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 100);
        let out = bound_tool_output(&text);
        assert!(out.truncated);
        assert!(out.text.contains(MARKER));
        assert!(out.text.len() < text.len());
    }

    #[test]
    fn estimate_tokens_is_chars_over_four_rounded_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
