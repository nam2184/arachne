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
