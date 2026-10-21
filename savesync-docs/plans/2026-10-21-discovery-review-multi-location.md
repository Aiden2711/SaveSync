# Discovery Review & Multi-Location Saves Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Discovery tab that scans for save games, filters and multi-selects them, enrolls only on explicit user action, and lets one game own several save folders that snapshot and restore as a single revision.

**Architecture:** Discovery stays in `savesync-core/src/discovery.rs` and returns candidates plus match metadata. The engine gains location planning, a 16-location cap, and multi-root snapshots using prefixed manifest entries (`loc-<slot>/`). Restore groups entries by slot, backs up and restores each root, and journals every root so a crash or failure rolls back all of them. The desktop adds a Discovery view over new Tauri commands.

**Tech Stack:** Rust 2024, rusqlite (SQLite/WAL), tar + zstd, Tauri 2, React 19 + TypeScript, Vite.

**Spec:** `savesync-docs/specs/2026-10-21-discovery-review-multi-location-design.md`

## Global Constraints

- Discovery never enrolls anything. Enrollment requires an explicit user action.
- A scan never writes to a save folder.
- Every path passes `savesync_core::paths::validate_root` before enrollment.
- Maximum 16 directory locations per game, enforced in the engine.
- Maximum 1,000 discovery candidates per scan; report `total` and `truncated`.
- One snapshot covers every location and produces exactly one revision.
- Manifest format version is stored per manifest set; existing v1 hashes must never change.
- Safety backups are verified before the restore journal row is written.
- Restore refuses before any live write when a slot has no local location.
- All archive and manifest paths stay sanitized; `loc-<slot>/` prefixes are generated, never user input.
- Core commands: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Desktop commands: `npm run build`, `cargo clippy --all-targets -- -D warnings`.

## Review Focus

1. **Old revisions after upgrade.** A stored v1 manifest must load with version 1 and its original hash; otherwise sync and restore reject existing history. Pinned by Task 5 and Task 8 tests.
2. **Partial multi-location restore.** A failure while installing the second root must roll the first root back from its verified backup. Pinned by Task 8.
3. **Overlapping folders.** Adding a folder that is a parent or child of an existing location must be rejected, not written twice. Pinned by Task 3.
4. **Broad profile roots.** A Proton/Lutris profile root must be flagged, unchecked by default, and refused without explicit confirmation. Pinned by Tasks 1 and 3.
5. **Title split/merge.** `"Foo"` and `"Foo (Proton)"` must be one candidate and match one existing game, including unlinked games. Pinned by Tasks 2 and 3.

---

### Task 1: Discovery Types, Match Key, Category, Broad Detection

**Files:**
- Modify: `savesync-core/src/discovery.rs`
- Test: `savesync-core/src/discovery.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum DiscoveryCategory { Emulator, Steam, Local }` (`serde(rename_all = "snake_case")`)
  - `pub struct SavePath { pub path: PathBuf, pub exists: bool, pub broad: bool }`
  - `pub struct ExistingMatch { pub game_id: String, pub enrolled: bool, pub new_paths: Vec<PathBuf> }`
  - `pub fn match_key(title: &str) -> String`
  - `pub fn categorize(sources: &[DiscoverySource], paths: &[SavePath]) -> DiscoveryCategory`
  - `pub fn is_broad_path(path: &Path) -> bool`
  - `DiscoveredGame` gains `sources: Vec<DiscoverySource>`, `category`, `match_key`, `existing: Option<ExistingMatch>`, and `save_paths: Vec<SavePath>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn match_key_merges_proton_and_case() {
    assert_eq!(match_key("Hades"), match_key("  hades "));
    assert_eq!(match_key("Hades"), match_key("Hades (Proton)"));
    assert_ne!(match_key("Hades"), match_key("Hades II"));
}

#[test]
fn category_prefers_emulator_then_steam_then_local() {
    let emu = vec![SavePath { path: "/games/retroarch/saves".into(), exists: true, broad: false }];
    let steam = vec![SavePath { path: "/steam/userdata/1/2/remote".into(), exists: true, broad: false }];
    assert_eq!(categorize(&[DiscoverySource::Steam { appid: 2 }], &emu), DiscoveryCategory::Emulator);
    assert_eq!(categorize(&[DiscoverySource::Steam { appid: 2 }], &steam), DiscoveryCategory::Steam);
    assert_eq!(categorize(&[DiscoverySource::Lutris { slug: "x".into() }], &steam), DiscoveryCategory::Emulator);
    assert_eq!(categorize(&[DiscoverySource::Ludusavi { title: "x".into() }], &steam), DiscoveryCategory::Local);
}

#[test]
fn broad_paths_are_flagged() {
    assert!(is_broad_path(Path::new("/steam/steamapps/compatdata/1/pfx/drive_c/users/steamuser")));
    assert!(is_broad_path(Path::new("/steam/steamapps/compatdata/1/pfx/drive_c/users")));
    assert!(!is_broad_path(Path::new("/steam/steamapps/compatdata/1/pfx/drive_c/users/steamuser/Documents/MyGame")));
    assert!(!is_broad_path(Path::new("/home/me/.local/share/Hades/Saved")));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test discovery::tests::match_key_merges_proton_and_case discovery::tests::category_prefers discovery::tests::broad_paths -- --nocapture`
Expected: FAIL to compile (`match_key`, `SavePath`, `DiscoveryCategory` missing).

