//! XML tool-call format used by Arachne.
//!
//! The LLM is told (via the system prompt) to invoke tools by emitting a
//! block of the form:
//!
//! ```text
//! <tool_name>
//! <arg_a>value</arg_a>
//! <arg_b>{"nested":"json"}</arg_b>
//! </tool_name>
//! ```
//!
//! The opening tag must be on its own line so a literal `<read>` inside a
//! markdown snippet doesn't get treated as a tool call. Arguments are
//! declared as child tags whose name matches the JSON-schema property
//! name. Values are plain text by default, or JSON (parsed if the inner
//! text starts with `{` or `[`) for nested/structured arguments.

use serde_json::Value;
use std::collections::HashSet;

use crate::llm::events::ToolDefinition;

/// Render the list of available tools in a form the LLM can read from the
/// system prompt. Mirrors the layout opencode uses, with XML argument
/// names matching the parser below.
pub fn render_tools_as_prompt(tools: &[ToolDefinition]) -> String {
    let mut out = String::from(
        "\n\n# Tools\n\n\
         To call a tool, emit a fenced block of the form shown under each tool below. \
         The opening `<tool_name>` tag must be on its own line, the arguments as \
         child tags, and the closing `</tool_name>` on its own line. Argument values are \
         plain text by default; if a value looks like JSON (starts with `{` or `[`) it is \
         parsed as JSON so you can pass nested objects/arrays. \
         Do not call a tool that is not listed below. Do not invent argument names. \
         Do not put any other prose inside the tool block — the closing tag must follow the arguments directly. \
         Tool calls that violate the active mode will be rejected by the runtime; plan accordingly.\n\n",
    );

    for tool in tools {
        out.push_str(&format!("\n## `{}`\n\n", tool.name));
        out.push_str(&tool.description);
        out.push_str("\n\nArguments:\n");

        if let Some(args) = extract_properties(&tool.parameters) {
            if args.is_empty() {
                out.push_str("- (no arguments)\n");
            }
            for arg in args {
                let required_marker = if arg.required { " **(required)**" } else { "" };
                out.push_str(&format!(
                    "- `<{name}>` ({type_hint}){required} — {description}\n",
                    name = arg.name,
                    type_hint = arg.type_hint,
                    required = required_marker,
                    description = arg.description,
                ));
            }
        } else {
            out.push_str(
                "- (schema unavailable; pass whatever arguments the description mentions)\n",
            );
        }
        out.push('\n');
    }

    out.push_str(
        "\nExample — calling the `read` tool with a path:\n\n\
         ```\n\
         <read>\n\
         <path>src/lib.rs</path>\n\
         </read>\n\
         ```\n",
    );

    out
}

#[derive(Debug)]
struct ArgumentSpec {
    name: String,
    description: String,
    type_hint: String,
    required: bool,
}

fn extract_properties(schema: &Value) -> Option<Vec<ArgumentSpec>> {
    let obj = schema.as_object()?;
    let properties = obj.get("properties")?.as_object()?;
    let required: HashSet<String> = obj
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut out = Vec::new();
    for (name, spec) in properties {
        let type_hint = type_hint_for(spec);
        let description = spec
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(ArgumentSpec {
            name: name.clone(),
            description,
            type_hint,
            required: required.contains(name),
        });
    }
    // Preserve declared order by sorting required first, then alphabetical.
    out.sort_by(|a, b| b.required.cmp(&a.required).then(a.name.cmp(&b.name)));
    Some(out)
}

fn type_hint_for(spec: &Value) -> String {
    if let Some(enum_values) = spec.get("enum").and_then(|v| v.as_array()) {
        let names: Vec<String> = enum_values
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .map(|s| format!("\"{s}\""))
            .collect();
        if !names.is_empty() {
            return format!("enum: {}", names.join(" | "));
        }
    }

    match spec.get("type").and_then(|v| v.as_str()) {
        Some("string") => "string".to_string(),
        Some("integer") => "integer".to_string(),
        Some("number") => "number".to_string(),
        Some("boolean") => "boolean".to_string(),
        Some("array") => {
            let items = spec.get("items").and_then(|v| v.as_str()).unwrap_or("any");
            format!("array<{items}>")
        }
        Some("object") => "object".to_string(),
        Some("null") => "null".to_string(),
        _ => "any".to_string(),
    }
}

