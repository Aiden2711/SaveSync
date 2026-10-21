//! Repository functions over the SQLite schema.
//!
//! These helpers never touch user save files; they only read and write
//! metadata. Callers are expected to wrap multi-step metadata changes in
//! [`crate::db::Db::transaction`].

use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::manifest::{FileEntry, Manifest};
use crate::models::{
    AdapterType, Conflict, Device, Game, Location, LocationKind, RestoreJournalEntry, RestoreState,
    Revision,
};

/// Current UTC timestamp in RFC 3339 with millisecond precision.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Generate a new random identifier.
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// Insert a device row, or refresh its friendly name and last-seen time.
pub fn upsert_device(conn: &Connection, device: &Device) -> Result<()> {
    conn.execute(
        "INSERT INTO devices (id, name, platform, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             platform = excluded.platform,
             last_seen_at = excluded.last_seen_at",
        params![
            device.id,
            device.name,
            device.platform,
            device.created_at,
            device.last_seen_at
        ],
    )?;
    Ok(())
}

pub fn get_device(conn: &Connection, id: &str) -> Result<Option<Device>> {
    Ok(conn
        .query_row(
            "SELECT id, name, platform, created_at, last_seen_at FROM devices WHERE id = ?1",
            [id],
            |row| {
                Ok(Device {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    platform: row.get(2)?,
                    created_at: row.get(3)?,
                    last_seen_at: row.get(4)?,
                })
            },
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// Games
// ---------------------------------------------------------------------------

fn map_game(row: &Row<'_>) -> rusqlite::Result<Game> {
    let adapter: String = row.get(2)?;
    Ok(Game {
        id: row.get(0)?,
        title: row.get(1)?,
        adapter_type: AdapterType::parse(&adapter).unwrap_or(AdapterType::CustomDirectory),
        enrolled: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        active_head_revision_id: row.get(6)?,
        cover_path: row.get(7)?,
    })
}

const GAME_COLUMNS: &str = "id, title, adapter_type, enrolled, created_at, updated_at, active_head_revision_id, cover_path";

/// Register a game.
///
/// `enrolled` must be `false` for anything produced by discovery. Enrollment is
/// always a separate, explicit user action (`set_enrolled`).
pub fn create_game(
    conn: &Connection,
    title: &str,
    adapter_type: AdapterType,
    enrolled: bool,
) -> Result<Game> {
    create_game_with_id(conn, &new_id(), title, adapter_type, enrolled)
}

/// Register a game with a caller-supplied identity.
///
/// Used when a second device adopts a game identity that already exists on a
/// sync provider. Enrollment remains explicit; callers must not use this for
/// discovery.
pub fn create_game_with_id(
    conn: &Connection,
    id: &str,
    title: &str,
    adapter_type: AdapterType,
    enrolled: bool,
) -> Result<Game> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Error::InvalidState("game title may not be empty".into()));
    }
    if id.trim().is_empty() {
        return Err(Error::InvalidState("game id may not be empty".into()));
    }
    let now = now_iso();
    let game = Game {
        id: id.to_string(),
        title: title.to_string(),
        adapter_type,
        enrolled,
        created_at: now.clone(),
        updated_at: now,
        active_head_revision_id: None,
        cover_path: None,
    };
    conn.execute(
        "INSERT INTO games (id, title, adapter_type, enrolled, created_at, updated_at, active_head_revision_id, cover_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
        params![
            game.id,
            game.title,
            game.adapter_type.as_str(),
            i64::from(game.enrolled),
            game.created_at,
            game.updated_at
        ],
    )?;
    Ok(game)
}

pub fn get_game(conn: &Connection, id: &str) -> Result<Option<Game>> {
    Ok(conn
        .query_row(
            &format!("SELECT {GAME_COLUMNS} FROM games WHERE id = ?1"),
            [id],
            map_game,
        )
        .optional()?)
}

/// Fetch a game or fail with [`Error::NotFound`].
pub fn require_game(conn: &Connection, id: &str) -> Result<Game> {
    get_game(conn, id)?.ok_or_else(|| Error::NotFound(format!("game {id}")))
}

