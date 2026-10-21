# SaveSync Core (Phase 1 + Phase 2 groundwork)

Local-first, safety-critical engine for SaveSync. This crate implements the
"safe local foundation" — an immutable revision store, content-addressed
snapshot blobs, staged journaled restores — plus a provider-agnostic sync layer
for moving revisions between devices. Sync is optional; there is **no network
code** in this crate.

## Safety guarantees

| Guarantee | Implementation |
|-----------|----------------|
| No save is overwritten without a verified backup | Every restore copies and hashes the live tree into `rollback/<game>/<restore-id>/` before touching live files. |
| Interrupted restores are recoverable | A `prepared` row is committed to SQLite before any live write. `SaveSyncEngine::recover()` rolls back in-flight restores from the verified backup on next start. |
| Snapshot contents are verified | Files are hashed while archiving and again when restoring; any mismatch aborts before live files change. |
| Path traversal is impossible | All manifest/archive paths pass `paths::sanitize_relative_path` (rejects `..`, absolute paths, drive prefixes, NUL bytes, backslashes); provider object names are single safe segments. |
| No automatic enrollment | `register_candidate` creates `enrolled = 0` rows; `snapshot_game`, `restore`, and `sync` refuse unenrolled games. |
| No automatic merges | Revisions form a DAG; diverging heads are recorded as conflicts for explicit user resolution. |
| Sync never edits saves | Sync only copies immutable revisions and blobs; applying one requires an explicit `restore`. |
| Binary fidelity | Snapshots are deterministic `tar.zst` archives keyed by the manifest digest; identical content deduplicates to one blob. |

## On-disk layout

```
<data-root>/
  savesync.db                 SQLite (WAL, synchronous=FULL, foreign_keys=ON)
  blobs/<aa>/<sha256>.tar.zst content-addressed snapshots
  staging/<restore-id>/       verified unpack target (removed after a restore)
  rollback/<game>/<id>/       verified pre-restore safety backup (retained)
```

## Usage

```rust
use savesync_core::{AdapterType, RestoreMode, SaveSyncEngine};

let mut engine = SaveSyncEngine::open("/home/me/.local/share/savesync")?;

let game = engine.enroll_game("Hades", AdapterType::CustomDirectory)?;
engine.add_directory_location(&game.id, "/home/me/.local/share/Hades/Saved")?;

let v1 = engine.snapshot_game(&game.id, Some("before boss"))?;

// ... later, after a bad run ...
let report = engine.restore_with_mode(&v1.revision_id, RestoreMode::ExactMirror)?;
println!("safety backup: {}", report.safety_backup_path);

// On startup, before the UI is shown:
let recovered = engine.recover()?;
```

### Syncing revisions (optional)

```rust
use savesync_core::LocalFolderProvider;

let provider = LocalFolderProvider::open("/mnt/nas/savesync")?;
let report = engine.sync(&provider, &game.id)?;
println!("{} pulled, {} pushed, {} conflict(s)",
    report.pulled_revisions, report.pushed_revisions, report.conflict_ids.len());

// If the user resolves a divergence, create an explicit two-parent commit:
let revision = engine.resolve_conflict_with(&report.conflict_ids[0], &chosen.id)?;
engine.restore(&revision.id)?; // still an explicit, backed-up restore
```

`SyncProvider` is a narrow trait (list revisions, get/put manifest, get/put
blob). `LocalFolderProvider` is a verified filesystem implementation used for
tests, USB drives, and NAS folders.

### Google Drive (optional)

```rust
use savesync_core::{
    GoogleDriveProvider, OAuthConfig, PkcePair, ReqwestTransport,
    authorization_url, exchange_code,
};

let config = OAuthConfig::google(CLIENT_ID);
let pkce = PkcePair::generate();
let state = random_state(); // generate and verify a fresh value per flow
let transport = ReqwestTransport::default();

// Open this URL in the system browser and listen on a loopback redirect.
let url = authorization_url(&config, "http://127.0.0.1:45812/callback", &pkce, &state)?;

// After receiving ?code=...&state=...:
let tokens = exchange_code(&transport, &config, &code, &pkce, redirect_uri)?;

// Store `tokens.refresh_token` in the OS keyring, then:
let provider = GoogleDriveProvider::production(CLIENT_ID, &refresh_token)?;
let report = engine.sync(&provider, &game.id)?;
```

