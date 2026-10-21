//! Two-phase, journaled restore transactions.
//!
//! Every restore performs the same ordered steps:
//!
//! 1. extract the snapshot into a staging directory and verify every digest;
//! 2. capture and copy the current live tree into a safety backup, then verify
//!    the backup;
//! 3. persist a `prepared` journal row *before* touching live files;
//! 4. install files one by one with per-file atomic renames;
//! 5. verify the live tree, then mark the journal `applied` and move the game's
//!    active head.
//!
//! Any failure in step 4 or 5 triggers an automatic rollback from the verified
//! backup. A crash leaves a `prepared` journal row which
//! [`recover_in_flight_restores`] rolls back on next start.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::blob::BlobStore;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::manifest::{FileEntry, Manifest};
use crate::models::{RestoreJournalEntry, RestoreReport, RestoreState, Revision};
use crate::paths::{is_internal_artifact, sanitize_relative_path};
use crate::store;

/// How a restore treats files that exist on disk but not in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    /// Write snapshot files and leave unrelated live files in place.
    Overlay,
    /// Make the live directory exactly match the snapshot, removing extras.
    /// Removed files are still preserved in the safety backup.
    ExactMirror,
}

/// Input required to restore one revision into one location.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub game_id: String,
    pub revision_id: String,
    pub target_root: PathBuf,
    pub mode: RestoreMode,
}

/// Directories owned by the engine.
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub root: PathBuf,
}

impl DataPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("savesync.db")
    }

    pub fn blobs(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn rollback(&self) -> PathBuf {
        self.root.join("rollback")
    }

    pub fn staging_for(&self, restore_id: &str) -> PathBuf {
        self.staging().join(restore_id)
    }

    pub fn rollback_for(&self, game_id: &str, restore_id: &str) -> PathBuf {
        self.rollback().join(game_id).join(restore_id)
    }
}

