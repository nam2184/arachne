use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::database::connection::Database;
use crate::{
    AgentSession, Message, MessageRole, Project, ProviderConfig, ProviderProtocol, SessionGroup,
};

pub struct ProjectRepository;

impl ProjectRepository {
    pub fn insert(db: &Database, project: &Project) -> Result<(), String> {
        let tech_stack_json =
            serde_json::to_string(&project.tech_stack).map_err(|e| e.to_string())?;

        db.connection()
            .execute(
                "INSERT OR IGNORE INTO projects (id, path, name, tech_stack, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project.id,
                    project.path,
                    project.name,
                    tech_stack_json,
                    project.created_at.to_rfc3339()
                ],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn find_by_id(db: &Database, id: &str) -> Result<Option<Project>, String> {
        let mut stmt = db
            .connection()
            .prepare("SELECT id, path, name, tech_stack, created_at FROM projects WHERE id = ?1")
            .map_err(|e| e.to_string())?;

        Ok(stmt
            .query_row(params![id], |row| {
                let tech_stack_json: String = row.get(3)?;
                let created_at: String = row.get(4)?;

                Ok(Project {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    tech_stack: serde_json::from_str(&tech_stack_json).unwrap_or_default(),
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .ok())
    }

    pub fn list(db: &Database) -> Result<Vec<Project>, String> {
        let mut stmt = db
            .connection()
            .prepare("SELECT id, path, name, tech_stack, created_at FROM projects")
            .map_err(|e| e.to_string())?;

        let projects = stmt
            .query_map([], |row| {
                let tech_stack_json: String = row.get(3)?;
                let created_at: String = row.get(4)?;

                Ok(Project {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    tech_stack: serde_json::from_str(&tech_stack_json).unwrap_or_default(),
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(projects)
    }

    pub fn delete(db: &Database, id: &str) -> Result<(), String> {
        db.connection()
            .execute("DELETE FROM projects WHERE id = ?1", params![id])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

pub struct SessionRepository;

impl SessionRepository {
    pub fn insert(db: &Database, session: &AgentSession) -> Result<(), String> {
        db.connection()
            .execute(
                "INSERT INTO agent_sessions (id, project_id, directory, provider, model, summary_json, parent_session_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session.id,
                    session.project_id,
                    session.directory,
                    session.provider,
                    session.model,
                    session.summary_json,
                    session.parent_session_id,
                    session.created_at.to_rfc3339()
                ],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn list(db: &Database) -> Result<Vec<AgentSession>, String> {
        let mut stmt = db
            .connection()
            .prepare(
                "
                SELECT s.id, s.project_id, s.directory, s.provider, s.model, s.created_at,
                       s.parent_session_id, s.summary_json,
                       (SELECT group_id FROM session_group_sessions WHERE session_id = s.id LIMIT 1) AS group_id
                FROM agent_sessions s
                ",
            )
            .map_err(|e| e.to_string())?;

        let sessions = stmt
            .query_map([], |row| session_from_row(row))
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(sessions)
    }

    pub fn find_by_id(db: &Database, id: &str) -> Result<Option<AgentSession>, String> {
        let mut stmt = db
            .connection()
            .prepare(
                "
                SELECT s.id, s.project_id, s.directory, s.provider, s.model, s.created_at,
                       s.parent_session_id, s.summary_json,
                       (SELECT group_id FROM session_group_sessions WHERE session_id = s.id LIMIT 1) AS group_id
                FROM agent_sessions s
                WHERE s.id = ?1
                ",
            )
            .map_err(|e| e.to_string())?;

        Ok(stmt
            .query_row(params![id], |row| session_from_row(row))
            .ok())
    }

    pub fn find_by_project(db: &Database, project_id: &str) -> Result<Vec<AgentSession>, String> {
        let mut stmt = db
            .connection()
            .prepare(
                "
                SELECT s.id, s.project_id, s.directory, s.provider, s.model, s.created_at,
                       s.parent_session_id, s.summary_json,
                       (SELECT group_id FROM session_group_sessions WHERE session_id = s.id LIMIT 1) AS group_id
                FROM agent_sessions s
                WHERE s.project_id = ?1
                ",
            )
            .map_err(|e| e.to_string())?;

        let sessions = stmt
            .query_map(params![project_id], |row| session_from_row(row))
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(sessions)
    }

    /// Walk the `parent_session_id` chain starting at `id` and return each
    /// ancestor's id, nearest-first. Stops at the first row whose
    /// `parent_session_id` is NULL. Bounded to `max_hops` iterations as a
    /// safety net against corrupted chains.
    pub fn ancestors(db: &Database, id: &str, max_hops: usize) -> Result<Vec<String>, String> {
        Self::ancestors_via(db.connection(), id, max_hops)
    }

    /// Like `ancestors` but takes a raw `Connection`. The sub-agent
    /// registry uses this to avoid the cost of opening a new
    /// `Database` wrapper per check.
    pub fn ancestors_via(
        conn: &rusqlite::Connection,
        id: &str,
        max_hops: usize,
    ) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut current = id.to_string();
        for _ in 0..max_hops {
            let next: Option<String> = conn
                .query_row(
                    "SELECT parent_session_id FROM agent_sessions WHERE id = ?1",
                    params![&current],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            match next {
                Some(parent) if !parent.is_empty() => {
                    out.push(parent.clone());
                    current = parent;
                }
                _ => return Ok(out),
            }
        }
        Ok(out)
    }

    /// Direct children of a session. Used by the canvas to render sub-agents
    /// under their parent.
    pub fn children_of(db: &Database, parent_id: &str) -> Result<Vec<AgentSession>, String> {
        let mut stmt = db
            .connection()
            .prepare(
                "
                SELECT s.id, s.project_id, s.directory, s.provider, s.model, s.created_at,
                       s.parent_session_id, s.summary_json,
                       (SELECT group_id FROM session_group_sessions WHERE session_id = s.id LIMIT 1) AS group_id
                FROM agent_sessions s
                WHERE s.parent_session_id = ?1
                ORDER BY s.created_at ASC
                ",
            )
            .map_err(|e| e.to_string())?;
        let sessions = stmt
            .query_map(params![parent_id], |row| session_from_row(row))
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();
        Ok(sessions)
    }

    pub fn update_provider(
        db: &Database,
        id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), String> {
        db.connection()
            .execute(
                "UPDATE agent_sessions SET provider = ?1, model = ?2 WHERE id = ?3",
                params![provider, model, id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn update_summary(
        db: &Database,
        id: &str,
        summary_json: Option<&str>,
    ) -> Result<(), String> {
        db.connection()
            .execute(
                "UPDATE agent_sessions SET summary_json = ?1 WHERE id = ?2",
                params![summary_json, id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn delete(db: &Database, id: &str) -> Result<(), String> {
        db.connection()
            .execute(
                "DELETE FROM session_group_sessions WHERE session_id = ?1",
                params![id],
            )
            .map_err(|e| e.to_string())?;

        db.connection()
            .execute("DELETE FROM agent_sessions WHERE id = ?1", params![id])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

pub struct SessionGroupRepository;

impl SessionGroupRepository {
    pub fn insert(db: &Database, group: &SessionGroup) -> Result<(), String> {
        db.connection()
            .execute(
                "INSERT INTO session_groups (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![group.id, group.name, group.created_at.to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;

        for session_id in &group.session_ids {
            Self::add_session(db, &group.id, session_id)?;
        }

        Ok(())
    }

    pub fn list(db: &Database) -> Result<Vec<SessionGroup>, String> {
        let mut stmt = db
            .connection()
            .prepare("SELECT id, name, created_at FROM session_groups")
            .map_err(|e| e.to_string())?;

        let groups = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let created_at: String = row.get(2)?;
                Ok(SessionGroup {
                    session_ids: Self::session_ids(db, &id).unwrap_or_default(),
                    name: row.get(1)?,
                    id,
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(groups)
    }

    pub fn rename(db: &Database, id: &str, name: Option<String>) -> Result<(), String> {
        db.connection()
            .execute(
                "UPDATE session_groups SET name = ?1 WHERE id = ?2",
                params![name, id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn add_session(db: &Database, group_id: &str, session_id: &str) -> Result<(), String> {
        db.connection()
            .execute(
                "DELETE FROM session_group_sessions WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| e.to_string())?;

        db.connection()
            .execute(
                "INSERT OR IGNORE INTO session_group_sessions (group_id, session_id) VALUES (?1, ?2)",
                params![group_id, session_id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn remove_session(db: &Database, session_id: &str) -> Result<(), String> {
        db.connection()
            .execute(
                "DELETE FROM session_group_sessions WHERE session_id = ?1",
                params![session_id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn delete(db: &Database, id: &str) -> Result<(), String> {
        db.connection()
            .execute(
                "DELETE FROM session_group_sessions WHERE group_id = ?1",
                params![id],
            )
            .map_err(|e| e.to_string())?;

        db.connection()
            .execute("DELETE FROM session_groups WHERE id = ?1", params![id])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn session_ids(db: &Database, group_id: &str) -> Result<Vec<String>, String> {
        let mut stmt = db
            .connection()
            .prepare("SELECT session_id FROM session_group_sessions WHERE group_id = ?1")
            .map_err(|e| e.to_string())?;

        let session_ids = stmt
            .query_map(params![group_id], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(session_ids)
    }
}

pub struct MessageRepository;

impl MessageRepository {
    pub fn insert(db: &Database, message: &Message) -> Result<(), String> {
        db.connection()
            .execute(
                "INSERT INTO messages (id, session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    message.id,
                    message.session_id,
                    role_to_str(&message.role),
                    message.content,
                    message.timestamp.to_rfc3339()
                ],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn find_by_session(db: &Database, session_id: &str) -> Result<Vec<Message>, String> {
        let mut stmt = db
            .connection()
            .prepare("SELECT id, session_id, role, content, timestamp FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC")
            .map_err(|e| e.to_string())?;

        let messages = stmt
            .query_map(params![session_id], |row| {
                let role: String = row.get(2)?;
                let timestamp: String = row.get(4)?;

                Ok(Message {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: role_from_str(&role),
                    content: row.get(3)?,
                    timestamp: DateTime::parse_from_rfc3339(&timestamp)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(messages)
    }

    pub fn delete_by_session(db: &Database, session_id: &str) -> Result<(), String> {
        db.connection()
            .execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![session_id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentSession> {
    let created_at: String = row.get(5)?;
    let parent_session_id: Option<String> = row.get(6)?;
    Ok(AgentSession {
        id: row.get(0)?,
        project_id: row.get(1)?,
        directory: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        summary_json: row.get(7)?,
        group_id: row.get(8)?,
        parent_session_id: if parent_session_id.as_deref() == Some("") {
            None
        } else {
            parent_session_id
        },
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

fn role_to_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
    }
}

fn role_from_str(role: &str) -> MessageRole {
    match role {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => MessageRole::System,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::connection::test_support::test_db;
    use crate::{Project, ProviderConfig, ProviderProtocol, SessionGroup};
    use chrono::TimeZone;
    use rusqlite::Connection;

    fn ts(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
            .unwrap()
    }

    fn sample_project(id: &str, name: &str) -> Project {
        Project {
            id: id.to_string(),
            path: format!("/tmp/{id}"),
            name: name.to_string(),
            tech_stack: vec!["rust".to_string()],
            created_at: ts(2026, 1, 1, 12, 0, 0),
        }
    }

    fn sample_session(id: &str, project_id: &str) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            project_id: project_id.to_string(),
            directory: "/tmp/work".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            group_id: None,
            summary_json: None,
            parent_session_id: None,
            created_at: ts(2026, 1, 2, 12, 0, 0),
        }
    }

    fn sample_provider_config(name: &str, protocol: ProviderProtocol) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_key: Some("sk-test".to_string()),
            base_url: None,
            protocol,
            enabled: true,
        }
    }

    // ---------------------------------------------------------------------
    // ProjectRepository
    // ---------------------------------------------------------------------

    #[test]
    fn project_insert_then_find_by_id() {
        let (db, _guard) = test_db();
        let project = sample_project("p1", "arachne");
        ProjectRepository::insert(&db, &project).unwrap();
        let found = ProjectRepository::find_by_id(&db, "p1").unwrap().unwrap();
        assert_eq!(found.id, "p1");
        assert_eq!(found.name, "arachne");
        assert_eq!(found.tech_stack, vec!["rust".to_string()]);
    }

    #[test]
    fn project_list_returns_all_inserted() {
        let (db, _guard) = test_db();
        ProjectRepository::insert(&db, &sample_project("a", "alpha")).unwrap();
        ProjectRepository::insert(&db, &sample_project("b", "beta")).unwrap();
        let projects = ProjectRepository::list(&db).unwrap();
        assert_eq!(projects.len(), 2);
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn project_delete_removes_record() {
        let (db, _guard) = test_db();
        ProjectRepository::insert(&db, &sample_project("p1", "arachne")).unwrap();
        ProjectRepository::delete(&db, "p1").unwrap();
        let found = ProjectRepository::find_by_id(&db, "p1").unwrap();
        assert!(found.is_none());
        assert!(ProjectRepository::list(&db).unwrap().is_empty());
    }

    #[test]
    fn project_find_by_id_returns_none_for_missing() {
        let (db, _guard) = test_db();
        let found = ProjectRepository::find_by_id(&db, "does-not-exist").unwrap();
        assert!(found.is_none());
    }

    // ---------------------------------------------------------------------
    // SessionRepository
    // ---------------------------------------------------------------------

    fn seed_project(db: &Database) {
        ProjectRepository::insert(db, &sample_project("p1", "arachne")).unwrap();
    }

    #[test]
    fn session_insert_then_find_by_id() {
        let (db, _guard) = test_db();
        seed_project(&db);
        let session = sample_session("s1", "p1");
        SessionRepository::insert(&db, &session).unwrap();
        let found = SessionRepository::find_by_id(&db, "s1").unwrap().unwrap();
        assert_eq!(found.id, "s1");
        assert_eq!(found.project_id, "p1");
        assert_eq!(found.provider, "anthropic");
        assert_eq!(found.model, "claude-sonnet-4-20250514");
        assert!(found.group_id.is_none());
    }

    #[test]
    fn session_list_returns_all_sessions() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        SessionRepository::insert(&db, &sample_session("s2", "p1")).unwrap();
        let list = SessionRepository::list(&db).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn session_find_by_project_filters_correctly() {
        let (db, _guard) = test_db();
        ProjectRepository::insert(&db, &sample_project("p1", "alpha")).unwrap();
        ProjectRepository::insert(&db, &sample_project("p2", "beta")).unwrap();
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        SessionRepository::insert(&db, &sample_session("s2", "p2")).unwrap();
        SessionRepository::insert(&db, &sample_session("s3", "p1")).unwrap();

        let p1 = SessionRepository::find_by_project(&db, "p1").unwrap();
        let p2 = SessionRepository::find_by_project(&db, "p2").unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].id, "s2");
    }

    #[test]
    fn session_update_provider_changes_fields() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        SessionRepository::update_provider(&db, "s1", "openai", "gpt-4.1").unwrap();
        let found = SessionRepository::find_by_id(&db, "s1").unwrap().unwrap();
        assert_eq!(found.provider, "openai");
        assert_eq!(found.model, "gpt-4.1");
    }

    #[test]
    fn session_delete_removes_record() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        SessionRepository::delete(&db, "s1").unwrap();
        let found = SessionRepository::find_by_id(&db, "s1").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn session_list_populates_group_id() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        // Add to group, then list and check group_id is populated.
        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec!["s1".to_string()],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        let list = SessionRepository::list(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].group_id.as_deref(), Some("g1"));
    }

    // ---------------------------------------------------------------------
    // SessionGroupRepository
    // ---------------------------------------------------------------------

    #[test]
    fn session_group_insert_with_sessions_creates_links() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        SessionRepository::insert(&db, &sample_session("s2", "p1")).unwrap();

        let group = SessionGroup {
            id: "g1".to_string(),
            name: Some("Batch 1".to_string()),
            session_ids: vec!["s1".to_string(), "s2".to_string()],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();

        let listed = SessionGroupRepository::list(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "g1");
        assert_eq!(listed[0].name.as_deref(), Some("Batch 1"));
        assert_eq!(listed[0].session_ids.len(), 2);
        assert!(listed[0].session_ids.contains(&"s1".to_string()));
        assert!(listed[0].session_ids.contains(&"s2".to_string()));
    }

    #[test]
    fn session_group_list_with_no_sessions_returns_empty_ids() {
        let (db, _guard) = test_db();
        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec![],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        let listed = SessionGroupRepository::list(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].session_ids.is_empty());
    }

    #[test]
    fn session_group_rename_updates_name() {
        let (db, _guard) = test_db();
        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec![],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        SessionGroupRepository::rename(&db, "g1", Some("Renamed".to_string())).unwrap();
        let listed = SessionGroupRepository::list(&db).unwrap();
        assert_eq!(listed[0].name.as_deref(), Some("Renamed"));

        SessionGroupRepository::rename(&db, "g1", None).unwrap();
        let listed = SessionGroupRepository::list(&db).unwrap();
        assert!(listed[0].name.is_none());
    }

    #[test]
    fn session_group_add_session_inserts_link() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec![],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        SessionGroupRepository::add_session(&db, "g1", "s1").unwrap();

        let listed = SessionGroupRepository::list(&db).unwrap();
        assert!(listed[0].session_ids.contains(&"s1".to_string()));
    }

    #[test]
    fn session_group_add_session_moves_session_between_groups() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        let g1 = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec!["s1".to_string()],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        let g2 = SessionGroup {
            id: "g2".to_string(),
            name: None,
            session_ids: vec![],
            created_at: ts(2026, 1, 3, 0, 1, 0),
        };
        SessionGroupRepository::insert(&db, &g1).unwrap();
        SessionGroupRepository::insert(&db, &g2).unwrap();

        // Moving s1 from g1 to g2 should leave g1 empty and g2 with s1.
        SessionGroupRepository::add_session(&db, "g2", "s1").unwrap();
        let listed = SessionGroupRepository::list(&db).unwrap();
        let g1 = listed.iter().find(|g| g.id == "g1").unwrap();
        let g2 = listed.iter().find(|g| g.id == "g2").unwrap();
        assert!(g1.session_ids.is_empty());
        assert_eq!(g2.session_ids, vec!["s1".to_string()]);
    }

    #[test]
    fn session_group_remove_session_drops_link() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec!["s1".to_string()],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        SessionGroupRepository::remove_session(&db, "s1").unwrap();
        let listed = SessionGroupRepository::list(&db).unwrap();
        assert!(listed[0].session_ids.is_empty());
    }

    #[test]
    fn session_group_delete_cascades_to_links() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec!["s1".to_string()],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        SessionGroupRepository::delete(&db, "g1").unwrap();

        // Group is gone.
        assert!(SessionGroupRepository::list(&db).unwrap().is_empty());
        // Session still exists.
        assert!(SessionRepository::find_by_id(&db, "s1").unwrap().is_some());
    }

    // ---------------------------------------------------------------------
    // MessageRepository
    // ---------------------------------------------------------------------

    #[test]
    fn message_insert_then_find_by_session() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        let msg = Message::new("s1".to_string(), MessageRole::User, "Hello".to_string());
        MessageRepository::insert(&db, &msg).unwrap();

        let found = MessageRepository::find_by_session(&db, "s1").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, msg.id);
        assert_eq!(found[0].content, "Hello");
        assert_eq!(found[0].role, MessageRole::User);
    }

    #[test]
    fn message_find_by_session_orders_by_timestamp_asc() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        // Insert out of order on purpose.
        let mut third = Message::new(
            "s1".to_string(),
            MessageRole::Assistant,
            "third".to_string(),
        );
        third.timestamp = ts(2026, 1, 1, 13, 0, 0);
        let mut first = Message::new("s1".to_string(), MessageRole::User, "first".to_string());
        first.timestamp = ts(2026, 1, 1, 11, 0, 0);
        let mut second = Message::new("s1".to_string(), MessageRole::User, "second".to_string());
        second.timestamp = ts(2026, 1, 1, 12, 0, 0);

        MessageRepository::insert(&db, &third).unwrap();
        MessageRepository::insert(&db, &first).unwrap();
        MessageRepository::insert(&db, &second).unwrap();

        let found = MessageRepository::find_by_session(&db, "s1").unwrap();
        let contents: Vec<&str> = found.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["first", "second", "third"]);
    }

    #[test]
    fn message_find_by_session_returns_empty_for_unknown_session() {
        let (db, _guard) = test_db();
        let found = MessageRepository::find_by_session(&db, "nope").unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn message_delete_by_session_removes_all() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();
        let mut m1 = Message::new("s1".to_string(), MessageRole::User, "a".to_string());
        m1.timestamp = ts(2026, 1, 1, 11, 0, 0);
        let mut m2 = Message::new("s1".to_string(), MessageRole::Assistant, "b".to_string());
        m2.timestamp = ts(2026, 1, 1, 12, 0, 0);
        MessageRepository::insert(&db, &m1).unwrap();
        MessageRepository::insert(&db, &m2).unwrap();
        MessageRepository::delete_by_session(&db, "s1").unwrap();
        assert!(MessageRepository::find_by_session(&db, "s1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn message_role_round_trip_user_assistant_system() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        for (i, role) in [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::System,
        ]
        .into_iter()
        .enumerate()
        {
            let mut m = Message::new("s1".to_string(), role.clone(), format!("msg-{i}"));
            m.timestamp = ts(2026, 1, 1, 10, i as u32, 0);
            MessageRepository::insert(&db, &m).unwrap();
        }

        let found = MessageRepository::find_by_session(&db, "s1").unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].role, MessageRole::User);
        assert_eq!(found[1].role, MessageRole::Assistant);
        assert_eq!(found[2].role, MessageRole::System);
    }

    // ---------------------------------------------------------------------
    // ProviderConfigRepository
    // ---------------------------------------------------------------------

    #[test]
    fn provider_config_upsert_inserts_new() {
        let (db, _guard) = test_db();
        let cfg = sample_provider_config("anthropic", ProviderProtocol::Anthropic);
        ProviderConfigRepository::upsert(&db, &cfg).unwrap();

        let found = ProviderConfigRepository::find_by_name(&db, "anthropic")
            .unwrap()
            .unwrap();
        assert_eq!(found.name, "anthropic");
        assert_eq!(found.protocol, ProviderProtocol::Anthropic);
        assert!(found.enabled);
        assert_eq!(found.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn provider_config_upsert_replaces_existing() {
        let (db, _guard) = test_db();
        let mut cfg = sample_provider_config("anthropic", ProviderProtocol::Anthropic);
        ProviderConfigRepository::upsert(&db, &cfg).unwrap();

        cfg.model = "claude-opus-4-20250514".to_string();
        cfg.api_key = Some("sk-rotated".to_string());
        cfg.enabled = false;
        ProviderConfigRepository::upsert(&db, &cfg).unwrap();

        let found = ProviderConfigRepository::find_by_name(&db, "anthropic")
            .unwrap()
            .unwrap();
        assert_eq!(found.model, "claude-opus-4-20250514");
        assert_eq!(found.api_key.as_deref(), Some("sk-rotated"));
        assert!(!found.enabled);
    }

    #[test]
    fn provider_config_find_by_name_returns_none_for_missing() {
        let (db, _guard) = test_db();
        let found = ProviderConfigRepository::find_by_name(&db, "nope").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn provider_config_list_returns_all() {
        let (db, _guard) = test_db();
        ProviderConfigRepository::upsert(
            &db,
            &sample_provider_config("anthropic", ProviderProtocol::Anthropic),
        )
        .unwrap();
        ProviderConfigRepository::upsert(
            &db,
            &sample_provider_config("openai", ProviderProtocol::OpenAI),
        )
        .unwrap();
        ProviderConfigRepository::upsert(
            &db,
            &sample_provider_config("minimax", ProviderProtocol::OpenAI),
        )
        .unwrap();
        let list = ProviderConfigRepository::list(&db).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn provider_config_delete_removes_record() {
        let (db, _guard) = test_db();
        ProviderConfigRepository::upsert(
            &db,
            &sample_provider_config("anthropic", ProviderProtocol::Anthropic),
        )
        .unwrap();
        ProviderConfigRepository::delete(&db, "anthropic").unwrap();
        let found = ProviderConfigRepository::find_by_name(&db, "anthropic").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn provider_config_protocol_round_trip() {
        let (db, _guard) = test_db();
        ProviderConfigRepository::upsert(
            &db,
            &sample_provider_config("anthropic", ProviderProtocol::Anthropic),
        )
        .unwrap();
        ProviderConfigRepository::upsert(
            &db,
            &sample_provider_config("openai", ProviderProtocol::OpenAI),
        )
        .unwrap();

        let anthropic = ProviderConfigRepository::find_by_name(&db, "anthropic")
            .unwrap()
            .unwrap();
        let openai = ProviderConfigRepository::find_by_name(&db, "openai")
            .unwrap()
            .unwrap();
        assert_eq!(anthropic.protocol, ProviderProtocol::Anthropic);
        assert_eq!(openai.protocol, ProviderProtocol::OpenAI);
    }

    // ---------------------------------------------------------------------
    // Multi-connection sanity check (proves temp-file approach works)
    // ---------------------------------------------------------------------

    #[test]
    fn second_connection_sees_inserts_from_first() {
        let (db, guard) = test_db();
        let path = guard.path().join("test.sqlite");
        ProjectRepository::insert(&db, &sample_project("p1", "arachne")).unwrap();

        // Open a fresh connection to the same file and verify it sees the project.
        let conn2 = Connection::open(&path).unwrap();
        let count: i64 = conn2
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // ---------------------------------------------------------------------
    // Cascade behavior (FK enabled)
    // ---------------------------------------------------------------------

    #[test]
    fn deleting_session_leaves_group_but_drops_link() {
        // Sessions don't have a CASCADE on the link table by design (groups persist
        // even if a session is removed). Verify the current behavior.
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("s1", "p1")).unwrap();

        let group = SessionGroup {
            id: "g1".to_string(),
            name: None,
            session_ids: vec!["s1".to_string()],
            created_at: ts(2026, 1, 3, 0, 0, 0),
        };
        SessionGroupRepository::insert(&db, &group).unwrap();
        SessionRepository::delete(&db, "s1").unwrap();

        // Group still exists with empty session list.
        let groups = SessionGroupRepository::list(&db).unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].session_ids.is_empty());
    }

    #[test]
    fn ancestors_walks_parent_chain() {
        let (db, _guard) = test_db();
        seed_project(&db);
        let mut grandparent = sample_session("gp", "p1");
        grandparent.parent_session_id = None;
        let mut parent = sample_session("p", "p1");
        parent.parent_session_id = Some("gp".to_string());
        let mut child = sample_session("c", "p1");
        child.parent_session_id = Some("p".to_string());

        SessionRepository::insert(&db, &grandparent).unwrap();
        SessionRepository::insert(&db, &parent).unwrap();
        SessionRepository::insert(&db, &child).unwrap();

        let ancestors = SessionRepository::ancestors(&db, "c", 8).unwrap();
        assert_eq!(ancestors, vec!["p".to_string(), "gp".to_string()]);
    }

    #[test]
    fn ancestors_stops_at_max_hops() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("a", "p1")).unwrap();
        SessionRepository::insert(&db, &{
            let mut s = sample_session("b", "p1");
            s.parent_session_id = Some("a".to_string());
            s
        })
        .unwrap();
        SessionRepository::insert(&db, &{
            let mut s = sample_session("c", "p1");
            s.parent_session_id = Some("b".to_string());
            s
        })
        .unwrap();

        // c -> b -> a, but max_hops=1 only walks one step.
        let ancestors = SessionRepository::ancestors(&db, "c", 1).unwrap();
        assert_eq!(ancestors, vec!["b".to_string()]);
    }

    #[test]
    fn children_of_returns_only_direct_children() {
        let (db, _guard) = test_db();
        seed_project(&db);
        SessionRepository::insert(&db, &sample_session("p", "p1")).unwrap();
        SessionRepository::insert(&db, &{
            let mut s = sample_session("c1", "p1");
            s.parent_session_id = Some("p".to_string());
            s
        })
        .unwrap();
        SessionRepository::insert(&db, &{
            let mut s = sample_session("c2", "p1");
            s.parent_session_id = Some("p".to_string());
            s
        })
        .unwrap();
        SessionRepository::insert(&db, &{
            let mut s = sample_session("grandchild", "p1");
            s.parent_session_id = Some("c1".to_string());
            s
        })
        .unwrap();

        let kids = SessionRepository::children_of(&db, "p").unwrap();
        assert_eq!(kids.len(), 2);
        let ids: Vec<&str> = kids.iter().map(|k| k.id.as_str()).collect();
        assert!(ids.contains(&"c1"));
        assert!(ids.contains(&"c2"));
    }
}

pub struct ProviderConfigRepository;

impl ProviderConfigRepository {
    pub fn upsert(db: &Database, config: &ProviderConfig) -> Result<(), String> {
        db.connection()
            .execute(
                "INSERT OR REPLACE INTO provider_configs (name, model, api_key, base_url, protocol, enabled) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    config.name,
                    config.model,
                    config.api_key,
                    config.base_url,
                    config.protocol.as_str(),
                    config.enabled as i32
                ],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn find_by_name(db: &Database, name: &str) -> Result<Option<ProviderConfig>, String> {
        let mut stmt = db
            .connection()
            .prepare("SELECT name, model, api_key, base_url, protocol, enabled FROM provider_configs WHERE name = ?1")
            .map_err(|e| e.to_string())?;

        let result = stmt
            .query_row(params![name], |row| {
                Ok(ProviderConfig {
                    name: row.get(0)?,
                    model: row.get(1)?,
                    api_key: row.get(2)?,
                    base_url: row.get(3)?,
                    protocol: ProviderProtocol::from_name(row.get::<_, String>(4)?.as_str()),
                    enabled: row.get::<_, i32>(5)? != 0,
                })
            })
            .ok();

        Ok(result)
    }

    pub fn list(db: &Database) -> Result<Vec<ProviderConfig>, String> {
        let mut stmt = db
            .connection()
            .prepare(
                "SELECT name, model, api_key, base_url, protocol, enabled FROM provider_configs",
            )
            .map_err(|e| e.to_string())?;

        let configs = stmt
            .query_map([], |row| {
                Ok(ProviderConfig {
                    name: row.get(0)?,
                    model: row.get(1)?,
                    api_key: row.get(2)?,
                    base_url: row.get(3)?,
                    protocol: ProviderProtocol::from_name(row.get::<_, String>(4)?.as_str()),
                    enabled: row.get::<_, i32>(5)? != 0,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|row| row.ok())
            .collect();

        Ok(configs)
    }

    pub fn delete(db: &Database, name: &str) -> Result<(), String> {
        db.connection()
            .execute(
                "DELETE FROM provider_configs WHERE name = ?1",
                params![name],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