- [ ] **Step 3: Implement the types and helpers**

`match_key`: Unicode lowercase, trim, collapse whitespace, strip a trailing `" (proton)"` case-insensitively.
`is_broad_path`: true only when the path ends at `drive_c/users` or `drive_c/users/<one user>`, or equals the home directory. Deeper paths are not broad.
`categorize`: Emulator when any path component matches `retroarch|pcsx2|rpcs3|dolphin|ryujinx|yuzu|duckstation|ppsspp|melonds|mgba` (case-insensitive) or sources are exactly `[Lutris]`; else Steam when any source is Steam/Proton; else Local.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test discovery::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/discovery.rs
git commit -m "feat(core): add discovery category and match helpers"
```

---

### Task 2: Discovery Aggregation, Existing Matches, Report Shape

**Files:**
- Modify: `savesync-core/src/discovery.rs`
- Modify: `savesync-core/src/engine.rs`
- Modify: `savesync-core/src/lib.rs`
- Test: `savesync-core/src/discovery.rs`, `savesync-core/src/engine.rs`

**Interfaces:**
- Consumes: Task 1 types and `match_key`.
- Produces:
  - `pub fn merge_candidates(candidates: Vec<DiscoveredGame>) -> Vec<DiscoveredGame>`
  - `discover_all_games(db: &mut Db) -> Vec<DiscoveredGame>` now sets `sources`, `category`, `match_key`, `existing`
  - `pub struct DiscoveryReport { pub games: Vec<DiscoveredGame>, pub total: usize, pub truncated: bool }`
  - `SaveSyncEngine::discover_games(&mut self) -> Result<DiscoveryReport>`
  - `lib.rs` re-exports `DiscoveryCategory`, `SavePath`, `ExistingMatch`, `DiscoveryReport`.
  - `store::find_game_by_title_key(conn, key: &str) -> Result<Option<Game>>` is **not** added; matching happens in `engine`/`discovery` via `list_games(conn, false)` plus `match_key`.

- [ ] **Step 1: Write the failing tests**

Inline in `discovery.rs` (unit tests can call `pub(crate)` helpers and `Db::open_in_memory`):

```rust
#[test]
fn merge_keeps_all_sources_and_paths() {
    let a = candidate("Hades", AdapterType::Ludusavi, DiscoverySource::Ludusavi { title: "Hades".into() }, "/saves/hades");
    let b = candidate("Hades (Proton)", AdapterType::Emulator, DiscoverySource::Proton { appid: 7 }, "/steam/compatdata/7");
    let merged = merge_candidates(vec![a, b]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].sources.len(), 2);
    assert_eq!(merged[0].save_paths.len(), 2);
    assert_eq!(merged[0].category, DiscoveryCategory::Steam);
}

#[test]
fn existing_match_reports_only_new_paths() {
    let mut db = Db::open_in_memory().unwrap();
    let game = store::create_game(db.connection(), "Hades", AdapterType::CustomDirectory, true).unwrap();
    store::add_location(db.connection(), &game.id, "/saves/old", LocationKind::Directory).unwrap();
    let mut candidates = vec![candidate_with_paths("Hades", &["/saves/old", "/saves/new"])];
    attach_existing_matches(&db, &mut candidates);
    let existing = candidates[0].existing.as_ref().unwrap();
    assert_eq!(existing.game_id, game.id);
    assert_eq!(existing.new_paths, vec![PathBuf::from("/saves/new")]);
}
```

In `engine.rs` add a `#[cfg(test)] mod tests` for the pure cap helper:

```rust
#[test]
fn report_truncates_at_one_thousand() {
    let games = (0..1_001).map(|i| candidate(&format!("G{i}"))).collect();
    let report = cap_report(games);
    assert_eq!(report.games.len(), 1_000);
    assert_eq!(report.total, 1_001);
    assert!(report.truncated);
}
```

Helper signatures to add: `fn candidate(title: &str, adapter: AdapterType, source: DiscoverySource, path: &str) -> DiscoveredGame`, `fn candidate_with_paths(title: &str, paths: &[&str]) -> DiscoveredGame`, `pub(crate) fn attach_existing_matches(db: &Db, candidates: &mut [DiscoveredGame])`, `pub(crate) fn cap_report(games: Vec<DiscoveredGame>) -> DiscoveryReport`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test discovery::tests::merge_keeps discovery::tests::existing_match engine::tests::report_truncates -- --nocapture`
Expected: FAIL to compile (`DiscoveryReport`, `merge_candidates` missing).

- [ ] **Step 3: Implement aggregation and the report**

- Rewrite scanner output to `SavePath { path, exists: path.is_dir(), broad: is_broad_path(&path) }` and collect `sources`.
- Keep Steam's `"<name> (Proton)"` candidate; `merge_candidates` merges it with the plain title by `match_key`.
- Merge sources with `PartialEq` dedupe, merge paths by exact path, recompute `category`.
- `attach_existing_matches` lists all games (enrolled and unlinked) and their locations, compares `match_key(game.title)`, and fills `ExistingMatch.new_paths`.
- `engine.discover_games` calls `discover_all_games`, then `cap_report` (1,000 max).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test discovery::tests engine::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/discovery.rs savesync-core/src/engine.rs savesync-core/src/lib.rs
git commit -m "feat(core): aggregate discovery sources and report truncation"
```

---

### Task 3: Enrollment Planning, Engine Add APIs, Global Cap

