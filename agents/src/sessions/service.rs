use std::path::PathBuf;
use std::sync::Arc;

use crate::database::{Database, ProjectRepository, SessionGroupRepository, SessionRepository};
use crate::sandbox::path::strip_verbatim_prefix;
use crate::{AgentSession, SessionGroup};

#[derive(Debug, Clone)]
pub struct CreatedSessionChat {
    pub root_session_id: String,
    pub chat_session_id: String,
    pub created_root_session_id: Option<String>,
}

/// Canonicalize session directories at the DB boundary so UI display,
/// prompts, tool cwd, and sandbox roots use one path identity.
pub(crate) fn canonicalize_directory(input: &str) -> PathBuf {
    let raw = PathBuf::from(input);
    let canonical = raw.canonicalize().unwrap_or_else(|_| raw.clone());
    strip_verbatim_prefix(canonical)
}

pub struct SessionService {
    db_path: PathBuf,
}

impl SessionService {
    pub fn new(db_path: PathBuf) -> Arc<Self> {
        let service = Arc::new(Self { db_path });
        if let Err(error) = service.db() {
            tracing::warn!("Failed to initialize session database: {}", error);
        }
        service
    }

    pub fn create_session(
        &self,
        project_id: String,
        directory: String,
        provider: String,
        model: String,
    ) -> Result<String, String> {
        self.create_session_with_parent(project_id, directory, provider, model, None)
            .map(|(id, _created)| id)
    }

    pub fn create_top_level_session(
        &self,
        project_id: String,
        directory: String,
        provider: String,
        model: String,
    ) -> Result<(String, bool), String> {
        let db = self.db()?;
        if ProjectRepository::find_by_id(&db, &project_id)?.is_none() {
            return Err("Project must exist before creating sessions".to_string());
        }

        let raw_path = PathBuf::from(&directory);
        if !raw_path.exists() || !raw_path.is_dir() {
            return Err(format!("Session directory does not exist: {directory}"));
        }

        let canonical = canonicalize_directory(&directory)
            .to_string_lossy()
            .to_string();
        if let Some(existing) =
            SessionRepository::find_top_level_by_project_directory(&db, &project_id, &canonical)?
        {
            return Ok((existing.id, false));
        }

        self.create_session_with_parent(project_id, directory, provider, model, None)
    }

    /// Same as `create_session` but with an optional `parent_session_id` for
    /// sub-agents. When `parent.is_some()` the directory is allowed to differ
    /// from the caller's but it still must exist.
    pub fn create_session_with_parent(
        &self,
        project_id: String,
        directory: String,
        provider: String,
        model: String,
        parent: Option<String>,
    ) -> Result<(String, bool), String> {
        let db = self.db()?;
        if ProjectRepository::find_by_id(&db, &project_id)?.is_none() {
            return Err("Project must exist before creating sessions".to_string());
        }

        if let Some(parent_id) = parent.as_deref() {
            let parent_session = SessionRepository::find_by_id(&db, parent_id)?
                .ok_or_else(|| format!("parent session not found: {parent_id}"))?;
            if parent_session.parent_session_id.is_some() {
                return Err(
                    "persistent session chats can only be created under a root session".to_string(),
                );
            }
        }

        // Validate the raw input first so the error preserves the user's path.
        let raw_path = PathBuf::from(&directory);
        if !raw_path.exists() || !raw_path.is_dir() {
            return Err(format!("Session directory does not exist: {directory}"));
        }

        let canonical = canonicalize_directory(&directory)
            .to_string_lossy()
            .to_string();

        let mut session = AgentSession::new(project_id, canonical, provider, model);
        session.parent_session_id = parent;
        let id = session.id.clone();
        SessionRepository::insert(&db, &session)?;
        if session.parent_session_id.is_none() {
            if let Err(error) = self.refresh_session_summary(&id) {
                tracing::warn!(session_id = %id, error = %error, "failed to generate routing summary");
            }
        }
        Ok((id, true))
    }

