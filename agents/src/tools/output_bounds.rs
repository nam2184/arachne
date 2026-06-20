/// Hard cap on the number of lines a single tool result may emit
/// before being truncated for model consumption.
pub const MAX_TOOL_OUTPUT_LINES: usize = 2_000;
/// Hard cap on the number of bytes a single tool result may emit
/// before being truncated for model consumption.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 50 * 1024;
/// Hard cap on the number of chars in a *single line* of a tool
/// result. Mirrors opencode's `MAX_LINE_LENGTH = 2000`: a
/// minified JS file or a base64 blob would otherwise be one giant
/// line that the byte cap truncates mid-content with no signal
/// to the model. After clamping, the line is suffixed with
/// `LINE_SUFFIX` so the model knows the truncation happened and
/// can re-read with `offset`/`limit` to see the rest.
pub const MAX_LINE_LENGTH: usize = 2_000;
/// Approximate chars per token used for budget estimation. Matches
/// opencode's `CHARS_PER_TOKEN` heuristic.
pub const CHARS_PER_TOKEN: usize = 4;
/// Default glob result cap (opencode parity). Tool is allowed to
/// accept its own `limit` argument but defaults to this constant.
pub const GLOB_DEFAULT_LIMIT: usize = 100;
pub const GREP_DEFAULT_LIMIT: usize = 100;
pub const READ_DEFAULT_LIMIT: usize = 2_000;

const MARKER: &str = "... output truncated; full content suppressed to fit context ...";
const MARKER_SPILL_PREFIX: &str = "... output truncated; full content saved to: ";
const MARKER_SPILL_SUFFIX: &str =
    " (use `read` with offset/limit or `grep` on the file to dig deeper) ...";
const LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";

/// Append the truncation marker to `kept`. When `spill_path` is
/// `Some`, the marker tells the model where the full output was
/// saved so it can `Read` that file with offset/limit. When
/// `None`, the marker is the legacy "suppressed" phrasing and
/// the full output is gone.
fn append_marker(kept: &mut String, spill_path: Option<&str>) {
    match spill_path {
        Some(path) => {
            kept.push_str(MARKER_SPILL_PREFIX);
            kept.push_str(path);
            kept.push_str(MARKER_SPILL_SUFFIX);
        }
        None => kept.push_str(MARKER),
    }
}

/// Process-wide counter for spillover file names. Each call to
/// `spill_to_disk` increments this and uses the new value as a
/// per-spill sequence number so concurrent spills don't
/// collide on the filesystem. Atomic so it works across
/// threads.
static SPILL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write the full (untruncated) text to a file in `spill_dir`
/// and return the path. Returns `None` when `spill_dir` is
/// `None` or when the write fails (e.g. permission denied,
/// disk full). Mirrors opencode's `Truncate.write` helper.
///
/// The filename pattern is `<hint>-<seq>.txt` so a directory
/// listing reveals the tool the spillover came from.
fn spill_to_disk(text: &str, spill_dir: Option<&std::path::Path>, hint: &str) -> Option<String> {
    let dir = spill_dir?;
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::warn!(?error, ?dir, "spill_to_disk: create_dir_all failed");
        return None;
    }
    let seq = SPILL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let safe_hint: String = hint
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let filename = format!("{safe_hint}-{seq}.txt");
    let path = dir.join(&filename);
    if let Err(error) = std::fs::write(&path, text) {
        tracing::warn!(?error, ?path, "spill_to_disk: write failed");
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

/// Outcome of bounding a tool result for model consumption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    pub text: String,
    pub truncated: bool,
    /// Number of *lines* the input had, before the per-line cap
    /// and the line-count cap were applied. Useful for the
    /// runner's "results truncated" hint that the opencode
    /// read/grep/glob tools surface.
    pub dropped_lines: usize,
    /// When truncation happened, the path the full output was
    /// spilled to (opencode parity). The runner's
    /// `truncation_dir` config controls whether this is set;
    /// when `None` the full output is just dropped and the
    /// marker uses the legacy "suppressed to fit context"
    /// phrasing.
    pub spill_path: Option<String>,
}

/// Clip a single line to `MAX_LINE_LENGTH` characters, appending a
/// `LINE_SUFFIX` so the model can see it was truncated. Counts
/// chars (Unicode scalar values) rather than bytes so the cap
/// matches what the model sees — a single 4000-byte UTF-8 line is
/// roughly 4000 chars for ASCII but the byte count would have
/// stopped it sooner than the displayed length.
fn clip_line(line: &str) -> (String, bool) {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX_LINE_LENGTH {
        return (line.to_string(), false);
    }
    let mut out: String = chars.iter().take(MAX_LINE_LENGTH).copied().collect();
    out.push_str(LINE_SUFFIX);
    (out, true)
}