**Files:**
- Modify: `savesync-core/src/discovery.rs`
- Modify: `savesync-core/src/engine.rs`
- Modify: `savesync-core/src/error.rs` (only if a dedicated variant is needed; otherwise reuse `InvalidState`)
- Modify: `savesync-core/src/lib.rs`
- Test: `savesync-core/src/discovery.rs`, `tests/phase1_safety.rs`

**Interfaces:**
- Consumes: Task 1/2 types, `paths::validate_root`.
- Produces:
  - `pub const MAX_LOCATIONS_PER_GAME: usize = 16` in `engine.rs`
  - `pub struct SkippedPath { pub path: PathBuf, pub reason: SkipReason }`
  - `pub enum SkipReason { Missing, Duplicate, Nested, Overlapping, TooManyLocations, BroadUnconfirmed }`
  - `pub struct EnrollOutcome { pub game: Game, pub added: Vec<Location>, pub skipped: Vec<SkippedPath> }`
  - `lib.rs` re-exports `SkippedPath`, `SkipReason`, `EnrollOutcome`.
  - `pub(crate) fn plan_locations(existing: &[Location], requested: &[PathBuf], allow_broad: bool) -> (Vec<PathBuf>, Vec<SkippedPath>)`
  - `SaveSyncEngine::enroll_discovered_candidate(&mut self, title: &str, adapter: AdapterType, paths: &[PathBuf], allow_broad: bool) -> Result<EnrollOutcome>`
  - `SaveSyncEngine::add_discovered_locations(&mut self, game_id: &str, paths: &[PathBuf], allow_broad: bool) -> Result<EnrollOutcome>`
  - `SaveSyncEngine::find_game_by_match_key(&self, key: &str) -> Result<Option<Game>>`
  - `add_directory_location` enforces the cap.

- [ ] **Step 1: Write the failing tests**

Unit tests in `discovery.rs` for `plan_locations`:

```rust
#[test]
fn plan_rejects_nested_and_overlapping_paths() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("Game");
    let child = parent.join("saves");
    fs::create_dir_all(&child).unwrap();
    let existing = vec![location(&parent)];
    let (planned, skipped) = plan_locations(&existing, &[child.clone()], false);
    assert!(planned.is_empty());
    assert_eq!(skipped[0].reason, SkipReason::Overlapping);

    let (planned, skipped) = plan_locations(&[], &[parent.clone(), child], false);
    assert_eq!(planned, vec![parent]);
    assert_eq!(skipped[0].reason, SkipReason::Nested);
}

#[test]
fn broad_path_requires_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let broad = dir.path().join("drive_c/users/steamuser");
    fs::create_dir_all(&broad).unwrap();
    let (planned, skipped) = plan_locations(&[], &[broad.clone()], false);
    assert!(planned.is_empty());
    assert_eq!(skipped[0].reason, SkipReason::BroadUnconfirmed);
    let (planned, _) = plan_locations(&[], &[broad], true);
    assert_eq!(planned.len(), 1);
}
```

Integration tests in `tests/phase1_safety.rs`:

```rust
#[test]
fn seventeenth_location_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let game = engine.enroll_game("Many", AdapterType::CustomDirectory).unwrap();
    let dirs: Vec<_> = (0..16).map(|i| tempdir_with_file(&format!("d{i}/save.dat"), b"x")).collect();
    for dir in &dirs { engine.add_directory_location(&game.id, dir.path()).unwrap(); }
    let extra = tempdir_with_file("extra/save.dat", b"x");
    let error = engine.add_directory_location(&game.id, extra.path()).unwrap_err();
    assert!(error.to_string().contains("16"));
}

#[test]
fn duplicate_title_refuses_and_unlinked_title_is_found() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let save = tempdir_with_file("saves/save.dat", b"x");
    let game = engine.enroll_game("Hades", AdapterType::CustomDirectory).unwrap();
    engine.add_directory_location(&game.id, save.path()).unwrap();
    let error = engine.enroll_discovered_candidate("hades", AdapterType::Ludusavi, &[], false).unwrap_err();
    assert!(error.to_string().contains("already"));
    engine.unlink(&game.id).unwrap();
    assert_eq!(engine.find_game_by_match_key("Hades").unwrap().unwrap().id, game.id);
}
```

Add `fn tempdir_with_file(relative: &str, bytes: &[u8]) -> tempfile::TempDir` to `phase1_safety.rs`; it creates the file under a fresh temp dir and returns the dir. Add `fn location(path: &Path) -> Location` in the `discovery.rs` test module.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test discovery::tests::plan_rejects discovery::tests::broad_path -- --nocapture`
Run: `cargo test --test phase1_safety seventeenth_location duplicate_title -- --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Implement planning and engine methods**

