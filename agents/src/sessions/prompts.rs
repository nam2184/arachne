//! Named prompt presets. **Planned future API**.
//!
//! Each constant is a stable, hand-curated prompt the runner
//! can offer as a starting point for new sessions (or surface
//! in a UI dropdown). Today the runner uses inline prompts in
//! `system_prompt_for_session` and `sessions/compaction.rs`; the
//! preset library will replace those once the v2 preset loader
//! is in. The strings here are intentionally a bit terser than
//! the inline ones so the UI can display them as previews.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PromptPreset {
    pub id: String,
    pub title: String,
    pub content: String,
}

pub const SYSTEM_PROMPT: &str = "You are Arachne, a pragmatic coding agent. Prefer small correct changes, inspect the codebase before editing, and keep durable session history separate from UI rendering.";

pub const TITLE_PROMPT: &str = "Generate a short title for this session from the first meaningful user request. Return only the title.";

pub const COMPACTION_PROMPT: &str = "Summarize the prior conversation for continuation. Preserve decisions, constraints, file paths, tool results, unresolved tasks, and user preferences. Omit filler.";

pub const REVIEW_PROMPT: &str = "Review the relevant changes for bugs, regressions, missing tests, and risky assumptions. Report findings first, ordered by severity.";

pub fn presets() -> Vec<PromptPreset> {
    vec![
        PromptPreset {
            id: "system".to_string(),
            title: "Default system prompt".to_string(),
            content: SYSTEM_PROMPT.to_string(),
        },
        PromptPreset {
            id: "title".to_string(),
            title: "Session title prompt".to_string(),
            content: TITLE_PROMPT.to_string(),
        },
        PromptPreset {
            id: "compact".to_string(),
            title: "Session compaction prompt".to_string(),
            content: COMPACTION_PROMPT.to_string(),
        },
        PromptPreset {
            id: "review".to_string(),
            title: "Code review prompt".to_string(),
            content: REVIEW_PROMPT.to_string(),
        },
    ]
}
