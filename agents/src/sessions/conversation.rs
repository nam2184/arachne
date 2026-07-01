use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::snapshot::SessionFileDiff;
use crate::{Message, MessageRole};

pub struct ConversationService {
    base_dir: PathBuf,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConversationFile {
    pub session_id: String,
    pub messages: Vec<ConversationMessage>,
    pub summary: Option<String>,
    /// Structured recent-tail messages preserved verbatim across a
    /// compaction. Mirrors opencode's `tail_start_id` / `recent`:
    /// after compaction, the runner replays these as real
    /// conversation messages between the synthetic summary user
    /// message and any post-tail user input. Default `[]` for
    /// sessions that have never been compacted and for older
    /// conversation files written before this field existed; the
    /// runner falls back to the legacy `<recent-context>` system
    /// block when this is empty but `messages` contains one.
    #[serde(default)]
    pub recent_messages: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConversationMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTurnDiff {
    pub message_id: String,
    pub diff: Vec<SessionFileDiff>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionDiffFile {
    session_id: String,
    turns: Vec<SessionTurnDiff>,
}

impl ConversationService {
    pub fn new(base_dir: PathBuf) -> Arc<Self> {
        if let Some(parent) = base_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::create_dir_all(&base_dir);
        Arc::new(Self {
            base_dir,
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn create_conversation(&self, session_id: &str) -> Result<(), String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        let conv = ConversationFile {
            session_id: session_id.to_string(),
            messages: Vec::new(),
            summary: None,
            recent_messages: Vec::new(),
        };
        self.write_ai_conversation(session_id, &conv)?;
        self.write_ui_conversation(session_id, &conv)
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: String,
    ) -> Result<String, String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        let message =
            ConversationMessage::from(Message::new(session_id.to_string(), role, content));
        let message_id = message.id.clone();

        let mut ai_conv = self.read_ai_conversation(session_id)?;
        ai_conv.messages.push(message.clone());
        self.write_ai_conversation(session_id, &ai_conv)?;

        let mut ui_conv = self.read_ui_conversation(session_id)?;
        ui_conv.messages.push(message);
        self.write_ui_conversation(session_id, &ui_conv)?;

        Ok(message_id)
    }

    pub fn append_ui_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: String,
    ) -> Result<String, String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        let message =
            ConversationMessage::from(Message::new(session_id.to_string(), role, content));
        let message_id = message.id.clone();
        let mut ui_conv = self.read_ui_conversation(session_id)?;
        ui_conv.messages.push(message);
        self.write_ui_conversation(session_id, &ui_conv)?;

        Ok(message_id)
    }

    /// Persist an assistant message inline during streaming. Unlike
    /// `append_message` (which buffers the whole assistant turn in memory
    /// and writes once at the end), this updates the persisted content
    /// of an existing assistant message in place. If no message with
    /// `message_id` exists yet, a new one is created with the supplied
    /// initial content. Subsequent calls REPLACE the content of that
    /// message (callers compose the full ContentPart JSON before each
    /// write).
    ///
    /// This is what makes the runner crash-resilient: tool calls and
    /// tool results are flushed to disk as they arrive, so an
    /// unexpected process exit leaves a coherent partial message in
    /// the file rather than losing the entire turn.
    pub fn upsert_message_content(
        &self,
        session_id: &str,
        message_id: &str,
        role: MessageRole,
        content: &str,
    ) -> Result<(), String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        let mut ai_conv = self.read_ai_conversation(session_id)?;
        if let Some(existing) = ai_conv.messages.iter_mut().find(|m| m.id == message_id) {
            existing.content = content.to_string();
        } else {
            let mut message = ConversationMessage::from(Message::new(
                session_id.to_string(),
                role.clone(),
                content.to_string(),
            ));
            message.id = message_id.to_string();
            ai_conv.messages.push(message);
        }
        self.write_ai_conversation(session_id, &ai_conv)?;

        let mut ui_conv = self.read_ui_conversation(session_id)?;
        if let Some(existing) = ui_conv.messages.iter_mut().find(|m| m.id == message_id) {
            existing.content = content.to_string();
        } else {
            let mut message = ConversationMessage::from(Message::new(
                session_id.to_string(),
                role.clone(),
                content.to_string(),
            ));
            message.id = message_id.to_string();
            ui_conv.messages.push(message);
        }
        self.write_ui_conversation(session_id, &ui_conv)?;

        Ok(())
    }

    pub fn read_ai_conversation(&self, session_id: &str) -> Result<ConversationFile, String> {
        self.read_conversation_or_empty(session_id, self.ai_file_path(session_id), "conversation")
    }

    pub fn read_ui_conversation(&self, session_id: &str) -> Result<ConversationFile, String> {
        let path = self.ui_file_path(session_id);
        if !path.exists() {
            return self.read_ai_conversation(session_id);
        }
        self.read_conversation_or_empty(session_id, path, "UI conversation")
    }

    pub fn get_messages(&self, session_id: &str) -> Result<Vec<ConversationMessage>, String> {
        Ok(self.read_ai_conversation(session_id)?.messages)
    }

    pub fn write_session_diff(
        &self,
        session_id: &str,
        message_id: &str,
        diff: Vec<SessionFileDiff>,
    ) -> Result<(), String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        let mut file = self.read_session_diff_file(session_id)?;
        file.turns.retain(|turn| turn.message_id != message_id);
        file.turns.push(SessionTurnDiff {
            message_id: message_id.to_string(),
            diff,
        });
        self.write_session_diff_file(session_id, &file)
    }