`plan_locations` canonicalizes each requested path with `validate_root`; records `Missing` on failure. Deduplicates canonical paths (`Duplicate`). Sorts by path length, keeps a path only if no already-kept or existing location is an ancestor (`Nested`/`Overlapping`). Rejects broad paths unless `allow_broad`.
`add_directory_location` counts existing locations and returns `InvalidState("game <id> already has 16 save locations")` at the cap. `enroll_discovered_candidate` refuses when `find_game_by_match_key` returns any game (enrolled or not), with a message naming the existing id; the UI decides re-link vs add.
`EnrollOutcome.added` returns the created `Location` rows.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test discovery::tests -- --nocapture`
Run: `cargo test --test phase1_safety -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/discovery.rs savesync-core/src/engine.rs savesync-core/src/lib.rs savesync-core/src/error.rs
git commit -m "feat(core): validate and enroll discovered locations"
```

---

### Task 4: Schema v4 for Slots, Manifest Version, Revision Locations, Journal

**Files:**
- Create: `savesync-core/src/sql/004_multi_location.sql`
- Modify: `savesync-core/src/db.rs`
- Modify: `savesync-core/src/models.rs`
- Modify: `savesync-core/src/store.rs`
- Test: `savesync-core/src/db.rs`, `savesync-core/src/store.rs`

**Interfaces:**
- Produces:
  - `Location { id, game_id, path, kind, slot: u32 }`
  - `store::add_location` assigns `COALESCE(MAX(slot), -1) + 1` per game.
  - `store::list_locations` orders by `slot`.
  - `store::set_revision_locations(conn, revision_id: &str, slots: &[(u32, String)]) -> Result<()>`
  - `store::revision_locations(conn, revision_id: &str) -> Result<Vec<(u32, String)>>`
  - `manifest_sets.format_version` and `restore_journal.locations_json` columns exist.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn migration_backfills_slots_and_revision_locations() {
    let mut db = open_at_version(3); // test helper: migrations 1-3 applied, user_version = 3
    db.connection().execute_batch(
        "INSERT INTO games (id,title,adapter_type,enrolled,created_at,updated_at) VALUES ('g','G','custom_directory',1,'t','t');
         INSERT INTO locations (id,game_id,path,kind) VALUES ('l1','g','/a','directory'),('l2','g','/b','directory');
         INSERT INTO devices (id,name,platform,created_at,last_seen_at) VALUES ('d','D','p','t','t');
         INSERT INTO revisions (id,game_id,parent_ids,device_id,created_at,manifest_hash,blob_hash) VALUES ('r','g','[]','d','t','m',NULL);",
    ).unwrap();
    db.migrate().unwrap();
    let slots: Vec<u32> = db.connection().prepare("SELECT slot FROM locations ORDER BY rowid").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(slots, vec![0, 1]);
    let mapped: i64 = db.connection().query_row("SELECT COUNT(*) FROM revision_locations WHERE revision_id='r'", [], |r| r.get(0)).unwrap();
    assert_eq!(mapped, 1); // v1 revisions map to the game's first location
}
```

Add the test helper `fn open_at_version(version: u32) -> Db` inside `db.rs`'s test module: open a raw `Connection::open_in_memory`, apply `MIGRATIONS[..version]` in order, set `PRAGMA user_version = version`, then return `Db { conn }`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test db::tests::migration_backfills -- --nocapture`
Expected: FAIL (no column `slot`).

- [ ] **Step 3: Write migration 004 and update models/store**

`004_multi_location.sql`:
```sql
ALTER TABLE locations ADD COLUMN slot INTEGER NOT NULL DEFAULT 0;
UPDATE locations SET slot = (
  SELECT COUNT(*) FROM locations l2
  WHERE l2.game_id = locations.game_id AND l2.rowid < locations.rowid
);
CREATE UNIQUE INDEX locations_game_slot ON locations (game_id, slot);

ALTER TABLE manifest_sets ADD COLUMN format_version INTEGER NOT NULL DEFAULT 1;

CREATE TABLE revision_locations (
  revision_id TEXT NOT NULL REFERENCES revisions(id) ON DELETE CASCADE,
  slot        INTEGER NOT NULL,
  location_id TEXT NOT NULL,
  PRIMARY KEY (revision_id, slot)
);

INSERT INTO revision_locations (revision_id, slot, location_id)
SELECT r.id, l.slot, l.id
FROM revisions r
JOIN locations l ON l.game_id = r.game_id
WHERE l.slot = (SELECT MIN(l2.slot) FROM locations l2 WHERE l2.game_id = r.game_id);

ALTER TABLE restore_journal ADD COLUMN locations_json TEXT;
```
Register the file in `MIGRATIONS`. Update `Location` and `store::add_location`/`list_locations`; keep existing callers compiling.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test db::tests store::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/sql/004_multi_location.sql savesync-core/src/db.rs savesync-core/src/models.rs savesync-core/src/store.rs
git commit -m "feat(core): add location slots and revision mapping schema"
```

---

### Task 5: Stored Manifest Version and v2 Prefix Helpers

**Files:**
- Modify: `savesync-core/src/manifest.rs`
- Modify: `savesync-core/src/store.rs`
- Test: `savesync-core/src/manifest.rs`, `savesync-core/src/store.rs`

**Interfaces:**
- Produces:
  - `MANIFEST_FORMAT_VERSION = 2`
  - `Manifest::new_with_version(version: u32, entries: Vec<FileEntry>) -> Result<Manifest>`
  - `Manifest::from_dir_prefixed(root: &Path, slot: u32) -> Result<Vec<FileEntry>>`
  - `pub fn prefix_for_slot(slot: u32) -> String` (`"loc-3"`)
  - `pub fn parse_slot_prefix(path: &str) -> Result<Option<(u32, String)>>`
  - `store::store_manifest` writes `format_version`; `store::load_manifest` reads it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn load_preserves_v1_version_and_hash() {
    let mut db = Db::open_in_memory().unwrap();
    let entries = vec![FileEntry { path: "a.bin".into(), size: 1, sha256: "0".repeat(64), mtime_ms: None }];
    let v1 = Manifest::new_with_version(1, entries).unwrap();
    store::store_manifest(db.connection(), &v1).unwrap();
    let loaded = store::load_manifest(db.connection(), &v1.hash()).unwrap();
    assert_eq!(loaded.version, 1);
    assert_eq!(loaded.hash(), v1.hash());
}

#[test]
fn prefixed_entries_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("save.dat"), b"x").unwrap();
    let entries = Manifest::from_dir_prefixed(dir.path(), 3).unwrap();
    assert_eq!(entries[0].path, "loc-3/save.dat");
    assert_eq!(parse_slot_prefix(&entries[0].path).unwrap(), Some((3, "save.dat".into())));
    assert!(parse_slot_prefix("../loc-3/x").is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test manifest::tests::load_preserves manifest::tests::prefixed store::tests -- --nocapture`
