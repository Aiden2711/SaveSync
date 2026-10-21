//! SQLite connection handling and forward-only migrations.
//!
//! Connections run in WAL mode with `synchronous=FULL` and foreign keys on.
//! Migrations are applied inside a transaction and tracked with
//! `PRAGMA user_version`, so a partially applied upgrade can never be
//! observed.

use std::path::Path;

use rusqlite::{Connection, Transaction};

use crate::error::{Error, Result};

/// Ordered list of schema migrations. Never edit an existing entry: append a
/// new one and bump [`SCHEMA_VERSION`].
const MIGRATIONS: &[&str] = &[
    include_str!("sql/001_initial.sql"),
    include_str!("sql/002_add_cover_path.sql"),
    include_str!("sql/003_discovery_cache.sql"),
];

/// Latest schema version understood by this binary.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// A configured SQLite connection.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the database at `path`, configure it, and migrate.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database (tests and previews only).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // A generous busy timeout keeps a concurrent reader from failing a
        // snapshot commit; UI reads should never block a write indefinitely.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        // WAL is a no-op for in-memory databases; ignore the returned mode.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Apply any pending migrations. Safe to call repeatedly.
    pub fn migrate(&mut self) -> Result<u32> {
        let current: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?
            as u32;
        if current > SCHEMA_VERSION {
            return Err(Error::InvalidState(format!(
                "database schema v{current} is newer than this build (v{SCHEMA_VERSION})"
            )));
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let version = index as u32 + 1;
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(SCHEMA_VERSION)
    }

    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as u32)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Run a closure inside an immediate write transaction.
    ///
    /// `IMMEDIATE` avoids upgrade deadlocks when two writers race and keeps
    /// metadata changes atomic with the mutations they describe.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent() {
        let mut db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        db.migrate().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Db::open_in_memory().unwrap();
        let result = db.connection().execute(
            "INSERT INTO locations (id, game_id, path, kind) VALUES ('l1', 'missing', '/tmp', 'directory')",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn wal_mode_is_enabled_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("savesync.db")).unwrap();
        let mode: String = db
            .connection()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