/// Outcome of attempting to parse one complete tool block out of the
/// text buffer.
#[derive(Debug, PartialEq)]
pub enum ParsedToolBlock {
    /// A complete, well-formed tool block with all arguments parsed.
    Valid {
        id: String,
        tool: String,
        input: Value,
    },
    /// A complete block but with problems the model should know about
    /// (unknown tool, bad JSON, etc.). The runtime synthesises a
    /// `ToolError` from this and feeds it back to the LLM.
    Invalid { tool: String, reason: String },
    /// No complete block was found yet — keep accumulating text.
    Incomplete,
}

/// Ordered output from draining a text segment for XML tool blocks.
#[derive(Debug, PartialEq)]
pub enum DrainedToolSegment {
    /// Ordinary assistant text outside tool blocks.
    Text(String),
    /// A parsed tool block found in the text stream.
    Tool(ParsedToolBlock),
}

/// Pull the next complete `<tool_name>...</tool_name>` block out of the
/// start of `buffer`. Returns `(parsed_block, remaining_buffer)`.
///
/// The `known_tools` set is used to validate the tool name; an unknown
/// tool yields `Invalid` rather than `Valid` so the LLM gets feedback.
pub fn parse_next_tool_block(
    buffer: &str,
    known_tools: &HashSet<String>,
    id_counter: &mut u32,
) -> (ParsedToolBlock, String) {
    // Find the start of a tool block. Returns the resolved tool name
    // (using the `name="..."` attribute if present, falling back to
    // the wrapping tag name), plus the byte offsets of the opening
    // `<` and the byte right after the closing `>`. We also need the
    // original wrapping tag name to find the close marker, because
    // the LLM may use `<tool name="read">…</tool>` (close matches
    // the wrapper, not the resolved name).
    let (_open_byte_idx, tool_name, open_tag_end, close_tag_name) = match find_opening_tag(buffer) {
        Some(found) => found,
        None => return (ParsedToolBlock::Incomplete, buffer.to_string()),
    };

    // Skip past the entire opening tag (including any attributes and
    // the trailing `>`). For `<tool name="read">` this advances 18
    // bytes, not just `tool_name.len() + 2`.
    let after_open = &buffer[open_tag_end..];

    // Find the matching closing tag on its own line. The close tag
    // matches the wrapping element (e.g. `</tool>`), NOT the
    // resolved tool name from the `name` attribute — because the
    // LLM writes `<tool name="read">…</tool>` and we want to
    // recognize that.
    let close_marker = format!("</{}>", close_tag_name);
    let close_idx = match find_closing_tag(after_open, &close_marker) {
        Some(idx) => idx,
        None => return (ParsedToolBlock::Incomplete, buffer.to_string()),
    };

    let inner = &after_open[..close_idx];
    // The byte length of the consumed prefix — from the start of the
    // opening tag through the end of the closing tag.
    let consumed = open_tag_end + close_idx + close_marker.len();

    if !known_tools.contains(&tool_name) {
        let mut remaining = strip_prefix(buffer, consumed);
        skip_leading_whitespace(&mut remaining);
        return (
            ParsedToolBlock::Invalid {
                tool: tool_name.clone(),
                reason: format!(
                    "unknown tool '{tool_name}'. Available tools: {}",
                    sorted_known(known_tools)
                ),
            },
            remaining,
        );
    }

    match parse_inner_args(inner) {
        Ok(input) => {
            *id_counter += 1;
            let id = format!("xml-tool-{}", *id_counter);
            let mut remaining = strip_prefix(buffer, consumed);
            skip_leading_whitespace(&mut remaining);
            (
                ParsedToolBlock::Valid {
                    id,
                    tool: tool_name,
                    input,
                },
                remaining,
            )
        }
        Err(reason) => {
            let mut remaining = strip_prefix(buffer, consumed);
            skip_leading_whitespace(&mut remaining);
            (
                ParsedToolBlock::Invalid {
                    tool: tool_name,
                    reason,
                },
                remaining,
            )
        }
    }
}

fn skip_leading_whitespace(s: &mut String) {
    let trimmed = s.trim_start().to_string();
    *s = trimmed;
}

fn sorted_known(known: &HashSet<String>) -> String {
    let mut names: Vec<&str> = known.iter().map(String::as_str).collect();
    names.sort();
    names.join(", ")
}

