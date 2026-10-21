//! # SaveSync Core
//!
//! Local-first, safety-critical save data engine.
//!
//! The crate provides five layers:
//!
//! - [`db`] / [`store`]: transactional SQLite metadata (games, revisions,
//!   manifests, conflicts, restore journal).
//! - [`manifest`] / [`hash`]: content-addressed snapshot identities.
//! - [`blob`]: verified `tar.zst` archive storage.
//! - [`restore`] / [`engine`]: staged, journaled restores with an automatic
//!   safety backup and rollback.
//! - [`provider`] / [`sync`]: optional, mockable transport for immutable
//!   revisions. Live saves are never changed by sync.
//! - [`watcher`]: passive save-directory change detection with debouncing.
//!
//! Nothing in this crate requires a network connection to operate.

pub mod blob;
pub mod credentials;
pub mod db;
pub mod discovery;
pub mod engine;
pub mod error;
pub mod google_drive;
pub mod hash;
pub mod http;
pub mod manifest;
pub mod models;
pub mod oauth;
pub mod oauth_loopback;
pub mod paths;
pub mod provider;
pub mod restore;
pub mod store;
pub mod sync;
pub mod watcher;

pub use credentials::{KeyringTokenStore, MemoryTokenStore, TokenStore};
pub use discovery::{DiscoveredGame, DiscoverySource};
pub use engine::SaveSyncEngine;
pub use error::{Error, Result};
pub use google_drive::GoogleDriveProvider;
pub use http::{HttpTransport, ReqwestTransport, RetryPolicy};
pub use manifest::{FileEntry, Manifest, ManifestDiff};
pub use models::{
    AdapterType, Conflict, Device, Game, Location, LocationKind, RestoreJournalEntry,
    RestoreReport, RestoreState, Revision, SnapshotInfo,
};
pub use oauth::{
    DRIVE_FILE_SCOPE, OAuthConfig, OAuthTokenManager, PkcePair, StaticToken, TokenProvider,
    TokenSet, authorization_url, exchange_code, refresh_tokens,
};
pub use oauth_loopback::OAuthFlow;
pub use provider::{LocalFolderProvider, SyncProvider};
pub use restore::{DataPaths, RestoreMode, RestoreRequest};
pub use sync::{SyncDirection, SyncReport};
pub use watcher::{ChangeTracker, DebouncePolicy, DirtyEvent, DirtyReason, SaveWatcher};