/// Execute a restore transaction.
pub fn restore_revision(
    db: &mut Db,
    blobs: &BlobStore,
    paths: &DataPaths,
    request: RestoreRequest,
) -> Result<RestoreReport> {
    if store::list_in_flight_restores(db.connection())?
        .iter()
        .any(|entry| entry.game_id == request.game_id)
    {
        return Err(Error::RestoreInProgress(request.game_id));
    }

    let revision = store::require_revision(db.connection(), &request.revision_id)?;
    if revision.game_id != request.game_id {
        return Err(Error::InvalidState(format!(
            "revision {} does not belong to game {}",
            revision.id, request.game_id
        )));
    }
    let manifest = store::load_manifest(db.connection(), &revision.manifest_hash)?;
    let game = store::require_game(db.connection(), &request.game_id)?;
    if !game.enrolled {
        return Err(Error::InvalidState(format!(
            "game {} is not enrolled; refusing to modify its files",
            game.title
        )));
    }

    let target_root = normalize_target(&request.target_root)?;
    let restore_id = store::new_id();
    let staging = paths.staging_for(&restore_id);
    let backup = paths.rollback_for(&request.game_id, &restore_id);

    // Phase 1: validate the live tree and capture its manifest before any work.
    let backup_manifest = if target_root.exists() {
        Manifest::from_dir(&target_root)?
    } else {
        Manifest::empty()
    };

    // Phase 2: stage and verify the snapshot before anything live is touched.
    if let Err(error) = blobs.extract_verified(&manifest, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    if !backup_manifest.entries.is_empty() {
        if let Err(error) = copy_manifest_files(&target_root, &backup, &backup_manifest)
            .and_then(|()| backup_manifest.verify_dir(&backup))
        {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_dir_all(&backup);
            return Err(error);
        }
    } else {
        fs::create_dir_all(&backup).map_err(|e| Error::io(&backup, e))?;
    }
    let backup_manifest_json = backup_manifest.to_json()?;

    // Phase 3: durably record the intent before modifying live files.
    let now = store::now_iso();
    let journal = RestoreJournalEntry {
        id: restore_id.clone(),
        game_id: request.game_id.clone(),
        revision_id: revision.id.clone(),
        target_root: target_root.to_string_lossy().into_owned(),
        staging_path: staging.to_string_lossy().into_owned(),
        backup_path: backup.to_string_lossy().into_owned(),
        backup_manifest: backup_manifest_json,
        state: RestoreState::Prepared,
        deleted_paths: Vec::new(),
        error: None,
        created_at: now.clone(),
        updated_at: now,
    };
    if let Err(error) = db.transaction(|tx| store::insert_journal(tx, &journal)) {
        let _ = fs::remove_dir_all(&staging);
        if let Error::Database(rusqlite::Error::SqliteFailure(_, ref message)) = error {
            // Partial unique index violation: another restore is in flight.
            if message.as_deref().unwrap_or_default().contains("UNIQUE") {
                return Err(Error::RestoreInProgress(request.game_id.clone()));
            }
        }
        return Err(error);
    }

    // Phase 4: apply.
    let rollback_context = RollbackContext {
        staging: &staging,
        target_root: &target_root,
        backup: &backup,
        backup_manifest: &backup_manifest,
        restore_id: &restore_id,
    };
    let deleted = match apply_manifest(&staging, &target_root, &manifest, request.mode) {
        Ok(deleted) => deleted,
        Err(error) => {
            return Err(rollback_after_failure(
                db,
                &rollback_context,
                &deleted_so_far(&manifest, &backup_manifest),
                error,
            ));
        }
    };

    // Phase 5: verify the result and commit metadata.
    let verification = match request.mode {
        RestoreMode::ExactMirror => manifest.verify_dir(&target_root),
        RestoreMode::Overlay => verify_present_files(&target_root, &manifest),
    };
    if let Err(error) = verification {
        return Err(rollback_after_failure(
            db,
            &rollback_context,
            &deleted,
            error,
        ));
    }

    let commit = db.transaction(|tx| {
        store::update_journal(tx, &restore_id, RestoreState::Applied, &deleted, None)?;
        store::set_active_head(tx, &request.game_id, &revision.id)?;
        Ok(())
    });
    if let Err(error) = commit {
        return Err(rollback_after_failure(
            db,
            &rollback_context,
            &deleted,
            error,
        ));
    }

    let _ = fs::remove_dir_all(&staging);

    Ok(RestoreReport {
        restore_id,
        revision_id: revision.id,
        files_restored: manifest.entries.len(),
        bytes_restored: manifest.total_size(),
        safety_backup_manifest_hash: backup_manifest.hash(),
        safety_backup_path: backup.to_string_lossy().into_owned(),
    })
}

/// Recover restores that were interrupted (crash, power loss) after their
/// journal row was written. Every `prepared` row is rolled back from its
/// verified safety backup; returns the number of recovered restores.
pub fn recover_in_flight_restores(db: &mut Db) -> Result<usize> {
    let pending = store::list_in_flight_restores(db.connection())?;
    let mut recovered = 0;
    for entry in pending {
        let backup = PathBuf::from(&entry.backup_path);
        let target = PathBuf::from(&entry.target_root);

        let backup_manifest = match Manifest::from_json(&entry.backup_manifest) {
            Ok(manifest) => manifest,
            Err(error) => {
                mark_recovered(
                    db,
                    &entry.id,
                    RestoreState::RollbackFailed,
                    &format!("safety backup manifest is unreadable: {error}"),
                )?;
                continue;
            }
        };

        let outcome = if backup.exists() {
            rollback_from_backup(&target, &backup, &backup_manifest)
        } else {
            Err(Error::NotFound(format!(
                "safety backup missing: {}",
                backup.display()
            )))
        };

        let _ = fs::remove_dir_all(&entry.staging_path);
        match outcome {
            Ok(()) => {
                mark_recovered(
                    db,
                    &entry.id,
                    RestoreState::RolledBack,
                    "recovered after interrupted restore",
                )?;
                recovered += 1;
            }
            Err(error) => {
                mark_recovered(
                    db,
                    &entry.id,
                    RestoreState::RollbackFailed,
                    &error.to_string(),
                )?;
            }
        }
    }
    Ok(recovered)
}

fn mark_recovered(db: &mut Db, entry_id: &str, state: RestoreState, message: &str) -> Result<()> {
    db.transaction(|tx| store::update_journal(tx, entry_id, state, &[], Some(message)))
}

/// Filesystem context needed to undo a failed restore.
struct RollbackContext<'a> {
    staging: &'a Path,
    target_root: &'a Path,
    backup: &'a Path,
    backup_manifest: &'a Manifest,
    restore_id: &'a str,
}

