//! Platform adapter contract.
//!
//! Every enrolled save location belongs to exactly one [`AdapterType`], and
//! that type decides three things: how the location is discovered, how it is
//! captured as a snapshot, and how a snapshot is written back. This module
//! states that decision as an explicit [`PlatformAdapter`] trait so a platform
//! can change its on-disk layout without changing the engine.
//!
//! # Contract mapping
//!
//! | AGENTS.md interface | Method |
//! | --- | --- |
//! | `DiscoverLocations(game_id)` | [`PlatformAdapter::discover`] |
//! | `SnapshotLocations(mapping_id)` | [`PlatformAdapter::snapshot_location`] |
//! | `RestoreLocations(manifest, mapping_id)` | [`PlatformAdapter::restore_location`] |
//!
//! Two deliberate departures from that shape are documented where they appear:
//!
//! - Discovery is scan-based, not addressable by game id. SaveSync's sources
//!   (Ludusavi, Lutris, Steam/Proton) enumerate installed games and report the
//!   identity they found, so [`PlatformAdapter::discover`] takes no game id;
//!   [`LocationCandidate::title`] and [`LocationCandidate::source`] carry it.
//! - `mapping_id` is the `locations.id` of the stored mapping, which the schema
//!   defines as a UUID `TEXT` primary key (`sql/001_initial.sql`), not a rowid.
//!
//! Phase 2 is behaviour-preserving: one kind-agnostic filesystem
//! implementation (see [`filesystem`]) serves every adapter kind, because
//! snapshot and restore are currently identical for all of them. Platform
//! layouts that genuinely differ (JKSV exports, emulator memory cards) become
//! their own [`PlatformAdapter`] implementations in Phase 3.

pub mod discovery_sources;
pub mod filesystem;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::blob::BlobStore;
use crate::db::Db;
use crate::discovery::{DiscoveredGame, DiscoverySource};
use crate::error::Result;
use crate::manifest::Manifest;
use crate::models::{AdapterType, RestoreReport};
use crate::restore::{DataPaths, RestoreMode};

pub use discovery_sources::{LudusaviAdapter, LutrisAdapter, SteamAdapter};
pub use filesystem::{CustomDirectoryAdapter, EmulatorAdapter, JksvAdapter};

/// What a restore returns. Alias of the report
/// [`crate::restore::restore_revision`] already produces, named here so the
/// adapter contract does not have to borrow the restore module's vocabulary.
pub type RestoreResult = RestoreReport;

/// One save location proposed for explicit enrollment.
///
/// A candidate is only ever *proposed*: discovery must never enroll anything,
/// so a caller has to ask the user before a candidate becomes a stored mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationCandidate {
    /// Canonical, enrollable save directory. Already passed
    /// [`crate::paths::validate_discovered_path`]; never a remote share, never
    /// a path that escapes through `..`, never a symlink.
    pub path: PathBuf,
    /// The game this location belongs to, as named by the source that found it.
    pub title: String,
    /// The adapter kind that produced the candidate.
    pub adapter_type: AdapterType,
    /// Which scan produced the candidate (Ludusavi entry, Lutris slug, Steam
    /// app id, Proton prefix, ...).
    pub source: DiscoverySource,
    /// Candidates the same scan had to drop for this game because enrollment
    /// would refuse them.
    ///
    /// It is repeated on every location of one game so that grouping the
    /// locations back into a [`DiscoveredGame`] can report the drop count once
    /// per game.
    pub skipped_paths: usize,
}

/// Engine-owned state a restore needs.
///
/// An adapter never reimplements journaling, the pre-restore safety backup, or
/// rollback: it hands this context to
/// [`crate::restore::restore_revision`], which owns that transaction. The
/// adapter's only decision is what should be true on disk when the transaction
/// finishes.
pub struct RestoreContext<'a> {
    pub db: &'a mut Db,
    pub blobs: &'a BlobStore,
    pub paths: &'a DataPaths,
    /// Game whose files are being replaced. Must stay enrolled.
    pub game_id: &'a str,
    /// Revision being restored; its digest is re-verified from the database.
    pub revision_id: &'a str,
    pub mode: RestoreMode,
}

