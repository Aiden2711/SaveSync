//! Graph-aware synchronization.
//!
//! Sync moves immutable revision metadata and verified blobs between the local
//! database and a [`SyncProvider`]. It never edits live save files: applying a
//! revision is always a separate, explicit restore. Divergent histories are
//! recorded as conflicts and left for the user to resolve.

use std::collections::{HashMap, HashSet};
use std::fs;

use serde::{Deserialize, Serialize};

use crate::blob::BlobStore;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::models::{Conflict, Device, Revision};
use crate::provider::SyncProvider;
use crate::store;

/// Which side(s) of the sync to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    /// Pull remote metadata/blobs, then push local-only revisions.
    Both,
    /// Only upload local revisions and blobs.
    Push,
    /// Only download remote revisions and blobs.
    Pull,
}

/// Outcome of one synchronization pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub game_id: String,
    pub pulled_revisions: usize,
    pub pushed_revisions: usize,
    pub downloaded_blobs: usize,
    pub uploaded_blobs: usize,
    /// Number of revision DAG heads after the merge.
    pub heads: usize,
    /// Open conflicts detected or refreshed by this pass.
    pub conflict_ids: Vec<String>,
}

/// Synchronize one enrolled game against a provider.
///
/// The operation is ordered so that a failure leaves only additive state:
/// metadata and blobs may be added, but existing revisions, blobs, and save
/// files are never modified or deleted.
pub fn sync_game(
    db: &mut Db,
    blobs: &BlobStore,
    provider: &dyn SyncProvider,
    game_id: &str,
    direction: SyncDirection,
) -> Result<SyncReport> {
    let game = store::require_game(db.connection(), game_id)?;
    if !game.enrolled {
        return Err(Error::InvalidState(format!(
            "game {} is not enrolled; refusing to sync",
            game.title
        )));
    }

    let mut report = SyncReport {
        game_id: game_id.to_string(),
        ..SyncReport::default()
    };

    let remote_before = provider.list_revisions(game_id)?;
    let remote_ids: HashSet<String> = remote_before.iter().map(|r| r.id.clone()).collect();

    if matches!(direction, SyncDirection::Pull | SyncDirection::Both) {
        pull_metadata(db, provider, game_id, &remote_before, &mut report)?;
    }

    if matches!(direction, SyncDirection::Push | SyncDirection::Both) {
        push_metadata(db, blobs, provider, game_id, &remote_ids, &mut report)?;
    }

    if matches!(direction, SyncDirection::Pull | SyncDirection::Both) {
        report.downloaded_blobs = pull_blobs(db, blobs, provider, game_id)?;
    }

    let heads = store::list_heads(db.connection(), game_id)?;
    report.heads = heads.len();
    if heads.len() > 1 {
        report.conflict_ids = record_heads_conflict(db, game_id, &heads)?;
    }

    Ok(report)
}

fn pull_metadata(
    db: &mut Db,
    provider: &dyn SyncProvider,
    game_id: &str,
    remote: &[Revision],
    report: &mut SyncReport,
) -> Result<()> {
    let local_list = store::list_revisions(db.connection(), game_id)?;
    let local: HashMap<String, Revision> = local_list
        .iter()
        .cloned()
        .map(|revision| (revision.id.clone(), revision))
        .collect();

    // A provider must never change the meaning of an immutable revision.
    for remote_revision in remote {
        if let Some(local_revision) = local.get(&remote_revision.id)
            && (local_revision.manifest_hash != remote_revision.manifest_hash
                || local_revision.parent_ids != remote_revision.parent_ids)
        {
            return Err(Error::IntegrityMismatch {
                path: format!("remote revision {}", remote_revision.id),
                expected: local_revision.manifest_hash.clone(),
                actual: remote_revision.manifest_hash.clone(),
            });
        }
    }

    let new_remote: Vec<Revision> = remote
        .iter()
        .filter(|revision| !local.contains_key(&revision.id))
        .cloned()
        .collect();
    if new_remote.is_empty() {
        return Ok(());
    }

    // Fetch and verify every manifest before mutating the database.
    let mut manifests: HashMap<String, Manifest> = HashMap::new();
    for revision in &new_remote {
        if manifests.contains_key(&revision.manifest_hash) {
            continue;
        }
        let manifest = provider.get_manifest(&revision.manifest_hash)?;
        if manifest.hash() != revision.manifest_hash {
            return Err(Error::IntegrityMismatch {
                path: format!("remote manifest {}", revision.manifest_hash),
                expected: revision.manifest_hash.clone(),
                actual: manifest.hash(),
            });
        }
        manifests.insert(revision.manifest_hash.clone(), manifest);
    }

    let inserted = db.transaction(|tx| {
        // Remote devices may be unknown locally; create placeholder rows so the
        // revision foreign key remains meaningful.
        for revision in &new_remote {
            if store::get_device(tx, &revision.device_id)?.is_none() {
                let now = store::now_iso();
                store::upsert_device(
                    tx,
                    &Device {
                        id: revision.device_id.clone(),
                        name: format!(
                            "Remote device {}",
                            revision.device_id.chars().take(8).collect::<String>()
                        ),
                        platform: "unknown".into(),
                        created_at: now.clone(),
                        last_seen_at: now,
                    },
                )?;
            }
        }
        for manifest in manifests.values() {
            store::store_manifest(tx, manifest)?;
        }

        let mut known: HashSet<String> = local.keys().cloned().collect();
        let mut pending = new_remote.clone();
        let mut inserted = 0usize;
        while !pending.is_empty() {
            let mut deferred = Vec::new();
            let mut progressed = 0usize;
            for revision in pending {
                let parents_present = revision
                    .parent_ids
                    .iter()
                    .map(|parent| {
                        if known.contains(parent) {
                            Ok(true)
                        } else {
                            Ok(store::get_revision(tx, parent)?.is_some())
                        }
                    })
                    .collect::<Result<Vec<bool>>>()?
                    .into_iter()
                    .all(|present| present);
                if parents_present {
                    store::insert_revision(tx, &revision)?;
                    known.insert(revision.id.clone());
                    progressed += 1;
                    inserted += 1;
                } else {
                    deferred.push(revision);
                }
            }
            if progressed == 0 {
                return Err(Error::InvalidState(format!(
                    "remote history for game {game_id} has {} revision(s) with missing ancestors",
                    deferred.len()
                )));
            }
            pending = deferred;
        }
        Ok(inserted)
    })?;

    report.pulled_revisions = inserted;
    Ok(())
}

