//! XML tool call block detector.
//!
//! Some local / open-weight / instruct-tuned models emit tool calls
//! inside the assistant text stream using a variety of XML shapes
//! instead of the provider's native tool-calling channel. Examples:
//!
//! - `<tool_call><tool name="shell">{"command":"ls"}</tool></tool_call>`
//! - `<tool name="read">{"path":"/tmp/x"}</tool>`
//! - `<invoke name="glob">{"pattern":"**/*.rs"}</invoke>`
//! - `<function_calls>…</function_calls>` (Mistral-style)
//! - `<antml:function_calls>…</antml:function_calls>`
//! - `<tool name="x" />` (self-closing form)
//!
//! The runner collects these blocks via [`XmlToolCallScanner`] and
//! turns them into typed [`InvalidToolCallHit`] records. The session
//! then strips the block from the visible text and appends a
//! `tool_result` describing the violation to the assistant message so
//! the model can self-correct on the next turn.
//!
//! The scanner operates on a stream of chunks (SSE text deltas) and
//! buffers characters that look like the start of a tag so a block
//! that is split across two chunks is reassembled before being
//! reported. The scanner is the only place that decides "this text
//! is a tool call"; providers stay text-only.
//!
//! The implementation is a small hand-rolled state machine. The
//! crate does not depend on `regex`, matching the rest of the agent
//! crate's style.

use std::sync::atomic::{AtomicU64, Ordering};

/// One detected XML tool call block. The runner uses `name` to label
/// the synthetic `tool_result` it appends to the assistant message
/// and `raw` as the diagnostic body (the inner payload for
/// paired-tag forms, the full self-closing tag for self-closing
/// forms, or a `<unterminated …>` marker for partial input at flush
/// time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidToolCallHit {
    pub name: String,
    pub raw: String,
}

/// Per-stream state for the XML tool call detector. One scanner per
/// session turn.
#[derive(Debug, Default)]
pub struct XmlToolCallScanner {
    /// Characters that arrived in a chunk but have not yet been
    /// emitted as visible text because they might be the start of a
    /// tag. Flushed to the caller verbatim if the stream ends with
    /// no further tag activity.
    held: String,
    /// Tracks whether we are currently inside an XML tag whose
    /// opening we have already accepted as a tool-call match. While
    /// `true`, we are looking for a closing tag and will not emit
    /// text until the block ends.
    in_block: bool,
    /// Buffer of the inner body of the open tool-call block. Drained
    /// into a hit when the closing tag arrives.
    body: String,
    /// Tag name we matched at the opening (lower-cased), so we can
    /// match the corresponding `</tag>` close.
    open_tag: String,
    /// Name attribute value extracted from the opening tag, or
    /// "xml" when the opening tag has no `name` attribute. This is
    /// what the runner surfaces in the synthetic `tool_result`
    /// feedback to the model.
    hit_name: String,
}

/// Outcome of feeding one chunk into the scanner.
///
/// `text` is the portion of `chunk` that is not part of any
/// detected XML tool call block and should be surfaced to the user.
/// The caller appends this to its text buffer in place of the
/// original chunk.
///
/// `hits` is the list of complete blocks detected while consuming
/// this chunk. A hit is reported only once, on the chunk where the
/// closing tag (or self-closing `/>`) was seen. A hit whose opening
/// tag was seen in an earlier chunk but whose closing tag has not
/// arrived yet is *not* in `hits` — it stays buffered in the
/// scanner.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct XmlFeedOutcome {
    pub text: String,
    pub hits: Vec<InvalidToolCallHit>,
}

static HIT_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_hit_id() -> String {
    let n = HIT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("xml-invalid-{n}")
}

