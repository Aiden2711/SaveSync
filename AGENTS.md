# SaveSync AGENTS.md

This document defines project-specific agent rules, architectural boundaries, commands, and verification steps. All agents must follow these rules when planning, implementing, reviewing, or testing changes.

## Repository Map

| Path | What lives there |
| --- | --- |
| `savesync-core/` | Rust engine. Must stay free of Tauri/UI dependencies so it remains testable on its own. |
| `savesync-core/src/sql/` | Forward-only migrations (`001_initial.sql`, `002_add_cover_path.sql`, `003_discovery_cache.sql`), applied in order through `PRAGMA user_version`. |
| `savesync-core/tests/` | Integration suites: `phase1_safety.rs` (enrollment gating, snapshot, restore, journal), `phase2_sync.rs` (revision DAG and conflict flow), `google_drive_sync.rs` (provider against a mock transport). |
| `savesync-desktop/` | Tauri 2 shell and React 19 UI. `src-tauri/src/lib.rs` holds every `#[tauri::command]`; `src/api.ts` is the typed wrapper the UI calls. |
| `savesync-desktop/mockups/` | Static HTML design previews. Not shipped, not part of the build. |
| `savesync-docs/` | Architecture, roadmap, sync protocol, threat model, testing strategy, ADRs, plus `plans/` and `specs/` for work in flight. |
| `.opencode/` | Project agents (`agents/`) and skills (`skills/`) referenced by this document. |

### Core Module Ownership

- `engine.rs` — `SaveSyncEngine`, the single façade the Tauri command layer drives. It owns the database, the blob store, and the on-disk safety paths.
- `store.rs` / `db.rs` — Repository functions over the SQLite schema, and connection/migration handling (WAL, `synchronous=FULL`, foreign keys). `store` never touches user save files.
- `manifest.rs` / `hash.rs` / `blob.rs` — Canonical manifests, SHA-256 helpers, and content-addressed `tar.zst` snapshot blobs.
- `restore.rs` — Two-phase journaled restores (`prepared → applied | rolled_back | rollback_failed`) with pre-restore safety backup.
- `sync.rs` / `provider.rs` / `google_drive.rs` / `http.rs` / `oauth.rs` / `oauth_loopback.rs` / `credentials.rs` — Sync engine, provider abstraction, Drive REST v3, retrying transport, PKCE flow, and OS keyring token storage.
- `discovery.rs` / `paths.rs` — Save discovery (Ludusavi/Lutris/adapter sources) and path-safety validation for untrusted manifests and archives.
- `watcher.rs` — Passive change detection only. It must never snapshot, read save bytes, or write.
- `models.rs` / `error.rs` / `lib.rs` — Serde domain types, explicit error taxonomy, and public re-exports.

### Desktop Conventions

- Commands return `Result<T, String>`. Blocking work runs inside `tauri::async_runtime::spawn_blocking` (see `sync_game`) so the webview never stalls.
- UI state flows `src/App.tsx` → `src/api.ts` → `src/tauri.ts`. Keep `src/types.ts` aligned with the Rust `serde` shapes.
- Destructive dialogs follow the focus-trap contract exercised by `src/App.dialogs.test.tsx`.
- Vitest runs through `vitest.config.ts` (jsdom, `src/test/setup.ts`) against `src/test/tauri-stub.ts`. Tests must never require a live Tauri runtime.

### Current Phase

Phase 3 (platform adapters and production hardening) is in progress. The active work item is the SteamGridDB cover-art pipeline: the core client landed in `1270f65` (`savesync-core/src/artwork.rs`), with engine-side caching, the Tauri IPC and image-serving layer, and UI rendering still in flight. Game launching and controller-aware prompts remain backlog items in `savesync-docs/roadmap.md`.

The desktop shell uses the "Console System UI, cool" design system documented in `DESIGN.md`. The earlier "Warm Console" palette described in `savesync-docs/specs/2026-10-23-ui-warm-console-theme.md` is superseded — do not reintroduce warm brown surfaces, grain, or the `SS` brand badge.

