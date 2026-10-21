---
name: sync-engine
description: Directed Acyclic Graph (DAG) revision management, multi-device snapshot tracking, and deterministic conflict resolution protocols for SaveSync.
---

# Sync Engine Architecture & Protocols

## Revision Graph Structure (DAG)

SaveSync tracks save history as a Directed Acyclic Graph (DAG) stored in SQLite:

- **Revision Node**:
  - `id`: UUIDv4 or SHA-256 commit hash
  - `game_id`: Foreign key to enrolled game
  - `parent_ids`: JSON array of parent revision IDs (supports branching/forking across devices)
  - `device_id`: Fingerprint of the machine that created the snapshot
  - `timestamp`: UTC ISO-8601 string
  - `manifest_hash`: SHA-256 hash of the complete file tree manifest
  - `cloud_file_id`: Google Drive file ID (if synced)

- **Snapshot Manifest**:
  - Map of relative file paths to SHA-256 hashes, file sizes, and modified timestamps.

## Branching & Conflict Rules

1. **Deterministic Branching**: When Machine A and Machine B create independent snapshots from the same parent snapshot `P`, two head revisions `R_A` and `R_B` are formed.
2. **Conflict Detection**:
   - If a sync check reveals multiple concurrent head revisions without a common descendant, a **Sync Conflict** is flagged.
3. **Conflict Resolution Strategy**:
   - SaveSync NEVER automatically merges binary files.
   - The user is presented with both revisions (showing metadata: device name, timestamp, file size changes, preview images if present).
   - The user selects either `R_A` or `R_B` as the active live state.
   - A new resolution revision `R_C` is created with `parent_ids: ["R_A", "R_B"]`, pointing to the chosen manifest hash, resolving the conflict in the revision DAG cleanly.

## Sync Loop Pipeline

1. **Scan Local State**: Hash active save files -> compare against current head manifest.
2. **Fetch Remote Graph**: Query remote manifest metadata from Google Drive appDataFolder/custom drive folder.
3. **Merge Graphs**: Insert remote revision nodes into local SQLite database.
4. **Determine Delta**: Compute missing revisions or content blobs.
5. **Prompt / Execute Sync**: Stage and download missing revisions; resolve conflicts if diverged.