    pub fn get_session_diff(
        &self,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<SessionFileDiff>, String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let file = self.read_session_diff_file(session_id)?;
        if let Some(message_id) = message_id {
            return Ok(file
                .turns
                .into_iter()
                .find(|turn| turn.message_id == message_id)
                .map(|turn| turn.diff)
                .unwrap_or_default());
        }
        Ok(file
            .turns
            .last()
            .map(|turn| turn.diff.clone())
            .unwrap_or_default())
    }

    pub fn compact_conversation(&self, session_id: &str, summary: String) -> Result<(), String> {
        self.compact_conversation_with_recent_messages(session_id, &summary, &[])
    }

    /// Replace the persisted AI conversation with a summary and a
    /// "recent" tail (preserved verbatim) carried as a string. The
    /// string is wrapped in a single `<recent-context>` system
    /// message inside `messages`. Kept for back-compat with callers
    /// that already have a serialized tail. New callers should
    /// prefer `compact_conversation_with_recent_messages` so the
    /// tail is replayed as real conversation messages on the next
    /// turn instead of a JSON blob in a system message.
    pub fn compact_conversation_with_recent(
        &self,
        session_id: &str,
        summary: &str,
        recent: &str,
    ) -> Result<(), String> {
        let recent_messages: Vec<ConversationMessage> = if recent.trim().is_empty() {
            Vec::new()
        } else {
            vec![ConversationMessage {
                id: format!("recent-{}", uuid::Uuid::new_v4()),
                role: "system".to_string(),
                content: format!("<recent-context>\n{recent}\n</recent-context>"),
                timestamp: chrono::Utc::now().to_rfc3339(),
            }]
        };
        self.compact_conversation_with_recent_messages(session_id, summary, &recent_messages)
    }

    /// Replace the persisted AI conversation with a summary and a
    /// structured list of recent-tail messages. Mirrors opencode's
    /// `tail_start_id` / `recent` selection: after compaction, the
    /// runner replays the `recent_messages` between a synthetic
    /// summary user-message and any post-tail user input, so the
    /// model sees the recent turn verbatim rather than as a JSON
    /// blob in a system message. The UI conversation file is left
    /// intact (existing transcript messages preserved) and gets a
    /// new system-role entry appended that records the compaction
    /// summary so the chat panel can render the checkpoint.
    /// Tauri callers that drive compaction from the UI can pass
    /// an empty `recent_messages` to wipe the AI context.
    pub fn compact_conversation_with_recent_messages(
        &self,
        session_id: &str,
        summary: &str,
        recent_messages: &[ConversationMessage],
    ) -> Result<(), String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        let mut ai_conv = self.read_ai_conversation(session_id)?;
        ai_conv.summary = Some(summary.to_string());
        ai_conv.recent_messages = recent_messages.to_vec();
        ai_conv.messages.clear();