fn strip_prefix(s: &str, n_bytes: usize) -> String {
    // `consumed` is in bytes, but `s.is_char_boundary(n_bytes)` should
    // hold because we only slice at tag boundaries. Guard anyway.
    if n_bytes >= s.len() {
        String::new()
    } else if s.is_char_boundary(n_bytes) {
        s[n_bytes..].to_string()
    } else {
        s.to_string()
    }
}

/// Find the first line that looks like a tool-block opening tag. The
/// tag may be either:
///
/// 1. `<name>` (opencode style, no attributes)
/// 2. `<name attr="...">` (XML attribute style, common when the LLM
///    uses `<tool name="websearch">` or `<parameter name="query">`)
///
/// In case (2) the `name` attribute is preferred as the tool identifier
/// if present; otherwise the tag name itself is used. This keeps the
/// parser robust to either convention the LLM invents.
///
/// Returns `(byte_offset_of_lt, resolved_tool_name, byte_offset_after_gt,
/// wrapper_tag_name)` so callers can advance past the full opening tag
/// and match the original closing tag.
fn find_opening_tag(s: &str) -> Option<(usize, String, usize, String)> {
    let mut cursor = 0;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(&['\n', '\r'][..]);
        if let Some((tag_name, name_attr)) = parse_tag_with_name_attr(trimmed) {
            // Tool opening tags can be `<tool>`, `<tool name="x">`, or
            // even `<tool _call>` (which we accept as a fallback to
            // the `name` attribute). We treat the resolved name as the
            // tool identifier.
            let close_tag_name = tag_name.clone();
            let resolved = match name_attr {
                Some(n) if !n.is_empty() => n,
                _ => tag_name,
            };
            if is_valid_tool_name(&resolved) {
                // End offset = cursor (start of line) + length of
                // trimmed line. For `<tool name="read">` the trimmed
                // length is 16 bytes, so end is 0 + 16 = 16.
                let end = cursor + trimmed.len();
                return Some((cursor, resolved, end, close_tag_name));
            }
        }
        cursor += line.len();
    }
    None
}

/// Tool names are lowercase identifiers, optionally with `_`. We
/// keep this stricter than the general XML-name check so that things
/// like `<tool _call>` (where `_call` is the tag name) get rejected
/// here, BUT we do allow names with hyphens to accommodate providers
/// that namespace their tools (e.g. `web-search`).
fn is_valid_tool_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn parse_open_tag_line(line: &str) -> Option<String> {
    if !line.starts_with('<') || !line.ends_with('>') {
        return None;
    }
    let inner = &line[1..line.len() - 1];
    if inner.is_empty() || inner.contains(' ') || inner.contains('\t') {
        return None;
    }
    if !inner
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    if !inner.chars().next().unwrap().is_ascii_alphabetic() && inner.chars().next().unwrap() != '_'
    {
        return None;
    }
    Some(inner.to_string())
}

/// Parse an XML element that may have a `name="..."` attribute. Used
/// for the tool-name opening tag (`<tool>`, `<tool name="websearch">`,
/// `<tool _call>`) and for argument tags (`<parameter name="query">`).
/// Returns `(tag_name_without_attrs, optional_name_attr_value)`.
fn parse_tag_with_name_attr(line: &str) -> Option<(String, Option<String>)> {
    if !line.starts_with('<') || !line.ends_with('>') {
        return None;
    }
    let inner = &line[1..line.len() - 1];
    if inner.is_empty() {
        return None;
    }

    // Split on whitespace so we can inspect attributes.
    let mut parts = inner.splitn(2, char::is_whitespace);
    let tag_name = parts.next()?.to_string();
    if !is_valid_xml_name(&tag_name) {
        return None;
    }

    let name_attr = parts.next().and_then(parse_name_attribute);
    Some((tag_name, name_attr))
}

/// Extract a `name="..."` (or `name='...'`) attribute value from a
/// raw attribute string. Tolerant of trailing whitespace and other
/// attributes we don't care about.
fn parse_name_attribute(attrs: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Look for `name=` or `name =`.
        if i + 5 <= bytes.len() && bytes[i..i + 4] == *b"name" {
            // Make sure the char after "name" is whitespace or `=`.
            let after = bytes.get(i + 4).copied().unwrap_or(b' ');
            if after == b'=' || after.is_ascii_whitespace() {
                let mut j = i + 4;
                if after.is_ascii_whitespace() {
                    // Skip whitespace then expect `=`.
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j >= bytes.len() || bytes[j] != b'=' {
                        i = j;
                        continue;
                    }
                    j += 1;
                } else {
                    j += 1; // skip the `=`
                }
                // Skip whitespace after `=`.
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= bytes.len() {
                    return None;
                }
                let quote = bytes[j];
                if quote != b'"' && quote != b'\'' {
                    return None;
                }
                j += 1;
                let start = j;
                while j < bytes.len() && bytes[j] != quote {
                    j += 1;
                }
                if j >= bytes.len() {
                    return None;
                }
                return Some(String::from_utf8_lossy(&bytes[start..j]).to_string());
            }
        }
        // Skip this attribute: advance to the next whitespace.
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    None
}