impl XmlToolCallScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if there is an in-flight tool-call block whose
    /// closing tag has not yet arrived. Useful for assertions in
    /// tests.
    pub fn in_block(&self) -> bool {
        self.in_block
    }

    /// Feed a chunk of assistant text. Returns the visible text and
    /// any complete blocks detected.
    pub fn feed(&mut self, chunk: &str) -> XmlFeedOutcome {
        self.feed_xml(chunk)
    }

    fn feed_xml(&mut self, chunk: &str) -> XmlFeedOutcome {
        let mut outcome = XmlFeedOutcome::default();

        if self.in_block {
            self.body.push_str(chunk);
            let tail = self.try_close_block(&mut outcome);
            return self.resume_with(outcome, tail.unwrap_or_default());
        }

        let combined: std::borrow::Cow<'_, str>;
        let scan: &str = if self.held.is_empty() {
            combined = std::borrow::Cow::Borrowed(chunk);
            &combined
        } else {
            let mut s = std::mem::take(&mut self.held);
            s.push_str(chunk);
            combined = std::borrow::Cow::Owned(s);
            &combined
        };

        let mut visible_start = 0usize;
        let bytes = scan.as_bytes();
        let mut cursor = 0usize;

        while cursor < bytes.len() {
            let Some(at) = find_byte_from(bytes, b'<', cursor) else {
                break;
            };

            if let Some(open_end) = try_open_tool_call(&scan[at..]) {
                if at > visible_start {
                    outcome.text.push_str(&scan[visible_start..at]);
                }
                let opening = &scan[at..at + open_end];
                if opening.trim_end().ends_with("/>") {
                    let attr_name = extract_name_attr(opening);
                    outcome.hits.push(InvalidToolCallHit {
                        name: if attr_name.is_empty() {
                            "xml".to_string()
                        } else {
                            attr_name
                        },
                        raw: opening.to_string(),
                    });
                    cursor = at + open_end;
                    visible_start = cursor;
                } else {
                    let attr_name = extract_name_attr(opening);
                    self.in_block = true;
                    self.open_tag = extract_tag_name(opening).to_lowercase();
                    self.hit_name = if attr_name.is_empty() {
                        "xml".to_string()
                    } else {
                        attr_name
                    };
                    self.body.clear();
                    let body_start = at + open_end;
                    if body_start < scan.len() {
                        self.body.push_str(&scan[body_start..]);
                    }
                    let tail = self.try_close_block(&mut outcome);
                    if self.in_block {
                        // Close did not arrive; remaining chunk
                        // content is already in `self.body`.
                        return outcome;
                    }
                    let tail = tail.unwrap_or_default();
                    if tail.is_empty() {
                        return outcome;
                    }
                    return self.resume_with(outcome, tail);
                }
            } else if let Some(gt) = find_byte_from(bytes, b'>', at + 1) {
                // Whole `<…>` is visible text. The held buffer
                // stays empty (we resolved the partial).
                if at > visible_start {
                    outcome.text.push_str(&scan[visible_start..at]);
                }
                outcome.text.push_str(&scan[at..=gt]);
                cursor = gt + 1;
                visible_start = cursor;
            } else {
                self.held.push_str(&scan[at..]);
                if at > visible_start {
                    outcome.text.push_str(&scan[visible_start..at]);
                }
                return outcome;
            }
        }

        if visible_start < scan.len() {
            outcome.text.push_str(&scan[visible_start..]);
        }
        outcome
    }

    /// Internal helper: re-enter `feed` with the bytes that came
    /// after a tool-call block that closed within the same chunk.
    /// Lets a single chunk contain multiple tool-call blocks and
    /// also process the text that follows them.
    fn resume_with(&mut self, mut outcome: XmlFeedOutcome, tail: String) -> XmlFeedOutcome {
        if tail.is_empty() {
            return outcome;
        }
        let more = self.feed(&tail);
        outcome.text.push_str(&more.text);
        outcome.hits.extend(more.hits);
        outcome
    }

    /// Flush any held text and any in-flight block. Call this on
    /// `TextEnd` and at stream end.
    pub fn flush(&mut self) -> XmlFeedOutcome {
        let mut outcome = XmlFeedOutcome::default();

        if self.in_block {
            let preview: String = self
                .body
                .chars()
                .take(64)
                .collect::<String>()
                .replace('\n', " ");
            let body = std::mem::take(&mut self.body);
            let open_tag = std::mem::take(&mut self.open_tag);
            let hit_name = std::mem::take(&mut self.hit_name);
            let raw = if body.is_empty() {
                format!(
                    "<unterminated tool-call block: tag={} name={} (no body received)>",
                    open_tag, hit_name
                )
            } else {
                format!(
                    "<unterminated tool-call block: tag={} name={} body={:?}>",
                    open_tag, hit_name, preview
                )
            };
            outcome.hits.push(InvalidToolCallHit {
                name: if hit_name.is_empty() {
                    "xml".to_string()
                } else {
                    hit_name
                },
                raw,
            });
            self.in_block = false;
        }

        if !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            outcome.text.push_str(&held);
        }

        outcome
    }

    /// When in_block, scan `self.body` for a closing tag matching
    /// `self.open_tag` (or, as a pragmatic fallback, the closing
    /// tag of any other recognized tag — some models wrap an inner
    /// tool-call block inside an outer `<tool_call>…</tool_call>`).
    /// On close, emit a hit and return the bytes that follow the
    /// outer's close (which the caller re-feeds through `feed` so
    /// any subsequent tool-call blocks in the same chunk are
    /// detected). When an inner close is matched, the outer
    /// close that wraps it is also consumed so the recursive
    /// `feed` does not re-detect it as a fresh tool-call opener.
    fn try_close_block(&mut self, outcome: &mut XmlFeedOutcome) -> Option<String> {
        if !self.in_block {
            return None;
        }
        const ALL_OPEN_TAGS: &[&str] = &[
            "antml:function_calls",
            "antml_function_calls",
            "function_calls",
            "response_create",
            "tool_call",
            "tool-call",
            "tool",
            "invoke",
        ];

        // Find the *earliest* recognized close tag in the body.
        // The inner block is what the model actually wanted; the
        // outer wrapper (e.g. `<tool_call>`) is cosmetic. We pick
        // the earliest close rather than the first matching name
        // so a `</tool>` inside a
        // `<tool_call>…</tool></tool_call>` wrapper wins.
        let mut best: Option<(usize, usize, String)> = None;
        for name in ALL_OPEN_TAGS {
            let needle = format!("</{}", name);
            if let Some((rel, gt)) = find_close(&self.body, &needle) {
                if best.as_ref().map_or(true, |b| rel < b.0) {
                    best = Some((rel, gt, name.to_string()));
                }
            }
        }
        let (inner_rel, inner_gt, _matched_name) = best?;
        let inner = &self.body[..inner_rel];
        let trimmed = inner.trim_start();
        let raw = if let Some(stripped) = strip_leading_tool_tag(trimmed) {
            stripped.trim().to_string()
        } else {
            trimmed.to_string()
        };
        if let Some(leading_name) = leading_tool_tag_name(trimmed) {
            if !leading_name.is_empty() {
                self.hit_name = leading_name;
            }
        }
        let name = std::mem::take(&mut self.hit_name);
        outcome.hits.push(InvalidToolCallHit { name, raw });

        // If the inner close we matched is NOT the outer's close,
        // also skip the outer's close so the caller doesn't see
        // the wrapper as a fresh tool-call opener. The outer's
        // close, if present, immediately follows the inner
        // close's `>`.
        let needle_outer = format!("</{}", self.open_tag);
        let outer_gt = if let Some((rel, gt)) = find_close(&self.body, &needle_outer) {
            if rel == inner_rel {
                inner_gt
            } else {
                gt
            }
        } else {
            inner_gt
        };
        let tail = self.body[outer_gt + 1..].to_string();
        self.body.clear();
        self.in_block = false;
        self.open_tag.clear();
        if tail.is_empty() {
            None
        } else {
            Some(tail)
        }
    }
}

