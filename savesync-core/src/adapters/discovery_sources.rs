//! Adapters that can find games on their own.
//!
//! Each of these wraps one existing discovery scan: the scan itself (placeholder
//! expansion, Steam VDF parsing, Lutris YAML parsing, and the
//! [`crate::paths::validate_discovered_path`] filtering with its drop count)
//! stays in [`crate::discovery`], so discovery keeps behaving exactly as it did
//! before the adapter contract existed.
//!
//! Their snapshot, restore, and validation methods are still the kind-agnostic
//! filesystem behaviour: a Ludusavi or Steam location is an ordinary directory
//! tree today.

use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::discovery;
use crate::error::Result;
use crate::manifest::Manifest;
use crate::models::AdapterType;

use super::filesystem;
use super::{
    LocationCandidate, PlatformAdapter, RestoreContext, RestoreResult, candidates_from_discovered,
};

/// Locations from the Ludusavi manifest, cached in SQLite and refreshed by ETag.
///
/// The scan needs a database handle for that cache, which is why
/// [`PlatformAdapter::discover`] passes one.
pub struct LudusaviAdapter;

impl PlatformAdapter for LudusaviAdapter {
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Ludusavi
    }

    fn discover(&self, db: &mut Db) -> Result<Vec<LocationCandidate>> {
        Ok(candidates_from_discovered(discovery::discover_ludusavi(db)))
    }

    fn validate_mapping(&self, root: &Path) -> Result<PathBuf> {
        filesystem::validate_mapping(root)
    }

    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest> {
        filesystem::snapshot_location(mapping_id, root)
    }

    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult> {
        filesystem::restore_location(context, manifest, mapping_id, target)
    }
}

/// Locations from Lutris game configuration files.
///
/// `config_dirs` overrides the detected system directories; `None` means "scan
/// the standard Lutris locations", which is what the registry uses.
pub struct LutrisAdapter {
    config_dirs: Option<Vec<PathBuf>>,
}

impl LutrisAdapter {
    /// Scan the Lutris directories detected from the environment.
    pub const fn system() -> Self {
        Self { config_dirs: None }
    }

    /// Scan an explicit set of Lutris game directories instead.
    pub fn in_dirs(config_dirs: Vec<PathBuf>) -> Self {
        Self {
            config_dirs: Some(config_dirs),
        }
    }
}

impl PlatformAdapter for LutrisAdapter {
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Lutris
    }

    fn discover(&self, _db: &mut Db) -> Result<Vec<LocationCandidate>> {
        let games = match &self.config_dirs {
            Some(dirs) => discovery::discover_lutris_in(dirs.clone()),
            None => discovery::discover_lutris(),
        };
        Ok(candidates_from_discovered(games))
    }

    fn validate_mapping(&self, root: &Path) -> Result<PathBuf> {
        filesystem::validate_mapping(root)
    }

    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest> {
        filesystem::snapshot_location(mapping_id, root)
    }

    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult> {
        filesystem::restore_location(context, manifest, mapping_id, target)
    }
}

/// Locations from Steam and Proton libraries.
///
/// `libraries` overrides the detected library roots; `None` means "scan every
/// installed library", which is what the registry uses.
pub struct SteamAdapter {
    libraries: Option<Vec<PathBuf>>,
}

impl SteamAdapter {
    /// Scan the Steam libraries detected from the environment.
    pub const fn system() -> Self {
        Self { libraries: None }
    }

    /// Scan an explicit set of Steam library roots instead.
    pub fn in_libraries(libraries: Vec<PathBuf>) -> Self {
        Self {
            libraries: Some(libraries),
        }
    }
}

