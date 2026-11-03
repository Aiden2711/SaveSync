//! The kind-agnostic filesystem implementation of [`PlatformAdapter`].
//!
//! Phase 2 has one on-disk behaviour for every adapter kind: a save location is
//! a directory tree that is hashed with [`Manifest::from_dir`] and restored by
//! the journaled [`restore::restore_revision`] transaction. The kinds that will
//! diverge (JKSV exports, emulator memory cards) are already separate adapter
//! types so Phase 3 only has to replace a method body; the engine does not
//! change.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::manifest::Manifest;
use crate::models::AdapterType;
use crate::paths;
use crate::restore::{self, RestoreRequest};

use super::{LocationCandidate, PlatformAdapter, RestoreContext, RestoreResult};

/// Validate a stored mapping root and return its canonical form.
///
/// This is exactly [`paths::validate_root`], the check snapshots have always
/// applied. Discovery-time restrictions (no UNC, no symlinked final component)
/// stay at discovery, where untrusted third-party metadata is filtered.
pub(crate) fn validate_mapping(root: &Path) -> Result<PathBuf> {
    paths::validate_root(root)
}

/// Hash one location into a manifest.
///
/// `mapping_id` is unused here: directory trees look the same to every Phase 2
/// adapter. A platform adapter that needs the mapping (a JKSV timestamp folder,
/// an emulator memory-card slot) overrides this method.
pub(crate) fn snapshot_location(_mapping_id: &str, root: &Path) -> Result<Manifest> {
    Manifest::from_dir(root)
}

/// Restore one location through the shared journaled transaction.
///
/// `manifest` and `mapping_id` are unused here for the same reason as in
/// [`snapshot_location`]: the transaction re-reads the manifest it verified from
/// the database by digest, and `restore_revision` owns staging, the safety
/// backup, the journal, and rollback.
pub(crate) fn restore_location(
    context: RestoreContext<'_>,
    _manifest: &Manifest,
    _mapping_id: &str,
    target: &Path,
) -> Result<RestoreResult> {
    restore::restore_revision(
        context.db,
        context.blobs,
        context.paths,
        RestoreRequest {
            game_id: context.game_id.to_string(),
            revision_id: context.revision_id.to_string(),
            target_root: target.to_path_buf(),
            mode: context.mode,
        },
    )
}

/// A directory the user chose explicitly.
///
/// Such a location has no discovery source — enrollment is the only way it
/// enters SaveSync — so [`PlatformAdapter::discover`] reports nothing.
pub struct CustomDirectoryAdapter;

impl PlatformAdapter for CustomDirectoryAdapter {
    fn adapter_type(&self) -> AdapterType {
        AdapterType::CustomDirectory
    }

    fn discover(&self, _db: &mut crate::db::Db) -> Result<Vec<LocationCandidate>> {
        Ok(Vec::new())
    }

    fn validate_mapping(&self, root: &Path) -> Result<PathBuf> {
        validate_mapping(root)
    }

    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest> {
        snapshot_location(mapping_id, root)
    }

    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult> {
        restore_location(context, manifest, mapping_id, target)
    }
}

/// Emulator memory cards and save directories.
///
/// Phase 2 treats an emulator location as a plain directory tree. Phase 3 will
/// override the snapshot/restore methods here for card image layouts; no engine
/// change is required for that.
pub struct EmulatorAdapter;

impl PlatformAdapter for EmulatorAdapter {
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Emulator
    }

    fn discover(&self, _db: &mut crate::db::Db) -> Result<Vec<LocationCandidate>> {
        Ok(Vec::new())
    }

    fn validate_mapping(&self, root: &Path) -> Result<PathBuf> {
        validate_mapping(root)
    }

    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest> {
        snapshot_location(mapping_id, root)
    }

    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult> {
        restore_location(context, manifest, mapping_id, target)
    }
}

/// A JKSV export folder copied off a Nintendo Switch.
///
/// JKSV writes `/JKSV/<Game>/<Timestamp>/`, so an enrolled location names one
/// export rather than a game's live save area. Phase 2 snapshots that export
/// folder as an ordinary tree; Phase 3 will decide how a restore maps back onto
/// the export layout, by override here only.
pub struct JksvAdapter;

impl PlatformAdapter for JksvAdapter {
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Jksv
    }

    fn discover(&self, _db: &mut crate::db::Db) -> Result<Vec<LocationCandidate>> {
        Ok(Vec::new())
    }

    fn validate_mapping(&self, root: &Path) -> Result<PathBuf> {
        validate_mapping(root)
    }

    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest> {
        snapshot_location(mapping_id, root)
    }

    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult> {
        restore_location(context, manifest, mapping_id, target)
    }
}