## Permanent Project Rules

1. **SAFETY AND DATA INTEGRITY FIRST:** No operation may delete or modify the only known-good copy of a save unless the operation is guaranteed to succeed or a recoverable alternative exists. Destructive actions require preview, step-by-step confirmation, or explicit user selection.

2. **MANUAL FIRST:** Implement opt-in automation. Never start automated backup or synchronization without explicit enrollment by the user.

3. **VERSIONED REVISIONS:** Represent history as a DAG of immutable revisions, not as timestamps. Use SQLite to persist the baseline, snapshots, operations, and conflicts.

4. **LOCAL FAILOVER:** Remote operations, including Google Drive, must not become a runtime requirement. Local backup and restore must work offline.

5. **PERSISTENCE OF LOGICAL IDENTITY:** Each enrolled game or save profile has a UUID independent of its installation. Contributions to the game registry must outlive any single device.

6. **NON-AUTONOMOUS DISCOVERY:** Automatically discovering games must never automatically enroll them. Enrollment requires explicit user selection and explicit enrollment mode confirmation.

7. **PLUGGABLE PLATFORM:** Use platform adapters for save discovery. The core component must not depend on specific tools, operating systems, or platform implementations.

8. **PERFORMANCE IS A CORRECTNESS CONCERN:** Avoid unnecessary work, unbounded resource consumption, blocking operations on critical execution paths, and performance regressions. Performance optimizations must preserve save integrity, recoverability, synchronization semantics, and user control.

9. **MEASURE BEFORE CLAIMING:** Do not present a suspected performance issue as a proven bottleneck. Distinguish measured regressions, code-demonstrable risks, and hypotheses that require profiling or benchmarking.

10. **NO PREMATURE COMPLEXITY:** Prefer the simplest design that satisfies correctness, safety, and performance requirements. Do not introduce caches, concurrency, background workers, or elaborate abstractions without a justified need and defined invalidation, cancellation, and failure behavior.

## Architecture Boundaries

### UI Layer

- The UI only manipulates state exposed through Rust call-ins and commands, including transitions and user-visible text.
- No direct filesystem operations or external tool invocations from the UI.
- Keep the interface responsive during long-running filesystem, database, backup, restore, and synchronization operations.
- Show meaningful progress for operations where progress can be determined reliably.
- Avoid unnecessary rendering, repeated state updates, duplicated requests, and oversized Tauri IPC payloads.
- UI optimizations must not bypass Rust-side validation or safety checks.

### Core Engine

- Implement the core in Rust, compatible with Tauri 2.
- Do not expose external network listeners.
- Keep filesystem, database, backup, restore, and synchronization responsibilities explicit.
- Avoid unnecessary cloning, serialization, allocation, repeated filesystem traversal, and synchronous work on async executor threads.
- Use concurrency only when it improves the operation without compromising ordering, cancellation, consistency, resource limits, or recovery.
- Bound memory usage and concurrent work where workloads may grow with the number or size of save files.
- Preserve actionable errors and recovery information instead of silently swallowing failures.

### Storage Provider Interface

- Local SQLite stores the game registry, snapshots, operations, and conflicts.
- Remote upload interface:

  `UploadRevision { revision, parent_revision, manifest }`

  Upload revision objects through the cloud provider.

- Remote download interface:

  `DownloadRevision { object_ids }`

  Download revision objects through the cloud provider.

- The provider interface must be mockable for tests and support an optional real provider.
- Remote availability must not be required for local backups or restores.
- Avoid unnecessary object downloads, uploads, database reads, and repeated manifest processing.
- Operations must be retry-safe and preserve immutable revision semantics.
- Apply bounded timeouts and resource limits where appropriate.

### Platform Adapters

Required interface shapes:

- `DiscoverLocations(game_id) → Vec<LocationCandidate>`
- `SnapshotLocations(mapping_id) → Manifest`
- `RestoreLocations(manifest, mapping_id) → RestoreResult`