fn rollback_after_failure(
    db: &mut Db,
    context: &RollbackContext<'_>,
    added_paths: &[String],
    original: Error,
) -> Error {
    let _ = fs::remove_dir_all(context.staging);
    let rollback =
        rollback_from_backup(context.target_root, context.backup, context.backup_manifest);
    let (state, message) = match &rollback {
        Ok(()) => (
            RestoreState::RolledBack,
            "pre-restore state restored".to_string(),
        ),
        Err(error) => (
            RestoreState::RollbackFailed,
            format!(
                "automatic rollback failed ({error}); backup retained at {}",
                context.backup.display()
            ),
        ),
    };
    let _ = db.transaction(|tx| {
        store::update_journal(tx, context.restore_id, state, added_paths, Some(&message))
    });

    Error::RestoreFailed {
        message: original.to_string(),
        rollback: message,
    }
}

/// Install every file from `source_root` into `target_root`, verifying each
/// file's digest while copying. Returns paths deleted by `ExactMirror`.
fn apply_manifest(
    source_root: &Path,
    target_root: &Path,
    manifest: &Manifest,
    mode: RestoreMode,
) -> Result<Vec<String>> {
    fs::create_dir_all(target_root).map_err(|e| Error::io(target_root, e))?;

    for entry in &manifest.entries {
        let source = source_root.join(sanitize_relative_path(&entry.path)?);
        let target = target_root.join(sanitize_relative_path(&entry.path)?);
        install_file(&source, &target, entry)?;
    }

    let mut deleted = Vec::new();
    if mode == RestoreMode::ExactMirror {
        let live = Manifest::from_dir(target_root)?;
        for entry in &live.entries {
            if manifest.find(&entry.path).is_none() {
                let path = target_root.join(sanitize_relative_path(&entry.path)?);
                fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
                deleted.push(entry.path.clone());
            }
        }
        prune_empty_dirs(target_root)?;
    }
    Ok(deleted)
}

/// Copy one verified file into place with a same-directory temp file and an
/// atomic rename. Refuses to write through symlinks.
fn install_file(source: &Path, target: &Path, expected: &FileEntry) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::UnsafePath(format!("target has no parent: {}", target.display())))?;
    fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;

    if let Ok(metadata) = fs::symlink_metadata(target)
        && metadata.file_type().is_symlink()
    {
        return Err(Error::UnsafePath(format!(
            "refusing to replace symlink: {}",
            target.display()
        )));
    }

    let temp = parent.join(format!(".savesync-tmp-{}", Uuid::new_v4()));
    let result = copy_verified(source, &temp, expected);
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(target, error));
    }
    sync_dir(parent);
    Ok(())
}

