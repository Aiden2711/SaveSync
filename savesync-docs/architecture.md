# SaveSync Architecture Specification

## 1. System Overview

SaveSync is a local-first, cross-platform game save manager and synchronization engine designed for Windows 11, CachyOS, and Bazzite (SteamOS/Linux handhelds).

```
+------------------------------------------------------------------------+
|                     Tauri 2 Frontend (React + TS)                     |
|  - Game Library View       - Conflict Resolution Modal                |
|  - Revision Graph Timeline - Cloud / Storage Settings                 |
+------------------------------------+-----------------------------------+
                                     | IPC (Tauri Commands & Events)
+------------------------------------v-----------------------------------+
|                        SaveSync Rust Engine                            |
|                                                                        |
|  +---------------------+  +--------------------+  +------------------+ |
|  |   Safety Engine     |  |   Sync DAG Engine  |  | Platform Adapter | |
|  | - Pre-restore backup|  | - Snapshot commits |  | - Ludusavi CLI   | |
|  | - Staged swap/atomic|  | - Branch tracking  |  | - Custom dirs    | |
|  | - SHA-256 verifier  |  | - Conflict detect  |  | - Emulators/JKSV | |
|  +---------------------+  +--------------------+  +------------------+ |
|                                                                        |
|  +---------------------+  +--------------------+                       |
|  |  SQLite Metadata    |  |  Cloud Provider    |                       |
|  |  (WAL rusqlite)     |  |  (Google Drive v3) |                       |
|  +---------------------+  +--------------------+                       |
+------------------------------------------------------------------------+
```

## 2. Component Architecture

### 2.1 Tauri 2 Desktop Shell & IPC Boundary
- High-performance, low-memory native shell with webview renderer.
- IPC interface strictly typed via serde / TS types.
- Background tasks (file hashing, compression, sync polling) executed on Rust thread pools without blocking the UI thread.

### 2.2 Storage Layout
All SaveSync data is isolated in the user's application data directory (`~/.local/share/savesync` on Linux, `%LOCALAPPDATA%/SaveSync` on Windows):
- `savesync.db`: SQLite database running in WAL mode.
- `blobs/`: Content-addressable storage (CAS) storing compressed snapshot archives keyed by SHA-256 digest (`blobs/<first_2_chars>/<full_sha256>.tar.zst`).
- `staging/`: Ephemeral staging folder for in-flight restore unpacks or snapshot builds (`staging/<uuid>/`).
- `rollback/`: Safety reserve directory containing pre-restore safety backups before any file replacement (`rollback/<game_id>/<timestamp>/`).

### 2.3 SQLite Metadata Schema
- **`games`**: Enrolled games (ID, title, adapter type, root paths, enrollment status, active head revision ID).
- **`revisions`**: DAG revision nodes (revision ID, game ID, parent IDs JSON, manifest hash, device ID, created_at, cloud_file_id, is_active).
- **`manifests`**: File-level records (manifest hash, relative path, file size, sha256 hash, mtime, permissions).
- **`conflicts`**: Active unresolved conflict states (game ID, competing revision IDs, detected_at, status).
- **`devices`**: Machine identifiers (device ID, friendly name, platform, last_seen).
- **`rollback_journal`**: Audit log of restore operations and rollback backups for 1-click recovery.

### 2.4 Cloud Provider Architecture (Google Drive v3)
- OAuth 2.0 PKCE (`S256`) with the minimal `drive.file` scope. The desktop shell
  binds a loopback listener on `127.0.0.1:<ephemeral>`, validates `state`, and
  stores the refresh token in the OS keyring (never SQLite).
- Objects live in the app-only `appDataFolder`; filenames and `appProperties`
  identify revisions, manifests, and blobs. Existing objects are immutable.
- JSON objects use multipart upload. Blobs ≤ 5 MiB use multipart; larger blobs
  use 8 MiB resumable chunks with `Content-Range` resume handling.
- Transient failures (`408`, `425`, `429`, `5xx`, rate-limit `403`, network)
  use bounded exponential backoff with jitter; `401` refreshes the token once.
- Downloaded manifests are hash-verified and downloaded archives are fully
  extracted and verified before entering local content-addressed storage.
