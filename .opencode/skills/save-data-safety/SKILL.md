---
name: save-data-safety
description: Guidance and verification workflows for save data safety invariants, zero-loss backups, integrity checking, and safe restore mechanics in SaveSync.
---

# Save Data Safety Invariants & Verification Workflow

## Core Safety Principles

SaveSync operates under non-negotiable safety rules for user game saves:

1. **Zero Data Loss Guarantee**: Save files MUST NOT be overwritten, moved, or deleted without a verifiably created and checksummed local backup stored in a isolated rollback directory.
2. **Zero Automatic Enrollment**: Discovered save files or games from Ludusavi/scans MUST NEVER be auto-enrolled into background sync without explicit user opt-in per title.
3. **No Automatic Merges**: Binary save files or complex multi-file save bundles MUST NEVER undergo automatic multi-way text merges. Conflicts MUST be represented explicitly and resolved by the user choosing specific snapshot revisions.
4. **Staged Restore Atomic Transactions**: Restores must extract into a temporary staging folder (`.savesync/tmp_staging/<restore_id>`), verify SHA-256 integrity of all files against the snapshot manifest, create a safety backup of existing live save files into `.savesync/rollback/<backup_id>`, and swap files atomically.
5. **Path Traversal Protection**: Archive headers and manifest paths MUST be validated to guarantee all extracted target paths remain inside approved game save directories or staging areas.

## Verification Checklist for Save Data Operations

When implementing or reviewing save data code paths:

- [ ] Does every write/restore command check for existing files before overwriting?
- [ ] Is a pre-restore safety snapshot created and verified prior to file replacement?
- [ ] Is SHA-256 integrity verified for all source and target files?
- [ ] Is path sanitization applied to prevent `../` path traversal vulnerabilities?
- [ ] Are database metadata updates bound inside a SQLite WAL-mode write transaction alongside filesystem operations?
