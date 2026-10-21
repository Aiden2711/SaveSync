//! Remote storage abstraction.
//!
//! Cloud sync is optional and must never be required for local operation. This
//! module defines the narrow provider interface the sync engine uses, plus a
//! filesystem-backed provider for offline testing and LAN/NAS sync. A Google
//! Drive implementation will target the same trait in Phase 2.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::models::Revision;

/// Storage operations required to move immutable revisions between devices.
///
/// Implementations must be idempotent: uploading an existing revision or blob
/// is a success. They must never mutate or delete existing objects.
pub trait SyncProvider {
    /// All revisions known for a game, in any order.
    fn list_revisions(&self, game_id: &str) -> Result<Vec<Revision>>;

    /// Fetch a manifest by its content hash.
    fn get_manifest(&self, manifest_hash: &str) -> Result<Manifest>;

    /// Store a revision node and its manifest. The blob must already have been
    /// uploaded via [`SyncProvider::put_blob`].
    fn put_revision(&self, revision: &Revision, manifest: &Manifest) -> Result<()>;

    fn has_blob(&self, manifest_hash: &str) -> Result<bool>;

    /// Copy a verified blob from local storage into the provider.
    fn put_blob(&self, manifest_hash: &str, source: &Path) -> Result<()>;

    /// Copy a blob from the provider to `destination` (a temp path chosen by
    /// the caller). The caller verifies the contents before making it visible.
    fn get_blob(&self, manifest_hash: &str, destination: &Path) -> Result<()>;
}

/// A provider that stores everything in a local directory.
///
/// Layout:
/// ```text
/// <root>/blobs/<aa>/<sha256>.tar.zst
/// <root>/manifests/<sha256>.json
/// <root>/games/<game-id>/revisions/<revision-id>.json
/// ```
///
/// This is useful for tests, USB drives, and NAS folders. It performs the same
/// immutable-write and digest-verification rules a cloud provider must follow.
#[derive(Debug, Clone)]
pub struct LocalFolderProvider {
    root: PathBuf,
}

impl LocalFolderProvider {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| Error::io(&root, e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn revision_dir(&self, game_id: &str) -> Result<PathBuf> {
        validate_segment(game_id)?;
        Ok(self.root.join("games").join(game_id).join("revisions"))
    }

    fn blob_path(&self, manifest_hash: &str) -> Result<PathBuf> {
        validate_hash(manifest_hash)?;
        Ok(self
            .root
            .join("blobs")
            .join(&manifest_hash[..2])
            .join(format!("{manifest_hash}.tar.zst")))
    }

    fn manifest_path(&self, manifest_hash: &str) -> Result<PathBuf> {
        validate_hash(manifest_hash)?;
        Ok(self
            .root
            .join("manifests")
            .join(format!("{manifest_hash}.json")))
    }
}

impl SyncProvider for LocalFolderProvider {
    fn list_revisions(&self, game_id: &str) -> Result<Vec<Revision>> {
        let directory = self.revision_dir(game_id)?;
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut revisions = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|e| Error::io(&directory, e))? {
            let entry = entry.map_err(|e| Error::io(&directory, e))?;
            if !entry.path().extension().is_some_and(|ext| ext == "json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|e| Error::io(entry.path(), e))?;
            let revision: Revision = serde_json::from_slice(&bytes)?;
            if revision.game_id != game_id {
                return Err(Error::InvalidState(format!(
                    "revision {} claims game {}, expected {game_id}",
                    revision.id, revision.game_id
                )));
            }
            let stem = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| Error::InvalidState("non-UTF-8 revision filename".into()))?
                .to_string();
            if revision.id != stem {
                return Err(Error::InvalidState(format!(
                    "revision filename {} does not match id {}",
                    entry.path().display(),
                    revision.id
                )));
            }
            validate_segment(&revision.id)?;
            revisions.push(revision);
        }
        Ok(revisions)
    }

    fn get_manifest(&self, manifest_hash: &str) -> Result<Manifest> {
        let path = self.manifest_path(manifest_hash)?;
        let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        let manifest = Manifest::from_json(std::str::from_utf8(&bytes).map_err(|_| {
            Error::InvalidState(format!("manifest {} is not UTF-8", path.display()))
        })?)?;
        if manifest.hash() != manifest_hash {
            return Err(Error::IntegrityMismatch {
                path: path.display().to_string(),
                expected: manifest_hash.to_string(),
                actual: manifest.hash(),
            });
        }
        Ok(manifest)
    }

    fn put_revision(&self, revision: &Revision, manifest: &Manifest) -> Result<()> {
        validate_segment(&revision.id)?;
        validate_segment(&revision.game_id)?;
        let manifest_hash = manifest.hash();
        if revision.manifest_hash != manifest_hash {
            return Err(Error::InvalidState(format!(
                "revision {} does not match manifest {}",
                revision.id, manifest_hash
            )));
        }

        let manifest_path = self.manifest_path(&manifest_hash)?;
        write_atomic(&manifest_path, manifest.to_json()?.as_bytes())?;

        let directory = self.revision_dir(&revision.game_id)?;
        fs::create_dir_all(&directory).map_err(|e| Error::io(&directory, e))?;
        let path = directory.join(format!("{}.json", revision.id));
        write_atomic(&path, &serde_json::to_vec(revision)?)
    }

    fn has_blob(&self, manifest_hash: &str) -> Result<bool> {
        Ok(self.blob_path(manifest_hash)?.is_file())
    }