        // Append a checkpoint entry to the UI conversation so the
        // chat panel can render the new summary. Existing messages
        // are preserved so the user still sees the full transcript.
        let mut ui_conv = self.read_ui_conversation(session_id)?;
        let checkpoint = ConversationMessage {
            id: format!("compaction-{}", uuid::Uuid::new_v4()),
            role: "system".to_string(),
            content: format!("<conversation-checkpoint>\n{summary}\n</conversation-checkpoint>"),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        ui_conv.messages.push(checkpoint);

        self.write_ai_conversation(session_id, &ai_conv)?;
        self.write_ui_conversation(session_id, &ui_conv)
    }

    pub fn delete_conversation(&self, session_id: &str) -> Result<(), String> {
        let session_lock = self.get_lock(session_id);
        let _session_guard = session_lock.lock();
        let lock_path = self.lock_file_path(session_id);
        let _file_guard = self.acquire_lock(&lock_path)?;

        for path in [
            self.ai_file_path(session_id),
            self.ui_file_path(session_id),
            self.diff_file_path(session_id),
        ] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("Failed to delete conversation file: {e}"))?;
            }
        }
        Ok(())
    }

    pub fn get_conversation_path(&self, session_id: &str) -> PathBuf {
        self.ai_file_path(session_id)
    }

    fn get_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock();
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn ai_file_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.json"))
    }

    fn ui_file_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.ui.json"))
    }

    fn diff_file_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.diffs.json"))
    }

    fn lock_file_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.lock"))
    }

    fn read_conversation_or_empty(
        &self,
        session_id: &str,
        path: PathBuf,
        label: &str,
    ) -> Result<ConversationFile, String> {
        if !path.exists() {
            return Ok(ConversationFile {
                session_id: session_id.to_string(),
                messages: Vec::new(),
                summary: None,
                recent_messages: Vec::new(),
            });
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {label} file: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse {label} file: {e}"))
    }

    fn read_session_diff_file(&self, session_id: &str) -> Result<SessionDiffFile, String> {
        let path = self.diff_file_path(session_id);
        if !path.exists() {
            return Ok(SessionDiffFile {
                session_id: session_id.to_string(),
                turns: Vec::new(),
            });
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read session diff file: {e}"))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse session diff file: {e}"))
    }

    fn write_session_diff_file(
        &self,
        session_id: &str,
        diff: &SessionDiffFile,
    ) -> Result<(), String> {
        let path = self.diff_file_path(session_id);
        let content = serde_json::to_string_pretty(diff)
            .map_err(|e| format!("Failed to serialize session diff file: {e}"))?;
        std::fs::write(&path, content)
            .map_err(|e| format!("Failed to write session diff file: {e}"))
    }

    fn write_ai_conversation(
        &self,
        session_id: &str,
        conv: &ConversationFile,
    ) -> Result<(), String> {
        self.write_conversation(self.ai_file_path(session_id), conv, "AI conversation")
    }

    fn write_ui_conversation(
        &self,
        session_id: &str,
        conv: &ConversationFile,
    ) -> Result<(), String> {
        self.write_conversation(self.ui_file_path(session_id), conv, "UI conversation")
    }

    fn write_conversation(
        &self,
        path: PathBuf,
        conv: &ConversationFile,
        label: &str,
    ) -> Result<(), String> {
        let content = serde_json::to_string_pretty(conv)
            .map_err(|e| format!("Failed to serialize {label}: {e}"))?;
        std::fs::write(&path, content).map_err(|e| format!("Failed to write {label} file: {e}"))
    }

    fn acquire_lock(&self, lock_path: &Path) -> Result<LockGuard, String> {
        LockGuard::acquire(lock_path)
    }
}

impl From<Message> for ConversationMessage {
    fn from(message: Message) -> Self {
        Self {
            id: message.id,
            role: match message.role {
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "assistant".to_string(),
                MessageRole::System => "system".to_string(),
            },
            content: message.content,
            timestamp: message.timestamp.to_rfc3339(),
        }
    }
}

struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(path: &Path) -> Result<Self, String> {
        let mut attempts = 0;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(file) => {
                    let _ = file;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempts += 1;
                    if attempts > 100 {
                        return Err("Failed to acquire lock after 100 attempts".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => return Err(format!("Failed to create lock file: {error}")),
            }
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn create_conversation_service(base_dir: PathBuf) -> Arc<ConversationService> {
    ConversationService::new(base_dir)
}
