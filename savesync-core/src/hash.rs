//! SHA-256 hashing helpers.
//!
//! SaveSync never trusts a size or timestamp alone: every blob and every file
//! that participates in a snapshot or restore is verified by digest.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Size of the streaming read buffer.
const BUFFER_SIZE: usize = 64 * 1024;

/// Hash an in-memory byte slice and return a lowercase hex digest.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// Stream a reader to completion and return a lowercase hex SHA-256 digest.
pub fn hash_reader<R: Read>(mut reader: R) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; BUFFER_SIZE];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// Hash a file on disk.
pub fn hash_file(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    hash_reader(BufReader::new(file))
}

/// Return `true` when the file's digest matches `expected` (case-insensitive).
pub fn verify_file(path: impl AsRef<Path>, expected: &str) -> Result<bool> {
    Ok(hash_file(path)?.eq_ignore_ascii_case(expected))
}

fn to_hex(bytes: &[u8]) -> String {
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
    use std::io::Write;

    #[test]
    fn known_sha256_vectors() {
        assert_eq!(
            hash_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn streaming_matches_memory_hashing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut file = File::create(&path).unwrap();
        file.write_all(&payload).unwrap();
        drop(file);

        assert_eq!(hash_file(&path).unwrap(), hash_bytes(&payload));
        assert!(verify_file(&path, &hash_bytes(&payload)).unwrap());
        assert!(!verify_file(&path, &"0".repeat(64)).unwrap());
    }
}