/// Returns true if `s` is a valid XML element/attribute name
/// (letters, digits, `_`, `-`, `.`, starting with a letter or `_`).
fn is_valid_xml_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn find_closing_tag(s: &str, marker: &str) -> Option<usize> {
    let mut cursor = 0;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(&['\n', '\r'][..]);
        if trimmed == marker {
            return Some(cursor);
        }
        cursor += line.len();
    }
    None
}

/// Parse the inner content of a tool block into a JSON object whose
/// keys are the argument names and values are the trimmed text (or
/// JSON-parsed if the trimmed text starts with `{` or `[`).
///
/// Tolerates three layouts:
///
/// 1. Opencode style: `<argname>value</argname>` (one per line, or
///    on the same line as the closing tag)
/// 2. XML-attribute style: `<parameter name="argname">value</parameter>`
///    — the LLM sometimes invents this form
/// 3. Split across multiple lines: `<name>\nvalue\n</name>`
fn parse_inner_args(inner: &str) -> Result<Value, String> {
    let mut args: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut remaining: &str = inner;

    loop {
        // Skip blank lines and leading whitespace.
        let trimmed_start = remaining
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        remaining = &remaining[trimmed_start..];
        if remaining.is_empty() {
            break;
        }

        // Find the next argument opening tag.
        let lt_idx = remaining
            .find('<')
            .ok_or_else(|| format!("expected an argument tag, got tail: {remaining:?}"))?;
        let gt_idx = remaining[lt_idx..]
            .find('>')
            .ok_or_else(|| format!("unterminated opening tag: {remaining:?}"))?;
        let open_tag = &remaining[lt_idx..=lt_idx + gt_idx];

        // Try the opencode style first: a bare tag like `<path>`.
        // If that fails, try the attribute style: `<parameter
        // name="path">`. In both cases the resolved key is the
        // argument name we want.
        let (arg_name, value_close_marker) = if let Some(name) = parse_open_tag_line(open_tag) {
            let marker = format!("</{name}>");
            (name, marker)
        } else if let Some((tag, Some(name_attr))) = parse_tag_with_name_attr(open_tag) {
            // Attribute style: <parameter name="...">...</parameter>.
            // The wrapping tag (here `parameter`) may be anything
            // the LLM picked; we use its name to find the close.
            let marker = format!("</{tag}>");
            (name_attr, marker)
        } else {
            return Err(format!("expected an argument tag, got: {open_tag:?}"));
        };

        let after_open = &remaining[lt_idx + gt_idx + 1..];
        let close_idx = after_open.find(&value_close_marker).ok_or_else(|| {
            format!("argument <{arg_name}> opened but not closed with {value_close_marker}")
        })?;
        let raw_value = &after_open[..close_idx].trim();
        let value = parse_value(raw_value);
        args.insert(arg_name, value);

        // Advance past the close marker (and any trailing whitespace).
        let after_close = &after_open[close_idx + value_close_marker.len()..];
        let trimmed_end = after_close
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after_close.len());
        remaining = &after_close[trimmed_end..];
    }

    Ok(Value::Object(args))
}

fn parse_value(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            return v;
        }
    }
    if trimmed.is_empty() {
        return Value::Null;
    }
    if trimmed == "true" {
        return Value::Bool(true);
    }
    if trimmed == "false" {
        return Value::Bool(false);
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = trimmed.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Value::Number(num);
        }
    }
    Value::String(trimmed.to_string())
}

/// Helper used by the runner to derive the known-tool set from a
/// `&[ToolDefinition]` slice.
pub fn known_tool_set(tools: &[ToolDefinition]) -> HashSet<String> {
    tools.iter().map(|t| t.name.clone()).collect()
}

