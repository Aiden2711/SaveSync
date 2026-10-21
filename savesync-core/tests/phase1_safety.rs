//! Phase 1 integration tests: enrollment gating, verified snapshots, staged
//! restores, rollback, and crash recovery.

use std::fs;
use std::path::{Path, PathBuf};

use savesync_core::db::Db;
use savesync_core::manifest::Manifest;
use savesync_core::models::{AdapterType, RestoreJournalEntry, RestoreState};
use savesync_core::paths::sanitize_relative_path;
use savesync_core::restore::{DataPaths, RestoreMode};
use savesync_core::store;
use savesync_core::{Error, Game, SaveSyncEngine};

struct Fixture {
    _temp: tempfile::TempDir,
    save_dir: PathBuf,
    data_root: PathBuf,
    engine: SaveSyncEngine,
    game: Game,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let save_dir = temp.path().join("live_save");
        fs::create_dir_all(save_dir.join("slot1")).unwrap();
        fs::write(save_dir.join("slot1/a.bin"), b"alpha-v1").unwrap();
        fs::write(save_dir.join("slot1/b.bin"), b"beta-v1").unwrap();

        let data_root = temp.path().join("data");
        let mut engine = SaveSyncEngine::open(&data_root).unwrap();
        let game = engine
            .enroll_game("Test Game", AdapterType::CustomDirectory)
            .unwrap();
        engine.add_directory_location(&game.id, &save_dir).unwrap();

        Self {
            _temp: temp,
            save_dir,
            data_root,
            engine,
            game,
        }
    }

    fn live(&self, relative: &str) -> PathBuf {
        self.save_dir
            .join(sanitize_relative_path(relative).unwrap())
    }
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn discovered_games_cannot_be_snapshotted_until_enrolled() {
    let temp = tempfile::tempdir().unwrap();
    let save_dir = temp.path().join("saves");
    fs::create_dir_all(&save_dir).unwrap();
    fs::write(save_dir.join("a.sav"), b"data").unwrap();

    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let candidate = engine
        .register_candidate("Discovered", AdapterType::Ludusavi)
        .unwrap();
    assert!(!candidate.enrolled);
    engine
        .add_directory_location(&candidate.id, &save_dir)
        .unwrap();

    let error = engine.snapshot_game(&candidate.id, None).unwrap_err();
    assert!(matches!(error, Error::InvalidState(_)));

    assert!(
        engine
            .list_games(true)
            .unwrap()
            .iter()
            .all(|game| game.id != candidate.id)
    );

    let enrolled = engine.enroll(&candidate.id).unwrap();
    assert!(enrolled.enrolled);
    let snapshot = engine.snapshot_game(&candidate.id, Some("first")).unwrap();
    assert_eq!(snapshot.file_count, 1);
}