/// How one adapter kind discovers, captures, and restores save locations.
///
/// Implementations must be cheap to hold in a `static`, must never enroll
/// anything on their own, and must never weaken the path validation in
/// [`crate::paths`].
pub trait PlatformAdapter: Send + Sync {
    /// The kind this adapter serves. One adapter per [`AdapterType`].
    fn adapter_type(&self) -> AdapterType;

    /// Scan this adapter's own sources and propose locations for explicit
    /// enrollment.
    ///
    /// `db` is passed because a source may keep a cache in SQLite (Ludusavi's
    /// ETag-cached manifest). Adapters without a discovery source return no
    /// candidates: discovery must not invent locations, and a user-chosen
    /// directory or an emulator path reaches SaveSync through explicit
    /// enrollment instead.
    fn discover(&self, db: &mut Db) -> Result<Vec<LocationCandidate>>;

    /// Validate a stored mapping root before it is read or written.
    ///
    /// Returns the canonical root the caller must use, so that the tree that is
    /// hashed is the tree that was validated.
    fn validate_mapping(&self, root: &Path) -> Result<PathBuf>;

    /// Capture one location (`mapping_id`) as an immutable manifest.
    fn snapshot_location(&self, mapping_id: &str, root: &Path) -> Result<Manifest>;

    /// Restore `manifest` into `target`, which is the stored mapping `mapping_id`.
    ///
    /// The whole transaction — staging, verification, safety backup, journal,
    /// rollback — belongs to [`crate::restore`]; the manifest argument lets an
    /// adapter inspect the layout it is about to write.
    fn restore_location(
        &self,
        context: RestoreContext<'_>,
        manifest: &Manifest,
        mapping_id: &str,
        target: &Path,
    ) -> Result<RestoreResult>;
}

static LUDUSAVI: LudusaviAdapter = LudusaviAdapter;
static LUTRIS: LutrisAdapter = LutrisAdapter::system();
static STEAM: SteamAdapter = SteamAdapter::system();
static CUSTOM_DIRECTORY: CustomDirectoryAdapter = CustomDirectoryAdapter;
static EMULATOR: EmulatorAdapter = EmulatorAdapter;
static JKSV: JksvAdapter = JksvAdapter;

/// The adapter responsible for one kind of save location.
///
/// Dispatch has no fallible path on purpose: an unknown kind cannot occur,
/// because [`AdapterType`] is a closed enum and every arm is answered here.
pub fn adapter_for(kind: AdapterType) -> &'static dyn PlatformAdapter {
    match kind {
        AdapterType::Ludusavi => &LUDUSAVI,
        AdapterType::Lutris => &LUTRIS,
        AdapterType::Steam => &STEAM,
        AdapterType::CustomDirectory => &CUSTOM_DIRECTORY,
        AdapterType::Emulator => &EMULATOR,
        AdapterType::Jksv => &JKSV,
    }
}

/// The adapters that can find games on their own, in scan order.
///
/// Order matters only for which source a merged candidate keeps; results are
/// sorted by title afterwards.
pub fn discovery_sources() -> [&'static dyn PlatformAdapter; 3] {
    [&LUDUSAVI, &LUTRIS, &STEAM]
}

/// Scan every discovery source and merge the results per game.
///
/// A source that cannot be scanned contributes nothing instead of failing the
/// scan: discovery is advisory, reads third-party metadata, and must never let
/// one broken source hide the others.
pub fn discover_all(db: &mut Db) -> Vec<DiscoveredGame> {
    let mut candidates = Vec::new();
    for adapter in discovery_sources() {
        if let Ok(found) = adapter.discover(db) {
            candidates.extend(found);
        }
    }
    merge_candidates(candidates)
}

