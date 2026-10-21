# SaveSync Testing Strategy & Reliability Plan

## 1. Safety & Reliability Invariants Testing

All safety invariants must be validated via automated tests before release:

| Invariant | Test Strategy |
|-----------|---------------|
| Zero Data Loss | Pre-restore backup creation verified; simulated restore failure triggers automatic rollback |
| Zero Auto-Enrollment | Ludusavi discovery produces candidate list; database checks guarantee `is_enrolled = false` until explicitly toggled |
| No Automatic Merges | Diverged DAG heads trigger conflict state in database; verified no background file overwrites occur |
| Staged Restore Integrity | SHA-256 integrity verification fails on tampered files in staging; bad restore aborted cleanly |
| Path Traversal Protection | Test suites with `../`, absolute paths, symlinks verify rejection |

## 2. Testing Layers

### 2.1 Rust Unit Tests
- Database models, migrations, and transactional methods.
- Hashing (SHA-256), manifest comparison, and DAG graph operations.
- Path sanitization and traversal prevention helpers.

### 2.2 Integration Tests
- Full snapshot -> archive -> staged restore workflow in temporary test directories.
- Platform adapter path expansion and file discovery.
- Offline database recovery from corrupted/incomplete state.
- Two-device synchronization through `LocalFolderProvider` and through the
  Google Drive provider against an in-memory Drive fake (metadata queries,
  multipart uploads, media downloads). Resumable chunk ranges, pagination,
  token refresh, and query escaping are covered by provider protocol tests.

### 2.3 Simulated Fault Injection
- Corrupted compressed archives (bit flips).
- Interrupted network uploads/downloads during sync.
- System crash mid-restore simulation (verifying rollback journal recovery).

### 2.4 End-to-End (E2E) UI Tests
- React/TS desktop UI integration with Rust IPC layer.
- Mock Tauri IPC calls for game enrollment, snapshot restoration, and conflict resolution modal.
