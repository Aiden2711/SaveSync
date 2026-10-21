//! Phase 2 groundwork tests: graph-aware sync through a mockable provider,
//! divergence detection, and explicit conflict resolution.
//!
//! No network is involved: `LocalFolderProvider` stands in for Google Drive.

use std::fs;
use std::path::Path;

use savesync_core::{
    AdapterType, LocalFolderProvider, RestoreMode, SaveSyncEngine, SyncProvider, SyncReport,
};

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn assert_synced(
    report: &SyncReport,
    pushed: usize,
    pulled: usize,
    uploaded: usize,
    downloaded: usize,
) {
    assert_eq!(report.pushed_revisions, pushed, "pushed: {report:?}");
    assert_eq!(report.pulled_revisions, pulled, "pulled: {report:?}");
    assert_eq!(
        report.uploaded_blobs, uploaded,
        "uploaded blobs: {report:?}"
    );
    assert_eq!(
        report.downloaded_blobs, downloaded,
        "downloaded blobs: {report:?}"
    );
}

#[test]
fn revision_and_blob_travel_to_a_second_device_without_auto_restore() {
    let temp = tempfile::tempdir().unwrap();
    let provider = LocalFolderProvider::open(temp.path().join("remote")).unwrap();

    // Device A creates and publishes a snapshot.
    let a_save = temp.path().join("a_save");
    write(&a_save.join("slot1/a.bin"), b"alpha-from-a");
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game_a = a
        .enroll_game("Shared Game", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game_a.id, &a_save).unwrap();
    let v1 = a.snapshot_game(&game_a.id, Some("first")).unwrap();

    let report = a.sync(&provider, &game_a.id).unwrap();
    assert_synced(&report, 1, 0, 1, 0);
    assert_eq!(report.heads, 1);

    // Device B adopts the same game identity and location, then pulls.
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    let game_b = b
        .enroll_game_with_id(&game_a.id, "Shared Game", AdapterType::CustomDirectory)
        .unwrap();
    b.add_directory_location(&game_b.id, &b_save).unwrap();

    let report = b.pull(&provider, &game_a.id).unwrap();
    assert_synced(&report, 0, 1, 0, 1);
    assert_eq!(b.history(&game_b.id).unwrap().len(), 1);

    // Pulling must not write save files: B's directory stays empty.
    assert!(fs::read_dir(&b_save).unwrap().next().is_none());

    // Applying the revision is an explicit restore.
    b.restore(&v1.revision_id).unwrap();
    assert_eq!(
        fs::read(b_save.join("slot1/a.bin")).unwrap(),
        b"alpha-from-a"
    );
}

