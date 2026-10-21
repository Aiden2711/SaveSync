# SaveSync Sync Protocol & DAG Revision Graph

## 1. Revision Graph Data Model (DAG)

### 1.1 Revision Node Structure
```
Revision {
  id: UUIDv4,
  game_id: Uuid,
  parent_ids: [UUID, ...],  // Supports branching
  device_id: String,
  timestamp: ISO8601,
  manifest_hash: Sha256,
  cloud_file_id: String?,  // Google Drive file ID if synced
  is_active: Boolean,
}
```

### 1.2 Manifest Structure
```
Manifest {
  hash: Sha256,
  files: Map<RelativePath, FileEntry>,
}

FileEntry {
  relative_path: String,
  size: u64,
  sha256: Sha256,
  mtime: DateTime,
}
```

## 2. Sync Pipeline (Local → Remote)

### 2.1 Snapshot Creation
1. **Adapter Scan**: Platform adapter enumerates save files and calculates SHA-256 manifests.
2. **Database Transaction**:
   - Insert new revision node with `parent_ids` = `[current_head_id]`.
   - Insert manifest entry into `manifests` table.
   - Update `games.active_head_revision_id`.
3. **Archive Creation**:
   - Compress all files in staging area using `tar.zst`.
   - Compute SHA-256 of archive.
   - Store compressed blob in `blobs/<first_2_chars>/<sha256>.tar.zst`.
4. **Cloud Upload**:
   - Upload archive to Google Drive appDataFolder.
   - Record `cloud_file_id` in revision node.

### 2.2 Remote Pull (Sync Check)
1. **Fetch Remote Graph**:
   - Query Google Drive for remote revision manifest metadata.
2. **Merge Graph**:
   - Insert remote revisions into local SQLite.
3. **Delta Detection**:
   - Compare local manifest hashes with remote.
   - Identify missing blobs or new revisions.
4. **Download & Verify**:
   - Download missing blobs.
   - Verify SHA-256 integrity of each blob.
5. **Conflict Detection**:
   - If multiple heads exist without a common descendant, flag a conflict.

## 3. Conflict Resolution Workflow

### 3.1 Conflict Detection
When two devices independently create snapshots from the same parent:
- Local device has head `R_L`.
- Remote device has head `R_R`.
- No descendant exists linking `R_L` and `R_R`.

### 3.2 Resolution UI Flow
1. User opens conflict modal showing:
   - Both revisions (metadata, file size diff, preview images if applicable).
   - Device name and timestamp for each.
2. User selects one revision as the chosen state.
3. Resolution commit:
   - Create new revision `R_C` with `parent_ids: [R_L, R_R]`.
   - Update `games.active_head_revision_id` to `R_C`.
   - Mark conflict as resolved in `conflicts` table.
   - Perform staged restore of `R_C` manifest into live game directory.

### 3.3 Resolution Modes
- **Choose Left (Local)**: Keep local state, discard remote changes.
- **Choose Right (Remote)**: Overwrite local with remote state.
- **Compare & Merge (Future)**: Text-based merge support for config files; binary files always require manual selection.