/// Copy `source` to `destination`, hashing as we go, and fsync the result.
fn copy_verified(source: &Path, destination: &Path, expected: &FileEntry) -> Result<()> {
    let mut input = File::open(source).map_err(|e| Error::io(source, e))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| Error::io(destination, e))?;

    let mut hasher = Sha256::new();
    let mut copied: u64 = 0;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|e| Error::io(source, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|e| Error::io(destination, e))?;
        copied += read as u64;
    }
    output.flush().map_err(|e| Error::io(destination, e))?;
    output.sync_all().map_err(|e| Error::io(destination, e))?;

    let digest = hex(hasher.finalize());
    if copied != expected.size || !digest.eq_ignore_ascii_case(&expected.sha256) {
        let _ = fs::remove_file(destination);
        return Err(Error::IntegrityMismatch {
            path: expected.path.clone(),
            expected: expected.sha256.clone(),
            actual: digest,
        });
    }
    Ok(())
}

/// Copy every file listed in `manifest` from `source_root` into `destination`.
fn copy_manifest_files(source_root: &Path, destination: &Path, manifest: &Manifest) -> Result<()> {
    fs::create_dir_all(destination).map_err(|e| Error::io(destination, e))?;
    for entry in &manifest.entries {
        let source = source_root.join(sanitize_relative_path(&entry.path)?);
        let target = destination.join(sanitize_relative_path(&entry.path)?);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        copy_verified(&source, &target, entry)?;
    }
    Ok(())
}

/// Restore the live tree to a previously verified backup.
fn rollback_from_backup(
    target_root: &Path,
    backup: &Path,
    backup_manifest: &Manifest,
) -> Result<()> {
    fs::create_dir_all(target_root).map_err(|e| Error::io(target_root, e))?;
    for entry in &backup_manifest.entries {
        let source = backup.join(sanitize_relative_path(&entry.path)?);
        let target = target_root.join(sanitize_relative_path(&entry.path)?);
        install_file(&source, &target, entry)?;
    }

    if target_root.exists() {
        let live = Manifest::from_dir(target_root)?;
        for entry in &live.entries {
            if backup_manifest.find(&entry.path).is_none() {
                let path = target_root.join(sanitize_relative_path(&entry.path)?);
                fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
            }
        }
        prune_empty_dirs(target_root)?;
        backup_manifest.verify_dir(target_root)?;
    }
    Ok(())
}

/// Verify only the files the manifest names, tolerating unrelated extras.
fn verify_present_files(target_root: &Path, manifest: &Manifest) -> Result<()> {
    for entry in &manifest.entries {
        let path = target_root.join(sanitize_relative_path(&entry.path)?);
        if !path.is_file() {
            return Err(Error::MissingFile {
                path: entry.path.clone(),
            });
        }
        let actual = crate::hash::hash_file(&path)?;
        if !actual.eq_ignore_ascii_case(&entry.sha256) {
            return Err(Error::IntegrityMismatch {
                path: entry.path.clone(),
                expected: entry.sha256.clone(),
                actual,
            });
        }
    }
    Ok(())
}

/// Paths that exist in the new manifest but not in the pre-restore manifest;
/// used to report what an interrupted restore may have added.
fn deleted_so_far(manifest: &Manifest, backup: &Manifest) -> Vec<String> {
    manifest
        .entries
        .iter()
        .filter(|entry| backup.find(&entry.path).is_none())
        .map(|entry| entry.path.clone())
        .collect()
}

fn normalize_target(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::UnsafePath(format!(
            "restore target must be absolute: {}",
            path.display()
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::UnsafePath(format!(
            "restore target may not contain '..': {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn prune_empty_dirs(root: &Path) -> Result<()> {
    let mut directories: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir() && entry.path() != root)
        .map(|entry| entry.path().to_path_buf())
        .collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        if directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_internal_artifact)
        {
            continue;
        }
        if fs::read_dir(&directory)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&directory);
        }
    }
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(unix)]
fn sync_dir(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) {}

/// Load the revision and manifest that a journal entry points at. Exposed for
/// the recovery UI.
pub fn revision_for_journal(db: &Db, entry: &RestoreJournalEntry) -> Result<Revision> {
    store::require_revision(db.connection(), &entry.revision_id)
}
