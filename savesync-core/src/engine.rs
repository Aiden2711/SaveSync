//! High-level engine facade used by the Tauri command layer.
//!
//! The engine owns the database, the blob store, and the on-disk safety
//! directories. All public methods enforce the project's safety invariants:
//! enrollment is explicit, snapshots are verified, and restores are journaled.

use std::fs;
use std::path::{Path, PathBuf};

use crate::blob::BlobStore;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::manifest::{Manifest, ManifestDiff};
use crate::models::{
    AdapterType, Conflict, Device, Game, Location, LocationKind, RestoreJournalEntry,
    RestoreReport, Revision, SnapshotInfo,
};
use crate::paths::validate_root;
use crate::provider::SyncProvider;
use crate::restore::{self, DataPaths, RestoreMode, RestoreRequest};
use crate::store;
use crate::sync::{self, SyncDirection, SyncReport};

/// Settings key holding the stable device UUID.
const DEVICE_ID_KEY: &str = "device_id";

/// The SaveSync core engine.
pub struct SaveSyncEngine {
    db: Db,
    blobs: BlobStore,
    paths: DataPaths,
    device: Device,
}

impl SaveSyncEngine {
    /// Open (or initialize) a SaveSync data directory.
    ///
    /// Layout: `savesync.db`, `blobs/`, `staging/`, `rollback/`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let paths = DataPaths::new(root.as_ref().to_path_buf());
        for directory in [&paths.root, &paths.staging(), &paths.rollback()] {
            fs::create_dir_all(directory).map_err(|e| Error::io(directory, e))?;
        }
        let blobs = BlobStore::open(paths.blobs())?;
        let mut db = Db::open(paths.database())?;
        let device = load_or_create_device(&mut db)?;
        Ok(Self {
            db,
            blobs,
            paths,
            device,
        })
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Read a non-secret setting (for example the Google OAuth client id).
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        store::get_setting(self.db.connection(), key)
    }

    /// Persist a non-secret setting. Never use this for tokens.
    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<()> {
        self.db.transaction(|tx| store::set_setting(tx, key, value))
    }

    pub fn paths(&self) -> &DataPaths {
        &self.paths
    }

    /// Register a game that the user enrolled explicitly.
    pub fn enroll_game(&mut self, title: &str, adapter: AdapterType) -> Result<Game> {
        self.db
            .transaction(|tx| store::create_game(tx, title, adapter, true))
    }

    /// Enroll a game using an identity received from a sync provider.
    ///
    /// Explicit enrollment is still required; this only lets a second device
    /// share the game's UUID so revisions can be merged into one DAG.
    pub fn enroll_game_with_id(
        &mut self,
        id: &str,
        title: &str,
        adapter: AdapterType,
    ) -> Result<Game> {
        self.db
            .transaction(|tx| store::create_game_with_id(tx, id, title, adapter, true))
    }

    /// Register a discovered candidate. It cannot be snapshotted until
    /// [`Self::enroll`] is called.
    pub fn register_candidate(&mut self, title: &str, adapter: AdapterType) -> Result<Game> {
        self.db
            .transaction(|tx| store::create_game(tx, title, adapter, false))
    }

    /// Explicitly enroll a previously discovered candidate.
    pub fn enroll(&mut self, game_id: &str) -> Result<Game> {
        self.db.transaction(|tx| {
            store::set_enrolled(tx, game_id, true)?;
            store::require_game(tx, game_id)
        })
    }

    /// Remove a game from the library without deleting any of its data.
    ///
    /// This only sets `enrolled` to `false`. Save locations, revisions,
    /// conflicts, manifests, and content-addressed blobs are left untouched in
    /// SQLite and on disk, and the game keeps its identity and active head.
    /// Snapshots are refused while the game is unlinked; calling
    /// [`Self::enroll`] later restores it to the library with its full history.
    /// Deleting history is a separate, explicit action and is not performed
    /// here.
    pub fn unlink(&mut self, game_id: &str) -> Result<Game> {
        self.db.transaction(|tx| {
            store::set_enrolled(tx, game_id, false)?;
            store::require_game(tx, game_id)
        })
    }

    pub fn list_games(&self, enrolled_only: bool) -> Result<Vec<Game>> {
        store::list_games(self.db.connection(), enrolled_only)
    }

    pub fn get_game(&self, game_id: &str) -> Result<Game> {
        store::require_game(self.db.connection(), game_id)
    }

    /// Set or clear the cover image path for a game.
    ///
    /// The path is stored as-is; callers should validate that it points to an
    /// existing image file before calling. Pass `None` to remove the cover.
    pub fn set_cover_path(&mut self, game_id: &str, cover_path: Option<&str>) -> Result<Game> {
        self.db.transaction(|tx| {
            store::set_cover_path(tx, game_id, cover_path)?;
            store::require_game(tx, game_id)
        })
    }

    /// Attach a save location to a game.
    ///
    /// The path must already exist as a directory; it is canonicalized before
    /// being stored so duplicate enrollments cannot point at the same data
    /// through different spellings.
    pub fn add_directory_location(
        &mut self,
        game_id: &str,
        path: impl AsRef<Path>,
    ) -> Result<Location> {
        let canonical = validate_root(path)?;
        self.db.transaction(|tx| {
            store::add_location(
                tx,
                game_id,
                &canonical.to_string_lossy(),
                LocationKind::Directory,
            )
        })
    }

    pub fn list_locations(&self, game_id: &str) -> Result<Vec<Location>> {
        store::list_locations(self.db.connection(), game_id)
    }

    /// Discover save game candidates across all supported sources.
    ///
    /// This does NOT auto-enroll any game — it returns candidates for explicit
    /// user enrollment (respecting "zero auto-enrollment").
    ///
    /// Sources queried:
    /// 1. Ludusavi manifest (with ETag cache in SQLite)
    /// 2. Lutris configuration (Linux only)
    /// 3. Steam / Proton library scans
    pub fn discover_games(&mut self) -> Result<Vec<crate::discovery::DiscoveredGame>> {
        Ok(crate::discovery::discover_all_games(&mut self.db))
    }

    /// Capture the game's save directory as an immutable revision.
    ///
    /// Steps: hash the directory, archive it into the blob store, persist the
    /// manifest and revision in one transaction, then move the active head.
    pub fn snapshot_game(&mut self, game_id: &str, message: Option<&str>) -> Result<SnapshotInfo> {
        let game = store::require_game(self.db.connection(), game_id)?;
        if !game.enrolled {
            return Err(Error::InvalidState(format!(
                "game {} is not enrolled; refusing to snapshot",
                game.title
            )));
        }

        let location = self.single_directory_location(game_id)?;
        let root = validate_root(&location.path)?;
        let manifest = Manifest::from_dir(&root)?;
        let blob_hash = self.blobs.put(&manifest, &root)?;
        let manifest_hash = manifest.hash();
        if blob_hash != manifest_hash {
            return Err(Error::InvalidState(
                "blob digest does not match manifest digest".into(),
            ));
        }

        let revision_id = store::new_id();
        let created_at = store::now_iso();
        let parent_ids: Vec<String> = game.active_head_revision_id.clone().into_iter().collect();
        let device_id = self.device.id.clone();
        let title_message = message.map(str::to_string);
        let entries = manifest.entries.len();
        let total_bytes = manifest.total_size();

        self.db.transaction(|tx| {
            store::store_manifest(tx, &manifest)?;
            let revision = Revision {
                id: revision_id.clone(),
                game_id: game_id.to_string(),
                parent_ids,
                device_id,
                created_at,
                manifest_hash: manifest_hash.clone(),
                blob_hash: Some(blob_hash.clone()),
                message: title_message,
            };
            store::insert_revision(tx, &revision)?;
            store::set_active_head(tx, game_id, &revision.id)
        })?;

        Ok(SnapshotInfo {
            revision_id,
            manifest_hash,
            blob_hash,
            file_count: entries,
            total_bytes,
        })
    }

    /// Return the game's revision history, newest first.
    pub fn history(&self, game_id: &str) -> Result<Vec<Revision>> {
        let mut revisions = store::list_revisions(self.db.connection(), game_id)?;
        revisions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(revisions)
    }

    pub fn heads(&self, game_id: &str) -> Result<Vec<Revision>> {
        store::list_heads(self.db.connection(), game_id)
    }

    pub fn manifest_for_revision(&self, revision_id: &str) -> Result<Manifest> {
        let revision = store::require_revision(self.db.connection(), revision_id)?;
        store::load_manifest(self.db.connection(), &revision.manifest_hash)
    }

    pub fn diff(&self, old_revision: &str, new_revision: &str) -> Result<ManifestDiff> {
        let old = self.manifest_for_revision(old_revision)?;
        let new = self.manifest_for_revision(new_revision)?;
        Ok(old.diff(&new))
    }

    /// Restore a revision into the game's enrolled save directory.
    pub fn restore(&mut self, revision_id: &str) -> Result<RestoreReport> {
        self.restore_with_mode(revision_id, RestoreMode::Overlay)
    }

    /// Restore with an explicit extra-file policy.
    pub fn restore_with_mode(
        &mut self,
        revision_id: &str,
        mode: RestoreMode,
    ) -> Result<RestoreReport> {
        let revision = store::require_revision(self.db.connection(), revision_id)?;
        let location = self.single_directory_location(&revision.game_id)?;
        self.restore_to(revision_id, PathBuf::from(location.path), mode)
    }

    /// Restore a revision into an arbitrary directory (used for previews and
    /// exports). The same staging, backup, and verification rules apply, but
    /// the directory must live below the engine data root or be an enrolled
    /// location to avoid surprising writes. For Phase 1, only enrolled
    /// locations are accepted.
    pub fn restore_to(
        &mut self,
        revision_id: &str,
        target_root: PathBuf,
        mode: RestoreMode,
    ) -> Result<RestoreReport> {
        let revision = store::require_revision(self.db.connection(), revision_id)?;
        let locations = store::list_locations(self.db.connection(), &revision.game_id)?;
        let canonical_target = target_root.to_string_lossy().to_string();
        let is_enrolled_location = locations.iter().any(|location| {
            Path::new(&location.path) == target_root
                || Path::new(&location.path).to_string_lossy() == canonical_target
        });
        if !is_enrolled_location {
            return Err(Error::InvalidState(format!(
                "restore target {} is not an enrolled location of game {}",
                target_root.display(),
                revision.game_id
            )));
        }

        restore::restore_revision(
            &mut self.db,
            &self.blobs,
            &self.paths,
            RestoreRequest {
                game_id: revision.game_id,
                revision_id: revision.id,
                target_root,
                mode,
            },
        )
    }

    /// Roll back restores interrupted by a crash or power loss.
    pub fn recover(&mut self) -> Result<usize> {
        restore::recover_in_flight_restores(&mut self.db)
    }

    /// Restore journal entries for a game, newest first. Used by the UI audit
    /// view and by tests to assert rollback outcomes.
    pub fn restore_journal(&self, game_id: &str) -> Result<Vec<RestoreJournalEntry>> {
        store::list_journal_for_game(self.db.connection(), game_id)
    }

    pub fn open_conflicts(&self, game_id: &str) -> Result<Vec<Conflict>> {
        store::list_open_conflicts(self.db.connection(), game_id)
    }

    /// Record a divergence between heads. Does not modify save files.
    pub fn record_conflict(&mut self, game_id: &str, revision_ids: &[String]) -> Result<Conflict> {
        if revision_ids.len() < 2 {
            return Err(Error::InvalidState(
                "a conflict requires at least two revisions".into(),
            ));
        }
        for revision_id in revision_ids {
            let revision = store::require_revision(self.db.connection(), revision_id)?;
            if revision.game_id != game_id {
                return Err(Error::InvalidState(format!(
                    "revision {revision_id} does not belong to game {game_id}"
                )));
            }
        }
        let conflict = Conflict {
            id: store::new_id(),
            game_id: game_id.to_string(),
            revision_ids: revision_ids.to_vec(),
            detected_at: store::now_iso(),
            resolved_at: None,
            resolution_revision_id: None,
        };
        self.db
            .transaction(|tx| store::insert_conflict(tx, &conflict))?;
        Ok(conflict)
    }

    /// Resolve an open conflict by creating an explicit multi-parent merge
    /// revision pointing at `revision_id`'s snapshot.
    ///
    /// Live save files are not touched. The caller must separately restore the
    /// chosen revision if the user wants it applied on this device.
    pub fn resolve_conflict_with(
        &mut self,
        conflict_id: &str,
        revision_id: &str,
    ) -> Result<Revision> {
        let conflict = store::get_conflict(self.db.connection(), conflict_id)?
            .ok_or_else(|| Error::NotFound(format!("conflict {conflict_id}")))?;
        if conflict.resolved_at.is_some() {
            return Err(Error::InvalidState(format!(
                "conflict {conflict_id} is already resolved"
            )));
        }
        let chosen = store::require_revision(self.db.connection(), revision_id)?;
        if chosen.game_id != conflict.game_id {
            return Err(Error::InvalidState(format!(
                "revision {revision_id} does not belong to conflict {conflict_id}"
            )));
        }
        if !conflict.revision_ids.contains(&chosen.id) {
            return Err(Error::InvalidState(format!(
                "revision {revision_id} is not one of the conflicting heads"
            )));
        }
        if !self.blobs.contains(&chosen.manifest_hash) {
            return Err(Error::NotFound(format!(
                "blob for chosen revision {} is not available locally",
                chosen.id
            )));
        }

        let mut parent_ids = conflict.revision_ids.clone();
        parent_ids.sort();
        parent_ids.dedup();
        let revision = Revision {
            id: store::new_id(),
            game_id: conflict.game_id.clone(),
            parent_ids,
            device_id: self.device.id.clone(),
            created_at: store::now_iso(),
            manifest_hash: chosen.manifest_hash.clone(),
            blob_hash: chosen.blob_hash.clone(),
            message: Some(format!("conflict resolution: chose revision {}", chosen.id)),
        };
        self.db.transaction(|tx| {
            store::insert_revision(tx, &revision)?;
            store::set_active_head(tx, &conflict.game_id, &revision.id)?;
            store::resolve_conflict(tx, conflict_id, &revision.id)
        })?;
        Ok(revision)
    }

    /// Two-way sync against an optional storage provider.
    pub fn sync(&mut self, provider: &dyn SyncProvider, game_id: &str) -> Result<SyncReport> {
        sync::sync_game(
            &mut self.db,
            &self.blobs,
            provider,
            game_id,
            SyncDirection::Both,
        )
    }

    pub fn push(&mut self, provider: &dyn SyncProvider, game_id: &str) -> Result<SyncReport> {
        sync::sync_game(
            &mut self.db,
            &self.blobs,
            provider,
            game_id,
            SyncDirection::Push,
        )
    }

    pub fn pull(&mut self, provider: &dyn SyncProvider, game_id: &str) -> Result<SyncReport> {
        sync::sync_game(
            &mut self.db,
            &self.blobs,
            provider,
            game_id,
            SyncDirection::Pull,
        )
    }

    fn single_directory_location(&self, game_id: &str) -> Result<Location> {
        let locations = store::list_locations(self.db.connection(), game_id)?;
        match locations.as_slice() {
            [] => Err(Error::InvalidState(format!(
                "game {game_id} has no save locations"
            ))),
            [location] if location.kind == LocationKind::Directory => Ok(location.clone()),
            [location] => Err(Error::InvalidState(format!(
                "location {} is not a directory; Phase 1 supports directory locations only",
                location.path
            ))),
            _ => Err(Error::InvalidState(format!(
                "game {game_id} has {} locations; Phase 1 supports exactly one",
                locations.len()
            ))),
        }
    }
}

fn load_or_create_device(db: &mut Db) -> Result<Device> {
    if let Some(id) = store::get_setting(db.connection(), DEVICE_ID_KEY)?
        && let Some(mut device) = store::get_device(db.connection(), &id)?
    {
        device.last_seen_at = store::now_iso();
        store::upsert_device(db.connection(), &device)?;
        return Ok(device);
    }

    let now = store::now_iso();
    let device = Device {
        id: store::new_id(),
        name: std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "SaveSync device".to_string()),
        platform: std::env::consts::OS.to_string(),
        created_at: now.clone(),
        last_seen_at: now,
    };
    db.transaction(|tx| {
        store::upsert_device(tx, &device)?;
        store::set_setting(tx, DEVICE_ID_KEY, &device.id)
    })?;
    Ok(device)
}