Expected: FAIL to compile (`new_with_version`, `from_dir_prefixed` missing).

- [ ] **Step 3: Implement version handling and prefixes**

`from_json` must parse `version` from the document and call `new_with_version`. `store_manifest` inserts `format_version` into `manifest_sets`; `load_manifest` selects it and calls `new_with_version`. `from_dir_prefixed` reuses `from_dir` internals but prefixes each entry after `relative_to`. `parse_slot_prefix` requires the first component to be `loc-<digits>` and the rest to pass `sanitize_relative_path`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test manifest::tests store::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/manifest.rs savesync-core/src/store.rs
git commit -m "feat(core): preserve manifest versions and add slot prefixes"
```

---

### Task 6: Multi-Root Blob Writer

**Files:**
- Modify: `savesync-core/src/blob.rs`
- Test: `savesync-core/src/blob.rs`

**Interfaces:**
- Consumes: `prefix_for_slot`, `parse_slot_prefix`.
- Produces: `BlobStore::put_multi(&self, manifest: &Manifest, roots: &[(u32, PathBuf)]) -> Result<String>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn put_multi_archives_each_slot_from_its_root() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    fs::write(a.path().join("one.dat"), b"one").unwrap();
    fs::write(b.path().join("two.dat"), b"two").unwrap();
    let entries = [Manifest::from_dir_prefixed(a.path(), 0).unwrap(), Manifest::from_dir_prefixed(b.path(), 1).unwrap()].concat();
    let manifest = Manifest::new_with_version(2, entries).unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let store = BlobStore::open(blob_dir.path()).unwrap();
    store.put_multi(&manifest, &[(0, a.path().into()), (1, b.path().into())]).unwrap();
    let out = tempfile::tempdir().unwrap();
    store.extract_verified(&manifest, out.path()).unwrap();
    assert_eq!(fs::read(out.path().join("loc-0/one.dat")).unwrap(), b"one");
    assert_eq!(fs::read(out.path().join("loc-1/two.dat")).unwrap(), b"two");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test blob::tests::put_multi -- --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `put_multi`**

Factor the archive loop to accept a resolver: `fn archive_source(entry: &str, roots: &[(u32, PathBuf)]) -> Result<(PathBuf, String)>` returning the source path and the relative path inside the root. The archive entry name stays `entry.path` (already prefixed). Missing slot in `roots` is `InvalidState`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test blob::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/blob.rs
git commit -m "feat(core): archive multiple save roots into one blob"
```

---

### Task 7: Multi-Location Snapshot

**Files:**
- Modify: `savesync-core/src/engine.rs`
- Modify: `savesync-core/src/store.rs`
- Test: `savesync-core/src/engine.rs`, `tests/phase1_safety.rs`

**Interfaces:**
- Consumes: Task 4 slots, Task 5 prefixes/version, Task 6 `put_multi`.
- Produces: `snapshot_game` walks every location, stores one v2 manifest, and writes `revision_locations`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn snapshot_covers_every_location_in_one_revision() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let game = engine.enroll_game("Two Folders", AdapterType::CustomDirectory).unwrap();
    let a = tempdir_with_file("a/save.dat", b"a");
    let b = tempdir_with_file("b/save.dat", b"b");
    engine.add_directory_location(&game.id, a.path()).unwrap();
    engine.add_directory_location(&game.id, b.path()).unwrap();
    let info = engine.snapshot_game(&game.id, None).unwrap();
    let manifest = engine.manifest_for_revision(&info.revision_id).unwrap();
    assert_eq!(manifest.version, 2);
    assert!(manifest.entries.iter().any(|e| e.path.starts_with("loc-0/")));
    assert!(manifest.entries.iter().any(|e| e.path.starts_with("loc-1/")));
}

#[test]
fn snapshot_fails_closed_when_a_location_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(temp.path().join("data")).unwrap();
    let game = engine.enroll_game("Gone", AdapterType::CustomDirectory).unwrap();
    let a = tempdir_with_file("a/save.dat", b"a");
    engine.add_directory_location(&game.id, a.path()).unwrap();
    fs::remove_dir_all(a.path()).unwrap();
    assert!(engine.snapshot_game(&game.id, None).is_err());
    assert!(engine.history(&game.id).unwrap().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test phase1_safety snapshot_covers -- --nocapture`
Expected: FAIL (still expects exactly one location).

- [ ] **Step 3: Implement the snapshot**

