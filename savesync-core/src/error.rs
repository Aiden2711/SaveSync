//! Error types for the SaveSync core engine.
//!
//! Every failure mode that can touch user save data is modelled explicitly so
//! callers (and the UI) can distinguish "nothing was changed" from
//! "a rollback was attempted and needs attention".

use std::path::PathBuf;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the SaveSync engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("network error: {0}")]
    Network(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("storage provider error (HTTP {status}): {message}")]
    Provider { status: u16, message: String },

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A path from a manifest, archive, or user input could escape its root.
    #[error("unsafe path rejected: {0}")]
    UnsafePath(String),

    /// A file on disk does not match the recorded SHA-256 digest.
    #[error("integrity check failed for {path}: expected {expected}, got {actual}")]
    IntegrityMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    /// A file listed in a manifest is missing on disk.
    #[error("file listed in manifest is missing: {path}")]
    MissingFile { path: String },

    /// A file exists on disk that the manifest does not describe.
    #[error("unexpected file not present in manifest: {path}")]
    UnexpectedFile { path: String },

    /// Archives may not contain symlinks, directories as files, or duplicates.
    #[error("invalid archive: {0}")]
    InvalidArchive(String),

    #[error("entity not found: {0}")]
    NotFound(String),

    #[error("invalid operation: {0}")]
    InvalidState(String),

    #[error("a restore was already in progress for {0}")]
    RestoreInProgress(String),

    /// A restore failed and an automatic rollback was attempted. The message
    /// states whether the pre-restore state was recovered.
    #[error("restore failed: {message}; rollback: {rollback}")]
    RestoreFailed { message: String, rollback: String },
}

impl Error {
    /// Convenience constructor for I/O failures with path context.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            path: PathBuf::new(),
            source,
        }
    }
}