fn push_metadata(
    db: &Db,
    blobs: &BlobStore,
    provider: &dyn SyncProvider,
    game_id: &str,
    remote_ids: &HashSet<String>,
    report: &mut SyncReport,
) -> Result<()> {
    let mut local = store::list_revisions(db.connection(), game_id)?;
    local.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    for revision in local {
        if remote_ids.contains(&revision.id) {
            continue;
        }
        let blob_path = blobs.path_for(&revision.manifest_hash)?;
        if !blob_path.is_file() {
            return Err(Error::NotFound(format!(
                "local blob for revision {} is missing; snapshot it again before syncing",
                revision.id
            )));
        }
        if !provider.has_blob(&revision.manifest_hash)? {
            provider.put_blob(&revision.manifest_hash, &blob_path)?;
            report.uploaded_blobs += 1;
        }
        let manifest = store::load_manifest(db.connection(), &revision.manifest_hash)?;
        provider.put_revision(&revision, &manifest)?;
        report.pushed_revisions += 1;
    }
    Ok(())
}

fn pull_blobs(
    db: &Db,
    blobs: &BlobStore,
    provider: &dyn SyncProvider,
    game_id: &str,
) -> Result<usize> {
    let revisions = store::list_revisions(db.connection(), game_id)?;
    let mut downloaded = 0usize;
    let mut seen = HashSet::new();
    for revision in revisions {
        if !seen.insert(revision.manifest_hash.clone()) {
            continue;
        }
        if blobs.contains(&revision.manifest_hash) {
            continue;
        }
        let manifest = store::load_manifest(db.connection(), &revision.manifest_hash)?;
        let temp = blobs
            .root()
            .join("tmp")
            .join(format!("{}.download", revision.manifest_hash));
        fs::create_dir_all(temp.parent().expect("temp has parent"))
            .map_err(|e| Error::io(temp.parent().unwrap(), e))?;
        let result = provider
            .get_blob(&revision.manifest_hash, &temp)
            .and_then(|()| blobs.ingest(&manifest, &temp));
        let _ = fs::remove_file(&temp);
        result?;
        downloaded += 1;
    }
    Ok(downloaded)
}

fn record_heads_conflict(db: &mut Db, game_id: &str, heads: &[Revision]) -> Result<Vec<String>> {
    let mut ids: Vec<String> = heads.iter().map(|revision| revision.id.clone()).collect();
    ids.sort();

    let open = store::list_open_conflicts(db.connection(), game_id)?;
    for conflict in &open {
        let mut existing = conflict.revision_ids.clone();
        existing.sort();
        if existing == ids {
            return Ok(vec![conflict.id.clone()]);
        }
    }

    let conflict = Conflict {
        id: store::new_id(),
        game_id: game_id.to_string(),
        revision_ids: ids,
        detected_at: store::now_iso(),
        resolved_at: None,
        resolution_revision_id: None,
    };
    let conflict_id = conflict.id.clone();
    db.transaction(|tx| store::insert_conflict(tx, &conflict))?;
    Ok(vec![conflict_id])
}