impl PlatformAdapter for SteamAdapter {
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Steam
    }

    fn discover(&self, _db: &mut Db) -> Result<Vec<LocationCandidate>> {
        let games = match &self.libraries {
            Some(libraries) => discovery::discover_steam_in(libraries),
            None => discovery::discover_steam(),
        };
        Ok(candidates_from_discovered(games))
    }

    fn validate_mapping(&self, root: &Path) -> Result<PathBuf> {
        filesystem::validate_mapping(root)
    }

    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest> {
        filesystem::snapshot_location(mapping_id, root)
    }

    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult> {
        filesystem::restore_location(context, manifest, mapping_id, target)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::adapters::merge_candidates;

    fn memory_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// Create `<dir>/drive_c/users/steamuser` and return the prefix root.
    fn make_prefix(dir: &Path, name: &str) -> PathBuf {
        let prefix = dir.join(name);
        fs::create_dir_all(prefix.join("drive_c").join("users").join("steamuser")).unwrap();
        prefix
    }

    /// Write a Lutris game entry pointing at `prefix`.
    fn write_lutris_game(dir: &Path, file: &str, name: &str, prefix: &Path) {
        fs::create_dir_all(dir).unwrap();
        // Forward slashes keep the YAML free of escape sequences on Windows.
        let prefix = prefix.to_string_lossy().replace('\\', "/");
        let yaml = format!("name: \"{name}\"\ngame:\n  prefix: \"{prefix}\"\n");
        fs::write(dir.join(file), yaml).unwrap();
    }

    #[cfg(unix)]
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[test]
    fn lutris_adapter_reports_the_same_games_as_the_direct_scan() {
        let dir = tempfile::tempdir().unwrap();
        let games = dir.path().join("games");

        let good = make_prefix(dir.path(), "good-prefix");
        write_lutris_game(&games, "good.yml", "Good Game", &good);

        // The same prefix spelled with a parent component: the candidate exists
        // but resolves through `..`, exactly what enrollment rejects.
        let escaping = good.join("..").join("good-prefix");
        write_lutris_game(&games, "escaping.yml", "Escaping Game", &escaping);

        let mut db = memory_db();
        let through_adapter = LutrisAdapter::in_dirs(vec![games.clone()])
            .discover(&mut db)
            .unwrap();
        let direct = discovery::discover_lutris_in(vec![games]);

        assert_eq!(merge_candidates(through_adapter), direct);
    }

    #[test]
    fn lutris_adapter_never_proposes_a_symlinked_location() {
        // A symlinked save directory is not enrollable, so it must not appear
        // as a candidate even though the Lutris entry names it.
        let dir = tempfile::tempdir().unwrap();
        let games = dir.path().join("games");
        let prefix = dir.path().join("linked-prefix");
        let users = prefix.join("drive_c").join("users");
        fs::create_dir_all(&users).unwrap();

        let real = dir.path().join("real-saves");
        fs::create_dir_all(&real).unwrap();
        if !try_symlink_dir(&real, &users.join("steamuser")) {
            // Creating symlinks is privilege-gated on Windows; nothing to prove.
            return;
        }
        write_lutris_game(&games, "linked.yml", "Linked Game", &prefix);

        let mut db = memory_db();
        let found = LutrisAdapter::in_dirs(vec![games])
            .discover(&mut db)
            .unwrap();
        assert!(
            found.is_empty(),
            "a symlinked save directory is not proposed for enrollment"
        );
    }

    #[test]
    fn steam_adapter_reports_the_same_games_as_the_direct_scan() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("steamlib");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("appmanifest_480.acf"),
            r#""AppState"
{
    "appid"      "480"
    "name"       "Steam Test"
    "installdir" "SteamTest"
}
"#,
        )
        .unwrap();
        let proton_pfx = steamapps
            .join("compatdata")
            .join("480")
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser");
        fs::create_dir_all(&proton_pfx).unwrap();

        let mut db = memory_db();
        let through_adapter = SteamAdapter::in_libraries(vec![library.clone()])
            .discover(&mut db)
            .unwrap();
        let mut direct = discovery::discover_steam_in(&[library]);
        // The merge sorts by title, so compare against the same ordering.
        direct.sort_by(|a, b| a.title.cmp(&b.title));

        assert_eq!(merge_candidates(through_adapter), direct);
    }
}