/// Flatten per-game scan results into one [`LocationCandidate`] per location.
pub(crate) fn candidates_from_discovered(games: Vec<DiscoveredGame>) -> Vec<LocationCandidate> {
    let mut candidates = Vec::new();
    for game in games {
        for path in game.save_paths {
            candidates.push(LocationCandidate {
                path,
                title: game.title.clone(),
                adapter_type: game.adapter_type,
                source: game.source.clone(),
                skipped_paths: game.skipped_paths,
            });
        }
    }
    candidates
}

/// Group candidates back into one [`DiscoveredGame`] per normalized title.
///
/// Paths keep the order their source scanned them in, duplicates collapse, and
/// the drop count of each `(title, source)` scan is added exactly once even
/// though every one of its locations carries it.
fn merge_candidates(
    candidates: impl IntoIterator<Item = LocationCandidate>,
) -> Vec<DiscoveredGame> {
    let mut games: Vec<DiscoveredGame> = Vec::new();
    let mut position_by_title: HashMap<String, usize> = HashMap::new();
    let mut counted: HashSet<(String, DiscoverySource)> = HashSet::new();

    for candidate in candidates {
        let key = candidate.title.trim().to_lowercase();
        let position = match position_by_title.get(&key) {
            Some(&position) => position,
            None => {
                games.push(DiscoveredGame {
                    title: candidate.title.clone(),
                    adapter_type: candidate.adapter_type,
                    save_paths: Vec::new(),
                    source: candidate.source.clone(),
                    skipped_paths: 0,
                });
                position_by_title.insert(key, games.len() - 1);
                games.len() - 1
            }
        };

        let game = &mut games[position];
        if counted.insert((candidate.title.clone(), candidate.source.clone())) {
            game.skipped_paths += candidate.skipped_paths;
        }
        if !game.save_paths.contains(&candidate.path) {
            game.save_paths.push(candidate.path);
        }
    }

    games.sort_by(|a, b| a.title.cmp(&b.title));
    games
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::blob::BlobStore;
    use crate::models::{Device, LocationKind, Revision};
    use crate::restore::{self, RestoreRequest};
    use crate::store;

    const ALL_KINDS: [AdapterType; 6] = [
        AdapterType::Ludusavi,
        AdapterType::Lutris,
        AdapterType::Steam,
        AdapterType::CustomDirectory,
        AdapterType::Emulator,
        AdapterType::Jksv,
    ];

    #[test]
    fn registry_returns_one_adapter_per_kind() {
        for kind in ALL_KINDS {
            assert_eq!(
                adapter_for(kind).adapter_type(),
                kind,
                "dispatch for {kind:?} must be answered by an adapter of that kind"
            );
        }
    }

    #[test]
    fn only_scanning_adapters_are_discovery_sources() {
        let sources: Vec<AdapterType> = discovery_sources()
            .iter()
            .map(|adapter| adapter.adapter_type())
            .collect();
        assert_eq!(
            sources,
            vec![
                AdapterType::Ludusavi,
                AdapterType::Lutris,
                AdapterType::Steam
            ]
        );
    }

    #[test]
    fn validate_mapping_rejects_paths_enrollment_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("saves");
        fs::create_dir_all(&existing).unwrap();
        let adapter = adapter_for(AdapterType::CustomDirectory);

        for bad in [
            // Remote share or device name: never a local save directory.
            PathBuf::from(r"\\server\share\saves"),
            // Escapes through a parent component.
            existing.join("..").join("saves"),
            // Relative paths are not enrollable.
            PathBuf::from("relative/saves"),
            // Must exist.
            dir.path().join("missing"),
        ] {
            assert!(
                adapter.validate_mapping(&bad).is_err(),
                "expected rejection for {}",
                bad.display()
            );
        }

        assert_eq!(
            adapter.validate_mapping(&existing).unwrap(),
            existing.canonicalize().unwrap()
        );
    }

    #[test]
    fn every_kind_shares_the_kind_agnostic_filesystem_behaviour() {
        // Phase 2 invariant: no kind has its own on-disk behaviour yet, so all
        // six must produce the same snapshot for the same directory.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("saves");
        fs::create_dir_all(root.join("slot1")).unwrap();
        fs::write(root.join("slot1").join("data.bin"), b"payload").unwrap();
        fs::write(root.join("profile.sav"), b"profile").unwrap();

        let expected = Manifest::from_dir(&root).unwrap().to_json().unwrap();
        for kind in ALL_KINDS {
            let manifest = adapter_for(kind)
                .snapshot_location("mapping-1", &root)
                .unwrap();
            assert_eq!(
                manifest.to_json().unwrap(),
                expected,
                "{kind:?} must produce byte-identical snapshot JSON"
            );
        }
    }

    #[test]
    fn discovery_candidates_round_trip_through_their_games() {
        // Two sources report the same title: paths concatenate in scan order
        // and the per-scan drop counts add up exactly once per group.
        let games = vec![
            DiscoveredGame {
                title: "Same Game".to_string(),
                adapter_type: AdapterType::Steam,
                save_paths: vec![PathBuf::from("/steam/one"), PathBuf::from("/steam/two")],
                source: DiscoverySource::Steam { appid: 1 },
                skipped_paths: 2,
            },
            DiscoveredGame {
                title: "Same Game".to_string(),
                adapter_type: AdapterType::Steam,
                save_paths: vec![PathBuf::from("/steam/two"), PathBuf::from("/steam/three")],
                source: DiscoverySource::Proton { appid: 1 },
                skipped_paths: 3,
            },
            DiscoveredGame {
                title: "Another Game".to_string(),
                adapter_type: AdapterType::Lutris,
                save_paths: vec![PathBuf::from("/lutris/one")],
                source: DiscoverySource::Lutris {
                    slug: "another-game".to_string(),
                },
                skipped_paths: 0,
            },
        ];

        let candidates = candidates_from_discovered(games);
        assert_eq!(
            candidates.len(),
            5,
            "one candidate per location, not per game"
        );

        // Grouping collapses the two "Same Game" scans into one entry: paths
        // concatenate in scan order, the first source wins, and both drop
        // counts are added once. Titles are sorted by the merge.
        let merged = merge_candidates(candidates);
        assert_eq!(merged.len(), 2, "one entry per normalized title");
        assert_eq!(merged[0].title, "Another Game");
        assert_eq!(
            merged[1],
            DiscoveredGame {
                title: "Same Game".to_string(),
                adapter_type: AdapterType::Steam,
                save_paths: vec![
                    PathBuf::from("/steam/one"),
                    PathBuf::from("/steam/two"),
                    PathBuf::from("/steam/three"),
                ],
                source: DiscoverySource::Steam { appid: 1 },
                skipped_paths: 5,
            }
        );
    }

    /// Data root, database, blob store, and one enrolled game with a location.
    struct RestoreFixture {
        _dir: tempfile::TempDir,
        db: Db,
        blobs: BlobStore,
        paths: DataPaths,
        game_id: String,
        location_id: String,
        revision: Revision,
        manifest: Manifest,
        target: PathBuf,
    }

    impl RestoreFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = DataPaths::new(dir.path().join("data"));
            let mut db = Db::open(paths.database()).unwrap();
            let blobs = BlobStore::open(paths.blobs()).unwrap();

            let device = Device {
                id: store::new_id(),
                name: "adapter test".to_string(),
                platform: std::env::consts::OS.to_string(),
                created_at: store::now_iso(),
                last_seen_at: store::now_iso(),
            };
            store::upsert_device(db.connection(), &device).unwrap();

            let game = db
                .transaction(|tx| {
                    store::create_game(tx, "Fixture Game", AdapterType::CustomDirectory, true)
                })
                .unwrap();

            // Snapshot source and its enrolled live copy.
            let source = dir.path().join("source");
            fs::create_dir_all(source.join("slot1")).unwrap();
            fs::write(source.join("slot1").join("data.bin"), b"snapshot payload").unwrap();
            fs::write(source.join("profile.sav"), b"profile").unwrap();

            let target = dir.path().join("live");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("profile.sav"), b"edited").unwrap();
            fs::write(target.join("stale.sav"), b"stale").unwrap();

            let location = store::add_location(
                db.connection(),
                &game.id,
                &target.to_string_lossy(),
                LocationKind::Directory,
            )
            .unwrap();

            let manifest = Manifest::from_dir(&source).unwrap();
            let blob_hash = blobs.put(&manifest, &source).unwrap();
            let manifest_hash = store::store_manifest(db.connection(), &manifest).unwrap();
            let revision = Revision {
                id: store::new_id(),
                game_id: game.id.clone(),
                parent_ids: Vec::new(),
                device_id: device.id.clone(),
                created_at: store::now_iso(),
                manifest_hash,
                blob_hash: Some(blob_hash),
                message: None,
            };
            store::insert_revision(db.connection(), &revision).unwrap();

            Self {
                _dir: dir,
                db,
                blobs,
                paths,
                game_id: game.id,
                location_id: location.id,
                revision,
                manifest,
                target,
            }
        }

        fn reset_target(&self) {
            fs::remove_dir_all(&self.target).unwrap();
            fs::create_dir_all(&self.target).unwrap();
            fs::write(self.target.join("profile.sav"), b"edited").unwrap();
            fs::write(self.target.join("stale.sav"), b"stale").unwrap();
        }
    }

    /// Digest of the files a snapshot knows about, as they now exist on disk.
    ///
    /// An overlay restore leaves unrelated live files in place and every
    /// restore writes a fresh `mtime_ms`, so neither may take part in the
    /// comparison; [`Manifest::hash`] already ignores order and modification
    /// times.
    fn restored_digest(target: &Path, snapshot: &Manifest) -> String {
        let live = Manifest::from_dir(target).unwrap();
        let entries = live
            .entries
            .into_iter()
            .filter(|entry| {
                snapshot
                    .entries
                    .iter()
                    .any(|known| known.path == entry.path)
            })
            .collect();
        Manifest {
            version: live.version,
            entries,
        }
        .hash()
    }

    #[test]
    fn restore_through_the_adapter_matches_the_direct_transaction() {
        let mut fixture = RestoreFixture::new();
        let adapter = adapter_for(AdapterType::CustomDirectory);

        let via_adapter = adapter
            .restore_location(
                RestoreContext {
                    db: &mut fixture.db,
                    blobs: &fixture.blobs,
                    paths: &fixture.paths,
                    game_id: &fixture.game_id,
                    revision_id: &fixture.revision.id,
                    mode: RestoreMode::Overlay,
                },
                &fixture.manifest,
                &fixture.location_id,
                &fixture.target,
            )
            .unwrap();
        let adapter_tree = restored_digest(&fixture.target, &fixture.manifest);
        assert_eq!(adapter_tree, fixture.manifest.hash());
        assert!(
            fixture.target.join("stale.sav").exists(),
            "an overlay restore leaves unrelated live files alone"
        );

        fixture.reset_target();
        let direct = restore::restore_revision(
            &mut fixture.db,
            &fixture.blobs,
            &fixture.paths,
            RestoreRequest {
                game_id: fixture.game_id.clone(),
                revision_id: fixture.revision.id.clone(),
                target_root: fixture.target.clone(),
                mode: RestoreMode::Overlay,
            },
        )
        .unwrap();

        assert_eq!(via_adapter.files_restored, direct.files_restored);
        assert_eq!(via_adapter.bytes_restored, direct.bytes_restored);
        assert_eq!(via_adapter.revision_id, direct.revision_id);
        assert_eq!(
            restored_digest(&fixture.target, &fixture.manifest),
            adapter_tree
        );
    }
}