#[test]
fn unlink_keeps_locations_history_and_blobs_but_blocks_snapshots() {
    let mut fixture = Fixture::new();
    let snapshot = fixture
        .engine
        .snapshot_game(&fixture.game.id, Some("before unlink"))
        .unwrap();
    let manifest = fixture
        .engine
        .manifest_for_revision(&snapshot.revision_id)
        .unwrap();
    assert_eq!(manifest.hash(), snapshot.manifest_hash);

    let blob_path = fixture
        .engine
        .paths()
        .blobs()
        .join(&snapshot.manifest_hash[..2])
        .join(format!("{}.tar.zst", snapshot.manifest_hash));
    assert!(
        blob_path.is_file(),
        "snapshot blob must exist before unlink"
    );

    let unlinked = fixture.engine.unlink(&fixture.game.id).unwrap();
    assert_eq!(unlinked.id, fixture.game.id);
    assert!(!unlinked.enrolled);

    // Gone from the library, still present in storage.
    assert!(
        fixture
            .engine
            .list_games(true)
            .unwrap()
            .iter()
            .all(|game| game.id != fixture.game.id)
    );
    let stored = fixture
        .engine
        .list_games(false)
        .unwrap()
        .into_iter()
        .find(|game| game.id == fixture.game.id)
        .expect("unlinked game must remain stored");
    assert!(!stored.enrolled);

    // Revisions, manifests, blobs, and locations all survive.
    let history = fixture.engine.history(&fixture.game.id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, snapshot.revision_id);
    assert_eq!(
        fixture
            .engine
            .manifest_for_revision(&snapshot.revision_id)
            .unwrap()
            .hash(),
        snapshot.manifest_hash
    );
    assert!(blob_path.is_file(), "unlink must not delete blob data");
    assert!(fixture.data_root.join("savesync.db").is_file());

    let locations = fixture.engine.list_locations(&fixture.game.id).unwrap();
    assert_eq!(locations.len(), 1, "unlink must not delete locations");
    assert_eq!(
        PathBuf::from(&locations[0].path),
        fixture.save_dir.canonicalize().unwrap()
    );

    // Snapshots are refused while unlinked and create no revisions.
    let revisions_before = history.len();
    let error = fixture
        .engine
        .snapshot_game(&fixture.game.id, Some("must fail"))
        .unwrap_err();
    assert!(matches!(error, Error::InvalidState(_)));
    assert_eq!(
        fixture.engine.history(&fixture.game.id).unwrap().len(),
        revisions_before
    );

    // Re-enrolling restores library access to the same identity and history.
    let reenrolled = fixture.engine.enroll(&fixture.game.id).unwrap();
    assert!(reenrolled.enrolled);
    assert_eq!(
        fixture.engine.history(&fixture.game.id).unwrap().len(),
        revisions_before
    );
    assert!(
        fixture
            .engine
            .list_games(true)
            .unwrap()
            .iter()
            .any(|game| game.id == fixture.game.id)
    );
}

#[test]
fn snapshot_restore_roundtrip_restores_bytes_and_records_backup() {
    let mut fixture = Fixture::new();

    let v1 = fixture
        .engine
        .snapshot_game(&fixture.game.id, Some("v1"))
        .unwrap();
    assert_eq!(v1.file_count, 2);

    // Advance live state: modify one file, add another, delete a third.
    write(&fixture.live("slot1/a.bin"), b"alpha-v2");
    write(&fixture.live("extra.txt"), b"new-file");
    fs::remove_file(fixture.live("slot1/b.bin")).unwrap();

    let v2 = fixture
        .engine
        .snapshot_game(&fixture.game.id, Some("v2"))
        .unwrap();
    assert_eq!(v2.file_count, 2);

    let diff = fixture
        .engine
        .diff(&v1.revision_id, &v2.revision_id)
        .unwrap();
    assert_eq!(
        diff.added
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["extra.txt"]
    );
    assert_eq!(
        diff.removed
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["slot1/b.bin"]
    );
    assert_eq!(
        diff.modified
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["slot1/a.bin"]
    );

    let report = fixture
        .engine
        .restore_with_mode(&v1.revision_id, RestoreMode::ExactMirror)
        .unwrap();
    assert_eq!(report.files_restored, 2);
    assert_eq!(fs::read(fixture.live("slot1/a.bin")).unwrap(), b"alpha-v1");
    assert_eq!(fs::read(fixture.live("slot1/b.bin")).unwrap(), b"beta-v1");
    assert!(!fixture.live("extra.txt").exists());

    // The safety backup preserves exactly what was live before the restore.
    let backup_dir = PathBuf::from(&report.safety_backup_path);
    assert!(backup_dir.is_dir());
    assert_eq!(fs::read(backup_dir.join("extra.txt")).unwrap(), b"new-file");
    assert_eq!(
        fs::read(backup_dir.join("slot1/a.bin")).unwrap(),
        b"alpha-v2"
    );

    // Live tree now matches the restored manifest exactly.
    let manifest = fixture
        .engine
        .manifest_for_revision(&v1.revision_id)
        .unwrap();
    manifest.verify_dir(&fixture.save_dir).unwrap();

    // Active head moved to the restored revision.
    assert_eq!(
        fixture
            .engine
            .get_game(&fixture.game.id)
            .unwrap()
            .active_head_revision_id
            .as_deref(),
        Some(v1.revision_id.as_str())
    );
}