/// Convenience wrapper that the runner calls on every `text_delta`
/// chunk. Calls `parse_next_tool_block` in a loop until no more
/// complete blocks remain, accumulating results. Returns the
/// remaining buffer (with all complete blocks removed) and the
/// sequence of blocks that were extracted in order.
pub fn drain_complete_tool_blocks(
    buffer: &str,
    known_tools: &HashSet<String>,
    id_counter: &mut u32,
) -> (String, Vec<ParsedToolBlock>) {
    let mut out = Vec::new();
    let mut current = buffer.to_string();

    loop {
        let (parsed, remaining) = parse_next_tool_block(&current, known_tools, id_counter);
        match parsed {
            ParsedToolBlock::Incomplete => break,
            other => {
                out.push(other);
                current = remaining;
            }
        }
    }

    (current, out)
}

/// Drain a complete assistant text segment into ordered text/tool items.
/// Unlike `drain_complete_tool_blocks`, this preserves prose around tool
/// blocks so the persisted assistant message can reconstruct the turn in
/// the same order the model produced it.
pub fn drain_tool_blocks_preserving_text(
    buffer: &str,
    known_tools: &HashSet<String>,
    id_counter: &mut u32,
) -> Vec<DrainedToolSegment> {
    let mut out = Vec::new();
    let mut current = buffer;

    loop {
        let Some((open_byte_idx, tool_name, open_tag_end, close_tag_name)) =
            find_opening_tag(current)
        else {
            if !current.is_empty() {
                out.push(DrainedToolSegment::Text(current.to_string()));
            }
            break;
        };

        if open_byte_idx > 0 {
            out.push(DrainedToolSegment::Text(
                current[..open_byte_idx].to_string(),
            ));
        }

        let after_open = &current[open_tag_end..];
        let close_marker = format!("</{}>", close_tag_name);
        let Some(close_idx) = find_closing_tag(after_open, &close_marker) else {
            out.push(DrainedToolSegment::Text(
                current[open_byte_idx..].to_string(),
            ));
            break;
        };

        let inner = &after_open[..close_idx];
        let consumed = open_tag_end + close_idx + close_marker.len();

        let block = if !known_tools.contains(&tool_name) {
            ParsedToolBlock::Invalid {
                tool: tool_name.clone(),
                reason: format!(
                    "unknown tool '{tool_name}'. Available tools: {}",
                    sorted_known(known_tools)
                ),
            }
        } else {
            match parse_inner_args(inner) {
                Ok(input) => {
                    *id_counter += 1;
                    ParsedToolBlock::Valid {
                        id: format!("xml-tool-{}", *id_counter),
                        tool: tool_name,
                        input,
                    }
                }
                Err(reason) => ParsedToolBlock::Invalid {
                    tool: tool_name,
                    reason,
                },
            }
        };
        out.push(DrainedToolSegment::Tool(block));

        current = &current[consumed..];
    }

    out
}

