# SaveSync Implementation Roadmap

## Overview

SaveSync development is divided into four sequential phases, prioritizing data safety and local functionality before cloud synchronization and platform extensions.

```
+-------------------+     +-------------------+     +-------------------+     +-------------------+
|  Phase 0: Docs    | --> | Phase 1: Local    | --> | Phase 2: Cloud    | --> | Phase 3: Platform |
|  & Architecture   |     | Foundation        |     | Sync Engine       |     | Adapters          |
+-------------------+     +-------------------+     +-------------------+     +-------------------+
        [DONE]                  [DONE]                  [DONE]                 [PENDING]
```

---

## Phase 0: Setup & Architecture Definition (Completed)
- [x] Operational rules and agent boundaries established (`savesync-docs/AGENTS.md`).
- [x] Architecture Decision Record created (`savesync-docs/ADR-0001.md`).
- [x] Skills created for save-data safety, sync engine, platform adapters, and reliability testing.
- [x] Full architecture, threat model, sync protocol, testing strategy, and implementation roadmap documented.

---

## Phase 1: Safe Local Foundation (Completed)
- [x] **Data Model & SQLite Layer**:
  - Rust models for Game, Revision, Manifest, Conflict, Device, and Restore Journal.
  - SQLite via `rusqlite` with WAL, `synchronous=FULL`, foreign keys, and forward-only migrations (`PRAGMA user_version`).
- [x] **Hashing & Manifest Engine**:
  - Streaming SHA-256 for files and trees; canonical, content-addressed manifest digests.
  - Snapshot manifest comparison (added / removed / modified).
- [x] **Safety & Staged Restore Mechanics**:
  - Path traversal sanitizer and absolute-root validation.
  - Verified safety backup engine (`rollback/<game_id>/<restore_id>`).
  - Two-phase commit staged restore (`staging/<restore_id>`) with per-file atomic renames.
  - SQLite restore journal (`prepared` → `applied` / `rolled_back` / `rollback_failed`) and startup recovery.
- [x] **Local Snapshot Archiving**:
  - Deterministic `tar.zst` packaging with content-addressable blob storage (`blobs/`).
- [x] **Unit & Integration Tests**:
  - 31 tests covering enrollment gating, digest verification, corrupted blobs, failed-install rollback, crash recovery, path traversal, symlink rejection, DAG parents, and deduplication.

**Phase 1 limits (intentional):** one directory location per game, no adapters, no cloud, no UI. See `savesync-core/README.md`.

---

## Phase 2: Cloud Sync & DAG Engine (Completed)
- [x] **DAG Revision Graph Engine**:
  - Parent-child link tracking in SQLite; head computation.
  - Divergence detection (multiple heads without a common descendant) and durable conflict records.
  - Explicit multi-parent resolution commits (no automatic merging, no live-file changes).
- [x] **Provider-Agnostic Sync**:
  - `SyncProvider` trait (list revisions, manifest get/put, blob get/put) with immutable/idempotent semantics.
  - `LocalFolderProvider` filesystem implementation for offline, USB, and NAS sync.
  - Push/pull ordering that verifies manifests and archives before local visibility; remote revision equivocation is rejected.
- [x] **Google Drive v3 Integration**:
  - OAuth 2.0 PKCE authentication flow (`authorization_url`, `exchange_code`, refresh caching).
  - Non-sensitive `https://www.googleapis.com/auth/drive.file` scope only.
  - App-only `appDataFolder` storage with immutable, digest-keyed objects.
  - Multipart uploads under 5 MiB and 8 MiB-chunk resumable uploads above, with bounded exponential backoff + jitter and `401` refresh.
  - `GoogleDriveProvider` implements `SyncProvider`; protocol verified against an in-memory Drive fake, including two-device end-to-end sync.
- [x] **Desktop Integration**:
  - Loopback OAuth listener on `127.0.0.1` with PKCE and constant-time state validation.
  - Refresh tokens persisted in the OS keyring (Secret Service / Credential Manager / Keychain), never SQLite.
  - Tauri 2 commands for enrollment, snapshots, restore, history, conflicts, Drive connection, and sync.
  - React UI for game library, snapshot creation, revision timeline, overlay/exact restores, conflict resolution, and Drive setup.
  - Remaining polish: native folder picker, discovery/enrollment review screen, background sync scheduling, snapshot previews.

---

## Phase 3: Platform Adapters & Production Hardening
- [x] **Save-change watcher (core only):** debounce 5s/60s, generaciones para confirmar estado, cola acotada con contador de descartes, mapeo ruta→juego, filtro de artefactos internos, 20 tests (18 unitarios y 2 de integración con `notify` real). No captura snapshots ni escribe en la carpeta vigilada. Pendiente: IPC/Tauri y badge de "cambios sin capturar" en la UI.
- [ ] **Platform Adapters**:
  - Ludusavi CLI integration (`ludusavi --api api backup --preview --game <Name>`).
  - Custom Directory adapter with glob pattern support.
  - Emulator adapters (PCSX2 memory cards, RPCS3 HDD saves, Dolphin, Ryujinx).
  - JKSV Export adapter for Nintendo Switch saves.
- [ ] **Tauri Desktop Shell Packaging**:
  - Complete Tauri 2 desktop shell configuration for Linux and Windows.
  - System tray, auto-start, and keyboard shortcuts.
- [ ] **End-to-End Safety Audit**:
  - Fault injection testing (corrupted archives, mid-transfer drops, path traversal attack payloads).