#[test]
fn overlay_mode_never_deletes_unrelated_files() {
    let mut fixture = Fixture::new();
    let v1 = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();

    write(&fixture.live("keep-me.txt"), b"user data");
    write(&fixture.live("slot1/a.bin"), b"changed");

    fixture.engine.restore(&v1.revision_id).unwrap();

    assert_eq!(fs::read(fixture.live("slot1/a.bin")).unwrap(), b"alpha-v1");
    assert_eq!(fs::read(fixture.live("keep-me.txt")).unwrap(), b"user data");
}

#[test]
fn corrupted_blob_aborts_restore_without_touching_live_files() {
    let mut fixture = Fixture::new();
    let v1 = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();

    let blob = fixture
        .engine
        .paths()
        .blobs()
        .join(&v1.manifest_hash[..2])
        .join(format!("{}.tar.zst", v1.manifest_hash));
    assert!(blob.is_file());
    fs::write(&blob, b"not a valid zstd stream").unwrap();

    write(&fixture.live("slot1/a.bin"), b"live-after-corruption");

    let error = fixture.engine.restore(&v1.revision_id).unwrap_err();
    assert!(
        !matches!(error, Error::RestoreFailed { .. }),
        "corruption must be detected before live files are touched: {error:?}"
    );
    assert_eq!(
        fs::read(fixture.live("slot1/a.bin")).unwrap(),
        b"live-after-corruption"
    );
    assert_eq!(fs::read(fixture.live("slot1/b.bin")).unwrap(), b"beta-v1");
}

#[test]
fn failed_install_rolls_back_to_pre_restore_state() {
    let mut fixture = Fixture::new();
    let v1 = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();

    // Replace a file that the snapshot will install with a directory, which
    // makes the atomic rename fail after the first files may already be in
    // place. The rollback must restore the pre-restore bytes.
    fs::remove_file(fixture.live("slot1/b.bin")).unwrap();
    fs::create_dir_all(fixture.live("slot1/b.bin")).unwrap();
    write(&fixture.live("slot1/a.bin"), b"live-before-failure");

    let error = fixture
        .engine
        .restore_with_mode(&v1.revision_id, RestoreMode::ExactMirror)
        .unwrap_err();
    match &error {
        Error::RestoreFailed { rollback, .. } => {
            assert!(
                rollback.contains("restored"),
                "rollback message: {rollback}"
            );
        }
        other => panic!("expected a restore failure, got {other:?}"),
    }

    assert_eq!(
        fs::read(fixture.live("slot1/a.bin")).unwrap(),
        b"live-before-failure"
    );

    let journal = fixture.engine.restore_journal(&fixture.game.id).unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].state, RestoreState::RolledBack);
    assert!(
        journal[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("restored")
    );
}

