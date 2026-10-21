<div align="center">

# **SaveSync**

**Your game saves, backed up, restored, and synced — without losing a byte.**

An open-source backup and sync engine for emulator and PC game saves.
Steam Cloud alternative, local-first, fully offline-capable.

![Engine](https://img.shields.io/badge/engine-Rust-DEA584?logo=rust&logoColor=white)
![Desktop](https://img.shields.io/badge/desktop-Tauri_2-24C8DB?logo=tauri&logoColor=white)
![UI](https://img.shields.io/badge/ui-React-61DAFB?logo=react&logoColor=black)
![Storage](https://img.shields.io/badge/storage-SQLite-003B57?logo=sqlite&logoColor=white)
![Status](https://img.shields.io/badge/status-Phase_3_in_progress-orange)

</div>

> I'm a first year CS student so be warned:
> ¡¡¡(DISCLAIMER: vibe-coded)!!!
> Open source , Steam Cloud alternative for emulators and "legit" games. Cloud features are used via your typical cloud provider (iCloud, Google Drive, Dropbox, self hosted cloud support is soon to be)

SaveSync snapshots game saves into a local, versioned history you can browse, restore, and sync between devices. It is designed around one idea: **a save file is irreplaceable**, so nothing gets overwritten until a verified, recoverable copy exists somewhere else.

## How it works

| What | How |
| --- | --- |
| **Back up** | Snapshots every enrolled game's save locations into immutable, checksummed revisions stored locally in SQLite. Nothing is replaced without a verified, recoverable copy. |
| **Restore** | Staged, journaled restores with rollback, so an interrupted restore never leaves a half-written save. |
| **Sync** | History is a DAG of immutable revisions shared across devices; conflicts keep BOTH revisions by default, nothing is silently merged. |
| **Discover** | Finds candidate save locations for your games, but never enrolls anything automatically — you pick each game and confirm its mode first. |

## The rules it never breaks

- **Never touches the only good copy.** SaveSync will not modify or delete the only known-good copy of a save unless the operation is guaranteed to succeed — or a recoverable alternative already exists.
- **Everything is opt-in.** Backups and sync start only after you manually enroll a game and confirm its enrollment mode. No automation without explicit consent.
- **Offline first.** Local backup and restore work with no network at all. Google Drive is optional failover, never a runtime requirement.
- **Stable identity.** Every enrolled game keeps a UUID that survives reinstalls, hardware swaps, and device changes.

## Quick start

You need a **Rust** toolchain and **Node.js** installed.

| Where | What to do |
| --- | --- |
| **1. Check the engine** | `cd savesync-core && cargo test` |
| **2. Run the app** | `cd savesync-desktop && ./scripts/dev.sh` — starts Vite on `127.0.0.1:1420` together with the Tauri debug shell |
| **3. Build installers** | `cd savesync-desktop && ./scripts/build.sh` |

## Components

| Directory | What it is |
| --- | --- |
| `savesync-core/` | Rust synchronization and save-management engine. |
| `savesync-desktop/` | Tauri 2 and React desktop application. |
| `savesync-docs/` | Architecture, roadmap, threat model, and testing documentation. |
| `.opencode/` | Project-specific coding agents and skills. |

## Under the hood

| Piece | Detail |
| --- | --- |
| **Storage** | SQLite in WAL mode, with migrations applied through `PRAGMA user_version`. Stores the game registry, snapshots, operations, and conflicts. |
| **History** | Revisions are immutable and form a DAG — history is not timestamps. |
| **Sync providers** | A mockable storage-provider interface for remote upload/download. Google Drive v3 is implemented; local backup and restore never depend on it. |
| **Platform adapters** | Pluggable adapters for save discovery, isolated from the core. |
| **Release verification** | One command checks everything: `./savesync-docs/scripts/maint/verify-release.sh`. |

## Documentation

| Document | What's inside |
| --- | --- |
| [`savesync-docs/architecture.md`](savesync-docs/architecture.md) | System architecture and module boundaries. |
| [`savesync-docs/roadmap.md`](savesync-docs/roadmap.md) | Phased plan, from safe local foundation to production hardening. |
| [`savesync-docs/sync-protocol.md`](savesync-docs/sync-protocol.md) | The revision DAG, snapshot tracking, and conflict protocol. |
| [`savesync-docs/threat-model.md`](savesync-docs/threat-model.md) | Threat model for save data and filesystem boundaries. |
| [`savesync-docs/testing-strategy.md`](savesync-docs/testing-strategy.md) | Testing strategy across engine, safety, and desktop layers. |
| [`savesync-docs/ADR-0001.md`](savesync-docs/ADR-0001.md) | Architecture decision record: core technology stack. |

## Status

| Phase | State |
| --- | --- |
| **Phase 0** — Setup & Architecture | Done |
| **Phase 1** — Safe Local Foundation | Done |
| **Phase 2** — Cloud Sync & DAG Engine (incl. Google Drive v3) | Done |
| **Phase 3** — Platform Adapters & Production Hardening | In progress |

---

<details>
<summary><strong>All verification commands</strong></summary>

```bash
# Core engine tests
cd savesync-core && cargo test

# Rust formatting and lints
cd savesync-core && cargo fmt --all && cargo clippy --all-targets -- -D warnings

# Desktop production frontend and binary
cd savesync-desktop && npm run build

# Desktop shell lints and tests
cd savesync-desktop/src-tauri && cargo clippy --all-targets -- -D warnings
cd savesync-desktop/src-tauri && cargo test

# Database behavior: migrations, WAL, foreign keys
cd savesync-core && cargo test --lib db::tests

# Enrollment gating and discovery candidates
cd savesync-core && cargo test --test phase1_safety discovered_games

# Snapshot, restore, and journal behavior
cd savesync-core && cargo test --test phase1_safety

# Full release verification
./savesync-docs/scripts/maint/verify-release.sh
```

</details>

<br>

<details>
<summary><strong>Agents & skills (<code>.opencode/</code>)</strong></summary>

Development on this repo is structured around dedicated agents in `.opencode/agents/` — [orchestrator](.opencode/agents/orchestrator.md), [coder](.opencode/agents/coder.md), [reviewer](.opencode/agents/reviewer.md), [security-auditor](.opencode/agents/security-auditor.md), and [scout](.opencode/agents/scout.md). The orchestrator plans and delegates, only the coder edits the working tree, and every significant change goes through an independent review pass.

Project skills live in `.opencode/skills/`: [sync-engine](.opencode/skills/sync-engine), [save-data-safety](.opencode/skills/save-data-safety), [platform-adapters](.opencode/skills/platform-adapters), [reliability-testing](.opencode/skills/reliability-testing), and [code-review-and-quality](.opencode/skills/code-review-and-quality).

</details>
