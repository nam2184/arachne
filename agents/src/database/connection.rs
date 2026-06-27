use rusqlite::{Connection, Result};
use std::path::PathBuf;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new(path: PathBuf) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

    pub fn init(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                tech_stack TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS agent_sessions (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                directory TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                summary_json TEXT,
                parent_session_id TEXT,
                created_at TEXT NOT NULL,
                FOREIGN KEY (project_id) REFERENCES projects(id)
            );

            CREATE TABLE IF NOT EXISTS session_groups (
                id TEXT PRIMARY KEY,
                name TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS session_group_sessions (
                group_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                PRIMARY KEY (group_id, session_id),
                FOREIGN KEY (group_id) REFERENCES session_groups(id),
                FOREIGN KEY (session_id) REFERENCES agent_sessions(id)
            );

            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES agent_sessions(id)
            );

            CREATE TABLE IF NOT EXISTS memory (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                fact TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (project_id) REFERENCES projects(id)
            );

            CREATE TABLE IF NOT EXISTS provider_configs (
                name TEXT PRIMARY KEY,
                model TEXT NOT NULL,
                api_key TEXT,
                base_url TEXT,
                protocol TEXT NOT NULL DEFAULT 'openai',
                enabled INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS schema_migrations (
                id TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            ",
            )
            .map_err(|e| e.to_string())?;

        let _ = self
            .conn
            .execute("ALTER TABLE session_groups ADD COLUMN name TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE agent_sessions ADD COLUMN directory TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE agent_sessions ADD COLUMN provider TEXT NOT NULL DEFAULT 'anthropic'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE agent_sessions ADD COLUMN model TEXT NOT NULL DEFAULT 'claude-3-5-sonnet-20241022'",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE agent_sessions ADD COLUMN parent_session_id TEXT",
            [],
        );
        let _ = self.conn.execute(
            "ALTER TABLE agent_sessions ADD COLUMN summary_json TEXT",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE agent_sessions ADD COLUMN title TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE provider_configs ADD COLUMN protocol TEXT NOT NULL DEFAULT 'openai'",
            [],
        );
        let _ = self.conn.execute(
            "UPDATE provider_configs SET protocol = 'anthropic' WHERE lower(name) = 'anthropic'",
            [],
        );
        let _ = self.conn.execute(
            "UPDATE provider_configs SET protocol = 'openai' WHERE lower(name) IN ('openai', 'minimax')",
            [],
        );

        // Enforce foreign keys so cascade behavior is testable.
        self.conn
            .execute("PRAGMA foreign_keys = ON", [])
            .map_err(|e| e.to_string())?;

        // Indexes must be created after the column migrations above so that
        // pre-existing databases (which lack `parent_session_id`) get the
        // column added before the index is built.
        self.conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_sessions_project ON agent_sessions(project_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_parent ON agent_sessions(parent_session_id);
            CREATE INDEX IF NOT EXISTS idx_session_groups_session ON session_group_sessions(session_id);
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE INDEX IF NOT EXISTS idx_memory_project ON memory(project_id);
            "
        ).map_err(|e| e.to_string())?;

        self.migrate_session_directories_to_canonical()?;

        Ok(())
    }

    /// One-shot migration for pre-existing sessions. New sessions are
    /// canonicalized by `SessionService::create_session_with_parent`; this
    /// brings old rows to the same invariant without paying filesystem IO on
    /// every read or every boot.
    fn migrate_session_directories_to_canonical(&self) -> Result<(), String> {
        const MIGRATION_ID: &str = "20260623_canonical_agent_session_directories";
        use crate::sandbox::path::strip_verbatim_prefix;

        let already_applied: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE id = ?1)",
                [MIGRATION_ID],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if already_applied {
            return Ok(());
        }

        let mut stmt = self
            .conn
            .prepare("SELECT id, directory FROM agent_sessions")
            .map_err(|e| e.to_string())?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;

        let mut migrated = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        for (id, raw) in rows {
            if raw.trim().is_empty() {
                skipped += 1;
                continue;
            }

            let canonical = match PathBuf::from(&raw).canonicalize() {
                Ok(path) => strip_verbatim_prefix(path).to_string_lossy().to_string(),
                Err(error) => {
                    failed += 1;
                    tracing::warn!(
                        session_id = %id,
                        directory = %raw,
                        error = %error,
                        "could not canonicalize existing session directory; leaving it unchanged"
                    );
                    continue;
                }
            };
            if canonical == raw {
                skipped += 1;
                continue;
            }
            let result = self.conn.execute(
                "UPDATE agent_sessions SET directory = ?1 WHERE id = ?2",
                rusqlite::params![canonical, id],
            );
            match result {
                Ok(_) => {
                    migrated += 1;
                    tracing::info!(
                        session_id = %id,
                        old = %raw,
                        new = %canonical,
                        "migrated session directory to canonical form"
                    );
                }
                Err(error) => {
                    failed += 1;
                    tracing::warn!(
                        session_id = %id,
                        old = %raw,
                        new = %canonical,
                        error = %error,
                        "could not update session directory during canonicalization migration"
                    );
                }
            }
        }

        self.conn
            .execute(
                "INSERT OR IGNORE INTO schema_migrations (id, applied_at) VALUES (?1, datetime('now'))",
                [MIGRATION_ID],
            )
            .map_err(|e| e.to_string())?;

        tracing::info!(
            migrated,
            skipped,
            failed,
            "session directory canonicalization migration complete"
        );
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use tempfile::TempDir;

    /// Creates a fresh initialized database backed by a temp file.
    /// Returns the database and the temp directory guard (drop on test exit).
    pub(crate) fn test_db() -> (Database, TempDir) {
        let dir = TempDir::new().expect("failed to create tempdir");
        let path = dir.path().join("test.sqlite");
        let db = Database::new(path).expect("failed to open database");
        db.init().expect("failed to init database");
        (db, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::test_db;

    #[test]
    fn init_is_idempotent() {
        let (db, _guard) = test_db();
        // Re-running init should not fail or duplicate tables.
        db.init().expect("init must be idempotent");
        // Connection still works.
        let count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert!(count > 0);
    }

    #[test]
    fn multiple_connections_to_same_file_share_state() {
        // Demonstrates that the temp-file approach supports multi-connection access,
        // which the in-memory `:memory:` approach does not.
        let (_db, guard) = test_db();
        let path_a = guard.path().join("test.sqlite");
        let path_b = path_a.clone();

        let conn_a = Connection::open(&path_a).unwrap();
        let conn_b = Connection::open(&path_b).unwrap();

        conn_a
            .execute(
                "CREATE TABLE IF NOT EXISTS shared (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
                [],
            )
            .unwrap();
        conn_a
            .execute("INSERT INTO shared (label) VALUES (?1)", ["from-a"])
            .unwrap();

        let count: i64 = conn_b
            .query_row("SELECT COUNT(*) FROM shared", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let label: String = conn_b
            .query_row("SELECT label FROM shared LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(label, "from-a");
    }
}