#[test]
fn interrupted_restore_is_recovered_from_safety_backup() {
    let mut fixture = Fixture::new();
    fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();

    // Simulate a crash that happened after the journal row was written but
    // before the live tree was finalised: live files are in a half-applied
    // state and a prepared journal + safety backup exist.
    let restore_id = store::new_id();
    let backup_dir = fixture
        .data_root
        .join("rollback")
        .join(&fixture.game.id)
        .join(&restore_id);
    fs::create_dir_all(backup_dir.join("slot1")).unwrap();
    fs::write(backup_dir.join("slot1/a.bin"), b"alpha-v1").unwrap();
    fs::write(backup_dir.join("slot1/b.bin"), b"beta-v1").unwrap();
    let backup_manifest = Manifest::from_dir(&backup_dir).unwrap();

    write(&fixture.live("slot1/a.bin"), b"half-applied");
    write(&fixture.live("added-by-restore.bin"), b"junk");

    let data_root = fixture.data_root.clone();
    let game_id = fixture.game.id.clone();
    let save_dir = fixture.save_dir.clone();
    let path_json = backup_manifest.to_json().unwrap();

    // Release the engine's SQLite handle before writing the journal.
    drop(fixture.engine);

    let mut db = Db::open(DataPaths::new(&data_root).database()).unwrap();
    let now = store::now_iso();
    db.transaction(|tx| {
        store::insert_journal(
            tx,
            &RestoreJournalEntry {
                id: restore_id.clone(),
                game_id: game_id.clone(),
                revision_id: "unknown-revision".into(),
                target_root: save_dir.to_string_lossy().into_owned(),
                staging_path: data_root
                    .join("staging")
                    .join(&restore_id)
                    .to_string_lossy()
                    .into_owned(),
                backup_path: backup_dir.to_string_lossy().into_owned(),
                backup_manifest: path_json,
                state: RestoreState::Prepared,
                deleted_paths: Vec::new(),
                error: None,
                created_at: now.clone(),
                updated_at: now,
            },
        )
    })
    .unwrap();
    assert_eq!(
        store::list_in_flight_restores(db.connection())
            .unwrap()
            .len(),
        1,
        "journal row must be visible before recovery"
    );
    drop(db);

    let mut engine = SaveSyncEngine::open(&data_root).unwrap();
    let recovered = engine.recover().unwrap();
    assert_eq!(recovered, 1);

    assert_eq!(fs::read(save_dir.join("slot1/a.bin")).unwrap(), b"alpha-v1");
    assert_eq!(fs::read(save_dir.join("slot1/b.bin")).unwrap(), b"beta-v1");
    assert!(!save_dir.join("added-by-restore.bin").exists());
    backup_manifest.verify_dir(&save_dir).unwrap();

    let db = Db::open(DataPaths::new(&data_root).database()).unwrap();
    let journal = store::get_journal(db.connection(), &restore_id)
        .unwrap()
        .expect("journal row");
    assert_eq!(journal.state, RestoreState::RolledBack);
    drop(db);
    assert_eq!(engine.recover().unwrap(), 0);
}