Replace `single_directory_location` in `snapshot_game` with `store::list_locations`. Fail closed when the list is empty or any root is not a readable directory. Build entries per slot, `Manifest::new_with_version(2, entries)`, `put_multi`, then store manifest, revision, and `revision_locations` in one transaction. Keep the existing head movement.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test phase1_safety -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/engine.rs savesync-core/src/store.rs
git commit -m "feat(core): snapshot every save location in one revision"
```

---

### Task 8: Multi-Location Restore, Journal, and Recovery

**Files:**
- Modify: `savesync-core/src/models.rs`
- Modify: `savesync-core/src/store.rs`
- Modify: `savesync-core/src/restore.rs`
- Modify: `savesync-core/src/engine.rs`
- Test: `savesync-core/src/restore.rs`, `tests/phase1_safety.rs`

**Interfaces:**
- Consumes: Tasks 4, 5, 7.
- Produces:
  - `RestoreJournalEntry` gains `#[serde(default)] pub locations: Vec<RestoreJournalLocation>`.
  - `RestoreJournalLocation { slot, location_id, target_root, staging_path, backup_path, backup_manifest, deleted_paths }`.
  - `store::insert_journal`, `list_journal_for_game`, and `list_in_flight_restores` read/write `locations_json`.
  - `restore_revision` restores every slot; v1 revisions use their `revision_locations` row.
  - `recover_in_flight_restores` rolls back every location in `locations_json`; legacy rows keep the old single-root path.

- [ ] **Step 1: Write the failing tests**

Add a fixture and helpers to `tests/phase1_safety.rs`:

```rust
struct TwoLocationFixture {
    _temp: tempfile::TempDir,
    data_root: PathBuf,
    a: PathBuf,
    b: PathBuf,
    engine: SaveSyncEngine,
    game_id: String,
}

impl TwoLocationFixture {
    fn new() -> Self { /* write a/save.dat = "a", b/save.dat = "b", enroll, add both */ }
    fn snapshot(&mut self) -> String { self.engine.snapshot_game(&self.game_id, None).unwrap().revision_id }
}

fn seed_legacy_v1_revision(data_root: &Path, game_id: &str, slot: u32, location_id: &str, root: &Path) -> String {
    // BlobStore::put(Manifest::from_dir(root)), store_manifest, insert_revision, set_active_head,
    // insert revision_locations (revision_id, slot, location_id).
}
```

Tests:

```rust
#[test]
fn restore_writes_every_location() {
    let mut fx = TwoLocationFixture::new();
    let revision = fx.snapshot();
    fs::write(fx.a.join("save.dat"), b"changed").unwrap();
    fs::write(fx.b.join("save.dat"), b"changed").unwrap();
    fx.engine.restore(&revision).unwrap();
    assert_eq!(fs::read(fx.a.join("save.dat")).unwrap(), b"a");
    assert_eq!(fs::read(fx.b.join("save.dat")).unwrap(), b"b");
}

#[test]
fn missing_slot_refuses_before_writing() {
    let mut fx = TwoLocationFixture::new();
    let revision = fx.snapshot();
    fs::remove_dir_all(&fx.b).unwrap();
    let before = fs::read(fx.a.join("save.dat")).unwrap();
    let error = fx.engine.restore(&revision).unwrap_err();
    assert!(error.to_string().contains("location"));
    assert_eq!(fs::read(fx.a.join("save.dat")).unwrap(), before);
}

#[test]
fn second_location_failure_rolls_back_the_first() {
    let mut fx = TwoLocationFixture::new();
    let revision = fx.snapshot();
    fs::write(fx.a.join("save.dat"), b"live-a").unwrap();
    fs::write(fx.b.join("save.dat"), b"live-b").unwrap();
    fs::create_dir_all(fx.b.join("save.dat")).unwrap(); // file-vs-directory conflict
    assert!(fx.engine.restore(&revision).is_err());
    assert_eq!(fs::read(fx.a.join("save.dat")).unwrap(), b"live-a");
    let journal = fx.engine.restore_journal(&fx.game_id).unwrap();
    assert!(matches!(journal[0].state, RestoreState::RolledBack | RestoreState::RollbackFailed));
}

#[test]
fn legacy_v1_revision_restores_after_second_location_is_added() {
    let mut fx = TwoLocationFixture::new_only_first();
    let revision = seed_legacy_v1_revision(&fx.data_root, &fx.game_id, 0, &location_id, &fx.a);
    fx.engine.add_directory_location(&fx.game_id, &fx.b).unwrap();
    fx.engine.restore(&revision).unwrap();
    assert_eq!(fs::read(fx.a.join("save.dat")).unwrap(), b"a");
}
```