#[test]
fn divergent_heads_create_a_conflict_and_resolution_keeps_live_files() {
    let temp = tempfile::tempdir().unwrap();
    let provider = LocalFolderProvider::open(temp.path().join("remote")).unwrap();

    // Common base: device A snapshots and publishes v1.
    let a_save = temp.path().join("a_save");
    write(&a_save.join("progress.sav"), b"base");
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game = a
        .enroll_game("Diverging", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game.id, &a_save).unwrap();
    let v1 = a.snapshot_game(&game.id, Some("base")).unwrap();
    a.sync(&provider, &game.id).unwrap();

    // Device B pulls and restores the base so both devices start from v1.
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    b.enroll_game_with_id(&game.id, "Diverging", AdapterType::CustomDirectory)
        .unwrap();
    b.add_directory_location(&game.id, &b_save).unwrap();
    b.pull(&provider, &game.id).unwrap();
    b.restore(&v1.revision_id).unwrap();

    // Advance both devices independently from the same parent.
    write(&a_save.join("progress.sav"), b"progress-on-a");
    let v2a = a.snapshot_game(&game.id, Some("on a")).unwrap();
    write(&b_save.join("progress.sav"), b"progress-on-b");
    let v2b = b.snapshot_game(&game.id, Some("on b")).unwrap();
    assert_eq!(
        b.history(&game.id)
            .unwrap()
            .iter()
            .find(|r| r.id == v2b.revision_id)
            .unwrap()
            .parent_ids,
        vec![v1.revision_id.clone()]
    );

    // Publish A's head, then let B merge both histories.
    a.push(&provider, &game.id).unwrap();
    let report = b.sync(&provider, &game.id).unwrap();
    assert_synced(&report, 1, 1, 1, 1);
    assert_eq!(report.heads, 2);
    assert_eq!(report.conflict_ids.len(), 1);

    let conflicts = b.open_conflicts(&game.id).unwrap();
    assert_eq!(conflicts.len(), 1);
    let mut conflicting = conflicts[0].revision_ids.clone();
    conflicting.sort();
    let mut expected = vec![v2a.revision_id.clone(), v2b.revision_id.clone()];
    expected.sort();
    assert_eq!(conflicting, expected);

    // Re-syncing must not create duplicate conflict rows.
    let report = b.sync(&provider, &game.id).unwrap();
    assert_eq!(report.conflict_ids.len(), 1);
    assert_eq!(b.open_conflicts(&game.id).unwrap().len(), 1);

    // Resolution creates a two-parent commit but does not touch live files.
    let resolution = b
        .resolve_conflict_with(&conflicts[0].id, &v2a.revision_id)
        .unwrap();
    assert_eq!(resolution.parent_ids.len(), 2);
    assert!(b.open_conflicts(&game.id).unwrap().is_empty());
    assert_eq!(b.heads(&game.id).unwrap().len(), 1);
    assert_eq!(
        fs::read(b_save.join("progress.sav")).unwrap(),
        b"progress-on-b"
    );

    // The resolution is published and visible on the other device.
    b.push(&provider, &game.id).unwrap();
    let report = a.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 2); // v2b and the resolution commit
    let heads = a.heads(&game.id).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, resolution.id);

    // Applying the chosen state is still an explicit restore.
    a.restore_with_mode(&resolution.id, RestoreMode::ExactMirror)
        .unwrap();
    assert_eq!(
        fs::read(a_save.join("progress.sav")).unwrap(),
        b"progress-on-a"
    );
}

#[test]
fn corrupted_remote_blob_is_rejected_before_it_enters_local_storage() {
    let temp = tempfile::tempdir().unwrap();
    let provider = LocalFolderProvider::open(temp.path().join("remote")).unwrap();

    let save = temp.path().join("save");
    write(&save.join("a.bin"), b"payload");
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game = a
        .enroll_game("Corrupt", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game.id, &save).unwrap();
    let snapshot = a.snapshot_game(&game.id, None).unwrap();
    a.push(&provider, &game.id).unwrap();

    let remote_blob = provider
        .root()
        .join("blobs")
        .join(&snapshot.manifest_hash[..2])
        .join(format!("{}.tar.zst", snapshot.manifest_hash));
    fs::write(&remote_blob, b"garbage").unwrap();

    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    b.enroll_game_with_id(&game.id, "Corrupt", AdapterType::CustomDirectory)
        .unwrap();
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    b.add_directory_location(&game.id, &b_save).unwrap();

    let error = b.pull(&provider, &game.id).unwrap_err();
    assert!(
        matches!(error, savesync_core::Error::InvalidArchive(_)),
        "unexpected error: {error:?}"
    );
    assert!(b.restore(&snapshot.revision_id).is_err());
}

#[test]
fn candidates_are_never_synced() {
    let temp = tempfile::tempdir().unwrap();
    let provider = LocalFolderProvider::open(temp.path().join("remote")).unwrap();
    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let candidate = engine
        .register_candidate("Not enrolled", AdapterType::Ludusavi)
        .unwrap();

    let error = engine.sync(&provider, &candidate.id).unwrap_err();
    assert!(matches!(error, savesync_core::Error::InvalidState(_)));
    assert!(provider.list_revisions(&candidate.id).unwrap().is_empty());
}
