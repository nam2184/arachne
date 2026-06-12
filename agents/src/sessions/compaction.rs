//! Auto-compaction that follows opencode's flow:
//!
//! 1. **Select**: split the persisted conversation into a "head" to
//!    summarize and a "recent" tail to keep verbatim, sized against a
//!    `KEEP_RECENT_TOKENS` budget so we never blow the model window
//!    while leaving room for the summary output.
//! 2. **Prompt**: build a deterministic prompt that asks the model
//!    to emit the same Markdown template opencode uses
//!    (`## Goal`, `## Constraints & Preferences`, `## Progress`, …).
//!    If a previous summary exists, we ask the model to update it
//!    rather than start from scratch.
//! 3. **Stream**: open a single `provider.stream(...)` call against
//!    the same provider/model the session is using, with
//!    `max_tokens = SUMMARY_OUTPUT_TOKENS`, and collect the text
//!    deltas into a single `summary` string.
//! 4. **Persist**: replace the persisted `messages` with the
//!    new `summary` so the next LLM turn sees the summary
//!    checkpoint instead of the full history.
//!
//! The runner calls `compactor.compact_if_needed` before each turn;
//! if the model still rejects the request (because compaction
//! failed or the summary itself is too large), the runner surfaces
//! `SessionError::ContextOverflow` so the UI can prompt the user.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tracing::{info, warn};

use crate::llm::events::LlmEvent;
use crate::llm::providers::LlmProvider;
use crate::llm::request::{ContentPart, LlmMessage, LlmRequest};
use crate::llm::ProviderRegistry;
use crate::model_spec::ModelRegistry;
use crate::sessions::conversation::{ConversationMessage, ConversationService};
use crate::tools::estimate_tokens;

/// Opencode-aligned defaults. All tunable via the `CompactionConfig`
/// argument; the constants here are the "no config provided" values.
pub const DEFAULT_COMPACTION_BUFFER: usize = 20_000;
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 8_000;
pub const SUMMARY_OUTPUT_TOKENS: usize = 4_096;
pub const SUMMARY_TOOL_OUTPUT_MAX_CHARS: usize = 2_000;
pub const TOOL_OUTPUT_PREVIEW_FRACTION: f32 = 0.5;

const SUMMARY_TEMPLATE: &str = r#"Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Goal
- [single-sentence task summary]

## Constraints & Preferences
- [user constraints, preferences, specs, or "(none)"]

## Progress
### Done
- [completed work or "(none)"]

### In Progress
- [current work or "(none)"]

### Blocked
- [blockers or "(none)"]

## Key Decisions
- [decision and why, or "(none)"]

## Next Steps
- [ordered next actions, or "(none)"]

## Critical Context
- [important technical facts, errors, open questions, or "(none)"]

## Relevant Files
- [file or directory path: why it matters, or "(none)"]
</template>

Rules:
- Keep every section, even when empty.
- Use terse bullets, not prose paragraphs.
- Preserve exact file paths, commands, error strings, and identifiers when known.
- Do not mention the summary process or that context was compacted."#;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Whether the compactor may run automatically. Mirrors
    /// `Config.Document.compaction.auto` in opencode.
    pub auto: bool,
    /// Token buffer reserved for the model's output and tail
    /// conversation (opencode: `DEFAULT_BUFFER`).
    pub buffer_tokens: usize,
    /// Maximum tokens preserved verbatim from the most recent
    /// turns after compaction (opencode: `preserve_recent_tokens`).
    pub keep_recent_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            buffer_tokens: DEFAULT_COMPACTION_BUFFER,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub session_id: String,
    /// Provider name as stored on the session row.
    pub provider: String,
    /// Model id as stored on the session row.
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionOutcome {
    /// No compaction was needed.
    NotNeeded,
    /// Compaction ran and produced a new summary.
    Compacted { summary: String },
    /// Compaction was needed but failed; the runner should surface
    /// the error to the UI.
    Failed { reason: String },
}

/// The compactor drives the LLM-side summary flow. It holds
/// references to the services it needs so the runner can keep
/// doing I/O without re-plumbing dependencies.
pub struct CompactionService {
    conversation_service: Arc<ConversationService>,
    providers: Arc<ProviderRegistry>,
    model_registry: Arc<ModelRegistry>,
    config: CompactionConfig,
}

