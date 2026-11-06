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
    include_str!("sql/004_cover_variants.sql"),
    include_str!("sql/005_game_metadata.sql"),
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

    /// Insert a game row using only the columns schema v1 already had, so the
    /// statement stays valid on an older database.
    fn insert_v1_game(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO games
                 (id, title, adapter_type, enrolled, created_at, updated_at,
                  active_head_revision_id, cover_path)
             VALUES (?1, 'Hades', 'custom_directory', 1, 'now', 'now', NULL, ?2)",
            rusqlite::params![id, format!("covers/{id}.png")],
        )
        .unwrap();
    }

    /// v4 adds the wide cover and the featured flag without disturbing rows a
    /// previous build wrote: they keep their portrait cover and gain defaults.
    #[test]
    fn v4_adds_cover_variants_and_defaults_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 3u32).unwrap();
        insert_v1_game(&conn, "g1");

        let mut db = Db { conn };
        assert_eq!(db.migrate().unwrap(), SCHEMA_VERSION);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let game = crate::store::get_game(db.connection(), "g1")
            .unwrap()
            .expect("the pre-existing row must survive the migration");
        assert_eq!(game.cover_path.as_deref(), Some("covers/g1.png"));
        assert_eq!(game.wide_cover_path, None);
        assert!(!game.featured, "old rows are not featured by default");

        // Forward-only migrations stay re-runnable.
        db.migrate().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// The schema itself keeps "at most one featured game" true.
    #[test]
    fn only_one_game_can_be_featured() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.connection();
        insert_v1_game(conn, "g1");
        insert_v1_game(conn, "g2");

        conn.execute("UPDATE games SET featured = 1 WHERE id = 'g1'", [])
            .unwrap();
        let error = conn
            .execute("UPDATE games SET featured = 1 WHERE id = 'g2'", [])
            .unwrap_err();
        assert!(
            error.to_string().contains("UNIQUE"),
            "a second featured row must be rejected, got {error}"
        );
    }

    /// v5 adds an empty metadata cache table that older rows never touch, and
    /// a metadata row disappears with the game it describes.
    #[test]
    fn v5_adds_game_metadata_cache_and_cascades_with_the_game() {
        let conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..4] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 4u32).unwrap();
        // `from_connection` sets this in production; enable it here so the
        // cascade assertion exercises the schema rather than a disabled pragma.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        insert_v1_game(&conn, "g1");

        let mut db = Db { conn };
        assert_eq!(db.migrate().unwrap(), SCHEMA_VERSION);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        // The pre-existing game survives, and it simply has no metadata yet.
        let game = crate::store::get_game(db.connection(), "g1")
            .unwrap()
            .expect("the pre-existing row must survive the migration");
        assert_eq!(game.title, "Hades");
        assert!(
            crate::store::load_metadata(db.connection(), "g1")
                .unwrap()
                .is_none()
        );

        let metadata = crate::metadata::GameMetadata {
            source: crate::metadata::MetadataSource::Steam,
            source_id: "1145360".to_string(),
            name: "Hades".to_string(),
            release_year: Some(2020),
            release_date: Some("17 Sep, 2020".to_string()),
            genres: vec!["Action".to_string(), "Roguelike".to_string()],
            developers: vec!["Supergiant Games".to_string()],
            publishers: vec!["Supergiant Games".to_string()],
            description_short: Some("Defy the god of the dead.".to_string()),
            description_long: None,
            players: None,
            age_rating: None,
            franchise: None,
            themes: Vec::new(),
            fetched_at: "2026-11-06T10:00:00.000Z".to_string(),
        };
        crate::store::upsert_metadata(db.connection(), "g1", &metadata).unwrap();
        assert_eq!(
            crate::store::load_metadata(db.connection(), "g1")
                .unwrap()
                .expect("the cached row round-trips"),
            metadata
        );

        // One row per game: a later fetch replaces rather than duplicates.
        let mut updated = metadata.clone();
        updated.name = "Hades (updated)".to_string();
        updated.franchise = Some("Hades".to_string());
        crate::store::upsert_metadata(db.connection(), "g1", &updated).unwrap();
        assert_eq!(
            crate::store::load_metadata(db.connection(), "g1")
                .unwrap()
                .unwrap(),
            updated
        );

        // Deleting the game removes its metadata with it.
        db.connection()
            .execute("DELETE FROM games WHERE id = 'g1'", [])
            .unwrap();
        assert!(
            crate::store::load_metadata(db.connection(), "g1")
                .unwrap()
                .is_none(),
            "metadata must not outlive its game"
        );

        // Forward-only migrations stay re-runnable.
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
