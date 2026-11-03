//! High-level engine facade used by the Tauri command layer.
//!
//! The engine owns the database, the blob store, and the on-disk safety
//! directories. All public methods enforce the project's safety invariants:
//! enrollment is explicit, snapshots are verified, and restores are journaled.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::adapters::{self, RestoreContext};
use crate::artwork::{GridCandidate, SteamGridDbClient};
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
use crate::restore::{self, DataPaths, RestoreMode};
use crate::store;
use crate::sync::{self, SyncDirection, SyncReport};

/// Settings key holding the stable device UUID.
const DEVICE_ID_KEY: &str = "device_id";

/// Image formats a cached cover may use, in the order they are tried when
/// pruning a stale file left by a previous fetch.
const COVER_EXTENSIONS: [&str; 3] = ["png", "jpg", "webp"];

/// Error message used when the manually chosen grid is no longer offered by
/// SteamGridDB.
///
/// Exposed so the command layer can map exactly this failure onto a "that cover
/// is gone, pick another" message instead of a generic provider error. It is a
/// plain string because [`Error::NotFound`] carries one; no grid id, URL, or
/// title is included.
pub const GRID_UNAVAILABLE_MESSAGE: &str = "the selected cover is no longer available";

/// A SteamGridDB match together with the cover candidates offered for it.
///
/// `candidates` is ranked best first and may be empty when the matched game has
/// no usable grids. `sgdb_game_id` is carried only so a caller can tell which
/// game the list belongs to; a manual pick is matched by
/// [`GridCandidate::id`], never by position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverSearch {
    pub sgdb_game_id: u64,
    pub candidates: Vec<GridCandidate>,
}

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
    /// Layout: `savesync.db`, `blobs/`, `staging/`, `rollback/`, `covers/`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let paths = DataPaths::new(root.as_ref().to_path_buf());
        for directory in [
            &paths.root,
            &paths.staging(),
            &paths.rollback(),
            &paths.covers(),
        ] {
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

    /// Remove a non-secret setting, restoring the key's absent state. Used to
    /// undo a partially applied write; deleting an absent key is a no-op.
    pub fn delete_setting(&mut self, key: &str) -> Result<()> {
        self.db.transaction(|tx| store::delete_setting(tx, key))
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

    /// Directory where cached cover images live.
    pub fn covers_dir(&self) -> PathBuf {
        self.paths.covers()
    }

    /// Resolve a game's cached cover to an absolute path on disk.
    ///
    /// Returns `Ok(None)` when the game has no cover recorded. Fails if the
    /// recorded path is unsafe or does not point at a regular file.
    pub fn cover_file(&self, game_id: &str) -> Result<Option<PathBuf>> {
        let game = store::require_game(self.db.connection(), game_id)?;
        let Some(recorded) = game.cover_path else {
            return Ok(None);
        };

        // The database value is untrusted: it may have been written by an
        // older build or edited directly. It may only ever name a file below
        // the cover cache, and only through a plain relative path.
        if !recorded.starts_with("covers/") {
            return Err(Error::InvalidState(
                "cover path is outside the cover cache".into(),
            ));
        }
        let resolved = crate::paths::safe_join(&self.paths.root, &recorded)?;
        if !resolved.starts_with(self.paths.covers()) {
            return Err(Error::InvalidState(
                "cover path is outside the cover cache".into(),
            ));
        }

        match fs::symlink_metadata(&resolved) {
            // `symlink_metadata` does not follow links, so a symlink or a
            // directory is rejected instead of being served as an image.
            Ok(metadata) if metadata.file_type().is_file() => Ok(Some(resolved)),
            Ok(_) => Err(Error::InvalidState(format!(
                "cover path is not a regular file: {}",
                resolved.display()
            ))),
            // A missing cache file is not an error: the UI falls back to
            // generated artwork.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::io(&resolved, error)),
        }
    }

    /// Fetch, validate and cache cover art for a game, then record it.
    ///
    /// The automatic pick is the head of [`Self::search_cover_candidates`], so
    /// it can never rank candidates differently from the manual picker, and
    /// both paths share the same download-and-record step.
    ///
    /// On any failure the previously recorded cover is left untouched: the
    /// database value only changes after a complete, verified image is on
    /// disk.
    pub fn fetch_cover_art(
        &mut self,
        client: &SteamGridDbClient,
        api_key: &str,
        game_id: &str,
    ) -> Result<Game> {
        let candidate = self
            .search_cover_candidates(client, api_key, game_id)?
            .ok_or_else(|| Error::NotFound("no SteamGridDB match for this title".into()))?
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound("no cover art available for this game".into()))?;

        self.cache_cover(client, api_key, game_id, &candidate)
    }

    /// Read the SteamGridDB search term for a game: its recorded title.
    ///
    /// This is the only part of a cover search that needs engine state, so it
    /// is deliberately split out from [`Self::search_cover_candidates`]: a
    /// caller that does not already own the engine lock can take it for this
    /// brief database read, release it, and then run the two API requests
    /// through [`cover_candidates_for_title`] without blocking every other
    /// engine command behind a 60-second network timeout.
    ///
    /// No network request, cache write, or download happens here. Returns
    /// `Ok(None)` when the game is unknown, or when its stored title is blank:
    /// an empty autocomplete query can never match a game, so it is never sent
    /// upstream.
    pub fn cover_search_term(&mut self, game_id: &str) -> Result<Option<String>> {
        // Parity with the download path: an id that could never name a cover
        // file is rejected before it is used as a lookup key.
        cover_stem(game_id)?;

        Ok(store::get_game(self.db.connection(), game_id)?
            .map(|game| game.title.trim().to_string())
            .filter(|title| !title.is_empty()))
    }

    /// List the cover candidates SteamGridDB offers for an enrolled game.
    ///
    /// Returns `Ok(None)` when the game is unknown, when its title is blank, or
    /// when the title has no SteamGridDB match at all; `Ok(Some(search))`
    /// otherwise, whose `candidates` may still be empty when the matched game
    /// has no usable grids.
    ///
    /// This performs the same two API calls as [`Self::fetch_cover_art`] and
    /// downloads nothing: no image is fetched, no cache file is written, and
    /// the recorded cover is untouched.
    ///
    /// Callers that do not already hold the engine lock should use
    /// [`Self::cover_search_term`] plus [`cover_candidates_for_title`] instead,
    /// so the network I/O runs while the lock is released.
    pub fn search_cover_candidates(
        &mut self,
        client: &SteamGridDbClient,
        api_key: &str,
        game_id: &str,
    ) -> Result<Option<CoverSearch>> {
        let Some(title) = self.cover_search_term(game_id)? else {
            return Ok(None);
        };
        cover_candidates_for_title(client, api_key, &title)
    }

    /// Download and record one specific cover candidate chosen by the user.
    ///
    /// `grid_id` is used *only* to look the candidate up in a freshly fetched
    /// list: the download URL always comes from SteamGridDB, never from the
    /// caller, so a stale, guessed, or hostile id cannot redirect the request
    /// or reach a file name.
    ///
    /// Fails with [`Error::NotFound`] carrying [`GRID_UNAVAILABLE_MESSAGE`] when
    /// the id is no longer offered. As with [`Self::fetch_cover_art`], any
    /// failure leaves the previously recorded cover in place.
    pub fn set_cover_grid(
        &mut self,
        client: &SteamGridDbClient,
        api_key: &str,
        game_id: &str,
        grid_id: u64,
    ) -> Result<Game> {
        cover_stem(game_id)?;

        let game = store::require_game(self.db.connection(), game_id)?;

        let sgdb_id = client
            .search_game(api_key, &game.title)?
            .ok_or_else(|| Error::NotFound("no SteamGridDB match for this title".into()))?;
        let candidates = client.list_grids(api_key, sgdb_id)?;

        // Match by id, not by rank: an explicit choice overrides the automatic
        // one, even when the user picks a worse-ranked grid on purpose.
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.id == grid_id)
            .ok_or_else(|| Error::NotFound(GRID_UNAVAILABLE_MESSAGE.to_string()))?;

        self.cache_cover(client, api_key, game_id, candidate)
    }

    /// Clear a game's cover: clear the recorded path, then best-effort delete
    /// the cached file.
    ///
    /// The database is authoritative, so the record is cleared first. An
    /// orphaned cache file is harmless and never inside a save directory.
    pub fn clear_cover_art(&mut self, game_id: &str) -> Result<Game> {
        // Resolve before clearing; a path that no longer resolves means there
        // is nothing to delete.
        let cached = self.cover_file(game_id).ok().flatten();
        let updated = self.db.transaction(|tx| {
            store::set_cover_path(tx, game_id, None)?;
            store::require_game(tx, game_id)
        })?;
        if let Some(path) = cached {
            let _ = fs::remove_file(path);
        }
        Ok(updated)
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
        Ok(adapters::discover_all(&mut self.db))
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
        // The enrolled location is the mapping, so the adapter that owns the
        // game's kind both validates its root and captures it.
        let adapter = adapters::adapter_for(game.adapter_type);
        let root = adapter.validate_mapping(Path::new(&location.path))?;
        let manifest = adapter.snapshot_location(&location.id, &root)?;
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
        let location = locations.iter().find(|location| {
            Path::new(&location.path) == target_root
                || Path::new(&location.path).to_string_lossy() == canonical_target
        });
        let Some(location) = location else {
            return Err(Error::InvalidState(format!(
                "restore target {} is not an enrolled location of game {}",
                target_root.display(),
                revision.game_id
            )));
        };

        // The adapter loads nothing new: the game and the manifest are read
        // through the same store helpers the direct transaction used, and the
        // adapter hands them to `restore::restore_revision`, which still owns
        // staging, the safety backup, the journal, and rollback.
        let game = store::require_game(self.db.connection(), &revision.game_id)?;
        let manifest = store::load_manifest(self.db.connection(), &revision.manifest_hash)?;
        adapters::adapter_for(game.adapter_type).restore_location(
            RestoreContext {
                db: &mut self.db,
                blobs: &self.blobs,
                paths: &self.paths,
                game_id: &revision.game_id,
                revision_id: &revision.id,
                mode,
            },
            &manifest,
            &location.id,
            &target_root,
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

    /// Download `candidate` into the cover cache, record it, and prune stale
    /// formats.
    ///
    /// The single write path for cover art: the automatic pick and the manual
    /// pick both end here, so their atomicity, cleanup, and error behavior
    /// cannot drift apart.
    ///
    /// The database value only changes after a complete, signature-checked
    /// image is on disk, so a failure at any point — network, size, magic-byte
    /// sniff, rename, or the transaction itself — leaves the previously
    /// recorded cover exactly as it was.
    fn cache_cover(
        &mut self,
        client: &SteamGridDbClient,
        api_key: &str,
        game_id: &str,
        candidate: &GridCandidate,
    ) -> Result<Game> {
        // Re-validate here too: this is the only place that turns a game id
        // into a path component, and it must stay safe regardless of caller.
        let stem = cover_stem(game_id)?;

        // The final name depends on the image's own signature, which is only
        // known after the download, so the completed file lands on a staging
        // name inside the cache first and is then moved to `covers/<id>.<ext>`.
        let staging = self.paths.covers().join(format!("{stem}.download"));

        // Validates the CDN URL host and scheme, caps the size, sniffs the
        // magic bytes, and writes through `.part` + rename. Nothing here
        // re-implements any of that.
        let downloaded = client.download_grid(api_key, candidate, &staging)?;
        let extension = downloaded.extension;

        let dest = self.paths.covers().join(format!("{stem}.{extension}"));
        if let Err(error) = crate::artwork::rename_over(&staging, &dest) {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }

        let relative = format!("covers/{stem}.{extension}");
        let updated = self.db.transaction(|tx| {
            store::set_cover_path(tx, game_id, Some(&relative))?;
            store::require_game(tx, game_id)
        })?;

        // A switch may have produced a different format; drop the stale
        // variant so the cache holds exactly one file per game. A failed
        // deletion is harmless: an orphaned cache file is never read and is
        // never inside a save location.
        for other in COVER_EXTENSIONS {
            if other != extension {
                let _ = fs::remove_file(self.paths.covers().join(format!("{stem}.{other}")));
            }
        }

        Ok(updated)
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

/// Resolve a title into the ranked cover candidates SteamGridDB offers for it.
///
/// Pure network composition over a [`SteamGridDbClient`]: it reads no engine
/// state, downloads nothing, and writes nothing, so a caller must not hold the
/// engine lock across it. That is what lets the command layer keep the lock for
/// the local title lookup alone ([`SaveSyncEngine::cover_search_term`]) and run
/// these two requests — each with a timeout and a bounded retry policy — with
/// the lock released.
///
/// Returns `Ok(None)` when the title has no match at all.
pub fn cover_candidates_for_title(
    client: &SteamGridDbClient,
    api_key: &str,
    title: &str,
) -> Result<Option<CoverSearch>> {
    let Some(sgdb_game_id) = client.search_game(api_key, title)? else {
        return Ok(None);
    };
    let candidates = client.list_grids(api_key, sgdb_game_id)?;
    Ok(Some(CoverSearch {
        sgdb_game_id,
        candidates,
    }))
}

/// Validate that a game id can safely be used as a file name.
///
/// Only ASCII alphanumerics and `-` are accepted: no separators, no dots, and
/// no drive prefixes, so no database value can become a traversal component.
fn cover_stem(game_id: &str) -> Result<&str> {
    let safe = !game_id.is_empty()
        && game_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !safe {
        return Err(Error::InvalidState(
            "unsafe game id for a cover file name".into(),
        ));
    }
    Ok(game_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use crate::http::{HttpRequest, HttpResponse, HttpTransport};

    /// Stand-in credential: these tests assert it never reaches the CDN and
    /// never appears in an error.
    const API_KEY: &str = "sgdb-test-key-never-echoed";

    /// Real PNG signature; the download path validates magic bytes, not pixels.
    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    fn png_payload(tag: u8) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.push(tag);
        bytes
    }

    fn jpeg_payload(tag: u8) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.push(tag);
        bytes
    }

    /// Zero-network transport. Mirrors `ReqwestTransport` by streaming the body
    /// to `download_to` instead of buffering it in the response.
    struct ScriptedTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Network("no scripted response".to_string()))?;

            if let Some(path) = &request.download_to {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(path, &response.body).unwrap();
            }

            Ok(response)
        }
    }

    fn json_response(status: u16, value: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn image_response(bytes: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "image/png".to_string())],
            body: bytes.to_vec(),
        }
    }

    fn autocomplete_hit() -> HttpResponse {
        json_response(
            200,
            json!({"success": true, "data": [{"id": 7, "name": "Hades"}]}),
        )
    }

    /// Two candidates, ranked best first: id 10 (2:3) then id 20 (wide). The
    /// ids are what a manual pick is matched against.
    fn two_grids() -> HttpResponse {
        json_response(
            200,
            json!({
                "success": true,
                "data": [{
                    "id": 10,
                    "url": "https://cdn2.steamgriddb.com/grid/best.png",
                    "width": 600,
                    "height": 900,
                    "mime": "image/png"
                }, {
                    "id": 20,
                    "url": "https://cdn2.steamgriddb.com/grid/wide.png",
                    "width": 1920,
                    "height": 1080,
                    "mime": "image/png"
                }]
            }),
        )
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        engine: SaveSyncEngine,
        game: Game,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
            let game = engine
                .enroll_game("Hades", AdapterType::CustomDirectory)
                .unwrap();
            Self {
                _temp: temp,
                engine,
                game,
            }
        }

        fn client(responses: Vec<HttpResponse>) -> SteamGridDbClient {
            SteamGridDbClient::new(Arc::new(ScriptedTransport::new(responses)))
        }

        fn cached_entries(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.engine.covers_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    /// The split that keeps network I/O off the engine lock: the term lookup is
    /// a pure database read of the enrolled title.
    #[test]
    fn cover_search_term_reads_the_enrolled_title() {
        let mut fixture = Fixture::new();

        assert_eq!(
            fixture
                .engine
                .cover_search_term(&fixture.game.id)
                .unwrap()
                .as_deref(),
            Some("Hades")
        );
    }

    /// An unknown game is "no search term", not an error: the command layer
    /// still answers the picker with `Ok(None)` and never contacts the network.
    #[test]
    fn cover_search_term_is_none_for_an_unknown_game() {
        let mut fixture = Fixture::new();

        assert!(
            fixture
                .engine
                .cover_search_term("not-an-enrolled-game")
                .unwrap()
                .is_none()
        );
    }

    /// The schema allows a title that was written outside `create_game` (an
    /// older build, or a hand-edited database), so a blank one must be treated
    /// as "nothing to search for" rather than sent upstream as an empty query.
    #[test]
    fn cover_search_term_is_none_for_a_blank_title() {
        let mut fixture = Fixture::new();
        let blank = "   ".to_string();
        fixture
            .engine
            .db
            .connection()
            .execute(
                "UPDATE games SET title = ?1 WHERE id = ?2",
                (&blank, &fixture.game.id),
            )
            .unwrap();

        assert!(
            fixture
                .engine
                .cover_search_term(&fixture.game.id)
                .unwrap()
                .is_none()
        );
    }

    /// Parity with the download path: an id that could never name a cover file
    /// is still rejected before it becomes a lookup key.
    #[test]
    fn cover_search_term_rejects_an_id_that_cannot_name_a_cover_file() {
        let mut fixture = Fixture::new();

        let error = fixture.engine.cover_search_term("../escape").unwrap_err();

        assert!(
            matches!(error, Error::InvalidState(_)),
            "expected an invalid-state error, got {error:?}"
        );
    }

    /// The whole search is `term` + two requests. An unknown game must stop
    /// after the term lookup: the empty scripted transport fails on any request,
    /// so `Ok(None)` proves nothing was sent.
    #[test]
    fn search_cover_candidates_never_contacts_the_network_for_an_unknown_game() {
        let mut fixture = Fixture::new();
        let client = Fixture::client(Vec::new());

        assert!(
            fixture
                .engine
                .search_cover_candidates(&client, API_KEY, "not-an-enrolled-game")
                .unwrap()
                .is_none()
        );
    }

    /// An unknown title is "no match", not a failure: the picker shows its
    /// empty state instead of an error banner.
    #[test]
    fn search_cover_candidates_returns_none_when_the_game_is_unknown() {
        let mut fixture = Fixture::new();
        let client = Fixture::client(vec![json_response(
            200,
            json!({"success": true, "data": []}),
        )]);

        let search = fixture
            .engine
            .search_cover_candidates(&client, API_KEY, &fixture.game.id)
            .unwrap();

        assert!(search.is_none());
        assert!(
            fixture.cached_entries().is_empty(),
            "a search must write nothing"
        );
    }

    /// This is the picker's data source: the matched game id plus the ranked
    /// candidates, and nothing downloaded.
    #[test]
    fn search_cover_candidates_lists_ranked_candidates_without_downloading() {
        let mut fixture = Fixture::new();
        let client = Fixture::client(vec![autocomplete_hit(), two_grids()]);

        let search = fixture
            .engine
            .search_cover_candidates(&client, API_KEY, &fixture.game.id)
            .unwrap()
            .expect("Hades matches");

        assert_eq!(search.sgdb_game_id, 7);
        let ids: Vec<u64> = search
            .candidates
            .iter()
            .map(|candidate| candidate.id)
            .collect();
        assert_eq!(ids, vec![10, 20]);
        assert!(
            fixture.cached_entries().is_empty(),
            "a search must write nothing"
        );
        assert_eq!(
            fixture
                .engine
                .get_game(&fixture.game.id)
                .unwrap()
                .cover_path,
            None
        );
    }

    /// A manual pick overrides the ranking: the *requested* grid is downloaded,
    /// which the distinct payload proves, and it is recorded.
    #[test]
    fn set_cover_grid_downloads_the_requested_grid_not_the_best_ranked() {
        let mut fixture = Fixture::new();
        let best = png_payload(1);
        let chosen = png_payload(2);
        let client = Fixture::client(vec![
            autocomplete_hit(),
            two_grids(),
            image_response(&chosen),
        ]);

        let updated = fixture
            .engine
            .set_cover_grid(&client, API_KEY, &fixture.game.id, 20)
            .unwrap();

        let name = format!("{}.png", fixture.game.id);
        assert_eq!(
            updated.cover_path.as_deref(),
            Some(format!("covers/{name}").as_str())
        );
        assert_eq!(
            fs::read(fixture.engine.covers_dir().join(&name)).unwrap(),
            chosen
        );
        assert_ne!(
            fs::read(fixture.engine.covers_dir().join(&name)).unwrap(),
            best
        );
        assert_eq!(fixture.cached_entries(), vec![name]);
    }

    /// Switching formats must not leave the previous extension behind: the
    /// cache holds exactly one file per game, and the record follows the bytes.
    #[test]
    fn set_cover_grid_prunes_the_previous_extension() {
        let mut fixture = Fixture::new();
        let client = Fixture::client(vec![
            autocomplete_hit(),
            two_grids(),
            image_response(&png_payload(3)),
            autocomplete_hit(),
            two_grids(),
            image_response(&jpeg_payload(4)),
        ]);

        fixture
            .engine
            .set_cover_grid(&client, API_KEY, &fixture.game.id, 10)
            .unwrap();
        let updated = fixture
            .engine
            .set_cover_grid(&client, API_KEY, &fixture.game.id, 20)
            .unwrap();

        let name = format!("{}.jpg", fixture.game.id);
        assert_eq!(
            updated.cover_path.as_deref(),
            Some(format!("covers/{name}").as_str())
        );
        assert_eq!(fixture.cached_entries(), vec![name]);
        assert!(
            fs::read(
                fixture
                    .engine
                    .covers_dir()
                    .join(format!("{}.png", fixture.game.id))
            )
            .is_err()
        );
    }

    /// A grid id that upstream no longer offers is a distinguishable failure and
    /// changes nothing on disk or in the database.
    #[test]
    fn set_cover_grid_reports_a_grid_that_is_gone() {
        let mut fixture = Fixture::new();
        let client = Fixture::client(vec![autocomplete_hit(), two_grids()]);

        let error = fixture
            .engine
            .set_cover_grid(&client, API_KEY, &fixture.game.id, 999)
            .unwrap_err();

        assert!(
            matches!(&error, Error::NotFound(message) if message.as_str() == GRID_UNAVAILABLE_MESSAGE),
            "expected the grid-gone error, got {error:?}"
        );
        assert!(!format!("{error}").contains(API_KEY));
        assert!(fixture.cached_entries().is_empty());
        assert_eq!(
            fixture
                .engine
                .get_game(&fixture.game.id)
                .unwrap()
                .cover_path,
            None
        );
    }

    /// Settings must return to their absent state after a delete, because the
    /// desktop rollback relies on it to undo a first-ever configuration.
    #[test]
    fn settings_round_trip_and_delete_restores_the_absent_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
        let key = "webdav_endpoint";

        assert_eq!(engine.setting(key).unwrap(), None);
        engine
            .set_setting(key, "https://dav.example.com/savesync")
            .unwrap();
        assert_eq!(
            engine.setting(key).unwrap().as_deref(),
            Some("https://dav.example.com/savesync")
        );

        engine.delete_setting(key).unwrap();
        assert_eq!(engine.setting(key).unwrap(), None);
        // Deleting an absent key is a no-op, not an error.
        engine.delete_setting(key).unwrap();
    }
}