Responsibilities:

- Isolate platform-specific discovery and save handling.
- Sanitize and validate path mappings as part of discovery and before any write.
- Avoid redundant full filesystem traversals when safely reusable information is available.
- Handle large directories and file collections without unbounded memory growth.
- Avoid following unsafe links, escaping approved locations, or broadening the scope of a restore.
- Keep discovery separate from automatic enrollment.
- Preserve platform-independent core behavior.

### Google Drive Provider

- Use the `drive.file` scope, as required by ADR-0001.
- Do not depend on server-side redirection for ordinary upload behavior. Long-running operations may be used for export where necessary; the implementation should use the selected multipart creation flow.
- Retry transient failures with bounded exponential backoff and jitter, following applicable Tauri retry guidance.
- Do not retry permanent failures indefinitely.
- Respect API quotas and rate limits.
- Avoid redundant transfers and repeated requests where existing revision or object metadata can safely establish that work is unnecessary.
- Keep credentials out of logs, errors, source control, and user-visible diagnostic output.

## Agent Roles

OpenCode V2 agents live in `.opencode/agents/`.

### Orchestrator — Primary Agent

- Plans work, delegates one implementation task at a time, evaluates review findings, and verifies acceptance criteria.
- Cannot edit files.
- Ensures tasks have clear scope, safety requirements, and verification criteria.
- Requests independent review before accepting significant changes.
- Must not dismiss performance findings merely because functional tests pass.
- Resolves disagreements between performance improvements and safety, correctness, maintainability, or reliability by prioritizing the permanent project rules.

### Coder — Implementation Subagent

- The only agent allowed to edit the working tree.
- Implements one approved task and runs relevant checks.
- Must preserve existing behavior unless the task explicitly authorizes a change.
- Must consider algorithmic complexity, memory consumption, filesystem and database I/O, concurrency, and cancellation when implementing performance-sensitive functionality.
- Must provide evidence for claimed performance improvements.
- Must not introduce risky optimizations that weaken backup integrity, restore safety, or synchronization correctness.

### Reviewer — Correctness, Regression, and Performance Subagent

The Reviewer is read-only and independently reviews proposed changes. It must evaluate **functional correctness, regressions, resource usage, and performance**, not merely formatting or general code quality.

The Reviewer may inspect:

- `git status`, `git diff`, and `git log`.
- Changed source files and relevant surrounding implementation.
- Relevant call sites, interfaces, database schema and migrations, configuration, tests, and existing benchmarks.
- Relevant project documentation and architectural decisions.
- Available test output, benchmark results, profiling data, and build diagnostics.

The Reviewer must not edit the working tree, commit changes, or perform destructive operations. Prefer targeted, non-mutating checks. Do not run benchmarks or diagnostics that modify user data, overwrite known-good saves, access live credentials, or otherwise compromise project state.

#### Mandatory Review Areas

**A. Functional correctness and regressions**

- Verify the implementation satisfies the task's acceptance criteria.
- Inspect error handling, edge cases, cancellation, retries, partial failures, and cleanup.
- Check backward compatibility where existing behavior, database schemas, manifests, or revision formats are affected.
- Check whether tests adequately exercise the new behavior and failure paths.

**B. Save integrity and recovery safety**

- Verify snapshots and revisions preserve recoverable copies.
- Inspect restore ordering, staging, validation, rollback, path sanitization, and destructive-action confirmation where relevant.
- Ensure performance optimizations cannot cause partial writes, overwrite a known-good save, discard conflicting revisions, or weaken recovery guarantees.
- Verify cancellation and process termination cannot leave the application believing an incomplete operation succeeded.
- Check that concurrent operations cannot corrupt data or bypass safety gates.

**C. Algorithmic complexity and scalability**

