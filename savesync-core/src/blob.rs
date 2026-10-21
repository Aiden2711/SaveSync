//! Content-addressable snapshot storage.
//!
//! Snapshots are stored as deterministic `tar.zst` archives keyed by the
//! manifest digest. Because the key describes the *contents*, identical saves
//! deduplicate across revisions, devices, and providers for free.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tar::{Archive, Builder, EntryType, Header};
use zstd::stream::read::Decoder;
use zstd::stream::write::Encoder;

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::paths::sanitize_relative_path;
const ZSTD_LEVEL: i32 = 9;

/// Content-addressable blob store rooted at a directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (creating if needed) a blob store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp")).map_err(|e| Error::io(&root, e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the archive for `manifest_hash`.
    pub fn path_for(&self, manifest_hash: &str) -> Result<PathBuf> {
        validate_hash(manifest_hash)?;
        Ok(self
            .root
            .join(&manifest_hash[..2])
            .join(format!("{manifest_hash}.tar.zst")))
    }

    pub fn contains(&self, manifest_hash: &str) -> bool {
        self.path_for(manifest_hash)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    /// Archive `source_dir` (as described by `manifest`) into the store.
    ///
    /// The write is atomic: data goes to a temp file, is fsynced, then renamed
    /// into place. Existing blobs are trusted (their name is their digest) and
    /// never overwritten.
    pub fn put(&self, manifest: &Manifest, source_dir: &Path) -> Result<String> {
        let manifest_hash = manifest.hash();
        if self.contains(&manifest_hash) {
            return Ok(manifest_hash);
        }

        let final_path = self.path_for(&manifest_hash)?;
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let temp_path =
            self.root
                .join("tmp")
                .join(format!("{}.{}.part", manifest_hash, uuid::Uuid::new_v4()));
        fs::create_dir_all(temp_path.parent().expect("temp path has parent"))
            .map_err(|e| Error::io(&self.root, e))?;

        let result = self.write_archive(manifest, source_dir, &temp_path);
        if let Err(error) = result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        fs::rename(&temp_path, &final_path).map_err(|e| Error::io(&final_path, e))?;
        Ok(manifest_hash)
    }

    fn write_archive(&self, manifest: &Manifest, source_dir: &Path, temp: &Path) -> Result<()> {
        let file = File::create(temp).map_err(|e| Error::io(temp, e))?;
        let encoder = Encoder::new(file, ZSTD_LEVEL).map_err(archive_err)?;
        let mut builder = Builder::new(encoder);

        for entry in &manifest.entries {
            let source_path = source_dir.join(sanitize_relative_path(&entry.path)?);
            let source = File::open(&source_path).map_err(|e| Error::io(&source_path, e))?;
            let metadata = source.metadata().map_err(|e| Error::io(&source_path, e))?;
            if metadata.len() != entry.size {
                return Err(Error::InvalidState(format!(
                    "size changed before archiving: {} (manifest {}, disk {})",
                    entry.path,
                    entry.size,
                    metadata.len()
                )));
            }

            let mut header = Header::new_gnu();
            header.set_size(entry.size);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_entry_type(EntryType::Regular);

            let mut reader = HashingReader::new(BufReader::new(source));
            builder
                .append_data(&mut header, &entry.path, &mut reader)
                .map_err(archive_err)?;
            let (digest, bytes_read) = reader.finish()?;
            if bytes_read != entry.size {
                return Err(Error::InvalidState(format!(
                    "size changed while archiving: {} (manifest {}, read {})",
                    entry.path, entry.size, bytes_read
                )));
            }
            if !digest.eq_ignore_ascii_case(&entry.sha256) {
                return Err(Error::IntegrityMismatch {
                    path: entry.path.clone(),
                    expected: entry.sha256.clone(),
                    actual: digest,
                });
            }
        }

        let encoder = builder.into_inner().map_err(archive_err)?;
        let mut file = encoder.finish().map_err(archive_err)?;
        file.flush()?;
        file.sync_all().map_err(|e| Error::io(temp, e))?;
        Ok(())
    }

    /// Extract the blob for `manifest` into `destination`, then verify the
    /// extracted tree byte-for-byte against the manifest.
    ///
    /// `destination` must not already contain files; a fresh staging directory
    /// is the only supported target.
    pub fn extract_verified(&self, manifest: &Manifest, destination: &Path) -> Result<()> {
        let blob_path = self.path_for(&manifest.hash())?;
        if !blob_path.is_file() {
            return Err(Error::NotFound(format!("blob {}", blob_path.display())));
        }
        self.extract_archive(manifest, &blob_path, destination)
    }

    /// Import a blob file supplied by a provider.
    ///
    /// The archive is fully extracted and verified against `manifest` before it
    /// becomes visible in the store, so a corrupted or malicious download can
    /// never masquerade as a valid snapshot.
    pub fn ingest(&self, manifest: &Manifest, source: &Path) -> Result<()> {
        let manifest_hash = manifest.hash();
        if self.contains(&manifest_hash) {
            return Ok(());
        }
        if !source.is_file() {
            return Err(Error::NotFound(format!("blob {}", source.display())));
        }

        let temp = self
            .root
            .join("tmp")
            .join(format!("{manifest_hash}.{}.incoming", uuid::Uuid::new_v4()));
        fs::copy(source, &temp).map_err(|e| Error::io(&temp, e))?;

        let verify_dir = self
            .root
            .join("tmp")
            .join(format!("{manifest_hash}.{}.verify", uuid::Uuid::new_v4()));
        let verification = self.extract_archive(manifest, &temp, &verify_dir);
        let _ = fs::remove_dir_all(&verify_dir);
        if let Err(error) = verification {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }

        let final_path = self.path_for(&manifest_hash)?;
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        if final_path.is_file() {
            let _ = fs::remove_file(&temp);
            return Ok(());
        }
        fs::rename(&temp, &final_path).map_err(|e| Error::io(&final_path, e))?;
        Ok(())
    }

    fn extract_archive(
        &self,
        manifest: &Manifest,
        blob_path: &Path,
        destination: &Path,
    ) -> Result<()> {
        fs::create_dir_all(destination).map_err(|e| Error::io(destination, e))?;

        let expected: std::collections::HashSet<&str> =
            manifest.entries.iter().map(|e| e.path.as_str()).collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let file = File::open(blob_path).map_err(|e| Error::io(blob_path, e))?;
        let decoder = Decoder::new(file).map_err(archive_err)?;
        let mut archive = Archive::new(decoder);

        for entry in archive.entries().map_err(archive_err)? {
            let mut entry = entry.map_err(archive_err)?;
            let entry_type = entry.header().entry_type();
            if entry_type.is_dir() {
                continue;
            }
            if !entry_type.is_file() {
                return Err(Error::InvalidArchive(format!(
                    "non-regular archive entry type {entry_type:?}"
                )));
            }

            let raw_path = entry.path().map_err(archive_err)?;
            let path_text = raw_path
                .to_str()
                .ok_or_else(|| Error::InvalidArchive("non-UTF-8 entry path".into()))?
                .to_string();
            let relative = sanitize_relative_path(&path_text)?;
            let relative_text = relative
                .to_str()
                .ok_or_else(|| Error::InvalidArchive("non-UTF-8 entry path".into()))?
                .replace('\\', "/");

            let expected_entry = manifest.find(&relative_text).ok_or_else(|| {
                Error::InvalidArchive(format!(
                    "archive entry not present in manifest: {relative_text}"
                ))
            })?;
            if !seen.insert(relative_text.clone()) {
                return Err(Error::InvalidArchive(format!(
                    "duplicate archive entry: {relative_text}"
                )));
            }
            if entry.header().size().map_err(archive_err)? != expected_entry.size {
                return Err(Error::InvalidArchive(format!(
                    "archive entry size mismatch for {relative_text}"
                )));
            }

            let target = destination.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| Error::io(&target, e))?;
            io::copy(&mut entry, &mut out).map_err(|e| Error::io(&target, e))?;
            out.sync_all().map_err(|e| Error::io(&target, e))?;
        }

        if seen.len() != expected.len() {
            let missing: Vec<_> = expected
                .iter()
                .filter(|path| !seen.contains(**path))
                .collect();
            return Err(Error::InvalidArchive(format!(
                "archive is missing {} manifest entries (e.g. {:?})",
                missing.len(),
                missing.first()
            )));
        }

        manifest.verify_dir(destination)
    }
}

fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidState(format!(
            "invalid blob digest: {value:?}"
        )));
    }
    Ok(())
}

fn archive_err(error: impl std::fmt::Display) -> Error {
    Error::InvalidArchive(error.to_string())
}

/// Reader adapter that hashes and counts bytes as they are consumed.
struct HashingReader<R> {
    inner: R,
    hasher: sha2::Sha256,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    fn finish(self) -> Result<(String, u64)> {
        use sha2::Digest;
        Ok((hex(&self.hasher.finalize()), self.count))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use sha2::Digest;
        let read = self.inner.read(buf)?;
        self.hasher.update(&buf[..read]);
        self.count += read as u64;
        Ok(read)
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn round_trips_and_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("save");
        fs::create_dir_all(&source).unwrap();
        write(&source, "slot1/a.bin", b"alpha");
        write(&source, "slot1/b.bin", b"beta");

        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        let manifest = Manifest::from_dir(&source).unwrap();
        let hash1 = store.put(&manifest, &source).unwrap();
        let hash2 = store.put(&manifest, &source).unwrap();
        assert_eq!(hash1, hash2);
        assert!(store.contains(&hash1));

        let restored = dir.path().join("restored");
        store.extract_verified(&manifest, &restored).unwrap();
        assert_eq!(
            fs::read(restored.join("slot1/a.bin")).unwrap(),
            b"alpha".to_vec()
        );
    }