/// If `s` starts with a recognized tool-call opening tag, return
/// the bytes after that opening tag. Used to peel a nested inner
/// opening tag off a body that was wrapped in an outer
/// `<tool_call>…</tool_call>` so the raw contains just the
/// arguments. Returns `None` if the leading text is not a
/// recognized opening tag.
fn strip_leading_tool_tag(s: &str) -> Option<&str> {
    if !s.starts_with('<') {
        return None;
    }
    let len = try_open_tool_call(s)?;
    Some(&s[len..])
}

/// If `s` starts with a recognized tool-call opening tag, return
/// the value of its `name="…"` attribute. Returns `None` if there
/// is no such tag, and an empty string if the tag has no `name`
/// attribute. Used to lift the inner tool name out of a
/// `<tool_call><tool name="shell">…</tool></tool_call>` wrapper.
fn leading_tool_tag_name(s: &str) -> Option<String> {
    if !s.starts_with('<') {
        return None;
    }
    let len = try_open_tool_call(s)?;
    Some(extract_name_attr(&s[..len]))
}

/// Find the first occurrence of `close` (case-insensitive) in
/// `body`. Returns `(start_of_close, byte_offset_of_closing_gt)`
/// where the closing `>` is the `>` that ends the close tag
/// itself. The caller can then slice `body[..start_of_close]` for
/// the inner body and `body[closing_gt + 1..]` for the bytes
/// after the close.
fn find_close(body: &str, close: &str) -> Option<(usize, usize)> {
    let rel = find_case_insensitive(body, close)?;
    let tail = &body[rel..];
    let gt_offset = tail.find('>')?;
    Some((rel, rel + gt_offset))
}

