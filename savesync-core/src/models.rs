//! Domain models persisted in SQLite.

use serde::{Deserialize, Serialize};

/// How a game's save files are located.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterType {
    /// Locations discovered through the Ludusavi CLI.
    Ludusavi,
    /// Locations discovered from a Lutris game configuration.
    Lutris,
    /// Locations discovered from a Steam / Proton library scan.
    Steam,
    /// A directory explicitly chosen by the user.
    CustomDirectory,
    /// Emulator memory cards or save directories.
    Emulator,
    /// A JKSV export folder produced on a Nintendo Switch.
    Jksv,
}

impl AdapterType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ludusavi => "ludusavi",
            Self::Lutris => "lutris",
            Self::Steam => "steam",
            Self::CustomDirectory => "custom_directory",
            Self::Emulator => "emulator",
            Self::Jksv => "jksv",
        }
    }

    /// Parse a stored adapter identifier.
    ///
    /// Legacy rows are still readable: Lutris and Steam/Proton discoveries used
    /// to be recorded as `"emulator"`, which continues to resolve to
    /// [`AdapterType::Emulator`]. Only new rows carry the honest values.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ludusavi" => Self::Ludusavi,
            "lutris" => Self::Lutris,
            "steam" => Self::Steam,
            "custom_directory" => Self::CustomDirectory,
            "emulator" => Self::Emulator,
            "jksv" => Self::Jksv,
            _ => return None,
        })
    }
}

/// Whether a location path refers to a directory or a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationKind {
    Directory,
    File,
}

impl LocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "directory" => Self::Directory,
            "file" => Self::File,
            _ => return None,
        })
    }
}

/// This installation of SaveSync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub created_at: String,
    pub last_seen_at: String,
}

/// An enrolled game or a discovered candidate awaiting explicit enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub id: String,
    pub title: String,
    pub adapter_type: AdapterType,
    /// `false` means the game was only discovered and must never be synced
    /// automatically until the user enrolls it.
    pub enrolled: bool,
    pub created_at: String,
    pub updated_at: String,
    pub active_head_revision_id: Option<String>,
    /// Optional path to a cover image file for the game.
    pub cover_path: Option<String>,
    /// Optional path to a wide (banner) cover cached beside the portrait one.
    ///
    /// `None` for every game that has only ever had a portrait cover, which is
    /// what rows written before schema v4 report.
    pub wide_cover_path: Option<String>,
    /// Whether this game's wide art is the library's featured banner.
    ///
    /// At most one game may be featured at a time; [`crate::store::set_featured`]
    /// enforces that in one transaction.
    pub featured: bool,
}

/// A filesystem location belonging to a game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub id: String,
    pub game_id: String,
    pub path: String,
    pub kind: LocationKind,
}

/// An immutable node in the revision DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub id: String,
    pub game_id: String,
    /// Zero, one, or many parents. Multiple parents only occur on explicit
    /// conflict-resolution commits; SaveSync never creates them implicitly.
    pub parent_ids: Vec<String>,
    pub device_id: String,
    pub created_at: String,
    pub manifest_hash: String,
    pub blob_hash: Option<String>,
    pub message: Option<String>,
}

/// Lifecycle of a restore transaction, persisted in the rollback journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreState {
    /// Staging and backup are complete; live files are about to be touched.
    Prepared,
    /// Live files match the restored manifest.
    Applied,
    /// Live files were returned to their pre-restore state.
    RolledBack,
    /// Both apply and automatic rollback failed. Manual recovery required.
    RollbackFailed,
}

impl RestoreState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Applied => "applied",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "prepared" => Self::Prepared,
            "applied" => Self::Applied,
            "rolled_back" => Self::RolledBack,
            "rollback_failed" => Self::RollbackFailed,
            _ => return None,
        })
    }
}

/// A durable record of one restore attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreJournalEntry {
    pub id: String,
    pub game_id: String,
    pub revision_id: String,
    pub target_root: String,
    pub staging_path: String,
    pub backup_path: String,
    /// JSON-encoded manifest of the live directory before the restore.
    pub backup_manifest: String,
    pub state: RestoreState,
    /// Files present before the restore but absent from the restored manifest.
    pub deleted_paths: Vec<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// An unresolved divergence between revision DAG heads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    pub game_id: String,
    pub revision_ids: Vec<String>,
    pub detected_at: String,
    pub resolved_at: Option<String>,
    pub resolution_revision_id: Option<String>,
}

/// Result of a successful snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub revision_id: String,
    pub manifest_hash: String,
    pub blob_hash: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// Result of a successful restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub restore_id: String,
    pub revision_id: String,
    pub files_restored: usize,
    pub bytes_restored: u64,
    /// Digest of the live directory as it was before the restore.
    pub safety_backup_manifest_hash: String,
    pub safety_backup_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_type_round_trips_through_its_stored_string() {
        for adapter in [
            AdapterType::Ludusavi,
            AdapterType::Lutris,
            AdapterType::Steam,
            AdapterType::CustomDirectory,
            AdapterType::Emulator,
            AdapterType::Jksv,
        ] {
            assert_eq!(AdapterType::parse(adapter.as_str()), Some(adapter));
        }
        assert_eq!(AdapterType::Lutris.as_str(), "lutris");
        assert_eq!(AdapterType::Steam.as_str(), "steam");
    }

    #[test]
    fn adapter_type_keeps_legacy_emulator_rows_readable() {
        // Lutris and Steam/Proton discoveries were persisted as "emulator"
        // before the mislabel was corrected; those rows must keep loading.
        assert_eq!(AdapterType::parse("emulator"), Some(AdapterType::Emulator));
        let from_json: AdapterType = serde_json::from_str("\"emulator\"").unwrap();
        assert_eq!(from_json, AdapterType::Emulator);
    }

    #[test]
    fn adapter_type_serializes_to_snake_case_identifiers() {
        for (adapter, stored) in [
            (AdapterType::Ludusavi, "\"ludusavi\""),
            (AdapterType::Lutris, "\"lutris\""),
            (AdapterType::Steam, "\"steam\""),
            (AdapterType::CustomDirectory, "\"custom_directory\""),
            (AdapterType::Emulator, "\"emulator\""),
            (AdapterType::Jksv, "\"jksv\""),
        ] {
            assert_eq!(serde_json::to_string(&adapter).unwrap(), stored);
            assert_eq!(
                serde_json::from_str::<AdapterType>(stored).unwrap(),
                adapter
            );
        }
    }
}