    #[test]
    fn detects_tampered_blob() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("save");
        fs::create_dir_all(&source).unwrap();
        write(&source, "a.bin", b"original");
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        let manifest = Manifest::from_dir(&source).unwrap();
        let hash = store.put(&manifest, &source).unwrap();

        let blob = store.path_for(&hash).unwrap();
        fs::write(&blob, b"tampered").unwrap();

        let dest = dir.path().join("restored");
        assert!(store.extract_verified(&manifest, &dest).is_err());
    }

    #[test]
    fn rejects_path_traversal_entries() {
        // Build a malicious tar header by hand: `Builder::append_data`
        // (correctly) refuses to create traversal paths, so the archive is
        // assembled as raw bytes to simulate an attacker-supplied blob.
        let dir = tempfile::tempdir().unwrap();
        let blob_root = dir.path().join("blobs");
        fs::create_dir_all(&blob_root).unwrap();
        let empty_hash = Manifest::empty().hash();
        let target = blob_root
            .join(&empty_hash[..2])
            .join(format!("{empty_hash}.tar.zst"));
        fs::create_dir_all(target.parent().unwrap()).unwrap();

        let name = "../escape.bin";
        let payload = b"evil";
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", payload.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());

        let mut tar_bytes = header.to_vec();
        tar_bytes.extend_from_slice(payload);
        tar_bytes.resize(1024, 0);
        tar_bytes.extend_from_slice(&[0u8; 1024]);
        fs::write(&target, zstd::encode_all(&tar_bytes[..], 1).unwrap()).unwrap();

        let store = BlobStore::open(&blob_root).unwrap();
        let manifest = Manifest::empty();
        let dest = dir.path().join("out");
        let error = store.extract_verified(&manifest, &dest).unwrap_err();
        assert!(
            matches!(error, Error::UnsafePath(_) | Error::InvalidArchive(_)),
            "unexpected error: {error:?}"
        );
        assert!(!dir.path().join("escape.bin").exists());
        assert!(!dir.path().join("out/escape.bin").exists());
    }
}