/// If `s` begins with a recognized tool-call opening tag (with its
/// trailing `>`), return the byte length of the opening tag
/// (including the `>`). Self-closing `<…/>` and paired `<…>`
/// forms both return the same length; the caller inspects the
/// last two characters to distinguish.
fn try_open_tool_call(s: &str) -> Option<usize> {
    if !s.starts_with('<') {
        return None;
    }
    const NAMES: &[&str] = &[
        "antml:function_calls",
        "antml_function_calls",
        "function_calls",
        "response_create",
        "tool_call",
        "tool-call",
        "tool",
        "invoke",
    ];
    for name in NAMES {
        let needle = format!("<{}", name);
        if s.len() < needle.len() {
            continue;
        }
        if !s[..needle.len()].eq_ignore_ascii_case(&needle) {
            continue;
        }
        let after = &s[needle.len()..];
        let after_first = after.chars().next();
        match after_first {
            None => return None,
            Some(c) => {
                if c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_' {
                    continue;
                }
            }
        }
        if let Some(gt) = after.find('>') {
            return Some(needle.len() + gt + 1);
        }
        // No `>` yet — caller treats this as a partial tag, holds
        // it for the next chunk.
        return None;
    }
    None
}

/// Extract the tag name (without `<`, `>`, or attributes) from a
/// full opening tag. Lower-cased for use in `</tag>` matching.
fn extract_tag_name(opening: &str) -> String {
    let s = opening.trim();
    if !s.starts_with('<') {
        return String::new();
    }
    let body = &s[1..];
    let body = body.trim_end_matches('/').trim_end_matches('>').trim();
    body.split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Extract the value of the `name="…"` (or `name='…'`) attribute
/// from an opening tag. Returns an empty string when the attribute
/// is missing. Used to label the hit with the tool name the model
/// intended to call instead of the tag name (`tool` / `invoke` /
/// `function_calls` / `antml:function_calls` / etc.).
fn extract_name_attr(opening: &str) -> String {
    let bytes = opening.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find `name` as a whole word. Case-insensitive.
        if i + 4 <= bytes.len() && bytes[i..i + 4].eq_ignore_ascii_case(b"name") {
            let after = &opening[i + 4..];
            let after = after.trim_start();
            let Some(after) = after.strip_prefix('=') else {
                i += 1;
                continue;
            };
            let after = after.trim_start();
            if let Some(rest) = after.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    return rest[..end].to_string();
                }
                return rest.trim_end_matches('>').to_string();
            }
            if let Some(rest) = after.strip_prefix('\'') {
                if let Some(end) = rest.find('\'') {
                    return rest[..end].to_string();
                }
                return rest.trim_end_matches('>').to_string();
            }
            // Unquoted value: read up to whitespace or `>`.
            let value: String = after
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '>')
                .collect();
            return value;
        }
        i += 1;
    }
    String::new()
}

fn find_byte_from(bytes: &[u8], needle: u8, start: usize) -> Option<usize> {
    if start >= bytes.len() {
        return None;
    }
    bytes[start..]
        .iter()
        .position(|&b| b == needle)
        .map(|p| p + start)
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut i = 0;
    while i + nb.len() <= hb.len() {
        if hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            return Some(i);
        }
        i += 1;
    }
    None
}