The provider requests only `drive.file`, stores objects in the app-only
`appDataFolder`, and never mutates an existing object. Blobs above 5 MiB use
resumable uploads (8 MiB chunks); transient failures use bounded exponential
backoff with jitter; `401` refreshes the access token once. Access and refresh
tokens are redacted from `Debug` output.

For the desktop flow, `OAuthFlow` binds an ephemeral `127.0.0.1` listener and
validates the callback `state`; `KeyringTokenStore` persists the refresh token
in the OS credential store. See `savesync-desktop/` for the wired application.

`RestoreMode::Overlay` (the default) writes snapshot files but never deletes
unrelated live files. `RestoreMode::ExactMirror` makes the directory match the
snapshot exactly; deleted files still exist in the safety backup.

## Revision model

```
v1 ── v2 ── v3          linear history from one device
        └── v3'         divergent head detected by Phase 2 sync
```

Revisions are immutable. Only `games.active_head_revision_id` moves. A
conflict-resolution commit may have multiple `parent_ids`; SaveSync never
creates one implicitly.

## Change watcher

`watcher::SaveWatcher` observes only roots registered through `watch()`. It
never writes, deletes, or captures a snapshot itself: after `quiet` (5 s)
without changes, or once `max_hold` (60 s) elapses during continuous activity,
it emits a `DirtyEvent` with the game id and a generation used by `clear`, and
the consumer decides whether to snapshot. Delivery uses a bounded channel; a
full queue drops the event and increments `dropped_events()`. Tauri/IPC and UI
wiring are still pending. On Windows, `notify` ignores `follow_symlinks(false)`,
so junctions and reparse points inside a root may surface out-of-tree events.

## Testing

```bash
cargo test              # 88 tests: unit + watcher + safety + sync + Drive + OAuth loopback
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

The suites cover enrollment gating, round-trip restores, overlay vs. exact
mirror, corrupted blobs, failed installs with automatic rollback, crash
recovery, missing backups, symlink rejection, DAG parents/conflicts,
deduplication, empty directories, two-device sync, divergence detection,
multi-parent resolution, rejection of corrupted remote artifacts, PKCE
vectors, token refresh, Drive query escaping, multipart uploads, resumable
chunk ranges, pagination, loopback state validation, full end-to-end sync
through a fake Drive, and copying read-only source blobs. The `watcher` suite
covers `quiet`/`max_hold` debounce, re-arming and generations, internal-artifact
filtering, path mapping, and two integration tests against the real `notify`
backend.

## Phase 1/2 limitations (deliberate)

- Exactly one **directory** location per game.
- No Ludusavi/emulator adapters yet (Phase 3).
- The desktop shell lives in `../savesync-desktop`; this crate exposes the
  engine, loopback OAuth helper, and keyring store, but no UI.
- File-level locations are modelled in SQLite but rejected by snapshot/restore.
- Sync does not yet transfer game metadata/titles, only revisions and blobs.

## Modules

| Module | Responsibility |
|--------|----------------|
| `db` | SQLite connection, pragmas, forward-only migrations |
| `store` | Repository functions (games, revisions, manifests, conflicts, journal) |
| `manifest` | File hashing and canonical manifest identity |
| `hash` | Streaming SHA-256 helpers |
| `blob` | Verified `tar.zst` archive storage |
| `paths` | Traversal protection and root validation |
| `restore` | Two-phase restore transaction and crash recovery |
| `provider` | Optional transport trait and filesystem provider |
| `sync` | DAG merge, push/pull ordering, conflict detection |
| `oauth` | PKCE, token exchange/refresh, redacted token types |
| `oauth_loopback` | Loopback redirect listener with state validation |
| `credentials` | OS-keyring and in-memory refresh-token stores |
| `http` | Transport abstraction, reqwest backend, retry policy |
| `google_drive` | Drive v3 provider using `appDataFolder` |
| `watcher` | Passive change detection: debounce state machine and `notify`-backed observer |
| `engine` | Public facade enforcing safety invariants |
