# SaveSync Threat Model & Safety Analysis

## 1. Threat Identification & Attack Surfaces

### 1.1 Malicious or Corrupted Save Archives (Path Traversal / Zip Slip)
- **Threat**: A compromised sync peer or corrupted archive creates entries containing relative path traversal tokens (e.g. `../../../../etc/passwd` or `..\..\..\Windows\System32\malicious.dll`).
- **Mitigation**:
  1. Strict lexical canonicalization of all paths inside archive headers.
  2. Reject any path containing `..`, absolute drive letters (`C:\`), root prefixes (`/`), or null bytes.
  3. Validate that `canonicalize(target_dir.join(extracted_path))` begins with `canonicalize(target_dir)`.

### 1.2 Unintended Data Loss from Blind Overwrites
- **Threat**: Sync engine restores a stale remote snapshot or an empty state, destroying 100+ hours of user gameplay without warning.
- **Mitigation**:
  1. Mandatory safety snapshot created in `.savesync/rollback/<game_id>/<restore_id>` before live directory touches.
  2. Zero auto-enrollment: user must explicitly enroll titles.
  3. No automatic merges: multiple heads result in an explicit conflict UI state.

### 1.3 Incomplete Restores / Power Interruption
- **Threat**: Machine loses power or crashes halfway through extracting 50 save files into a game's folder, leaving the game in an unbootable, corrupted state.
- **Mitigation**:
  1. Two-phase commit / staged restore: files unpack fully into `.savesync/staging/<uuid>` and all SHA-256 digests are verified.
  2. Rollback journal log entry created in SQLite with state `PREPARED`.
  3. Atomic directory swap or fast copy with instant rollback on any file system error.
  4. On application boot, check for uncommitted staging states and automatically clean or restore from rollback journal.

### 1.4 Cloud Credential & Token Leaks
- **Threat**: OAuth access tokens or refresh tokens stored in plaintext exposed to other local processes.
- **Mitigation**:
  1. Use OS-level secure storage (OS keyring / SecretService on Linux, DPAPI / Credential Manager on Windows) for refresh tokens.
  2. Least-privilege OAuth scope: `https://www.googleapis.com/auth/drive.file` only.