#[test]
fn revisions_form_a_dag_and_conflicts_can_be_resolved() {
    let mut fixture = Fixture::new();
    let v1 = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();
    write(&fixture.live("slot1/a.bin"), b"alpha-v2");
    let v2 = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();

    let history = fixture.engine.history(&fixture.game.id).unwrap();
    assert_eq!(history.len(), 2);
    let v2_row = history.iter().find(|r| r.id == v2.revision_id).unwrap();
    assert_eq!(v2_row.parent_ids, vec![v1.revision_id.clone()]);

    let heads = fixture.engine.heads(&fixture.game.id).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, v2.revision_id);

    let conflict = fixture
        .engine
        .record_conflict(
            &fixture.game.id,
            &[v1.revision_id.clone(), v2.revision_id.clone()],
        )
        .unwrap();
    assert_eq!(
        fixture
            .engine
            .open_conflicts(&fixture.game.id)
            .unwrap()
            .len(),
        1
    );

    let resolution = fixture
        .engine
        .resolve_conflict_with(&conflict.id, &v2.revision_id)
        .unwrap();
    assert_eq!(resolution.parent_ids.len(), 2);
    assert!(
        fixture
            .engine
            .open_conflicts(&fixture.game.id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn internal_temp_artifacts_are_not_snapshotted() {
    let mut fixture = Fixture::new();
    write(&fixture.live(".savesync-tmp-stale"), b"leftover");
    let snapshot = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();
    assert_eq!(snapshot.file_count, 2);
    let manifest = fixture
        .engine
        .manifest_for_revision(&snapshot.revision_id)
        .unwrap();
    assert!(manifest.find(".savesync-tmp-stale").is_none());
}

#[test]
fn device_identity_is_stable_across_reopens() {
    let mut fixture = Fixture::new();
    let first = fixture.engine.device().id.clone();
    fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();
    drop(fixture.engine);

    let engine = SaveSyncEngine::open(&fixture.data_root).unwrap();
    assert_eq!(engine.device().id, first);
    assert!(!engine.device().name.is_empty());
}

#[test]
fn empty_save_directories_produce_valid_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let save_dir = temp.path().join("empty_save");
    fs::create_dir_all(&save_dir).unwrap();
    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let game = engine
        .enroll_game("Empty", AdapterType::CustomDirectory)
        .unwrap();
    engine.add_directory_location(&game.id, &save_dir).unwrap();

    let snapshot = engine.snapshot_game(&game.id, None).unwrap();
    assert_eq!(snapshot.file_count, 0);
    assert_eq!(snapshot.total_bytes, 0);

    write(&save_dir.join("new.sav"), b"created later");
    engine
        .restore_with_mode(&snapshot.revision_id, RestoreMode::ExactMirror)
        .unwrap();
    assert!(!save_dir.join("new.sav").exists());
}

#[cfg(unix)]
#[test]
fn snapshots_reject_symlinks_instead_of_following_them() {
    use std::os::unix::fs::symlink;

    let mut fixture = Fixture::new();
    let outside = fixture._temp.path().join("outside-secret.txt");
    fs::write(&outside, b"secret").unwrap();
    symlink(&outside, fixture.live("slot1/link.bin")).unwrap();

    let error = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap_err();
    assert!(matches!(error, Error::InvalidState(_)));
    // No revision may be created from a partially walked directory.
    assert!(fixture.engine.history(&fixture.game.id).unwrap().is_empty());
}

#[test]
fn identical_content_deduplicates_blobs_but_not_revisions() {
    let mut fixture = Fixture::new();
    let first = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();
    let second = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();

    assert_eq!(first.manifest_hash, second.manifest_hash);
    assert_eq!(first.blob_hash, second.blob_hash);
    assert_ne!(first.revision_id, second.revision_id);

    let blob_path = fixture
        .engine
        .paths()
        .blobs()
        .join(&first.manifest_hash[..2])
        .join(format!("{}.tar.zst", first.manifest_hash));
    assert!(blob_path.is_file());
    assert_eq!(fixture.engine.history(&fixture.game.id).unwrap().len(), 2);
}

#[test]
fn restore_refuses_targets_outside_enrolled_locations() {
    let mut fixture = Fixture::new();
    let snapshot = fixture
        .engine
        .snapshot_game(&fixture.game.id, None)
        .unwrap();
    let elsewhere = fixture._temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();

    let error = fixture
        .engine
        .restore_to(&snapshot.revision_id, elsewhere, RestoreMode::Overlay)
        .unwrap_err();
    assert!(matches!(error, Error::InvalidState(_)));
}

#[test]
fn recovery_marks_missing_backups_as_failed_without_panicking() {
    let temp = tempfile::tempdir().unwrap();
    let save_dir = temp.path().join("save");
    fs::create_dir_all(&save_dir).unwrap();
    let data_root = temp.path().join("data");
    let mut engine = SaveSyncEngine::open(&data_root).unwrap();
    let game = engine
        .enroll_game("Game", AdapterType::CustomDirectory)
        .unwrap();
    engine.add_directory_location(&game.id, &save_dir).unwrap();
    let snapshot = engine.snapshot_game(&game.id, None).unwrap();
    drop(engine);

    let mut db = Db::open(DataPaths::new(&data_root).database()).unwrap();
    let now = store::now_iso();
    db.transaction(|tx| {
        store::insert_journal(
            tx,
            &RestoreJournalEntry {
                id: store::new_id(),
                game_id: game.id.clone(),
                revision_id: snapshot.revision_id.clone(),
                target_root: save_dir.to_string_lossy().into_owned(),
                staging_path: data_root
                    .join("staging/missing")
                    .to_string_lossy()
                    .into_owned(),
                backup_path: data_root
                    .join("rollback/missing")
                    .to_string_lossy()
                    .into_owned(),
                backup_manifest: Manifest::empty().to_json().unwrap(),
                state: RestoreState::Prepared,
                deleted_paths: Vec::new(),
                error: None,
                created_at: now.clone(),
                updated_at: now,
            },
        )
    })
    .unwrap();
    drop(db);

    let mut engine = SaveSyncEngine::open(&data_root).unwrap();
    assert_eq!(engine.recover().unwrap(), 0);
    let journal = engine.restore_journal(&game.id).unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].state, RestoreState::RollbackFailed);
    assert!(journal[0].error.is_some());
}
