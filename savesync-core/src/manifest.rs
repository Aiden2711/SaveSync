//! Immutable snapshot manifests.
//!
//! A manifest is the canonical description of a save directory at a point in
//! time. Its hash is content-addressed: it depends only on the relative path,
//! size, and SHA-256 of every file — never on timestamps, ownership, or the
//! order in which the filesystem returned entries.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::hash;
use crate::paths::{is_internal_artifact, relative_to, sanitize_relative_path};

/// Version tag mixed into the manifest digest.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// One file inside a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path using `/` separators, e.g. `slot1/data.bin`.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the file contents.
    pub sha256: String,
    /// Modification time in milliseconds since the Unix epoch, if available.
    /// Recorded for display only; excluded from the manifest digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}

/// A complete snapshot manifest.
///
/// Entries are always sorted by [`FileEntry::path`] and paths are unique, so
/// two manifests describing identical content are byte-for-byte identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub entries: Vec<FileEntry>,
}

/// Difference between two manifests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDiff {
    /// Files present only in the "new" manifest.
    pub added: Vec<FileEntry>,
    /// Files present only in the "old" manifest.
    pub removed: Vec<FileEntry>,
    /// Files present in both but with a different digest.
    pub modified: Vec<FileEntry>,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

impl Manifest {
    /// Build a manifest from already-hashed entries.
    ///
    /// Returns an error when duplicate paths are supplied; silently picking a
    /// winner could hide corrupted input.
    pub fn new(mut entries: Vec<FileEntry>) -> Result<Self> {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        for pair in entries.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(Error::InvalidState(format!(
                    "duplicate manifest entry for {}",
                    pair[0].path
                )));
            }
        }
        for entry in &entries {
            sanitize_relative_path(&entry.path)?;
            validate_digest(&entry.sha256)?;
        }
        Ok(Self {
            version: MANIFEST_FORMAT_VERSION,
            entries,
        })
    }

    /// An empty manifest, used for the first snapshot of an empty directory.
    pub fn empty() -> Self {
        Self {
            version: MANIFEST_FORMAT_VERSION,
            entries: Vec::new(),
        }
    }

    /// Recursively hash `root` and build a manifest.
    ///
    /// Rejects symlinks (they can point outside the snapshot root), unstable
    /// files (modified while being hashed), and non-UTF-8 paths.
    pub fn from_dir(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(Error::InvalidState(format!(
                "snapshot root is not a directory: {}",
                root.display()
            )));
        }

        let mut entries = Vec::new();
        for walked in WalkDir::new(root).follow_links(false).sort_by_file_name() {
            let walked = walked.map_err(|e| {
                let path = e.path().unwrap_or(root).to_path_buf();
                Error::Io {
                    path,
                    source: e
                        .io_error()
                        .map(|io| std::io::Error::new(io.kind(), io.to_string()))
                        .unwrap_or_else(|| std::io::Error::other("directory walk failed")),
                }
            })?;
            let path = walked.path();
            if path == root {
                continue;
            }

            let file_type = walked.file_type();
            if file_type.is_symlink() {
                return Err(Error::InvalidState(format!(
                    "symlinks are not supported inside save locations: {}",
                    path.display()
                )));
            }
            if !file_type.is_file() {
                // Directories and special files are skipped; empty directories
                // carry no save data. Special files (FIFOs, sockets) are never
                // legitimately part of a save snapshot.
                if !file_type.is_dir() {
                    return Err(Error::InvalidState(format!(
                        "unsupported special file in save location: {}",
                        path.display()
                    )));
                }
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && is_internal_artifact(name)
            {
                continue;
            }

            entries.push(hash_entry(root, path)?);
        }

        Self::new(entries)
    }

    /// Content-addressed identity of this manifest.
    ///
    /// Canonical JSON keeps the digest unambiguous even for paths containing
    /// spaces, newlines, or unicode. `mtime_ms` is deliberately excluded.
    pub fn hash(&self) -> String {
        #[derive(Serialize)]
        struct CanonicalEntry<'a> {
            path: &'a str,
            size: u64,
            sha256: &'a str,
        }
        #[derive(Serialize)]
        struct CanonicalManifest<'a> {
            version: u32,
            entries: Vec<CanonicalEntry<'a>>,
        }

        let canonical = CanonicalManifest {
            version: self.version,
            entries: self
                .entries
                .iter()
                .map(|entry| CanonicalEntry {
                    path: &entry.path,
                    size: entry.size,
                    sha256: &entry.sha256,
                })
                .collect(),
        };
        // Serialization of owned primitives cannot fail.
        let bytes = serde_json::to_vec(&canonical).expect("manifest serialization is infallible");
        hash::hash_bytes(&bytes)
    }

    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|entry| entry.size).sum()
    }

    pub fn find(&self, path: &str) -> Option<&FileEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Compare this (old) manifest against a newer one.
    pub fn diff(&self, other: &Manifest) -> ManifestDiff {
        let mut diff = ManifestDiff::default();

        for old in &self.entries {
            match other.find(&old.path) {
                None => diff.removed.push(old.clone()),
                Some(new) if new.sha256 != old.sha256 || new.size != old.size => {
                    diff.modified.push(new.clone());
                }
                Some(_) => {}
            }
        }
        for new in &other.entries {
            if self.find(&new.path).is_none() {
                diff.added.push(new.clone());
            }
        }
        diff
    }

    /// Verify that `root` contains exactly the files described here, with
    /// matching digests. Extra files, missing files, and mismatches all fail.
    pub fn verify_dir(&self, root: impl AsRef<Path>) -> Result<()> {
        let root = root.as_ref();
        let actual = Manifest::from_dir(root)?;
        if actual.hash() == self.hash() {
            return Ok(());
        }

        for expected in &self.entries {
            match actual.find(&expected.path) {
                None => {
                    return Err(Error::MissingFile {
                        path: expected.path.clone(),
                    });
                }
                Some(found) if found.sha256 != expected.sha256 => {
                    return Err(Error::IntegrityMismatch {
                        path: expected.path.clone(),
                        expected: expected.sha256.clone(),
                        actual: found.sha256.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for found in &actual.entries {
            if self.find(&found.path).is_none() {
                return Err(Error::UnexpectedFile {
                    path: found.path.clone(),
                });
            }
        }
        // Reaching here without a digest match means the canonical encoding
        // differed; treat it as a manifest-level integrity failure.
        Err(Error::InvalidState(
            "manifest digest does not match directory contents".into(),
        ))
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let manifest: Manifest = serde_json::from_str(json)?;
        Self::new(manifest.entries)
    }
}

fn hash_entry(root: &Path, path: &Path) -> Result<FileEntry> {
    let before = fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let digest = hash::hash_file(path)?;
    let after = fs::metadata(path).map_err(|e| Error::io(path, e))?;

    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(Error::InvalidState(format!(
            "file changed while it was being snapshotted: {}",
            path.display()
        )));
    }
    validate_digest(&digest)?;

    let mtime_ms = after
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());

    Ok(FileEntry {
        path: relative_to(root, path)?,
        size: after.len(),
        sha256: digest,
        mtime_ms,
    })
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidState(format!(
            "invalid SHA-256 digest in manifest: {digest:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, bytes: &[u8]) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: bytes.len() as u64,
            sha256: hash::hash_bytes(bytes),
            mtime_ms: Some(1234),
        }
    }

    #[test]
    fn digest_is_order_and_mtime_independent() {
        let mut a = vec![entry("slot2/b.bin", b"b"), entry("slot1/a.bin", b"a")];
        let mut b = vec![entry("slot1/a.bin", b"a"), entry("slot2/b.bin", b"b")];
        b[0].mtime_ms = Some(999_999);
        b[1].mtime_ms = None;

        assert_eq!(
            Manifest::new(a.clone()).unwrap().hash(),
            Manifest::new(b).unwrap().hash()
        );
        // Extra allocation only to prove the first vector is still usable.
        a.push(entry("c.bin", b"c"));
        assert_ne!(
            Manifest::new(a).unwrap().hash(),
            Manifest::new(vec![entry("slot1/a.bin", b"a")])
                .unwrap()
                .hash()
        );
    }

    #[test]
    fn rejects_duplicate_and_unsafe_paths() {
        assert!(Manifest::new(vec![entry("a.bin", b"1"), entry("a.bin", b"2")]).is_err());
        let mut unsafe_entry = entry("a.bin", b"1");
        unsafe_entry.path = "../escape.bin".into();
        assert!(Manifest::new(vec![unsafe_entry]).is_err());
    }

    #[test]
    fn verifies_directory_contents_exactly() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("slot1")).unwrap();
        fs::write(dir.path().join("slot1/a.bin"), b"alpha").unwrap();
        fs::write(dir.path().join("b.bin"), b"beta").unwrap();

        let manifest = Manifest::from_dir(dir.path()).unwrap();
        manifest.verify_dir(dir.path()).unwrap();

        fs::write(dir.path().join("slot1/a.bin"), b"tampered").unwrap();
        assert!(matches!(
            manifest.verify_dir(dir.path()),
            Err(Error::IntegrityMismatch { .. })
        ));

        fs::write(dir.path().join("extra.bin"), b"extra").unwrap();
        fs::write(dir.path().join("slot1/a.bin"), b"alpha").unwrap();
        assert!(matches!(
            manifest.verify_dir(dir.path()),
            Err(Error::UnexpectedFile { .. })
        ));

        fs::remove_file(dir.path().join("extra.bin")).unwrap();
        fs::remove_file(dir.path().join("b.bin")).unwrap();
        assert!(matches!(
            manifest.verify_dir(dir.path()),
            Err(Error::MissingFile { .. })
        ));
    }

    #[test]
    fn empty_manifest_round_trips() {
        let manifest = Manifest::empty();
        assert_eq!(
            Manifest::from_json(&manifest.to_json().unwrap()).unwrap(),
            manifest
        );
        assert_eq!(manifest.entries.len(), 0);
        assert_eq!(manifest.total_size(), 0);
    }
}