// Expose a hit id for the session runner. The runner wants stable,
// unique ids that the rest of the event stream can correlate on.
pub fn next_invalid_tool_call_id() -> String {
    next_hit_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(s: &mut XmlToolCallScanner, chunks: &[&str]) -> (String, Vec<InvalidToolCallHit>) {
        let mut text = String::new();
        let mut hits = Vec::new();
        for c in chunks {
            let out = s.feed(c);
            text.push_str(&out.text);
            hits.extend(out.hits);
        }
        let tail = s.flush();
        text.push_str(&tail.text);
        hits.extend(tail.hits);
        (text, hits)
    }

    #[test]
    fn passes_through_plain_text() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(&mut s, &["hello world"]);
        assert_eq!(text, "hello world");
        assert!(hits.is_empty());
        assert!(!s.in_block());
    }

    #[test]
    fn strips_tool_call_wrapper() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(
            &mut s,
            &[
                "hello ",
                "<tool_call><tool name=\"shell\">{\"command\":\"ls\"}</tool></tool_call> world",
            ],
        );
        assert_eq!(text, "hello  world");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "shell");
        assert_eq!(hits[0].raw, r#"{"command":"ls"}"#);
    }

    #[test]
    fn captures_double_quote_name() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(&mut s, &[r#"<tool name="read">{"path":"/tmp/x"}</tool>"#]);
        assert_eq!(text, "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "read");
        assert_eq!(hits[0].raw, r#"{"path":"/tmp/x"}"#);
    }

    #[test]
    fn captures_single_quote_name() {
        let mut s = XmlToolCallScanner::new();
        let (_, hits) = feed_all(
            &mut s,
            &[r#"<tool name='glob'>{"pattern":"**/*.rs"}</tool>"#],
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "glob");
        assert_eq!(hits[0].raw, r#"{"pattern":"**/*.rs"}"#);
    }

    #[test]
    fn captures_invoke_block() {
        let mut s = XmlToolCallScanner::new();
        let (_, hits) = feed_all(
            &mut s,
            &[r#"<invoke name="shell">{"command":"ls"}</invoke>"#],
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "shell");
        assert_eq!(hits[0].raw, r#"{"command":"ls"}"#);
    }

    #[test]
    fn captures_function_calls() {
        let mut s = XmlToolCallScanner::new();
        let (_, hits) = feed_all(
            &mut s,
            &[r#"<function_calls>{"name":"read"}</function_calls>"#],
        );
        assert_eq!(hits.len(), 1);
        // The `<function_calls>` form has no outer `name="…"`
        // attribute — the tool name lives inside the body. The
        // scanner reports the raw body for diagnostics and uses
        // "xml" as the synthetic hit name. The runner's feedback
        // message still tells the model what to fix.
        assert_eq!(hits[0].name, "xml");
        assert_eq!(hits[0].raw, r#"{"name":"read"}"#);
    }

    #[test]
    fn captures_antml_function_calls() {
        let mut s = XmlToolCallScanner::new();
        let (_, hits) = feed_all(
            &mut s,
            &[r#"<antml:function_calls>{"name":"read"}</antml:function_calls>"#],
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "xml");
        assert_eq!(hits[0].raw, r#"{"name":"read"}"#);
    }

    #[test]
    fn captures_self_closing_tool() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(&mut s, &[r#"<tool name="noop" />"#]);
        assert_eq!(text, "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "noop");
        assert_eq!(hits[0].raw, r#"<tool name="noop" />"#);
    }

    #[test]
    fn unknown_name_yields_xml_default() {
        let mut s = XmlToolCallScanner::new();
        let (_, hits) = feed_all(&mut s, &[r#"<tool>{"command":"ls"}</tool>"#]);
        assert_eq!(hits.len(), 1);
        // No `name` attribute — the runner labels this as "xml" so
        // the model gets a clear "you forgot the name" signal.
        assert_eq!(hits[0].name, "xml");
        assert_eq!(hits[0].raw, r#"{"command":"ls"}"#);
    }

    #[test]
    fn keeps_surrounding_text() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(&mut s, &[r#"before <tool name="x">body</tool> after"#]);
        assert_eq!(text, "before  after");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "x");
        assert_eq!(hits[0].raw, "body");
    }

    #[test]
    fn reassembles_block_split_across_chunks() {
        let mut s = XmlToolCallScanner::new();
        let chunks = [r#"<tool name="re"#, r#"ad">{"pat"#, r#"h":"/x"}</tool>"#];
        let (text, hits) = feed_all(&mut s, &chunks);
        assert_eq!(text, "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "read");
        assert_eq!(hits[0].raw, r#"{"path":"/x"}"#);
    }

    #[test]
    fn unterminated_block_emits_synthetic_hit_on_flush() {
        let mut s = XmlToolCallScanner::new();
        let out1 = s.feed(r#"<tool name="shell">{"command":"ls"}"#);
        assert_eq!(out1.text, "");
        assert!(out1.hits.is_empty());
        assert!(s.in_block());

        let tail = s.flush();
        assert!(tail.text.is_empty());
        assert_eq!(tail.hits.len(), 1);
        // The hit preserves the `name="shell"` attribute so the
        // model sees which tool it half-emitted.
        assert_eq!(tail.hits[0].name, "shell");
        assert!(tail.hits[0].raw.contains("unterminated"));
        assert!(tail.hits[0].raw.contains("shell"));
    }

    #[test]
    fn non_tool_tag_is_visible() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(&mut s, &[r#"<div>hello</div>"#]);
        assert_eq!(text, "<div>hello</div>");
        assert!(hits.is_empty());
    }

    #[test]
    fn nested_inside_paired_block_does_not_double_emit() {
        let mut s = XmlToolCallScanner::new();
        // The body of the outer tool block happens to contain a
        // string that looks like a tool tag. The scanner should
        // treat it as opaque body content, not as a nested block.
        let payload = r#"{"reasoning":"<tool name='inner'/>"}"#;
        let input = format!(r#"<tool name="outer">{payload}</tool>"#);
        let (text, hits) = feed_all(&mut s, &[&input]);
        assert_eq!(text, "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "outer");
        assert!(hits[0].raw.contains("inner"));
    }

    #[test]
    fn two_blocks_in_one_chunk() {
        let mut s = XmlToolCallScanner::new();
        let (text, hits) = feed_all(
            &mut s,
            &[r#"<tool name="a">1</tool> mid <tool name="b">2</tool>"#],
        );
        assert_eq!(text, " mid ");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "a");
        assert_eq!(hits[0].raw, "1");
        assert_eq!(hits[1].name, "b");
        assert_eq!(hits[1].raw, "2");
    }

    #[test]
    fn opening_tag_split_across_chunks() {
        let mut s = XmlToolCallScanner::new();
        let out1 = s.feed(r#"<tool nam"#);
        assert!(out1.text.is_empty());
        assert!(out1.hits.is_empty());

        let out2 = s.feed(r#"e="x">{"k":1}</tool>"#);
        assert_eq!(out2.text, "");
        assert_eq!(out2.hits.len(), 1);
        assert_eq!(out2.hits[0].name, "x");
        assert_eq!(out2.hits[0].raw, r#"{"k":1}"#);
    }

    #[test]
    fn held_partial_tag_flushes_as_visible_text() {
        let mut s = XmlToolCallScanner::new();
        // `<div` looks like a tag start; if no closing `>` ever
        // arrives, the held buffer is just visible text at flush.
        let out = s.feed(r#"<div"#);
        assert!(out.text.is_empty());
        let tail = s.flush();
        assert_eq!(tail.text, "<div");
        assert!(tail.hits.is_empty());
    }

    #[test]
    fn matches_capitalized_opening() {
        let mut s = XmlToolCallScanner::new();
        let (_, hits) = feed_all(&mut s, &[r#"<TOOL NAME="x">body</TOOL>"#]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "x");
        assert_eq!(hits[0].raw, "body");
    }

    #[test]
    fn hit_id_is_unique() {
        let a = next_invalid_tool_call_id();
        let b = next_invalid_tool_call_id();
        assert_ne!(a, b);
    }
}