    pub fn root_session_id(&self, session_id: &str) -> Result<String, String> {
        let session = self
            .get_session(session_id)?
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        Ok(session.parent_session_id.unwrap_or(session.id))
    }

    pub fn root_session(&self, session_id: &str) -> Result<AgentSession, String> {
        let root_id = self.root_session_id(session_id)?;
        self.get_session(&root_id)?
            .ok_or_else(|| format!("root session not found: {root_id}"))
    }

    pub fn session_chats(&self, root_session_id: &str) -> Result<Vec<AgentSession>, String> {
        let db = self.db()?;
        let root = SessionRepository::find_by_id(&db, root_session_id)?
            .ok_or_else(|| format!("root session not found: {root_session_id}"))?;
        if root.parent_session_id.is_some() {
            return Err("session_chats requires a root session id".to_string());
        }
        SessionRepository::children_of(&db, root_session_id)
    }

    pub fn create_session_chat(&self, session_id: &str) -> Result<CreatedSessionChat, String> {
        let db = self.db()?;
        let session = SessionRepository::find_by_id(&db, session_id)?
            .ok_or_else(|| format!("session not found: {session_id}"))?;

        let (root, converted_root_id) =
            if let Some(parent_id) = session.parent_session_id.as_deref() {
                let root = SessionRepository::find_by_id(&db, parent_id)?
                    .ok_or_else(|| format!("root session not found: {parent_id}"))?;
                if root.parent_session_id.is_some() {
                    return Err("persistent chats cannot be nested below another chat".to_string());
                }
                (root, None)
            } else {
                let existing_chats = SessionRepository::children_of(&db, &session.id)?;
                if existing_chats.is_empty() {
                    let mut new_root = AgentSession::new(
                        session.project_id.clone(),
                        session.directory.clone(),
                        session.provider.clone(),
                        session.model.clone(),
                    );
                    new_root.summary_json = session.summary_json.clone();
                    let new_root_id = new_root.id.clone();
                    SessionRepository::insert(&db, &new_root)?;
                    SessionGroupRepository::replace_session(&db, &session.id, &new_root_id)?;
                    SessionRepository::update_parent(&db, &session.id, Some(&new_root_id))?;
                    (new_root, Some(new_root_id))
                } else {
                    (session.clone(), None)
                }
            };

        let chat = AgentSession::child_of(
            &root,
            root.directory.clone(),
            root.provider.clone(),
            root.model.clone(),
        );
        let chat_id = chat.id.clone();
        SessionRepository::insert(&db, &chat)?;

        Ok(CreatedSessionChat {
            root_session_id: root.id,
            chat_session_id: chat_id,
            created_root_session_id: converted_root_id,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<AgentSession>, String> {
        SessionRepository::find_by_id(&self.db()?, id)
    }

    pub fn get_all_sessions(&self) -> Result<Vec<AgentSession>, String> {
        SessionRepository::list(&self.db()?)
    }

    pub fn sessions_in_group(&self, group_id: &str) -> Result<Vec<AgentSession>, String> {
        let groups = self.get_all_groups()?;
        let Some(group) = groups.into_iter().find(|group| group.id == group_id) else {
            return Ok(Vec::new());
        };
        let mut sessions = Vec::new();
        for session_id in group.session_ids {
            let root_id = self.root_session_id(&session_id).unwrap_or(session_id);
            if let Some(session) = self.get_session(&root_id)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        SessionRepository::delete(&self.db()?, id)
    }

    pub fn create_group(&self, session_ids: Vec<String>) -> Result<String, String> {
        let db = self.db()?;
        let root_ids = self.normalize_session_ids_to_roots(session_ids)?;
        let group = SessionGroup::new(root_ids);
        let id = group.id.clone();
        SessionGroupRepository::insert(&db, &group)?;
        Ok(id)
    }

    pub fn get_all_groups(&self) -> Result<Vec<SessionGroup>, String> {
        SessionGroupRepository::list(&self.db()?)
    }

    pub fn delete_group(&self, id: &str) -> Result<(), String> {
        SessionGroupRepository::delete(&self.db()?, id)
    }

    pub fn rename_group(&self, id: &str, name: Option<String>) -> Result<(), String> {
        SessionGroupRepository::rename(&self.db()?, id, name)
    }

    pub fn add_session_to_group(&self, session_id: &str, group_id: &str) -> Result<(), String> {
        let root_id = self.root_session_id(session_id)?;
        SessionGroupRepository::add_session(&self.db()?, group_id, &root_id)
    }

    pub fn remove_session_from_group(&self, session_id: &str) -> Result<(), String> {
        let root_id = self.root_session_id(session_id)?;
        SessionGroupRepository::remove_session(&self.db()?, &root_id)
    }

    pub fn update_session_provider(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), String> {
        SessionRepository::update_provider(&self.db()?, session_id, provider, model)
    }

    pub fn update_session_summary(
        &self,
        session_id: &str,
        summary_json: Option<&str>,
    ) -> Result<(), String> {
        SessionRepository::update_summary(&self.db()?, session_id, summary_json)
    }

    pub fn update_session_title(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<(), String> {
        if self.get_session(session_id)?.is_none() {
            return Err(format!("session not found: {session_id}"));
        }
        SessionRepository::update_title(&self.db()?, session_id, title)
    }

    pub fn refresh_session_summary(&self, session_id: &str) -> Result<(), String> {
        let session = self
            .get_session(session_id)?
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let summary = crate::routing::DiscoveryDispatcher::default()
            .discover(std::path::Path::new(&session.directory));
        let json = serde_json::to_string(&summary).map_err(|e| e.to_string())?;
        self.update_session_summary(session_id, Some(&json))
    }

    fn db(&self) -> Result<Database, String> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let db = Database::new(self.db_path.clone()).map_err(|e| e.to_string())?;
        db.init()?;
        Ok(db)
    }

    fn normalize_session_ids_to_roots(
        &self,
        session_ids: Vec<String>,
    ) -> Result<Vec<String>, String> {
        let mut roots = Vec::new();
        for session_id in session_ids {
            let root_id = self.root_session_id(&session_id)?;
            if !roots.contains(&root_id) {
                roots.push(root_id);
            }
        }
        Ok(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ProjectRepository;
    use crate::Project;
    use chrono::Utc;
    use tempfile::TempDir;

    /// Builds a session service backed by a temp file, and returns the
    /// session working directory (also a temp dir) used to satisfy the
    /// `directory must exist` validation in `create_session`.
    fn make_service() -> (Arc<SessionService>, TempDir, TempDir) {
        let work_dir = TempDir::new().expect("tempdir for sessions");
        let db_dir = TempDir::new().expect("tempdir for db");
        let db_path = db_dir.path().join("sessions.sqlite");
        let service = SessionService::new(db_path);
        // Trigger eager initialization so init errors surface here.
        let _ = service.db();
        (service, work_dir, db_dir)
    }

    fn seed_project(service: &SessionService, id: &str, name: &str) {
        let db = service.db().expect("db");
        let project = Project {
            id: id.to_string(),
            path: format!("/tmp/{id}"),
            name: name.to_string(),
            tech_stack: vec![],
            created_at: Utc::now(),
        };
        ProjectRepository::insert(&db, &project).expect("insert project");
    }

    fn log(label: &str, message: &str) {
        println!("[session test] {} -> {}", label, message);
    }

    #[test]
    fn create_session_requires_existing_project() {
        let (service, work, _db) = make_service();
        let result = service.create_session(
            "missing-project".to_string(),
            work.path().to_string_lossy().to_string(),
            "anthropic".to_string(),
            "claude-sonnet-4-20250514".to_string(),
        );
        log(
            "create_session_requires_existing_project",
            &format!("{:?}", result),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Project must exist"));
    }

    #[test]
    fn create_session_requires_existing_directory() {
        let (service, _work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let result = service.create_session(
            "p1".to_string(),
            "/path/that/does/not/exist".to_string(),
            "anthropic".to_string(),
            "claude-sonnet-4-20250514".to_string(),
        );
        log(
            "create_session_requires_existing_directory",
            &format!("{:?}", result),
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Session directory does not exist"));
    }

    #[test]
    fn create_session_persists_and_returns_id() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .expect("create_session");

        log(
            "create_session_persists_and_returns_id",
            &format!("id={}", id),
        );
        assert!(!id.is_empty());

        let session = service
            .get_session(&id)
            .expect("get_session")
            .expect("found");
        assert_eq!(session.id, id);
        assert_eq!(session.project_id, "p1");
        assert_eq!(session.provider, "anthropic");
        assert_eq!(session.model, "claude-sonnet-4-20250514");
        assert_eq!(
            session.directory,
            work.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn get_all_sessions_lists_them() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let other = tempfile::tempdir().unwrap();
        let id_a = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let id_b = service
            .create_session(
                "p1".to_string(),
                other.path().to_string_lossy().to_string(),
                "openai".to_string(),
                "gpt-4.1".to_string(),
            )
            .unwrap();

        let sessions = service.get_all_sessions().expect("get_all_sessions");
        log(
            "get_all_sessions_lists_them",
            &format!("{} sessions", sessions.len()),
        );
        assert_eq!(sessions.len(), 2);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&id_a.as_str()));
        assert!(ids.contains(&id_b.as_str()));
    }

    #[test]
    fn create_session_allows_same_project_directory() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let id_a = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let id_b = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "openai".to_string(),
                "gpt-4.1".to_string(),
            )
            .unwrap();

        let sessions = service.get_all_sessions().expect("get_all_sessions");
        assert_ne!(id_a, id_b);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn create_top_level_session_reuses_same_project_directory() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let (id_a, created_a) = service
            .create_top_level_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let (id_b, created_b) = service
            .create_top_level_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "openai".to_string(),
                "gpt-4.1".to_string(),
            )
            .unwrap();

        let sessions = service.get_all_sessions().expect("get_all_sessions");
        assert!(created_a);
        assert!(!created_b);
        assert_eq!(id_a, id_b);
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn create_session_chat_converts_root_and_transfers_group() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let root_id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let group_id = service.create_group(vec![root_id.clone()]).unwrap();

        let created = service.create_session_chat(&root_id).unwrap();

        assert_ne!(created.root_session_id, root_id);
        assert_ne!(created.chat_session_id, root_id);
        let old_root = service.get_session(&root_id).unwrap().unwrap();
        let new_chat = service
            .get_session(&created.chat_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            old_root.parent_session_id.as_deref(),
            Some(created.root_session_id.as_str())
        );
        assert_eq!(
            new_chat.parent_session_id.as_deref(),
            Some(created.root_session_id.as_str())
        );

        let groups = service.get_all_groups().unwrap();
        let group = groups.iter().find(|group| group.id == group_id).unwrap();
        assert_eq!(group.session_ids, vec![created.root_session_id.clone()]);
    }

    #[test]
    fn create_session_chat_from_child_creates_sibling_under_root() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let root_id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let first = service.create_session_chat(&root_id).unwrap();
        let second = service.create_session_chat(&first.chat_session_id).unwrap();

        assert_eq!(second.root_session_id, first.root_session_id);
        let chats = service.session_chats(&first.root_session_id).unwrap();
        assert_eq!(chats.len(), 3);
        assert!(chats
            .iter()
            .all(|chat| chat.parent_session_id.as_deref() == Some(first.root_session_id.as_str())));
    }

    #[test]
    fn delete_session_removes_from_db() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();

        service.delete_session(&id).expect("delete_session");
        let found = service.get_session(&id).expect("get_session");
        log(
            "delete_session_removes_from_db",
            &format!("found={:?}", found),
        );
        assert!(found.is_none());
    }

    #[test]
    fn create_group_persists_with_session_links() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let other = tempfile::tempdir().unwrap();
        let id_a = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let id_b = service
            .create_session(
                "p1".to_string(),
                other.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();

        let group_id = service
            .create_group(vec![id_a.clone(), id_b.clone()])
            .expect("create_group");
        let groups = service.get_all_groups().expect("get_all_groups");
        log(
            "create_group_persists_with_session_links",
            &format!("{} groups", groups.len()),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, group_id);
        assert_eq!(groups[0].session_ids.len(), 2);

        // Sessions now report their group_id.
        let sa = service.get_session(&id_a).unwrap().unwrap();
        let sb = service.get_session(&id_b).unwrap().unwrap();
        assert_eq!(sa.group_id.as_deref(), Some(group_id.as_str()));
        assert_eq!(sb.group_id.as_deref(), Some(group_id.as_str()));
    }

    #[test]
    fn update_session_provider_changes_fields() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        service
            .update_session_provider(&id, "openai", "gpt-4.1")
            .expect("update_session_provider");
        let s = service.get_session(&id).unwrap().unwrap();
        log(
            "update_session_provider_changes_fields",
            &format!("{}/{}", s.provider, s.model),
        );
        assert_eq!(s.provider, "openai");
        assert_eq!(s.model, "gpt-4.1");
    }

    #[test]
    fn rename_group_updates_name() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let group_id = service.create_group(vec![id]).expect("create_group");

        service
            .rename_group(&group_id, Some("Plan A".to_string()))
            .expect("rename_group");
        let groups = service.get_all_groups().expect("get_all_groups");
        log(
            "rename_group_updates_name",
            &format!("{:?}", groups[0].name),
        );
        assert_eq!(groups[0].name.as_deref(), Some("Plan A"));

        service
            .rename_group(&group_id, None)
            .expect("rename_group to none");
        let groups = service.get_all_groups().expect("get_all_groups");
        assert!(groups[0].name.is_none());
    }