// Convenience constructors for tests and call sites that want to
// construct a `ToolDefinition` quickly.
pub fn make_tool(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

/// Returns a fresh, monotonically-increasing id for parsed tool calls.
pub fn new_id_counter() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, properties: Value, required: &[&str]) -> ToolDefinition {
        let mut schema = json!({ "type": "object", "properties": properties });
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        ToolDefinition {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: schema,
        }
    }

    fn known(tools: &[ToolDefinition]) -> HashSet<String> {
        tools.iter().map(|t| t.name.clone()).collect()
    }

    // ---------- render_tools_as_prompt ----------

    #[test]
    fn render_tools_prompt_lists_each_tool_with_arguments() {
        let tools = vec![
            tool(
                "read",
                json!({
                    "path": { "type": "string", "description": "File to read" },
                    "offset": { "type": "integer" },
                    "limit": { "type": "integer" }
                }),
                &["path"],
            ),
            tool(
                "shell",
                json!({
                    "command": { "type": "string", "description": "Shell command" },
                    "workdir": { "type": "string" }
                }),
                &["command"],
            ),
        ];
        let prompt = render_tools_as_prompt(&tools);
        assert!(prompt.contains("## `read`"));
        assert!(prompt.contains("## `shell`"));
        assert!(prompt.contains("`<path>`"));
        assert!(prompt.contains("**(required)**"));
        assert!(prompt.contains("(string)"));
        assert!(prompt.contains("(integer)"));
        // First argument listed under each tool should be a required one
        // (the renderer sorts required first).
        let read_section_start = prompt.find("## `read`").unwrap();
        let read_section_end = prompt.find("## `shell`").unwrap();
        let read_section = &prompt[read_section_start..read_section_end];
        let path_idx = read_section.find("<path>").unwrap();
        let offset_idx = read_section.find("<offset>").unwrap();
        assert!(
            path_idx < offset_idx,
            "required path must come before optional offset"
        );
    }

    #[test]
    fn render_tools_prompt_handles_tool_with_no_arguments() {
        let tools = vec![tool("noop", json!({}), &[])];
        let prompt = render_tools_as_prompt(&tools);
        assert!(prompt.contains("## `noop`"));
        assert!(prompt.contains("(no arguments)"));
    }

    #[test]
    fn render_tools_prompt_handles_tool_with_no_schema() {
        let tools = vec![ToolDefinition {
            name: "mystery".to_string(),
            description: "no schema".to_string(),
            parameters: json!(null),
        }];
        let prompt = render_tools_as_prompt(&tools);
        assert!(prompt.contains("## `mystery`"));
        assert!(prompt.contains("(schema unavailable"));
    }

    #[test]
    fn render_tools_prompt_includes_xml_invocation_example() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let prompt = render_tools_as_prompt(&tools);
        assert!(prompt.contains("<read>"));
        assert!(prompt.contains("<path>src/lib.rs</path>"));
        assert!(prompt.contains("</read>"));
    }

    // ---------- parse_next_tool_block ----------

    #[test]
    fn parser_extracts_complete_block_with_string_arg() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "I will read the file.\n<read>\n<path>src/lib.rs</path>\n</read>\nDone.";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid { tool, input, id } => {
                assert_eq!(tool, "read");
                assert_eq!(input["path"], "src/lib.rs");
                assert_eq!(id, "xml-tool-1");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
        assert_eq!(remaining, "Done.");
    }

    #[test]
    fn parser_keeps_buffer_when_block_is_incomplete() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "Reading...\n<read>\n<path>src/lib.rs</path>\n"; // missing </read>
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        assert_eq!(parsed, ParsedToolBlock::Incomplete);
        assert_eq!(remaining, buffer);
    }

    #[test]
    fn parser_handles_multiple_blocks_in_one_buffer() {
        let tools = vec![
            tool("read", json!({"path": {"type":"string"}}), &["path"]),
            tool("shell", json!({"command": {"type":"string"}}), &["command"]),
        ];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer =
            "<read>\n<path>a.rs</path>\n</read>\nThen:\n<shell>\n<command>ls</command>\n</shell>\n";
        let (mut parsed, mut remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match &parsed {
            ParsedToolBlock::Valid { tool, input, .. } => {
                assert_eq!(tool, "read");
                assert_eq!(input["path"], "a.rs");
            }
            other => panic!("expected Valid read, got {other:?}"),
        }
        (parsed, remaining) = parse_next_tool_block(&remaining, &known_set, &mut id);
        match &parsed {
            ParsedToolBlock::Valid { tool, input, .. } => {
                assert_eq!(tool, "shell");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected Valid shell, got {other:?}"),
        }
        assert!(remaining.is_empty());
    }

    #[test]
    fn parser_ignores_tag_like_text_in_prose_lines() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        // The `<read>` here is mid-line, not on its own line, so it should not trigger.
        let buffer = "Use the <read> tool to open the file.\n";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        assert_eq!(parsed, ParsedToolBlock::Incomplete);
        assert_eq!(remaining, buffer);
    }

    #[test]
    fn parser_rejects_unknown_tool_name() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<shell>\n<command>ls</command>\n</shell>\n";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Invalid { tool, reason } => {
                assert_eq!(tool, "shell");
                assert!(reason.contains("unknown tool"));
                assert!(reason.contains("read"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        // The malformed block should still be consumed so the loop can make progress.
        assert!(remaining.is_empty());
    }

    #[test]
    fn parser_parses_json_object_arg() {
        let tools = vec![tool(
            "edit",
            json!({"old_string": {"type":"string"}, "new_string": {"type":"string"}}),
            &["old_string", "new_string"],
        )];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<edit>\n<old_string>foo</old_string>\n<new_string>{\"replacement\":\"bar\"}</new_string>\n</edit>\n";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid { tool, input, .. } => {
                assert_eq!(tool, "edit");
                assert_eq!(input["old_string"], "foo");
                assert_eq!(input["new_string"]["replacement"], "bar");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
        assert!(remaining.is_empty());
    }

    #[test]
    fn parser_parses_boolean_int_float_args() {
        let tools = vec![tool(
            "fancy",
            json!({
                "flag": {"type":"boolean"},
                "count": {"type":"integer"},
                "ratio": {"type":"number"},
            }),
            &[],
        )];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer =
            "<fancy>\n<flag>true</flag>\n<count>42</count>\n<ratio>0.75</ratio>\n</fancy>\n";
        let (parsed, _) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid { input, .. } => {
                assert_eq!(input["flag"], true);
                assert_eq!(input["count"], 42);
                assert_eq!(input["ratio"], 0.75);
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn parser_reports_unclosed_argument_tag() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<read>\n<path>src/lib.rs\n</read>\n";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Invalid { reason, .. } => {
                assert!(reason.contains("opened but not closed"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert!(remaining.is_empty());
    }

    #[test]
    fn parser_handles_text_around_tool_block() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "Sure thing.\n<read>\n<path>x.rs</path>\n</read>\nLet me know.";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        assert!(matches!(parsed, ParsedToolBlock::Valid { .. }));
        assert_eq!(remaining, "Let me know.");
    }

    // ---------- drain_complete_tool_blocks ----------

    #[test]
    fn drain_pulls_multiple_blocks_in_order() {
        let tools = vec![
            tool("read", json!({"path": {"type":"string"}}), &["path"]),
            tool("shell", json!({"command": {"type":"string"}}), &["command"]),
        ];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<read>\n<path>a.rs</path>\n</read>\nmiddle\n<shell>\n<command>ls</command>\n</shell>\ntail";
        let (remaining, blocks) = drain_complete_tool_blocks(buffer, &known_set, &mut id);
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            ParsedToolBlock::Valid { tool, .. } => assert_eq!(tool, "read"),
            other => panic!("expected Valid, got {other:?}"),
        }
        match &blocks[1] {
            ParsedToolBlock::Valid { tool, .. } => assert_eq!(tool, "shell"),
            other => panic!("expected Valid, got {other:?}"),
        }
        assert_eq!(remaining, "tail");
    }

    #[test]
    fn drain_preserves_unterminated_block_in_remaining() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<read>\n<path>partial";
        let (remaining, blocks) = drain_complete_tool_blocks(buffer, &known_set, &mut id);
        assert!(blocks.is_empty());
        assert_eq!(remaining, buffer);
    }

    #[test]
    fn drain_with_text_preserves_ordered_prose_and_tool_blocks() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "Sure thing.\n<read>\n<path>x.rs</path>\n</read>\nDone.";
        eprintln!("=== INPUT ===\n{buffer}\n=== END INPUT ===");
        let segments = drain_tool_blocks_preserving_text(buffer, &known_set, &mut id);

        eprintln!("=== SEGMENTS ({}) ===", segments.len());
        for (i, seg) in segments.iter().enumerate() {
            match seg {
                DrainedToolSegment::Text(t) => eprintln!("[{i}] Text: {:?}", t),
                DrainedToolSegment::Tool(ParsedToolBlock::Valid { id, tool, input }) => {
                    eprintln!("[{i}] Tool Valid: id={id} tool={tool} input={input}");
                }
                DrainedToolSegment::Tool(ParsedToolBlock::Invalid { tool, reason }) => {
                    eprintln!("[{i}] Tool Invalid: tool={tool} reason={reason}");
                }
                DrainedToolSegment::Tool(ParsedToolBlock::Incomplete) => {
                    eprintln!("[{i}] Tool Incomplete");
                }
            }
        }
        eprintln!("=== END SEGMENTS ===");

        assert_eq!(segments.len(), 3);
        assert_eq!(
            segments[0],
            DrainedToolSegment::Text("Sure thing.\n".to_string())
        );
        match &segments[1] {
            DrainedToolSegment::Tool(ParsedToolBlock::Valid { tool, input, .. }) => {
                assert_eq!(tool, "read");
                assert_eq!(input["path"], "x.rs");
            }
            other => panic!("expected valid tool segment, got {other:?}"),
        }
        assert_eq!(segments[2], DrainedToolSegment::Text("\nDone.".to_string()));
    }

    // ---------- LLM-actual-style parsing ----------
    // These cover the format the LLM was observed to emit in the
    // debug logs: <tool _call>\n<parameter name="x">value</parameter>\n</tool>.
    // The runner needs to handle this even when the LLM invents tag
    // names that don't match the opencode style.

    #[test]
    fn llm_attribute_style_parameter_is_parsed() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        // The LLM used `<tool _call>` (no name attribute) plus
        // `<parameter name="path">src/lib.rs</parameter>`.
        let buffer = "<tool _call>\n<parameter name=\"path\">src/lib.rs</parameter>\n</tool>\n";
        let (parsed, _remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Invalid { tool, reason } => {
                // `_call` is not a known tool, so the parser
                // produces an Invalid block. The LLM gets
                // feedback to use a known tool name.
                assert_eq!(tool, "_call");
                assert!(reason.contains("unknown tool"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn llm_attribute_style_with_known_tool_via_name_attr() {
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        // The LLM used `<tool name="read">` to name the tool.
        let buffer =
            "<tool name=\"read\">\n<parameter name=\"path\">src/lib.rs</parameter>\n</tool>\n";
        let (parsed, remaining) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid {
                tool,
                input,
                id: call_id,
            } => {
                assert_eq!(tool, "read");
                assert_eq!(input["path"], "src/lib.rs");
                assert_eq!(call_id, "xml-tool-1");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
        assert!(remaining.is_empty());
    }

    #[test]
    fn llm_attribute_style_with_multiple_parameters() {
        let tools = vec![tool(
            "shell",
            json!({
                "command": {"type": "string"},
                "workdir": {"type": "string"}
            }),
            &["command"],
        )];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<shell>\n<parameter name=\"command\">cargo build</parameter>\n<parameter name=\"workdir\">/tmp/proj</parameter>\n</shell>\n";
        let (parsed, _) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid { tool, input, .. } => {
                assert_eq!(tool, "shell");
                assert_eq!(input["command"], "cargo build");
                assert_eq!(input["workdir"], "/tmp/proj");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn opencode_style_still_works() {
        // Sanity check: the original style must keep working.
        let tools = vec![tool("read", json!({"path": {"type":"string"}}), &["path"])];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<read>\n<path>src/lib.rs</path>\n</read>\n";
        let (parsed, _) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid { tool, input, .. } => {
                assert_eq!(tool, "read");
                assert_eq!(input["path"], "src/lib.rs");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn mixed_styles_in_same_block_are_rejected_clearly() {
        // Mixing styles (one arg as bare tag, one as <parameter>)
        // should still parse — the parser handles each line
        // independently.
        let tools = vec![tool(
            "shell",
            json!({
                "command": {"type": "string"},
                "workdir": {"type": "string"}
            }),
            &["command"],
        )];
        let known_set = known(&tools);
        let mut id = new_id_counter();
        let buffer = "<shell>\n<command>cargo build</command>\n<parameter name=\"workdir\">/tmp</parameter>\n</shell>\n";
        let (parsed, _) = parse_next_tool_block(buffer, &known_set, &mut id);
        match parsed {
            ParsedToolBlock::Valid { tool, input, .. } => {
                assert_eq!(tool, "shell");
                assert_eq!(input["command"], "cargo build");
                assert_eq!(input["workdir"], "/tmp");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn parse_name_attribute_handles_double_and_single_quotes() {
        assert_eq!(parse_name_attribute("name=\"foo\"").as_deref(), Some("foo"));
        assert_eq!(parse_name_attribute("name='bar'").as_deref(), Some("bar"));
        assert_eq!(
            parse_name_attribute("name = \"with space\"").as_deref(),
            Some("with space")
        );
        assert_eq!(
            parse_name_attribute("type=\"x\" name=\"query\" other=\"y\"").as_deref(),
            Some("query")
        );
        assert_eq!(parse_name_attribute("").as_deref(), None);
        assert_eq!(parse_name_attribute("id=\"x\"").as_deref(), None);
        assert_eq!(parse_name_attribute("name=unquoted").as_deref(), None);
    }

    #[test]
    fn is_valid_tool_name_rejects_uppercase_and_specials() {
        assert!(is_valid_tool_name("read"));
        assert!(is_valid_tool_name("_call"));
        assert!(is_valid_tool_name("web-search"));
        assert!(is_valid_tool_name("tool_2"));
        assert!(!is_valid_tool_name("Read")); // uppercase
        assert!(!is_valid_tool_name("2tool")); // digit-start
        assert!(!is_valid_tool_name("")); // empty
    }
}