pub fn list_games(conn: &Connection, enrolled_only: bool) -> Result<Vec<Game>> {
    let sql = if enrolled_only {
        format!("SELECT {GAME_COLUMNS} FROM games WHERE enrolled = 1 ORDER BY title")
    } else {
        format!("SELECT {GAME_COLUMNS} FROM games ORDER BY title")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], map_game)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Explicitly change enrollment. Enrolling a discovered candidate is the only
/// way it can ever participate in snapshots.
pub fn set_enrolled(conn: &Connection, game_id: &str, enrolled: bool) -> Result<()> {
    let changed = conn.execute(
        "UPDATE games SET enrolled = ?2, updated_at = ?3 WHERE id = ?1",
        params![game_id, i64::from(enrolled), now_iso()],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(format!("game {game_id}")));
    }
    Ok(())
}

pub fn set_active_head(conn: &Connection, game_id: &str, revision_id: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE games SET active_head_revision_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![game_id, revision_id, now_iso()],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(format!("game {game_id}")));
    }
    Ok(())
}

/// Update the cover image path for a game. Pass `None` to clear it.
pub fn set_cover_path(conn: &Connection, game_id: &str, cover_path: Option<&str>) -> Result<()> {
    let changed = conn.execute(
        "UPDATE games SET cover_path = ?2, updated_at = ?3 WHERE id = ?1",
        params![game_id, cover_path, now_iso()],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(format!("game {game_id}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

pub fn add_location(
    conn: &Connection,
    game_id: &str,
    path: &str,
    kind: LocationKind,
) -> Result<Location> {
    require_game(conn, game_id)?;
    let location = Location {
        id: new_id(),
        game_id: game_id.to_string(),
        path: path.to_string(),
        kind,
    };
    conn.execute(
        "INSERT INTO locations (id, game_id, path, kind) VALUES (?1, ?2, ?3, ?4)",
        params![location.id, location.game_id, location.path, kind.as_str()],
    )?;
    Ok(location)
}

pub fn list_locations(conn: &Connection, game_id: &str) -> Result<Vec<Location>> {
    let mut stmt = conn.prepare(
        "SELECT id, game_id, path, kind FROM locations WHERE game_id = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map([game_id], |row| {
        let kind: String = row.get(3)?;
        Ok(Location {
            id: row.get(0)?,
            game_id: row.get(1)?,
            path: row.get(2)?,
            kind: LocationKind::parse(&kind).unwrap_or(LocationKind::Directory),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// Revisions
// ---------------------------------------------------------------------------

fn map_revision(row: &Row<'_>) -> rusqlite::Result<Revision> {
    let parents: String = row.get(2)?;
    Ok(Revision {
        id: row.get(0)?,
        game_id: row.get(1)?,
        parent_ids: serde_json::from_str(&parents).unwrap_or_default(),
        device_id: row.get(3)?,
        created_at: row.get(4)?,
        manifest_hash: row.get(5)?,
        blob_hash: row.get(6)?,
        message: row.get(7)?,
    })
}

const REVISION_COLUMNS: &str =
    "id, game_id, parent_ids, device_id, created_at, manifest_hash, blob_hash, message";

/// Insert an immutable revision node.
pub fn insert_revision(conn: &Connection, revision: &Revision) -> Result<()> {
    for parent in &revision.parent_ids {
        let parent_revision = get_revision(conn, parent)?
            .ok_or_else(|| Error::NotFound(format!("parent revision {parent}")))?;
        if parent_revision.game_id != revision.game_id {
            return Err(Error::InvalidState(format!(
                "parent revision {parent} belongs to a different game"
            )));
        }
    }
    conn.execute(
        "INSERT INTO revisions (id, game_id, parent_ids, device_id, created_at, manifest_hash, blob_hash, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            revision.id,
            revision.game_id,
            serde_json::to_string(&revision.parent_ids)?,
            revision.device_id,
            revision.created_at,
            revision.manifest_hash,
            revision.blob_hash,
            revision.message
        ],
    )?;
    Ok(())
}

pub fn get_revision(conn: &Connection, id: &str) -> Result<Option<Revision>> {
    Ok(conn
        .query_row(
            &format!("SELECT {REVISION_COLUMNS} FROM revisions WHERE id = ?1"),
            [id],
            map_revision,
        )
        .optional()?)
}

pub fn require_revision(conn: &Connection, id: &str) -> Result<Revision> {
    get_revision(conn, id)?.ok_or_else(|| Error::NotFound(format!("revision {id}")))
}

pub fn list_revisions(conn: &Connection, game_id: &str) -> Result<Vec<Revision>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REVISION_COLUMNS} FROM revisions WHERE game_id = ?1 ORDER BY created_at, id"
    ))?;
    let rows = stmt.query_map([game_id], map_revision)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Return revisions that are not a parent of any other revision of the game.
pub fn list_heads(conn: &Connection, game_id: &str) -> Result<Vec<Revision>> {
    let revisions = list_revisions(conn, game_id)?;
    let mut parents = std::collections::HashSet::new();
    for revision in &revisions {
        for parent in &revision.parent_ids {
            parents.insert(parent.clone());
        }
    }
    Ok(revisions
        .into_iter()
        .filter(|revision| !parents.contains(&revision.id))
        .collect())
}

// ---------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------

/// Persist every file entry of a manifest. Entries are immutable once written.
pub fn store_manifest(conn: &Connection, manifest: &Manifest) -> Result<String> {
    let hash = manifest.hash();
    conn.execute(
        "INSERT INTO manifest_sets (manifest_hash, created_at) VALUES (?1, ?2)
         ON CONFLICT(manifest_hash) DO NOTHING",
        params![hash, now_iso()],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO manifests (manifest_hash, relative_path, size, sha256, mtime_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(manifest_hash, relative_path) DO NOTHING",
    )?;
    for entry in &manifest.entries {
        stmt.execute(params![
            hash,
            entry.path,
            i64::try_from(entry.size).unwrap_or(i64::MAX),
            entry.sha256,
            entry.mtime_ms
        ])?;
    }
    Ok(hash)
}

pub fn load_manifest(conn: &Connection, manifest_hash: &str) -> Result<Manifest> {
    let known: i64 = conn.query_row(
        "SELECT COUNT(*) FROM manifest_sets WHERE manifest_hash = ?1",
        [manifest_hash],
        |row| row.get(0),
    )?;
    if known == 0 {
        return Err(Error::NotFound(format!("manifest {manifest_hash}")));
    }

    let mut stmt = conn.prepare(
        "SELECT relative_path, size, sha256, mtime_ms
         FROM manifests WHERE manifest_hash = ?1 ORDER BY relative_path",
    )?;
    let rows = stmt.query_map([manifest_hash], |row| {
        Ok(FileEntry {
            path: row.get(0)?,
            size: row.get::<_, i64>(1)? as u64,
            sha256: row.get(2)?,
            mtime_ms: row.get(3)?,
        })
    })?;
    let entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Manifest::new(entries)
}

// ---------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------

pub fn insert_conflict(conn: &Connection, conflict: &Conflict) -> Result<()> {
    conn.execute(
        "INSERT INTO conflicts (id, game_id, revision_ids, detected_at, resolved_at, resolution_revision_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            conflict.id,
            conflict.game_id,
            serde_json::to_string(&conflict.revision_ids)?,
            conflict.detected_at,
            conflict.resolved_at,
            conflict.resolution_revision_id
        ],
    )?;
    Ok(())
}

pub fn get_conflict(conn: &Connection, id: &str) -> Result<Option<Conflict>> {
    Ok(conn
        .query_row(
            "SELECT id, game_id, revision_ids, detected_at, resolved_at, resolution_revision_id
             FROM conflicts WHERE id = ?1",
            [id],
            |row| {
                let ids: String = row.get(2)?;
                Ok(Conflict {
                    id: row.get(0)?,
                    game_id: row.get(1)?,
                    revision_ids: serde_json::from_str(&ids).unwrap_or_default(),
                    detected_at: row.get(3)?,
                    resolved_at: row.get(4)?,
                    resolution_revision_id: row.get(5)?,
                })
            },
        )
        .optional()?)
}

pub fn list_open_conflicts(conn: &Connection, game_id: &str) -> Result<Vec<Conflict>> {
    let mut stmt = conn.prepare(
        "SELECT id, game_id, revision_ids, detected_at, resolved_at, resolution_revision_id
         FROM conflicts WHERE game_id = ?1 AND resolved_at IS NULL ORDER BY detected_at",
    )?;
    let rows = stmt.query_map([game_id], |row| {
        let ids: String = row.get(2)?;
        Ok(Conflict {
            id: row.get(0)?,
            game_id: row.get(1)?,
            revision_ids: serde_json::from_str(&ids).unwrap_or_default(),
            detected_at: row.get(3)?,
            resolved_at: row.get(4)?,
            resolution_revision_id: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Mark a conflict resolved by an explicit resolution commit.
pub fn resolve_conflict(conn: &Connection, conflict_id: &str, revision_id: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE conflicts SET resolved_at = ?2, resolution_revision_id = ?3
         WHERE id = ?1 AND resolved_at IS NULL",
        params![conflict_id, now_iso(), revision_id],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(format!("open conflict {conflict_id}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore journal
// ---------------------------------------------------------------------------

fn map_journal(row: &Row<'_>) -> rusqlite::Result<RestoreJournalEntry> {
    let state: String = row.get(7)?;
    let deleted: String = row.get(8)?;
    Ok(RestoreJournalEntry {
        id: row.get(0)?,
        game_id: row.get(1)?,
        revision_id: row.get(2)?,
        target_root: row.get(3)?,
        staging_path: row.get(4)?,
        backup_path: row.get(5)?,
        backup_manifest: row.get(6)?,
        state: RestoreState::parse(&state).unwrap_or(RestoreState::RollbackFailed),
        deleted_paths: serde_json::from_str(&deleted).unwrap_or_default(),
        error: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

const JOURNAL_COLUMNS: &str = "id, game_id, revision_id, target_root, staging_path, backup_path, \
     backup_manifest, state, deleted_paths, error, created_at, updated_at";

pub fn insert_journal(conn: &Connection, entry: &RestoreJournalEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO restore_journal
             (id, game_id, revision_id, target_root, staging_path, backup_path,
              backup_manifest, state, deleted_paths, error, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            entry.id,
            entry.game_id,
            entry.revision_id,
            entry.target_root,
            entry.staging_path,
            entry.backup_path,
            entry.backup_manifest,
            entry.state.as_str(),
            serde_json::to_string(&entry.deleted_paths)?,
            entry.error,
            entry.created_at,
            entry.updated_at
        ],
    )?;
    Ok(())
}

pub fn update_journal(
    conn: &Connection,
    id: &str,
    state: RestoreState,
    deleted_paths: &[String],
    error: Option<&str>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE restore_journal
         SET state = ?2, deleted_paths = ?3, error = ?4, updated_at = ?5
         WHERE id = ?1",
        params![
            id,
            state.as_str(),
            serde_json::to_string(deleted_paths)?,
            error,
            now_iso()
        ],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(format!("restore journal entry {id}")));
    }
    Ok(())
}

pub fn get_journal(conn: &Connection, id: &str) -> Result<Option<RestoreJournalEntry>> {
    Ok(conn
        .query_row(
            &format!("SELECT {JOURNAL_COLUMNS} FROM restore_journal WHERE id = ?1"),
            [id],
            map_journal,
        )
        .optional()?)
}

pub fn list_in_flight_restores(conn: &Connection) -> Result<Vec<RestoreJournalEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOURNAL_COLUMNS} FROM restore_journal WHERE state = 'prepared' ORDER BY created_at"
    ))?;
    let rows = stmt.query_map([], map_journal)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn list_journal_for_game(conn: &Connection, game_id: &str) -> Result<Vec<RestoreJournalEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOURNAL_COLUMNS} FROM restore_journal WHERE game_id = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map([game_id], map_journal)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