`new_only_first` enrolls and adds only `a`; `location_id` is read from `store::list_locations` on a short-lived `Db` connection.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test phase1_safety restore_writes missing_slot second_location legacy_v1 -- --nocapture`
Expected: FAIL.

- [ ] **Step 3: Implement grouped restore and journaling**

Load the manifest, group entries by `parse_slot_prefix` when `version >= 2`; for v1, use the single `revision_locations` mapping and the unprefixed entries. If a pulled v1 revision has no mapping and the game has exactly one location, treat that location as slot 0; otherwise refuse with a mapping error. Resolve every slot to a live `Location` by id before writing; require `target_root.is_dir()` for every slot and report all missing slots in one error before any staging or backup writes. Stage once, then treat `staging/loc-<slot>` (or `staging` for v1) as each slot's source. Back up each existing target, verify, then write one journal row with `locations_json` before applying. Apply per slot in ascending slot order; on any error run rollback for every touched slot and mark the journal `rolled_back` or `rollback_failed`. Verify each root before `applied` and head movement.

`SaveSyncEngine::restore_with_mode` must stop calling `single_directory_location`; it should call `restore::restore_revision` directly and let the restore layer resolve all slots. Keep `restore_to` for explicit single-target exports; it must reject a v2 revision that contains more than one slot.

- [ ] **Step 4: Implement recovery**

Parse `locations_json`; for each entry, roll back from its verified backup, remove staging, and aggregate the worst state: any failure → `rollback_failed`, else `rolled_back`. Legacy rows with empty `locations` use the existing `target_root`/`backup_path`/`backup_manifest` fields.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test phase1_safety -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add savesync-core/src/models.rs savesync-core/src/store.rs savesync-core/src/restore.rs savesync-core/src/engine.rs savesync-core/tests/phase1_safety.rs
git commit -m "feat(core): restore and journal multiple save locations"
```

---

### Task 9: Watcher Supports Multiple Roots per Game

**Files:**
- Modify: `savesync-core/src/watcher.rs`
- Test: `savesync-core/src/watcher.rs`

