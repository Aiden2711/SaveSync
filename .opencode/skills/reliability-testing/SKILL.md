---
name: reliability-testing
description: Fault injection, recovery testing, and failure mode analysis for SaveSync's core safety invariants.
---

# Reliability Testing Framework

## Fault Injection Scenarios

### Network Failures

1. **Google Drive API Throttling**: Simulate 429 responses to test exponential backoff and jitter.
2. **Partial Upload Failures**: Abort multipart uploads at random points to verify resumable uploads.
3. **Network Partitioning**: Temporarily block network access to test offline mode recovery.

### Data Corruption

1. **SHA-256 Mismatches**: Inject bit flips into save files to test integrity verification.
2. **Database Corruption**: Simulate SQLite WAL file corruption to test recovery mechanisms.
3. **Path Traversal Attempts**: Inject `../` sequences into archive headers to test path sanitization.

## Recovery Testing

### Rollback Testing

1. **Forced Rollbacks**: Trigger rollbacks to previous snapshots and verify file integrity.
2. **Partial Restore Failures**: Abort restores mid-operation to test atomic transaction guarantees.

### Conflict Resolution Testing

1. **Simulated Conflicts**: Create concurrent revisions on different devices to test conflict detection.
2. **Manual Resolution**: Verify that user-selected resolutions properly update the revision DAG.

## Failure Mode Analysis

1. **Data Loss Scenarios**:
   - What happens if a save file is deleted during a sync?
   - What if the SQLite database is corrupted?
2. **Conflict Resolution**:
   - How does SaveSync handle conflicts when multiple devices have made changes?
   - What if a user selects an invalid resolution?
3. **Path Traversal**:
   - Can malicious save files or archives bypass path sanitization?
   - How does SaveSync handle invalid paths in manifests?