- Examine the time complexity of affected algorithms and important call paths.
- Identify avoidable nested loops, repeated scans, quadratic behavior, unnecessary sorting, and repeated processing of unchanged data.
- Consider growth in the number of enrolled games, save locations, files, revisions, conflicts, cloud objects, and database records.
- Identify unbounded loops, recursion, queues, retries, or work accumulation.
- Report the input sizes and conditions under which a suspected issue becomes significant.
- Do not label an algorithm inefficient without explaining its actual workload and impact.

**D. CPU, memory, and allocation costs**

- Look for unnecessary `clone()` calls, copying of large buffers, repeated serialization and deserialization, excessive temporary collections, and avoidable string or path conversions.
- Check whether large files or manifests are loaded entirely into memory when streaming or bounded processing would be more appropriate.
- Inspect whether caching is justified and whether its invalidation and memory limits are explicit.
- Look for repeated computation that could safely reuse an existing result.
- Check cleanup and resource lifetime for retained buffers, tasks, handles, and other resources.
- Consider memory pressure when multiple backup, restore, or synchronization operations run concurrently.

**E. Filesystem and database performance**

- Identify redundant directory traversals, repeated metadata lookups, unnecessary hashing, excessive small file operations, and repeated reads of unchanged files.
- Check whether hashing and file comparison strategies fit the relevant workload.
- Inspect SQLite transaction boundaries, query patterns, index usage, schema constraints, and N+1 query behavior.
- Look for queries that scan large tables, repeated queries that could safely be consolidated, and unnecessary writes or migrations.
- Verify WAL use, foreign keys, and transaction semantics remain correct.
- Do not recommend removing integrity checks or validation simply to improve throughput.

**F. Concurrency, asynchronous work, and responsiveness**

- Identify blocking filesystem, database, hashing, compression, or network operations on UI-critical or async executor threads.
- Check for excessive task spawning, unbounded parallelism, lock contention, deadlocks, and redundant synchronization.
- Verify that parallel work preserves ordering and that cancellation, retries, and shutdown leave recoverable state.
- Check that long-running tasks do not freeze the UI or overwhelm the system.
- Consider CPU and disk contention, especially during large backups or restore operations.

**G. Cloud and synchronization performance**

- Look for unnecessary transfers, repeated API requests, redundant manifest comparisons, and inefficient batching.
- Check pagination, request throttling, concurrency limits, timeouts, retries, and backoff.
- Ensure idempotent retries do not create duplicate or conflicting revisions.
- Verify offline operations do not incur unnecessary network waits.
- Ensure a slower or unavailable remote provider cannot indefinitely block local backup or restore.

**H. Tests, benchmarks, and evidence**

- Inspect existing tests and benchmarks relevant to the changed code.
- Determine whether a performance-sensitive behavior needs a regression test, benchmark, or profiling experiment.
- Recommend targeted measurements for claims that cannot be confirmed through code inspection.
- Do not invent benchmark numbers, profiling results, or test outcomes.
- Distinguish measured regressions from inferred complexity or resource risks.
- Prefer repeatable comparisons against a baseline, using realistic input sizes and environments.
- Avoid flaky timing assertions in ordinary unit tests. Use benchmarks or appropriately controlled performance tests for quantitative performance requirements.
- Consider correctness and recovery behavior under performance tests, including large inputs, interruption, retries, and concurrent workloads.

#### Required Review Output

The Reviewer must provide a concise review report containing:

1. **Verdict:** Approve, Approve with caveats, or Request changes.
2. **Correctness and safety:** Findings affecting behavior, save integrity, restore safety, synchronization, and data recovery.
3. **Performance:** Findings affecting algorithmic complexity, CPU, memory, disk I/O, database usage, network traffic, responsiveness, concurrency, or scalability.
4. **Regression and test coverage:** Missing tests, compatibility concerns, and unverified acceptance criteria.
5. **Verification evidence:** Checks actually performed and their results.

Every actionable finding must include:

- Severity: Critical, High, Medium, or Low.
- The relevant file and line or affected code path, when identifiable.
- A concrete explanation of the issue and how it can occur.
- The expected impact on users, correctness, reliability, or performance.
- Evidence supporting the finding, or an explicit statement that it is a hypothesis requiring measurement.
- A recommended correction or verification experiment.

Use severity proportionately. A measurable slowdown or demonstrated resource-exhaustion risk should not be dismissed as a style issue. Conversely, do not inflate a speculative micro-optimization into a blocker.

If no material performance issue is found, explicitly state that no material issue was identified within the review scope. Do not claim that the application is fully optimized or that performance has been measured unless it has.

### Security-Auditor — Read-Only Subagent

- Audits save handling, restore and rollback, synchronization, credentials, filesystem boundaries, and Tauri IPC.
- Checks trust boundaries, authentication, authorization, secret exposure, path traversal, and unsafe filesystem behavior.
- Evaluates whether performance optimizations alter security controls or permit resource-exhaustion attacks.
- Does not edit the working tree.

### Scout — Read-Only Research Subagent

- Performs external research for platform, dependency, API, protocol, and implementation questions.
- Prefers official documentation and primary sources.
- Reports source-backed findings and distinguishes verified behavior from assumptions.
- Does not edit the working tree.

Do not run multiple editing agents concurrently. Research and review agents may run in parallel when they only read, provided they do not interfere with the same checks or workloads.

## Commands

### Local Development — Rust

```bash
# All core engine tests
cd savesync-core && cargo test

# Rust formatting and lints
cd savesync-core && cargo fmt --all && cargo clippy --all-targets -- -D warnings
```

### Desktop App — Tauri 2 + React

```bash
cd savesync-desktop

# Starts Vite on 127.0.0.1:1420 and the Tauri debug shell together
./scripts/dev.sh

# Production frontend and binary
./scripts/build.sh

# If the Tauri CLI is installed
npm run tauri dev
```

A Tauri debug binary loads `build.devUrl`. Running it without Vite may show `Connection refused` on `127.0.0.1`. Use `dev.sh` or a release build as appropriate.

Both `dev.sh` and `verify-release.sh` assume `savesync-desktop/node_modules` exists. Run `npm install` once after cloning or when the lockfile changes.

### Build and Package

```bash
# Development binary with embedded frontend
cd savesync-desktop && ./scripts/build.sh

# Installers; requires the Tauri CLI
cd savesync-desktop && npm run tauri build
```

### Enrollment Verification

```bash
cd savesync-core

# Enrollment gating and discovery-candidate behavior
cargo test --test phase1_safety discovered_games

# Snapshot, restore, and journal behavior
cargo test --test phase1_safety
```

### UI and Feature Testing

```bash
# Start the application in development mode
cd savesync-desktop && npm run tauri dev
```

Verify that the application opens correctly and that browser developer tools show no new console errors. Use the appropriate debugging method for the current operating system and Tauri setup.

### Performance Verification

Use existing benchmarks and profiling tools where available. Do not assume a benchmark target or profiler is configured without checking the repository.

When assessing a performance-sensitive change:

1. Establish the relevant workload and expected behavior.
2. Inspect complexity and likely bottlenecks in the changed call path.
3. Run an existing relevant benchmark or profiling workflow if available and safe.
4. Compare against a known baseline where possible.
5. Check correctness and safety behavior alongside performance.
6. Record the environment, workload, and commands used for quantitative claims.
7. Report any measurement that could not be completed, rather than inventing a result.

Potential workloads include a large save directory, many enrolled games, many revisions, large manifests, repeated incremental backups, restoration, conflict resolution, and synchronization with a slow or unavailable provider.

Do not add permanent benchmark infrastructure or dependencies solely to support a minor change without establishing that the extra maintenance cost is justified.

## Required Verification Steps

### Pre-Commit Verification

1. **Rust formatting and linting**

   ```bash
   cd savesync-core && cargo fmt --all && cargo clippy --all-targets -- -D warnings
   ```