    fn put_blob(&self, manifest_hash: &str, source: &Path) -> Result<()> {
        let destination = self.blob_path(manifest_hash)?;
        if destination.is_file() {
            return Ok(());
        }
        if !source.is_file() {
            return Err(Error::NotFound(format!("blob {}", source.display())));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        copy_atomic(source, &destination)
    }

    fn get_blob(&self, manifest_hash: &str, destination: &Path) -> Result<()> {
        let source = self.blob_path(manifest_hash)?;
        if !source.is_file() {
            return Err(Error::NotFound(format!("blob {manifest_hash}")));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let bytes = fs::read(&source).map_err(|e| Error::io(&source, e))?;
        fs::write(destination, &bytes).map_err(|e| Error::io(destination, e))?;
        Ok(())
    }
}

/// Copy `source` to a temp file next to `destination`, fsync, then rename.
///
/// The temporary file is created by this function rather than with
/// [`fs::copy`], so it never inherits the source file's permissions. A
/// read-only source (Windows `READONLY`, Unix mode `0444`) would otherwise
/// produce a read-only temporary file that cannot be reopened for `sync_all`.
fn copy_atomic(source: &Path, destination: &Path) -> Result<()> {
    let temp = destination.with_extension(format!("part-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut input = fs::File::open(source).map_err(|e| Error::io(source, e))?;
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|e| Error::io(&temp, e))?;
        std::io::copy(&mut input, &mut output).map_err(|e| Error::io(&temp, e))?;
        output.sync_all().map_err(|e| Error::io(&temp, e))?;
        // Windows cannot rename a file while a writable handle to it is open.
        drop(output);
        drop(input);
        fs::rename(&temp, destination).map_err(|e| Error::io(destination, e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Write bytes atomically, refusing to replace an existing object with
/// different contents (objects are immutable).
fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
    if destination.is_file() {
        let existing = fs::read(destination).map_err(|e| Error::io(destination, e))?;
        if existing == bytes {
            return Ok(());
        }
        return Err(Error::InvalidState(format!(
            "refusing to overwrite immutable object {}",
            destination.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let temp = destination.with_extension(format!("part-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        fs::write(&temp, bytes).map_err(|e| Error::io(&temp, e))?;
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .map_err(|e| Error::io(&temp, e))?;
        file.sync_all().map_err(|e| Error::io(&temp, e))?;
        fs::rename(&temp, destination).map_err(|e| Error::io(destination, e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidState(format!("invalid digest: {value:?}")));
    }
    Ok(())
}

/// Provider object names must be single, traversal-free path segments.
fn validate_segment(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', '\0', ':'])
    {
        return Err(Error::UnsafePath(format!(
            "invalid provider object name: {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::models::Revision;

    fn revision(id: &str, game_id: &str, manifest_hash: &str) -> Revision {
        Revision {
            id: id.into(),
            game_id: game_id.into(),
            parent_ids: Vec::new(),
            device_id: "device".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            manifest_hash: manifest_hash.into(),
            blob_hash: Some(manifest_hash.into()),
            message: None,
        }
    }

    #[test]
    fn rejects_traversal_in_object_names() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalFolderProvider::open(dir.path()).unwrap();
        let manifest = Manifest::empty();

        for id in ["../escape", "..", "a/b", "a\\b", "C:evil"] {
            let error = provider
                .put_revision(&revision(id, "game", &manifest.hash()), &manifest)
                .unwrap_err();
            assert!(
                matches!(error, Error::UnsafePath(_)),
                "expected unsafe path for {id:?}, got {error:?}"
            );
        }

        assert!(
            provider
                .put_revision(&revision("ok", "game/../evil", &manifest.hash()), &manifest)
                .is_err()
        );
    }

    #[test]
    fn manifest_fetch_detects_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalFolderProvider::open(dir.path()).unwrap();
        let manifest = Manifest::empty();
        let hash = manifest.hash();
        let path = provider.manifest_path(&hash).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let other = Manifest::new(vec![crate::manifest::FileEntry {
            path: "a.bin".into(),
            size: 1,
            sha256: crate::hash::hash_bytes(b"a"),
            mtime_ms: None,
        }])
        .unwrap();
        fs::write(&path, other.to_json().unwrap()).unwrap();

        assert!(matches!(
            provider.get_manifest(&hash),
            Err(Error::IntegrityMismatch { .. })
        ));
    }

    #[test]
    fn missing_blobs_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalFolderProvider::open(dir.path()).unwrap();
        let hash = Manifest::empty().hash();
        assert!(!provider.has_blob(&hash).unwrap());
        assert!(provider.get_blob(&hash, &dir.path().join("out")).is_err());
    }

    #[test]
    fn copies_read_only_source_blob() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let contents = b"read-only blob payload";
        fs::write(&source, contents).unwrap();

        // `Permissions::set_readonly` sets the READONLY attribute on Windows
        // and clears write bits on Unix, covering both failure modes.
        let original_permissions = fs::metadata(&source).unwrap().permissions();
        let mut read_only = original_permissions.clone();
        read_only.set_readonly(true);
        fs::set_permissions(&source, read_only).unwrap();

        let provider = LocalFolderProvider::open(dir.path().join("provider")).unwrap();
        let hash = Manifest::empty().hash();
        let result = provider.put_blob(&hash, &source);

        // Restore the original permissions before asserting so tempdir cleanup
        // works on Windows even if the copy failed.
        fs::set_permissions(&source, original_permissions).unwrap();

        result.expect("copying a read-only source must succeed");
        let destination = provider.blob_path(&hash).unwrap();
        assert_eq!(fs::read(destination).unwrap(), contents);
    }
}
