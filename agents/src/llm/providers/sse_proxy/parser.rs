//! Minimal SSE frame extractor. Operates on UTF-8 byte chunks coming back
//! from the upstream body. We keep state across calls so a single SSE
//! event can span multiple `read()` calls.

use std::str;

/// One SSE event the proxy cares about. Other event types are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// `data: <payload>` line (single- or multi-line concatenated).
    Data(String),
    /// End-of-stream marker (`data: [DONE]` after trim).
    Done,
}

/// Stateful extractor. Feed it bytes via [`SseParser::feed`]; it returns
/// zero or more [`SseEvent`]s. `event:`/`id:` are kept so multi-line
/// `data:` is concatenated correctly per the SSE spec.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
    current_data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        // Lossy UTF-8 decode: SSE is supposed to be UTF-8; if a chunk
        // straddles a code point we accept the replacement char rather
        // than panicking. The diagnostics value of a perfectly lossless
        // decode is not worth the complexity here.
        let text = match str::from_utf8(chunk) {
            Ok(text) => text.to_string(),
            Err(error) => {
                let valid = error.valid_up_to();
                let mut partial = String::from_utf8_lossy(&chunk[..valid]).into_owned();
                partial.push('\u{FFFD}');
                partial.push_str(&String::from_utf8_lossy(&chunk[error.valid_up_to()..]));
                partial
            }
        };
        self.buffer.push_str(&text);

        let mut emitted = Vec::new();
        while let Some(newline_index) = self.buffer.find('\n') {
            let mut line: String = self.buffer.drain(..=newline_index).collect();
            if line.ends_with('\n') {
                line.pop();
            }
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(event) = self.consume_line(&line) {
                emitted.push(event);
            }
        }
        emitted
    }

    pub fn flush(&mut self) -> Vec<SseEvent> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let mut tail = String::new();
        std::mem::swap(&mut self.buffer, &mut tail);
        let mut emitted = Vec::new();
        for line in tail.split('\n') {
            let line = line.trim_end_matches('\r');
            if let Some(event) = self.consume_line(line) {
                emitted.push(event);
            }
        }
        if let Some(event) = self.dispatch_event() {
            emitted.push(event);
        }
        emitted
    }

    fn consume_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch_event();
        }
        if let Some(_value) = line.strip_prefix(':') {
            // Comment line — ignore.
            return None;
        }
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            if value.trim() == "[DONE]" {
                return Some(SseEvent::Done);
            }
            self.current_data.push(value.to_string());
            return None;
        }
        // Unknown field — ignore per spec.
        None
    }

    fn dispatch_event(&mut self) -> Option<SseEvent> {
        if self.current_data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.current_data);
        let payload = data.join("\n");
        Some(SseEvent::Data(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_event() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: hello world\n\n");
        assert_eq!(events, vec![SseEvent::Data("hello world".to_string())]);
    }

    #[test]
    fn parses_done_marker() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: [DONE]\n\n");
        assert_eq!(events.last(), Some(&SseEvent::Done));
    }

    #[test]
    fn concatenates_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(events, vec![SseEvent::Data("line1\nline2".to_string())]);
    }

    #[test]
    fn handles_split_chunks() {
        let mut parser = SseParser::new();
        let first = parser.feed(b"data: hel");
        assert!(first.is_empty());
        let second = parser.feed(b"lo\n\n");
        assert_eq!(second, vec![SseEvent::Data("hello".to_string())]);
    }

    #[test]
    fn ignores_comment_lines() {
        let mut parser = SseParser::new();
        let events = parser.feed(b": keepalive\ndata: actual\n\n");
        assert_eq!(events, vec![SseEvent::Data("actual".to_string())]);
    }
}