    #[test]
    fn add_session_to_group_links_correctly() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let other = tempfile::tempdir().unwrap();
        let id_a = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let id_b = service
            .create_session(
                "p1".to_string(),
                other.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();

        let g1 = service
            .create_group(vec![id_a.clone()])
            .expect("create_group");
        // Add id_b to the same group.
        service
            .add_session_to_group(&id_b, &g1)
            .expect("add_session_to_group");
        let groups = service.get_all_groups().expect("get_all_groups");
        log(
            "add_session_to_group_links_correctly",
            &format!("{:?}", groups[0].session_ids),
        );
        assert_eq!(groups[0].session_ids.len(), 2);
        assert!(groups[0].session_ids.contains(&id_a));
        assert!(groups[0].session_ids.contains(&id_b));
    }

    #[test]
    fn remove_session_from_group_unlinks() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let other = tempfile::tempdir().unwrap();
        let id_a = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();
        let id_b = service
            .create_session(
                "p1".to_string(),
                other.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();

        let g = service
            .create_group(vec![id_a.clone(), id_b.clone()])
            .expect("create_group");
        service
            .remove_session_from_group(&id_a)
            .expect("remove_session_from_group");
        let groups = service.get_all_groups().expect("get_all_groups");
        log(
            "remove_session_from_group_unlinks",
            &format!("{:?}", groups[0].session_ids),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].session_ids, vec![id_b.clone()]);
    }

    #[test]
    fn get_session_returns_none_for_unknown_id() {
        let (service, _work, _db) = make_service();
        let found = service.get_session("does-not-exist").expect("get_session");
        log(
            "get_session_returns_none_for_unknown_id",
            &format!("{:?}", found),
        );
        assert!(found.is_none());
    }

    #[test]
    fn multiple_services_can_share_db() {
        // Demonstrates the multi-connection capability with the temp file.
        let (svc_a, work, db_dir) = make_service();
        seed_project(&svc_a, "p1", "arachne");
        let id = svc_a
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();

        // A second service opened on the same file should see the same session.
        let db_path = db_dir.path().join("sessions.sqlite");
        let svc_b = SessionService::new(db_path);
        let s = svc_b.get_session(&id).unwrap().expect("found via svc_b");
        log(
            "multiple_services_can_share_db",
            &format!("id={} provider={}", s.id, s.provider),
        );
        assert_eq!(s.id, id);
        assert_eq!(s.provider, "anthropic");
    }

    #[test]
    fn directory_must_be_a_directory_not_a_file() {
        // Edge case: path exists but is a file, not a directory.
        let (service, _work, db_dir) = make_service();
        seed_project(&service, "p1", "arachne");

        let file_path = db_dir.path().join("not-a-dir.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let result = service.create_session(
            "p1".to_string(),
            file_path.to_string_lossy().to_string(),
            "anthropic".to_string(),
            "claude-sonnet-4-20250514".to_string(),
        );
        log(
            "directory_must_be_a_directory_not_a_file",
            &format!("{:?}", result),
        );
        assert!(result.is_err());
    }

    #[test]
    fn create_session_with_parent_persists_link() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let parent_id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .unwrap();

        // A different directory for the child (e.g. a subagent worktree).
        let peer_dir = tempfile::tempdir().unwrap();
        let (child_id, child_created) = service
            .create_session_with_parent(
                "p1".to_string(),
                peer_dir.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
                Some(parent_id.clone()),
            )
            .expect("create child");
        assert!(child_created);

        let child = service.get_session(&child_id).unwrap().unwrap();
        log(
            "create_session_with_parent_persists_link",
            &format!("child={} parent={:?}", child.id, child.parent_session_id),
        );
        assert_eq!(child.parent_session_id.as_deref(), Some(parent_id.as_str()));
        assert_eq!(
            child.directory,
            peer_dir
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn create_session_with_none_parent_keeps_null() {
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");
        let (id, created) = service
            .create_session_with_parent(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
                None,
            )
            .unwrap();
        assert!(created);

        let s = service.get_session(&id).unwrap().unwrap();
        assert!(s.parent_session_id.is_none());
    }

    #[test]
    fn create_session_canonicalizes_directory() {
        // The dialog can return a path with mixed casing or a path
        // that traverses a symlink. The persisted value must be the
        // canonical form so the sandbox, the system prompt, the UI
        // display, and `ToolContext.project_root` all see the same
        // string.
        let (service, work, _db) = make_service();
        seed_project(&service, "p1", "arachne");

        let id = service
            .create_session(
                "p1".to_string(),
                work.path().to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .expect("create_session");

        let session = service.get_session(&id).unwrap().unwrap();
        let expected = work.path().canonicalize().unwrap();
        let stored = PathBuf::from(&session.directory);

        // Byte-level equality on the canonical form. On POSIX,
        // `canonicalize` returns the same path the tempdir gave us.
        // On Windows it strips the `\\?\` verbatim prefix (handled by
        // canonicalize_directory's `strip_verbatim_prefix` call).
        assert_eq!(stored, expected);
    }

    #[test]
    #[cfg(unix)]
    fn create_session_canonicalizes_through_symlink() {
        // Regression: if the user's chosen directory is reached via a
        // symlink, the persisted value must follow the symlink so the
        // sandbox's project_root matches what `realpath` would return
        // when the sandbox canonicalizes the LLM's tool-call input.
        let (service, work_root, _db) = make_service();
        seed_project(&service, "p1", "arachne");

        let real = work_root.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let link = work_root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let id = service
            .create_session(
                "p1".to_string(),
                link.to_string_lossy().to_string(),
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string(),
            )
            .expect("create_session");

        let session = service.get_session(&id).unwrap().unwrap();
        let stored = PathBuf::from(&session.directory);

        // On POSIX, canonicalize follows the symlink. Windows symlink
        // tests are gated out — the production code path still uses
        // `canonicalize`, which Windows supports natively.
        assert_eq!(stored, real.canonicalize().unwrap());
        assert_ne!(
            stored,
            PathBuf::from(link.to_string_lossy().to_string()),
            "symlink must not be preserved in the persisted directory"
        );
    }
}