/// Bound a tool output string to the configured line and byte caps.
/// `MAX_TOOL_OUTPUT_LINES = 0` or `MAX_TOOL_OUTPUT_BYTES = 0` disables
/// that dimension of the check. The per-line `MAX_LINE_LENGTH` cap
/// runs first so a single minified line can't blow the byte cap on
/// its own; this is opencode's `MAX_LINE_LENGTH` semantic.
///
/// `spill_dir` controls opencode's "spillover" behavior: when
/// truncation happens and a directory is provided, the *full*
/// untruncated text is written to `<spill_dir>/<hint>-<seq>.txt`
/// and the marker tells the model where to find it. When
/// `spill_dir` is `None`, the full text is just dropped and the
/// marker uses the legacy "suppressed" phrasing. `hint` is a
/// short string (e.g. "shell" or "webfetch") used as the file
/// prefix to make the spillover directory browsable.
pub fn bound_tool_output(
    text: &str,
    spill_dir: Option<&std::path::Path>,
    hint: &str,
) -> BoundedOutput {
    bound_tool_output_with(text, spill_dir, hint)
}

/// Internal: `tail_bound_output` is a thin wrapper that swaps
/// the line-cap direction (keep last N instead of first N) but
/// otherwise reuses the same per-line clip, byte cap, and
/// spill path. The public callers go through the
/// `bound_tool_output` / `tail_bound_output` wrappers above.
fn bound_tool_output_with(
    text: &str,
    spill_dir: Option<&std::path::Path>,
    hint: &str,
) -> BoundedOutput {
    let line_cap = MAX_TOOL_OUTPUT_LINES;
    let byte_cap = MAX_TOOL_OUTPUT_BYTES;

    if line_cap == 0 && byte_cap == 0 {
        return BoundedOutput {
            text: text.to_string(),
            truncated: false,
            dropped_lines: 0,
            spill_path: None,
        };
    }

    // Split on '\n' so we can (a) clip each line independently
    // and (b) count the total number of lines for the "dropped"
    // hint. We do NOT re-join with '\n' here — the byte-cap stage
    // is a single String::truncate and doesn't care about
    // line structure.
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut truncated_lines = false;
    let mut truncated_line_length = false;
    let mut kept_lines: Vec<String> = Vec::with_capacity(raw_lines.len());

    // Step 1: per-line length cap. Done first so the line-count
    // and byte-count stages operate on clipped material.
    for line in &raw_lines {
        let (clipped, was_truncated) = clip_line(line);
        if was_truncated {
            truncated_line_length = true;
        }
        kept_lines.push(clipped);
    }

    // Step 2: line-count cap (opencode: `lines.truncate(line_cap)`).
    // Note `split('\n')` on a text that ends in '\n' produces a
    // trailing empty string; that empty line is preserved (matches
    // the existing pre-clip behavior in the line-cap path).
    if line_cap > 0 && kept_lines.len() > line_cap {
        kept_lines.truncate(line_cap);
        truncated_lines = true;
    }

    let mut kept = kept_lines.join("\n");
    let dropped_lines = raw_lines.len().saturating_sub(line_cap).min(usize::MAX);

    // Step 3: byte cap. Runs on the already-clipped material.
    let mut truncated_bytes = false;
    if byte_cap > 0 && kept.len() > byte_cap {
        kept.truncate(byte_cap);
        truncated_bytes = true;
    }

    let truncated = truncated_lines || truncated_line_length || truncated_bytes;
    let spill_path = if truncated {
        spill_to_disk(text, spill_dir, hint)
    } else {
        None
    };
    if truncated {
        if !kept.ends_with('\n') {
            kept.push('\n');
        }
        kept.push_str("\n");
        append_marker(&mut kept, spill_path.as_deref());
    }

    BoundedOutput {
        text: kept,
        truncated,
        dropped_lines,
        spill_path,
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

/// Rough token estimate used by request-budget pre-checks.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(CHARS_PER_TOKEN)
}

static TRUNCATED_TOTAL: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn note_truncation() {
    TRUNCATED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Bound a tool output to the last N lines, applying the same
/// per-line clip and byte cap as `bound_tool_output`. Used for
/// `shell` output (opencode's `tail()` function) where the
/// relevant signal — exit code, error trace, final state — is at
/// the *end* of the output, not the beginning. Webfetch, read,
/// and other tools keep using `bound_tool_output`.
///
/// The function clips each line to `MAX_LINE_LENGTH` first (same
/// as the head-bounded path) so a single very long line in the
/// tail can't blow the byte cap. Then it keeps the last
/// `MAX_TOOL_OUTPUT_LINES` lines, then applies the byte cap.
pub fn tail_bound_output(
    text: &str,
    spill_dir: Option<&std::path::Path>,
    hint: &str,
) -> BoundedOutput {
    tail_bound_output_with(text, spill_dir, hint)
}

/// Tail-bounded version: per-line clip, keep last N lines, byte
/// cap, spill. Mirrors opencode's `tail()` function.
fn tail_bound_output_with(
    text: &str,
    spill_dir: Option<&std::path::Path>,
    hint: &str,
) -> BoundedOutput {
    let line_cap = MAX_TOOL_OUTPUT_LINES;
    let byte_cap = MAX_TOOL_OUTPUT_BYTES;

    if line_cap == 0 && byte_cap == 0 {
        return BoundedOutput {
            text: text.to_string(),
            truncated: false,
            dropped_lines: 0,
            spill_path: None,
        };
    }

    let raw_lines: Vec<&str> = text.split('\n').collect();

    // Step 1: per-line length cap.
    let mut clipped_lines: Vec<String> = Vec::with_capacity(raw_lines.len());
    let mut truncated_line_length = false;
    for line in &raw_lines {
        let (clipped, was_truncated) = clip_line(line);
        if was_truncated {
            truncated_line_length = true;
        }
        clipped_lines.push(clipped);
    }

    // Step 2: line-count cap. Keep the LAST `line_cap` lines
    // (opencode's `tail` semantics). `dropped_lines` records
    // the head drop for the runner's "(showing last N of M
    // lines)" hint.
    let total = clipped_lines.len();
    let kept_lines: Vec<String> = if line_cap > 0 && total > line_cap {
        let drop = total - line_cap;
        clipped_lines[drop..].to_vec()
    } else {
        clipped_lines
    };

    let dropped_lines = total.saturating_sub(line_cap).min(usize::MAX);

    let mut kept = kept_lines.join("\n");
    let mut truncated_bytes = false;
    if byte_cap > 0 && kept.len() > byte_cap {
        kept.truncate(byte_cap);
        truncated_bytes = true;
    }

    let truncated = truncated_line_length || truncated_bytes || total > line_cap;
    let spill_path = if truncated {
        spill_to_disk(text, spill_dir, hint)
    } else {
        None
    };
    if truncated {
        if !kept.ends_with('\n') {
            kept.push('\n');
        }
        kept.push_str("\n");
        append_marker(&mut kept, spill_path.as_deref());
    }

    BoundedOutput {
        text: kept,
        truncated,
        dropped_lines,
        spill_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_passes_through_untouched() {
        let out = bound_tool_output("hello\nworld", None, "test");
        assert!(!out.truncated);
        assert_eq!(out.text, "hello\nworld");
    }

    #[test]
    fn truncates_when_over_line_limit() {
        let text: String = (0..(MAX_TOOL_OUTPUT_LINES + 50))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_tool_output(&text, None, "test");
        assert!(out.truncated);
        assert!(out.text.contains(MARKER));
    }

    #[test]
    fn truncates_when_over_byte_limit() {
        let text = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 100);
        let out = bound_tool_output(&text, None, "test");
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

    #[test]
    fn short_lines_pass_through_unchanged() {
        // Each line is well under MAX_LINE_LENGTH.
        let text = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let out = bound_tool_output(text, None, "test");
        assert!(!out.truncated);
        assert_eq!(out.text, text);
    }

    #[test]
    fn long_line_is_clipped_with_suffix() {
        // A single line over MAX_LINE_LENGTH must be clipped to
        // the cap and a `... (line truncated to 2000 chars)`
        // suffix appended so the model knows the truncation
        // happened and can re-read with offset/limit.
        let long: String = "a".repeat(MAX_LINE_LENGTH + 500);
        let out = bound_tool_output(&long, None, "test");
        assert!(out.truncated, "truncated flag should be set");
        let line = out.text.lines().next().expect("at least one line");
        assert!(line.contains("(line truncated to 2000 chars)"));
        // The kept prefix should be exactly MAX_LINE_LENGTH chars.
        let prefix_len = line
            .strip_suffix("... (line truncated to 2000 chars)")
            .expect("suffix present")
            .chars()
            .count();
        assert_eq!(prefix_len, MAX_LINE_LENGTH);
    }

    #[test]
    fn line_at_exact_cap_is_not_clipped() {
        // A line of exactly MAX_LINE_LENGTH chars must NOT be
        // clipped (off-by-one guard).
        let text: String = "x".repeat(MAX_LINE_LENGTH);
        let out = bound_tool_output(&text, None, "test");
        assert!(!out.truncated, "exact-cap line should not be clipped");
        assert!(!out.text.contains("line truncated to 2000"));
    }

    #[test]
    fn mixed_short_and_long_lines() {
        // Only the long line should carry the suffix; the short
        // ones should pass through byte-for-byte.
        let long: String = "L".repeat(MAX_LINE_LENGTH + 10);
        let text = format!("first\n{long}\nthird");
        let out = bound_tool_output(&text, None, "test");
        assert!(out.truncated);
        assert!(out.text.contains("first\n"));
        assert!(out.text.contains("third\n"));
        assert!(out.text.contains("(line truncated to 2000 chars)"));
    }

    #[test]
    fn line_clip_runs_before_byte_cap() {
        // Regression test: a single huge line that, after clipping,
        // is still under MAX_TOOL_OUTPUT_BYTES should not be byte-
        // truncated. This protects against the case where the
        // byte cap would otherwise amputate the suffix.
        let text: String = "z".repeat(MAX_LINE_LENGTH * 2);
        let out = bound_tool_output(&text, None, "test");
        // Single line → no '\n' at end → no line-count cap.
        // After clipping: line has MAX_LINE_LENGTH + suffix bytes
        // which is ~2 KB, well under 50 KB. So only the line
        // cap fires, not the byte cap. The marker should be
        // present (truncated=true) but the clipped line should
        // be intact, with the suffix preserved.
        assert!(out.truncated);
        let data_lines: Vec<&str> = out
            .text
            .lines()
            .filter(|line| !line.is_empty() && !line.contains("output truncated"))
            .collect();
        assert_eq!(
            data_lines.len(),
            1,
            "expected exactly one data line, got: {data_lines:?}"
        );
        assert!(data_lines[0].contains("(line truncated to 2000 chars)"));
    }

    #[test]
    fn dropped_lines_reported_in_struct() {
        // When the line-count cap fires, dropped_lines should
        // reflect how many were dropped. The runner uses this
        // to render a "(showing first N of M lines)" hint.
        let total = MAX_TOOL_OUTPUT_LINES + 50;
        let text: String = (0..total)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_tool_output(&text, None, "test");
        assert!(out.truncated);
        assert_eq!(out.dropped_lines, total - MAX_TOOL_OUTPUT_LINES);
    }

    #[test]
    fn empty_input_passes_through() {
        let out = bound_tool_output("", None, "test");
        assert!(!out.truncated);
        assert_eq!(out.text, "");
        assert_eq!(out.dropped_lines, 0);
    }

    #[test]
    fn tail_keeps_last_n_lines() {
        // opencode's tail() function: keep the LAST
        // MAX_TOOL_OUTPUT_LINES lines, drop the head. This
        // matches shell semantics where the trailing error /
        // exit state is what the model needs.
        let total = MAX_TOOL_OUTPUT_LINES + 100;
        let text: String = (0..total)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_bound_output(&text, None, "test");
        assert!(out.truncated);
        // First kept line should be `line 100` (the (cap+1)-th
        // line); the leading 100 lines are dropped.
        let first = out.text.lines().next().expect("at least one line");
        assert_eq!(first, "line 100");
        // The last line should be `line (total-1)`.
        let last = out
            .text
            .lines()
            .rev()
            .find(|l| !l.is_empty() && !l.contains("output truncated"))
            .expect("at least one data line");
        assert_eq!(last, format!("line {}", total - 1));
        // Drop count surfaces for the runner's hint.
        assert_eq!(out.dropped_lines, 100);
    }

    #[test]
    fn tail_under_cap_does_not_truncate() {
        let text: String = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_bound_output(&text, None, "test");
        assert!(!out.truncated);
        assert_eq!(out.text, text);
        assert_eq!(out.dropped_lines, 0);
    }

    #[test]
    fn tail_applies_byte_cap_to_kept_lines() {
        // After the line-count tail, if the kept tail is still
        // over the byte cap, the byte cap must fire (last-mile
        // safety). The kept text must end with the truncation
        // marker.
        let line = "x".repeat(100);
        // MAX_TOOL_OUTPUT_LINES lines of 100 bytes = 200 KB,
        // which is over 50 KB.
        let text: String = (0..MAX_TOOL_OUTPUT_LINES)
            .map(|_| line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_bound_output(&text, None, "test");
        assert!(out.truncated);
        // The kept text (before the marker) is at most the byte
        // cap plus the line cap's worth of delimiters + the
        // marker. Allow generous headroom.
        let without_marker = out.text.split(MARKER).next().unwrap_or("");
        assert!(without_marker.len() <= MAX_TOOL_OUTPUT_BYTES + 200);
    }

    #[test]
    fn tail_applies_per_line_clip() {
        // A long line in the tail is clipped, same as the
        // head-bounded path.
        let long: String = "T".repeat(MAX_LINE_LENGTH + 100);
        let text = format!("first\n{long}\nlast");
        let out = tail_bound_output(&text, None, "test");
        assert!(out.truncated);
        assert!(out.text.contains("first\n"));
        assert!(out.text.contains("last\n"));
        assert!(out.text.contains("(line truncated to 2000 chars)"));
    }

    // --- spillover tests (opencode Truncate.write parity) -----

    #[test]
    fn spill_path_is_none_when_no_spill_dir() {
        // No spill_dir → even with truncation, the full output
        // is dropped and the marker uses the legacy phrasing.
        let text: String = (0..(MAX_TOOL_OUTPUT_LINES + 5))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_tool_output(&text, None, "test");
        assert!(out.truncated);
        assert!(out.spill_path.is_none());
        assert!(out.text.contains("suppressed to fit context"));
    }

    #[test]
    fn spill_path_is_set_when_truncated_and_dir_provided() {
        // With a spill_dir, truncation writes the *full* text
        // (not the bounded one) to a file and the marker
        // includes the path.
        let tmp = tempfile::tempdir().expect("tempdir");
        let text: String = (0..(MAX_TOOL_OUTPUT_LINES + 5))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_tool_output(&text, Some(tmp.path()), "read");
        assert!(out.truncated);
        let path = out
            .spill_path
            .expect("spill_path set when spill_dir is Some");
        assert!(path.starts_with(tmp.path().to_str().unwrap()));
        assert!(path.ends_with(".txt"));
        // Filename uses the hint ("read") for grep-ability.
        let filename = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            filename.starts_with("read-"),
            "filename should start with hint, got: {filename}"
        );
        // Marker references the path.
        assert!(out.text.contains(&path));
        assert!(out.text.contains("saved to:"));
        // Full text was written.
        let spilled = std::fs::read_to_string(&path).expect("read spillover");
        assert_eq!(spilled, text);
    }

    #[test]
    fn spill_path_is_none_when_not_truncated() {
        // When truncation doesn't fire, the spillover is
        // skipped (no point writing a 100-char file the model
        // already has inline).
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = bound_tool_output("short output", Some(tmp.path()), "read");
        assert!(!out.truncated);
        assert!(out.spill_path.is_none());
    }

    #[test]
    fn tail_bound_also_spills() {
        // The tail-bounded path uses the same spillover helper;
        // a `shell` output that gets tail-bounded should still
        // have the full text written to the spill dir.
        let tmp = tempfile::tempdir().expect("tempdir");
        let total = MAX_TOOL_OUTPUT_LINES + 50;
        let text: String = (0..total)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_bound_output(&text, Some(tmp.path()), "shell");
        assert!(out.truncated);
        let path = out.spill_path.expect("spill_path set");
        let filename = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(filename.starts_with("shell-"), "got: {filename}");
        // Full text was written — including the dropped head.
        let spilled = std::fs::read_to_string(&path).expect("read spillover");
        assert_eq!(spilled, text);
    }

    #[test]
    fn spill_dir_creates_if_missing() {
        // The runner hands us a directory that may not exist
        // yet. The spill helper should create it rather than
        // failing the tool call.
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("tool-output").join("nested");
        let text: String = (0..(MAX_TOOL_OUTPUT_LINES + 5))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_tool_output(&text, Some(&nested), "read");
        assert!(out.truncated);
        assert!(out.spill_path.is_some());
        assert!(nested.exists(), "spill helper should create nested dirs");
    }
}
