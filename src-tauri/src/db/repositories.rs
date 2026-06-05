use rusqlite::{params, Result};
use crate::domain::{Project, AgentSession, Message, MessageRole};
use crate::db::connection::Database;
use chrono::{DateTime, Utc};

pub struct ProjectRepository;

impl ProjectRepository {
    pub fn insert(db: &Database, project: &Project) -> Result<(), String> {
        let tech_stack_json = serde_json::to_string(&project.tech_stack)
            .map_err(|e| e.to_string())?;

        db.connection().execute(
            "INSERT INTO projects (id, path, name, tech_stack, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                project.id,
                project.path,
                project.name,
                tech_stack_json,
                project.created_at.to_rfc3339()
            ],
        ).map_err(|e| e.to_string())
    }

    pub fn find_by_id(db: &Database, id: &str) -> Result<Option<Project>, String> {
        let mut stmt = db.connection().prepare(
            "SELECT id, path, name, tech_stack, created_at FROM projects WHERE id = ?1"
        ).map_err(|e| e.to_string())?;

        let project = stmt.query_row(params![id], |row| {
            let tech_stack_json: String = row.get(3)?;
            let tech_stack: Vec<String> = serde_json::from_str(&tech_stack_json).unwrap_or_default();
            let created_at: String = row.get(4)?;

            Ok(Project {
                id: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                tech_stack,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        }).ok();

        Ok(project)
    }

    pub fn list(db: &Database) -> Result<Vec<Project>, String> {
        let mut stmt = db.connection().prepare(
            "SELECT id, path, name, tech_stack, created_at FROM projects"
        ).map_err(|e| e.to_string())?;

        let projects = stmt.query_map([], |row| {
            let tech_stack_json: String = row.get(3)?;
            let tech_stack: Vec<String> = serde_json::from_str(&tech_stack_json).unwrap_or_default();
            let created_at: String = row.get(4)?;

            Ok(Project {
                id: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                tech_stack,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        }).map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

        Ok(projects)
    }

    pub fn delete(db: &Database, id: &str) -> Result<(), String> {
        db.connection().execute(
            "DELETE FROM projects WHERE id = ?1",
            params![id],
        ).map_err(|e| e.to_string())
    }
}

pub struct SessionRepository;

impl SessionRepository {
    pub fn insert(db: &Database, session: &AgentSession) -> Result<(), String> {
        db.connection().execute(
            "INSERT INTO agent_sessions (id, project_id, provider, model, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.id,
                session.project_id,
                session.provider,
                session.model,
                session.created_at.to_rfc3339()
            ],
        ).map_err(|e| e.to_string())
    }

    pub fn find_by_project(db: &Database, project_id: &str) -> Result<Vec<AgentSession>, String> {
        let mut stmt = db.connection().prepare(
            "SELECT id, project_id, provider, model, created_at FROM agent_sessions WHERE project_id = ?1"
        ).map_err(|e| e.to_string())?;

        let sessions = stmt.query_map(params![project_id], |row| {
            let created_at: String = row.get(4)?;
            Ok(AgentSession {
                id: row.get(0)?,
                project_id: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        }).map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

        Ok(sessions)
    }

    pub fn delete(db: &Database, id: &str) -> Result<(), String> {
        db.connection().execute(
            "DELETE FROM agent_sessions WHERE id = ?1",
            params![id],
        ).map_err(|e| e.to_string())
    }
}

pub struct MessageRepository;

impl MessageRepository {
    pub fn insert(db: &Database, message: &Message) -> Result<(), String> {
        let role_str = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        };

        db.connection().execute(
            "INSERT INTO messages (id, session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                message.id,
                message.session_id,
                role_str,
                message.content,
                message.timestamp.to_rfc3339()
            ],
        ).map_err(|e| e.to_string())
    }

    pub fn find_by_session(db: &Database, session_id: &str) -> Result<Vec<Message>, String> {
        let mut stmt = db.connection().prepare(
            "SELECT id, session_id, role, content, timestamp FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC"
        ).map_err(|e| e.to_string())?;

        let messages = stmt.query_map(params![session_id], |row| {
            let role_str: String = row.get(2)?;
            let role = match role_str.as_str() {
                "user" => MessageRole::User,
                "assistant" => MessageRole::Assistant,
                _ => MessageRole::System,
            };
            let timestamp: String = row.get(4)?;

            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                role,
                content: row.get(3)?,
                timestamp: DateTime::parse_from_rfc3339(&timestamp)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        }).map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

        Ok(messages)
    }

    pub fn delete_by_session(db: &Database, session_id: &str) -> Result<(), String> {
        db.connection().execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        ).map_err(|e| e.to_string())
    }
}