2. **Desktop shell**

   ```bash
   cd savesync-desktop && npm test
   cd savesync-desktop && npm run build
   cd savesync-desktop/src-tauri && cargo clippy --all-targets -- -D warnings
   ```

   `npm test` covers the destructive restore/exact-restore dialog contract (Cancel owns initial focus; Escape and backdrop never call the API; confirm fires exactly once). Treat a failure there as a data-safety regression, not a UI nit.

3. **Schema alignment**

   - Migrations live in `savesync-core/src/sql/` and are applied through `PRAGMA user_version`.
   - Run `cargo test --lib db::tests` to verify database behavior, including idempotency, WAL, and foreign keys.
   - Check migration behavior against existing databases and preserve data integrity.

4. **Targeted correctness and safety tests**

   - Test the relevant code paths and failure modes.
   - Include interruption, cancellation, retries, and rollback where applicable.
   - Verify that changes do not weaken safety guarantees.

5. **Performance review**

   - Have the Reviewer inspect the changed code and important adjacent call paths for complexity, unnecessary work, resource consumption, responsiveness, and scalability.
   - Run relevant existing benchmarks or profiling checks when justified and available.
   - Quantify performance claims when possible.
   - Record unresolved risks and distinguish them from measured regressions.

6. **Independent review**

   - Reviewers must inspect the actual changes and surrounding code.
   - A passing build or test suite alone is not sufficient to approve a change.
   - Resolve material correctness, safety, performance, and regression findings before acceptance, or explicitly document the approved exception.

### Before Major Changes

1. **Safety review:** Trigger the `save-data-safety` skill. Verify that writes for each game ID preserve a valid known-good copy, that destructive actions require the appropriate authorization, and that manual-destructive operations remain blocked unless explicitly confirmed.

2. **Sync-core review:** Trigger the `sync-engine` skill. Verify that collision resolution preserves both revisions by default and that retries, partial failures, and concurrent operations cannot silently discard data.

3. **Platform-adapter review:** Trigger the `platform-adapters` skill. Verify that path mappings are sanitized during discovery and revalidated before filesystem operations.

4. **Reliability testing:** Trigger the `reliability-testing` skill. Verify that termination at important operation boundaries leaves recoverable state and that retries produce idempotent outcomes.

5. **Performance review:** Ask the Reviewer to examine scalability, algorithmic complexity, file and database I/O, memory use, concurrency, and UI responsiveness. Add measurements when there is a credible risk of a material regression.

6. **Security review:** Request the Security-Auditor for changes touching credentials, filesystem boundaries, restore behavior, IPC, or network providers.

### Release Criteria

- **Core:**

  ```bash
  cd savesync-core && cargo clippy --all-targets -- -D warnings && cargo test
  ```

- **Desktop:**

  ```bash
  cd savesync-desktop && npm test
  cd savesync-desktop && npm run build
  cd savesync-desktop/src-tauri && cargo clippy --all-targets -- -D warnings && cargo test
  ```

- **Full verification:**

  ```bash
  ./savesync-docs/scripts/maint/verify-release.sh
  ```

  This script covers `cargo fmt --check`, core clippy, core tests, the desktop frontend build, desktop clippy, and desktop `cargo test`. It does **not** run the Vitest suite; run `cd savesync-desktop && npm test` separately.

- No unresolved critical or high-severity correctness, safety, security, or performance findings without an explicitly approved exception.
- No unqualified "future needs" comments; all GO decisions must be actionable.
- Claims about performance improvements must be supported by code-level reasoning, measurements, or both, with the evidence clearly stated.
- Do not claim release verification has passed unless the applicable commands actually ran successfully.

## Phase 0 Artifacts

- **ADR-0001:** Core Technology Stack.
- **`savesync-docs/ADR-0001.md`:** Architecture decision record.

Consult the relevant architecture decisions before changing established interfaces or storage semantics.

## License Notice

This SaveSync project documentation is created for the SaveSync project and follows the Apache 2.0 license of the parent repository, subject to the repository's contribution guidelines.