**Interfaces:**
- Produces: `watch` accepts several roots per game; `unwatch` removes every root for the game.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn several_roots_per_game_are_watched_and_all_removed() {
    let mut watcher = SaveWatcher::start().unwrap().0.unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    watcher.watch("g1", a.path()).unwrap();
    watcher.watch("g1", b.path()).unwrap();
    watcher.unwatch("g1").unwrap();
    assert!(watcher.watch("g2", a.path()).is_ok());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test watcher::tests::several_roots -- --nocapture`
Expected: FAIL ("already watched").

- [ ] **Step 3: Implement**

Remove the per-game uniqueness check in `add_root`; keep the exact-path check across all games. Rewrite `remove_root` to drain every entry for the game, unwatch each, and discard tracker state once. Keep `watch`/`unwatch` signatures.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test watcher::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add savesync-core/src/watcher.rs
git commit -m "feat(core): watch every save location of a game"
```

---

### Task 10: Tauri Commands and Watcher Wiring

**Files:**
- Modify: `savesync-desktop/src-tauri/src/lib.rs`
- Test: `savesync-desktop/src-tauri/src/lib.rs` (unit tests), `cargo clippy`

**Interfaces:**
- Produces commands:
  - `discover_games() -> DiscoveryReport`
  - `enroll_discovered_candidate(title, adapter, locations: Vec<String>, allow_broad: bool) -> EnrollOutcome`
  - `add_discovered_locations(game_id, locations: Vec<String>, allow_broad: bool) -> EnrollOutcome`
  - `list_locations(game_id) -> Vec<Location>`
  - `relink_game(game_id: String) -> Game`
- Removes `enroll_discovered(game_id)`.
- `enroll_game`, `add_location`, and both discovery commands watch every added location.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn discovery_commands_are_registered() {
    // Compile-time assertion that the handler list names exist.
    let _ = get_status;
    let _ = discover_games;
    let _ = enroll_discovered_candidate;
    let _ = add_discovered_locations;
    let _ = list_locations;
    let _ = relink_game;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd savesync-desktop/src-tauri; cargo test discovery_commands_are_registered -- --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Implement commands and wiring**

Convert `Vec<String>` paths with `PathBuf::from`, call the engine methods, then call a helper `watch_locations(&state, game_id, &added)` that loops over `added` and watches each `location.path`. `relink_game` calls `engine.enroll(game_id)` and then `watch_locations` for every location returned by `store::list_locations`. `unwatch_game` already removes all roots after Task 9. Update the `invoke_handler!` list.

- [ ] **Step 4: Run checks**

Run: `cd savesync-desktop/src-tauri; cargo test; cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add savesync-desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): expose discovery and multi-location commands"
```

---

### Task 11: Desktop Types and API Bindings

**Files:**
- Modify: `savesync-desktop/src/types.ts`
- Modify: `savesync-desktop/src/api.ts`
- Test: `npm run build`

**Interfaces:**
- Produces TS mirrors of Task 1/2/3 types and functions listed above.

- [ ] **Step 1: Add the types**

```ts
export type DiscoveryCategory = "emulator" | "steam" | "local";
export type SkipReason = "missing" | "duplicate" | "nested" | "overlapping" | "too_many_locations" | "broad_unconfirmed";
export interface SavePath { path: string; exists: boolean; broad: boolean; }
export interface ExistingMatch { game_id: string; enrolled: boolean; new_paths: string[]; }
export interface DiscoveredGame {
  title: string; adapter_type: AdapterType; save_paths: SavePath[];
  sources: DiscoverySource[]; category: DiscoveryCategory;
  match_key: string; existing: ExistingMatch | null;
}
export interface DiscoveryReport { games: DiscoveredGame[]; total: number; truncated: boolean; }
export interface SkippedPath { path: string; reason: SkipReason; }
export interface Location { id: string; game_id: string; path: string; kind: "directory" | "file"; slot: number; }
export interface EnrollOutcome { game: Game; added: Location[]; skipped: SkippedPath[]; }
```

- [ ] **Step 2: Add API functions and remove the broken one**

```ts
discoverGames: () => invoke<DiscoveryReport>("discover_games"),
enrollDiscoveredCandidate: (title, adapter, locations: string[], allow_broad = false) =>
  invoke<EnrollOutcome>("enroll_discovered_candidate", { title, adapter, locations, allow_broad }),
addDiscoveredLocations: (game_id, locations: string[], allow_broad = false) =>
  invoke<EnrollOutcome>("add_discovered_locations", { game_id, locations, allow_broad }),
listLocations: (game_id: string) => invoke<Location[]>("list_locations", { game_id }),
relinkGame: (game_id: string) => invoke<Game>("relink_game", { game_id }),
```

- [ ] **Step 3: Run the build**

Run: `cd savesync-desktop; npm run build`
Expected: PASS. Fix any type errors before continuing.

- [ ] **Step 4: Commit**

```bash
git add savesync-desktop/src/types.ts savesync-desktop/src/api.ts
git commit -m "feat(desktop): add discovery and location API types"
```

---

### Task 12: Discovery View and Tab

**Files:**
- Create: `savesync-desktop/src/components/DiscoveryView.tsx`
- Modify: `savesync-desktop/src/App.tsx`
- Modify: `savesync-desktop/src/styles.css`
- Test: `npm run build` plus manual flow

**Interfaces:**
- Consumes: Task 11 `api` and types.
- Produces: `<DiscoveryView busy={boolean} onLibraryChanged={() => void} />` with internal scan/filter/selection/context-menu state.

- [ ] **Step 1: Add the view switch in `App.tsx`**

Add `const [view, setView] = useState<"library" | "discover">("library")` and a two-button segmented control above the library content. Render `<DiscoveryView busy={!!busy} onLibraryChanged={() => void loadGames()} />` when `view === "discover"`, otherwise the existing library/detail JSX unchanged.

- [ ] **Step 2: Implement `DiscoveryView`**

State: `report: DiscoveryReport | null`, `scanning: boolean`, `filter: "all" | DiscoveryCategory`, `search: string`, `selected: Set<string>` (keys are `match_key`), `checked: Map<string, Set<string>>`, `menu: { key: string; x: number; y: number } | null`, `pendingBroad: { key: string; paths: string[] } | null`.

Behavior:
- Empty state with **Scan for games**; scanning calls `api.discoverGames()`.
- Cards show title, category badge, source labels, folder checkboxes; broad paths start unchecked and carry the warning; non-existent folders render disabled and unchecked with a short reason; existing matched games show **Already tracked** or **In library (unlinked)**.
- When `report.truncated`, show `Showing 1,000 of N candidates` above the grid.
- A failed scan sets an error banner but keeps the previous `report` visible, with a retry button.
- Filter chips and search derive `visible` from `report.games`; changing either clears `selected`.
- Context menu items: **Add to sync** and **Copy folder path** for each folder. The toolbar button and menu both call `addSelected`.
- `addSelected` builds `{title, adapter, paths}` per selected card. If any chosen path is broad, open `pendingBroad` confirmation first; only after confirmation pass `allow_broad: true`.
- Send one request per selected game, collect `EnrollOutcome`s, show a banner: `Added N games, M folders; skipped K (reasons)`.
- For an enrolled match, call `addDiscoveredLocations(existing.game_id, newPaths, allow_broad)` after confirmation.
- For an unlinked match, show **Re-link**. Re-link calls `api.relinkGame(existing.game_id)`, then, if folders are checked, calls `addDiscoveredLocations` for them under the same confirmation rules. Re-link never creates a second game row.
- Keyboard: space toggles the focused card, Enter opens its context menu, Escape closes the menu. Every icon-only action has an `aria-label`.

- [ ] **Step 3: Add styles**

Add `.discovery-tabs`, `.discovery-toolbar`, `.discovery-grid`, `.discovery-card`, `.discovery-path`, `.discovery-path.broad`, `.discovery-badge`, `.discovery-menu`, and `.discovery-confirm` rules consistent with existing cards/chips. Ensure focus-visible outlines and 44px minimum target height.

- [ ] **Step 4: Build and verify manually**

Run: `cd savesync-desktop; npm run build`
Expected: PASS.
Then run `./scripts/dev.sh`, open Discover, scan, filter each category, search, select two games, add them, and confirm the library updates. Check the browser console for errors.

- [ ] **Step 5: Commit**

```bash
git add savesync-desktop/src/components/DiscoveryView.tsx savesync-desktop/src/App.tsx savesync-desktop/src/styles.css
git commit -m "feat(desktop): add discovery review screen"
```

---

### Task 13: Documentation and Roadmap

**Files:**
- Modify: `savesync-core/README.md`
- Modify: `savesync-docs/roadmap.md`

- [ ] **Step 1: Update the core README**

Document v2 manifests, slot prefixes, the 16-location cap, `revision_locations`, and the multi-root restore guarantees. Note that cross-device multi-location restore is blocked.

- [ ] **Step 2: Update the roadmap**

Mark discovery review and multi-location saves done under Phase 3; list Epic-specific detection, scanner narrowing, and cross-device slot mapping as follow-ups.

- [ ] **Step 3: Run the full verification**

Run: `cd savesync-core; cargo fmt --all -- --check; cargo clippy --all-targets -- -D warnings; cargo test`
Run: `cd savesync-desktop; npm run build; cd src-tauri; cargo clippy --all-targets -- -D warnings; cargo test`
Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add savesync-core/README.md savesync-docs/roadmap.md
git commit -m "docs: document discovery review and multi-location saves"
```