impl CompactionService {
    pub fn new(
        conversation_service: Arc<ConversationService>,
        providers: Arc<ProviderRegistry>,
        model_registry: Arc<ModelRegistry>,
        config: CompactionConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            conversation_service,
            providers,
            model_registry,
            config,
        })
    }

    pub fn config(&self) -> CompactionConfig {
        self.config
    }

    pub fn model_registry(&self) -> &ModelRegistry {
        &self.model_registry
    }

    /// Pre-dispatch check: estimate the size of the next request
    /// against the model context window. If the assembled request
    /// would exceed `context - max_output`, return the estimated
    /// token count so the caller can trigger compaction.
    pub fn estimate_request_tokens(&self, request: &LlmRequest) -> usize {
        let spec = self
            .model_registry
            .lookup(&request.provider, &request.model);
        let system_chars: usize = request.system.iter().map(|s| s.len()).sum();
        let system_tokens = estimate_tokens(&" ".repeat(system_chars));
        let message_tokens: usize = request
            .messages
            .iter()
            .map(|m| {
                let chars: usize = m
                    .content
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => text.len(),
                        ContentPart::Reasoning { text } => text.len(),
                        ContentPart::ToolCall { input, .. } => {
                            serde_json::to_string(input).map(|s| s.len()).unwrap_or(0)
                        }
                        ContentPart::ToolResult { result, .. } => {
                            serde_json::to_string(result).map(|s| s.len()).unwrap_or(0)
                        }
                    })
                    .sum();
                estimate_tokens(&" ".repeat(chars))
            })
            .sum();
        let tool_tokens: usize = request
            .tools
            .iter()
            .map(|t| {
                estimate_tokens(&t.name)
                    + estimate_tokens(&t.description)
                    + serde_json::to_string(&t.parameters)
                        .map(|s| estimate_tokens(&s))
                        .unwrap_or(0)
            })
            .sum();
        let total = system_tokens + message_tokens + tool_tokens;
        let _ = spec;
        total
    }

    /// Pre-dispatch check used by the runner. Returns
    /// `Some(estimated)` if compaction should run.
    pub fn should_compact(&self, request: &LlmRequest) -> Option<usize> {
        if !self.config.auto {
            return None;
        }
        let spec = self
            .model_registry
            .lookup(&request.provider, &request.model);
        if spec.context_window == 0 {
            return None;
        }
        let estimated = self.estimate_request_tokens(request);
        let budget = spec
            .context_window
            .saturating_sub(spec.max_output.max(self.config.buffer_tokens));
        if estimated > budget {
            Some(estimated)
        } else {
            None
        }
    }

    /// Run the compaction flow: select, prompt, stream, persist.
    /// This is what the runner calls when `should_compact` reports
    /// overflow, and what the Tauri `compact_now` command calls
    /// when the user explicitly asks for compaction.
    pub async fn compact_now(&self, request: CompactionRequest) -> CompactionOutcome {
        let CompactionRequest {
            session_id,
            provider,
            model,
        } = request;

        let conv = match self.conversation_service.read_ai_conversation(&session_id) {
            Ok(conv) => conv,
            Err(error) => {
                warn!(session_id = %session_id, error = %error, "compact_now: failed to read conversation");
                return CompactionOutcome::Failed {
                    reason: format!("read conversation: {error}"),
                };
            }
        };

        info!(
            session_id = %session_id,
            persisted_messages = conv.messages.len(),
            persisted_message_ids = ?conv.messages.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            persisted_message_roles = ?conv.messages.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
            persisted_message_content_chars = ?conv.messages.iter().map(|m| m.content.len()).collect::<Vec<_>>(),
            persisted_summary_present = conv.summary.is_some(),
            persisted_summary_chars = conv.summary.as_deref().map(|s| s.len()).unwrap_or(0),
            "compact_now: read conversation"
        );

        // Combine the persisted summary and any prior recent-context
        // block with the live messages so the compactor has the
        // same view the runner is about to send to the provider.
        // This is the source of truth the runner relies on for the
        // pre-check.
        let mut synthesized_messages: Vec<ConversationMessage> = Vec::new();
        if let Some(summary) = conv.summary.as_deref() {
            if !summary.trim().is_empty() {
                synthesized_messages.push(ConversationMessage {
                    id: format!("summary-{}", uuid::Uuid::new_v4()),
                    role: "system".to_string(),
                    content: format!("<conversation-summary>\n{summary}\n</conversation-summary>"),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
        synthesized_messages.extend(conv.messages.iter().cloned());

        let (head, recent) = match self.select_from_persisted(&synthesized_messages) {
            Some(selected) => selected,
            None => {
                info!(
                    session_id = %session_id,
                    synthesized_message_count = synthesized_messages.len(),
                    "compact_now: nothing to compact"
                );
                return CompactionOutcome::NotNeeded;
            }
        };

        let prompt = build_prompt(conv.summary.as_deref(), &head);
        let summary_request = LlmRequest::new(&model, &provider)
            .with_message(LlmMessage::user(prompt))
            .max_tokens(SUMMARY_OUTPUT_TOKENS as u32);

        let provider_handle = match self.providers.get(&provider).await {
            Some(p) => p,
            None => {
                return CompactionOutcome::Failed {
                    reason: format!("no provider registered for {provider}"),
                };
            }
        };

        let summary = match stream_summary(&*provider_handle, summary_request).await {
            Ok(text) => text,
            Err(error) => {
                warn!(
                    session_id = %session_id,
                    provider = %provider,
                    model = %model,
                    error = %error,
                    "compact_now: provider stream failed"
                );
                return CompactionOutcome::Failed { reason: error };
            }
        };

        let trimmed = summary.trim();
        if trimmed.is_empty() {
            return CompactionOutcome::Failed {
                reason: "provider returned empty summary".to_string(),
            };
        }

        if let Err(error) = self.conversation_service.compact_conversation_with_recent(
            &session_id,
            trimmed,
            &recent,
        ) {
            warn!(session_id = %session_id, error = %error, "compact_now: failed to persist summary");
            return CompactionOutcome::Failed {
                reason: format!("persist: {error}"),
            };
        }

        info!(
            session_id = %session_id,
            provider = %provider,
            model = %model,
            summary_chars = trimmed.len(),
            "compact_now: summary persisted"
        );

        CompactionOutcome::Compacted {
            summary: trimmed.to_string(),
        }
    }

    /// Pick which persisted messages to keep verbatim and which to
    /// send for summarization. Mirrors opencode's `select`:
    ///
    /// - The conversation is serialized into a single string per
    ///   message.
    /// - We walk backwards from the most recent message, keeping
    ///   the most recent turns verbatim until we hit the
    ///   `keep_recent_tokens` budget.
    /// - Everything older is concatenated into `head` (and ends up
    ///   in the LLM summarization prompt).
    /// - If a single message itself exceeds the budget, we split
    ///   it: keep the tail of the message and summarize the head.
    /// - If after that split the `head` is empty, we fall back to
    ///   keeping the most recent message in `head` and pushing the
    ///   rest to `recent` so the compactor always has *something*
    ///   to summarize when the runner reports overflow.
    fn select_from_persisted(&self, messages: &[ConversationMessage]) -> Option<(String, String)> {
        if messages.is_empty() {
            return None;
        }
        let serialized: Vec<String> = messages
            .iter()
            .map(serialize_message)
            .filter(|s| !s.trim().is_empty())
            .collect();
        let result = self.select_from_serialized(&serialized);
        if result.is_some() {
            return result;
        }
        // Fallback: keep the most recent message as `head` so the
        // compactor can still emit a summary when the only thing
        // left on disk is a `<recent-context>` system block.
        if let Some(last) = messages.last() {
            let head = serialize_message(last);
            if !head.trim().is_empty() {
                return Some((head, String::new()));
            }
        }
        None
    }

    fn select_from_serialized(&self, serialized: &[String]) -> Option<(String, String)> {
        let serialized: Vec<&str> = serialized
            .iter()
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .collect();
        if serialized.is_empty() {
            return None;
        }

        let mut total = 0usize;
        let mut split = serialized.len();
        let mut split_prefix = String::new();
        let mut split_suffix = String::new();

        for index in (0..serialized.len()).rev() {
            let entry = serialized[index];
            let next = total + estimate_tokens(entry);
            if next > self.config.keep_recent_tokens {
                let remaining_chars = self
                    .config
                    .keep_recent_tokens
                    .saturating_sub(total)
                    .saturating_mul(4);
                if remaining_chars > 0 && entry.chars().count() > remaining_chars {
                    let keep = entry
                        .char_indices()
                        .nth(entry.chars().count() - remaining_chars)
                        .map(|(idx, _)| idx)
                        .unwrap_or(entry.len());
                    split_prefix = entry[..keep].to_string();
                    split_suffix = entry[keep..].to_string();
                }
                split = index + 1;
                break;
            }
            total = next;
            split = index;
        }

        let mut head_parts: Vec<String> =
            serialized[..split].iter().map(|s| s.to_string()).collect();
        if !split_prefix.is_empty() {
            head_parts.push(split_prefix);
        }
        let mut recent_parts: Vec<String> = Vec::new();
        if !split_suffix.is_empty() {
            recent_parts.push(split_suffix);
        }
        recent_parts.extend(serialized[split..].iter().map(|s| s.to_string()));

        let head = head_parts.join("\n\n");
        let recent = recent_parts.join("\n\n");
        if head.trim().is_empty() {
            return None;
        }
        Some((head, recent))
    }
}

/// Build the LLM prompt for a compaction pass. The prompt is
/// deterministic so we can test it.
pub fn build_prompt(previous_summary: Option<&str>, head: &str) -> String {
    let head_block = format!("<conversation-history>\n{head}\n</conversation-history>");
    match previous_summary {
        Some(previous) if !previous.trim().is_empty() => format!(
            "Update the anchored summary below using the conversation history above. \
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{previous}\n</previous-summary>\n\n\
             {SUMMARY_TEMPLATE}\n\n{head_block}"
        ),
        _ => format!(
            "Create a new anchored summary from the conversation history.\n\n\
             {SUMMARY_TEMPLATE}\n\n{head_block}"
        ),
    }
}

/// Serialize one persisted message into the form the summarization
/// LLM should see. Mirrors opencode's `serialize` function but
/// with our `ContentPart` shape.
pub fn serialize_message(message: &ConversationMessage) -> String {
    if message.role == "user" {
        return format!("[User]: {}", message.content);
    }
    if message.role == "assistant" {
        let parts: Vec<ContentPart> = serde_json::from_str(&message.content).unwrap_or_default();
        if parts.is_empty() {
            return format!("[Assistant]: {}", message.content);
        }
        let mut lines: Vec<String> = Vec::new();
        for part in parts {
            match part {
                ContentPart::Text { text } => {
                    if !text.is_empty() {
                        lines.push(format!("[Assistant]: {text}"));
                    }
                }
                ContentPart::Reasoning { text } => {
                    if !text.is_empty() {
                        lines.push(format!("[Assistant reasoning]: {text}"));
                    }
                }
                ContentPart::ToolCall { id: _, name, input } => {
                    let input = serde_json::to_string(&input).unwrap_or_default();
                    lines.push(format!("[Assistant tool call]: {name}({input})"));
                }
                ContentPart::ToolResult {
                    name: _, result, ..
                } => {
                    let result_text = serialize_tool_result(&result);
                    lines.push(format!(
                        "[Tool result]: {}",
                        truncate_chars(&result_text, SUMMARY_TOOL_OUTPUT_MAX_CHARS)
                    ));
                }
            }
        }
        let joined = lines.join("\n");
        if joined.is_empty() {
            return String::new();
        }
        return joined;
    }
    if message.role == "system" {
        format!("[System]: {}", message.content)
    } else {
        message.content.clone()
    }
}

fn serialize_tool_result(result: &serde_json::Value) -> String {
    match result {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| result.to_string()),
        _ => result.to_string(),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for ch in text.chars().take(max) {
        out.push(ch);
    }
    out.push_str("\n[truncated]");
    out
}

async fn stream_summary(provider: &dyn LlmProvider, request: LlmRequest) -> Result<String, String> {
    let stream = provider
        .stream(request)
        .await
        .map_err(|error| format!("provider stream: {error}"))?;
    let mut events = stream.events;
    let mut chunks: Vec<String> = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            LlmEvent::TextDelta { text, .. } => chunks.push(text),
            LlmEvent::ProviderError { message } => {
                return Err(format!("provider error: {message}"));
            }
            LlmEvent::Finish { .. } => break,
            _ => {}
        }
    }
    Ok(chunks.join(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::conversation::ConversationFile;

    fn user(id: &str, content: &str) -> ConversationMessage {
        ConversationMessage {
            id: id.to_string(),
            role: "user".to_string(),
            content: content.to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    fn assistant(id: &str, content: &str) -> ConversationMessage {
        ConversationMessage {
            id: id.to_string(),
            role: "assistant".to_string(),
            content: content.to_string(),
            timestamp: "2024-01-01T00:00:01Z".to_string(),
        }
    }

    #[test]
    fn build_prompt_with_no_previous_summary() {
        let prompt = build_prompt(None, "history body");
        assert!(prompt.contains("Create a new anchored summary"));
        assert!(prompt.contains("history body"));
        assert!(prompt.contains(SUMMARY_TEMPLATE));
    }

    #[test]
    fn build_prompt_with_previous_summary_asks_for_update() {
        let prompt = build_prompt(Some("old summary"), "history body");
        assert!(prompt.contains("Update the anchored summary"));
        assert!(prompt.contains("old summary"));
    }

    #[test]
    fn select_keeps_recent_tail_within_budget() {
        let service = CompactionService::new(
            test_conversation_service(),
            Arc::new(ProviderRegistry::new()),
            Arc::new(ModelRegistry::from_embedded_json()),
            CompactionConfig {
                auto: true,
                buffer_tokens: DEFAULT_COMPACTION_BUFFER,
                keep_recent_tokens: 1,
            },
        );
        let conv = ConversationFile {
            session_id: "s1".to_string(),
            messages: vec![
                user("u1", &"a".repeat(1024)),
                assistant("a1", &"b".repeat(1024)),
                user("u2", &"c".repeat(1024)),
                assistant("a2", &"d".repeat(1024)),
            ],
            summary: None,
        };
        let (head, recent) = service
            .select_from_persisted(&conv.messages)
            .expect("selection");
        assert!(!head.is_empty());
        assert!(!recent.is_empty());
        // The most recent message stays verbatim in `recent`.
        assert!(recent.contains("d"));
    }

    #[test]
    fn select_returns_none_for_empty_conversation() {
        let service = CompactionService::new(
            test_conversation_service(),
            Arc::new(ProviderRegistry::new()),
            Arc::new(ModelRegistry::from_embedded_json()),
            CompactionConfig::default(),
        );
        let conv = ConversationFile {
            session_id: "s1".to_string(),
            messages: vec![],
            summary: None,
        };
        assert!(service.select_from_persisted(&conv.messages).is_none());
    }

    #[test]
    fn select_falls_back_to_only_message_when_under_budget() {
        let service = CompactionService::new(
            test_conversation_service(),
            Arc::new(ProviderRegistry::new()),
            Arc::new(ModelRegistry::from_embedded_json()),
            CompactionConfig {
                auto: true,
                buffer_tokens: DEFAULT_COMPACTION_BUFFER,
                keep_recent_tokens: 8_000,
            },
        );
        // Simulate a session where the only persisted message is a
        // recent-context block from a prior compaction pass. The
        // compactor must still produce *some* head so the summary
        // LLM has something to work with.
        let recent_only = ConversationFile {
            session_id: "s1".to_string(),
            messages: vec![ConversationMessage {
                id: "recent-1".to_string(),
                role: "system".to_string(),
                content: "<recent-context>\nlast user turn\n</recent-context>".to_string(),
                timestamp: "2024-01-01T00:00:00Z".to_string(),
            }],
            summary: None,
        };
        let (head, recent) = service
            .select_from_persisted(&recent_only.messages)
            .expect("fallback selection");
        assert!(head.contains("last user turn"));
        assert!(recent.is_empty());
    }

    #[test]
    fn serialize_message_renders_user_and_assistant() {
        let user_msg = user("u1", "hello");
        let assistant_msg = assistant("a1", r#"[{"type":"text","text":"hi there"}]"#);
        assert_eq!(serialize_message(&user_msg), "[User]: hello");
        assert!(serialize_message(&assistant_msg).contains("[Assistant]: hi there"));
    }

    // Test helpers
    fn test_conversation_service() -> Arc<ConversationService> {
        ConversationService::new(std::env::temp_dir().join("arachne-compaction-test"))
    }
}
