# Discovery Review & Multi-Location Saves — Design

Date: 2026-10-21
Status: Draft for approval
Scope owner: SaveSync core + desktop

## Intent

Today discovery exists in the core and is exposed through a Tauri command, but the
desktop app never calls it. Users can only enroll a game by typing a title and
pasting one absolute save folder. Phase 3 should let a user scan their machine,
review what SaveSync found, filter it, select games, and explicitly add them to the
sync list — including games whose saves live in more than one folder.

The guiding rule is unchanged: **discovery never enrolls anything**. The user picks
every game and every folder.

## Goals

1. A Discovery view that lists candidates from the existing sources (Ludusavi,
   Lutris, Steam/Proton), with title, source, and save folders.
2. Filters for **Emulator**, **Steam**, and **Local** games, plus a text search.
3. Multi-select with a toolbar button and a right-click menu to add games.
4. Per-folder checkboxes when a candidate has several save folders.
5. Games already in the sync list are shown as tracked; when discovery finds new
   folders for them, the user can explicitly add those folders.
6. A game may have multiple directory locations. One snapshot covers all of them
   as a single immutable revision.
7. No regressions for existing single-location games or existing revisions.

## Non-goals

- No automatic enrollment, background scanning, or automatic snapshots.
- No Epic-specific scanner, Ludusavi CLI execution, emulator adapter
  implementations, JKSV, packaging, or tray work. These stay in Phase 3.
- No cross-device location remapping UI in this project. Multi-location revisions
  pulled from another device are blocked from restore with a clear error until a
  mapping step exists (see "Cross-device behaviour").
- No deletion of save data, revisions, or blobs.

## Anti-goals

- A scan must never write to a save folder.
- A wrong discovery heuristic must never silently enroll or back up a broad
  directory tree.
- A failed multi-location restore must never leave one folder restored and another
  untouched without recording it and preserving both safety backups.

## User flow

1. User opens the new **Discover** tab.
2. Empty state explains what will be scanned and offers **Scan for games**. Scanning
   is manual and on demand; the app never scans at startup.
3. While scanning, show an indeterminate progress state and keep the rest of the app
   usable. Results appear when the scan completes.
4. Results appear as cards with:
   - checkbox,
   - title,
   - category badge (Emulator / Steam / Local),
   - every discovered folder with its own checkbox (checked by default, except
     broad folders; see below),
   - a short source label ("Steam", "Proton", "Lutris", "Ludusavi").
5. Filter chips: **All**, **Emulator**, **Steam**, **Local**, each with a count.
   A search box filters by title as you type.
6. Selection is per card. The toolbar shows **Add N games to sync**. Changing a
   filter or the search text clears the selection, so hidden games are never added.
7. Right-clicking a card opens a small menu: **Add to sync** and **Copy folder
   path** (per folder). Opening a folder in the OS file manager is out of scope
   because the desktop shell has no opener plugin yet.
8. Adding opens no modal in the common case. It validates paths and enrolls. A
   result banner reports added games, skipped folders, and errors.
9. If a candidate's title matches an already-tracked game:
   - the card is dimmed and labelled **Already tracked**;
   - if discovery found folders not already attached, the card offers
     **Add N new folders**;
   - that action opens a confirmation listing only the new folders.
10. If a candidate has no existing folder after validation, the card is disabled
    with a short reason.

## Filter taxonomy

The backend returns one category and all provenance for each candidate.

| Category | Rule |
|----------|------|
| Emulator | Any save path matches a known emulator marker (RetroArch, PCSX2, RPCS3, Dolphin, Ryujinx, yuzu, DuckStation, PPSSPP, melonDS, mGBA), or the only source is Lutris. |
| Steam | No emulator marker, and any source is Steam or Proton. |
| Local | Everything else, including Ludusavi-only titles and Epic/GOG saves that Ludusavi happens to know about. |

Precedence is Emulator → Steam → Local. A title found by several sources keeps all
sources; deduplication merges by normalized title instead of keeping the first
source (current behaviour).

This is a heuristic and is documented as such in the UI's empty state ("Categories
are best-effort; every folder is still shown before you add a game").

## Backend changes

### Discovery result shape

```rust
pub struct DiscoveredGame {
    pub title: String,
    pub adapter_type: AdapterType,
    pub save_paths: Vec<SavePath>,
    pub sources: Vec<DiscoverySource>,   // was: single `source`
    pub category: DiscoveryCategory,     // new
    pub match_key: String,               // shared title normalization
    pub existing: Option<ExistingMatch>, // enrolled or unlinked game with same key
}

pub struct SavePath {
    pub path: PathBuf,
    pub exists: bool,
    pub broad: bool,   // known profile root; unchecked by default in the UI
}

pub struct ExistingMatch {
    pub game_id: String,
    pub enrolled: bool,
    pub new_paths: Vec<PathBuf>, // discovered folders not attached to the game
}
```

`DiscoveryCategory` is `Emulator | Steam | Local`. The desktop `types.ts` mirrors
it. Deduplication merges `sources` and paths, then recomputes the category.

### One title normalization, shared

`discovery::match_key(title)` is the only title comparison used by dedupe, the
duplicate refusal, and the UI match. It lowercases (Unicode), trims, collapses
whitespace, and strips a trailing ` (proton)` suffix. It deliberately does not strip
punctuation, which could merge genuinely different games.

The backend computes `existing` against both enrolled and unlinked games, so the UI
never re-implements matching.

The current Steam scanner emits `"<name> (Proton)"` as a separate candidate from
`"<name>"`. The shared key merges them, so the Emulator-beats-Steam rule actually
applies to the common Proton case.

### Discovery report

`discover_games` returns `DiscoveryReport` rather than a bare list:

```text
DiscoveryReport { games, total, truncated }
```

This makes the 1,000-candidate cap visible instead of ambiguous when exactly 1,000
results are returned.

### Enrollment command

The current `enroll_discovered(game_id)` cannot work: discovery never registers a
candidate row, so there is no id to pass. Replace it with:

```text
enroll_discovered_candidate(title, adapter_type, locations: Vec<String>)
    -> EnrollOutcome
```

```text
add_discovered_locations(game_id, locations: Vec<String>) -> EnrollOutcome
```

```text
list_locations(game_id) -> Vec<Location>   // needed for the tracked-card flow
```

`EnrollOutcome { game, added: Vec<String>, skipped: Vec<SkippedPath> }` reports
partial validation instead of failing silently. `SkippedPath { path, reason }` uses
plain reasons: missing, duplicate, nested, broad-confirmation-required, over-cap.

Rules for both add commands:

- Validate every path with `paths::validate_root`; only existing directories count.
- Drop duplicates and any path nested inside another path in the same request
  (parent wins).
- Reject — do not silently drop — any path that overlaps an **already attached**
  location of the same game, in either direction (new parent of existing child, or
  new child of existing parent). Overlap would archive the same files under two
  slots, back up twice, and write twice on restore. Report it as `overlapping`.
- Cap at 16 locations per game. The cap is enforced in
  `SaveSyncEngine::add_directory_location`, so the manual library path cannot bypass
  it either.
- Create/attach, start watching every new location, and return the outcome.
- Refuse if an enrolled game with the same `match_key` already exists; the UI then
  calls `add_discovered_locations` after confirmation.
- If the match is an **unlinked** game, do not create a second row. The UI offers
  **Re-link** (calls `engine.enroll`) plus the same new-folder confirmation. This
  preserves the logical UUID rule.

Neither command writes to save files.

### Existing locations and broad folders

`add_discovered_locations` never modifies or removes an existing location. When
discovery finds a parent of an existing location, the overlap is reported and
skipped; changing an existing location is a future, explicit action.

The current Steam and Lutris scanners can return a broad root (`drive_c/users/<user>`
or `drive_c/users`). This project does **not** narrow the scanners, because that risks
missing saves. Instead:

- `SavePath.broad` marks known profile roots.
- Broad folders are displayed but **unchecked by default**, with a plain warning:
  "This folder can contain unrelated personal files."
- Checking one requires an explicit confirmation in the add step.
- Broad and narrow folders for the same candidate are still merged as usual.

Narrowing the scanners to likely save subfolders is a documented follow-up, not part
of this project.

## Multi-location model

### Locations get a stable slot

Add `slot INTEGER NOT NULL` to `locations`, unique per game, assigned as
`max(slot) + 1` when a location is added. Existing rows are backfilled by `rowid`
order (the table has no creation timestamp). The slot is internal and stable for the
life of the location.

### Manifest format v2

One revision still has one manifest and one archive. To describe several roots, v2
prefixes every entry with `loc-<slot>/`:

```
loc-0/Documents/MyGame/save1.dat
loc-1/remote/save1.dat
```

The manifest digest keeps covering path, size, and SHA-256, so identical content
still deduplicates. `MANIFEST_FORMAT_VERSION` becomes 2.

- Snapshot walks every location, prefixes entries, and archives each entry from its
  owning root.
- `BlobStore` gains a multi-root writer that maps a prefix to a root. The archive
  layout needs no other change.
- A new table `revision_locations(revision_id, slot, location_id)` records the
  slot-to-location mapping for local restores.

**Version must be stored, not inferred.** `Manifest::new` currently stamps the
current constant, and `store::load_manifest` rebuilds through it. Bumping the
constant would change the hash of every existing manifest, breaking restores and
sync hash checks. Add `format_version` to `manifest_sets` and a
`Manifest::new_with_version(version, entries)` constructor:

- loaded manifests use the stored version;
- new snapshots store version 2;
- `from_json` accepts the version in the document instead of overriding it.

**Slots are per-device.** `loc-0` on one device is not necessarily `loc-0` on
another, so identical multi-folder content may not deduplicate across devices. That
is accepted; content dedupe within a device and for single-location content is
unchanged.

### Restore

- Group manifest entries by prefix.
- Resolve every slot to a current location. If any slot is unmapped, refuse before
  writing and report the missing slots.
- Stage the whole archive once under `staging/<restore_id>/`, verifying every file.
- Capture and verify a safety backup for every existing target root under
  `rollback/<game>/<restore_id>/loc-<slot>/`.
- Write the journal row before touching live files. Extend `restore_journal` with a
  `locations_json` column holding per-location target, staging, backup, and backup
  manifest data. Legacy rows keep using the existing columns.
- Apply per location. On any failure, roll back every location that was touched,
  mark the row `rolled_back` or `rollback_failed`, and keep all backups.
- Verify all roots before marking the row `applied` and moving the active head.
- Recovery rolls back every location recorded in `locations_json`.

### Backward compatibility

- Existing v1 manifests (no prefixes) keep their stored version and hash.
- Migration backfills `revision_locations` for every pre-upgrade revision to that
  game's single existing location. v1 restores use that mapping, **not**
  `single_directory_location`, so adding a second folder later does not strand old
  history.
- New snapshots always produce v2.
- Sync providers treat manifests as opaque, so the protocol does not change.
- A v2 revision is only restorable on a device that has a location for every slot.
  Locally that is always true unless a location was removed.

### Cross-device behaviour

Sync may pull a v2 revision from another device. Its slots refer to the other
device's location ids. Rather than guess, restore refuses with:

> This revision contains 2 save folders from another device. Map them to local
> folders before restoring.

The mapping UI is a follow-up project. Pulling and keeping the revision is safe; no
live files are touched.

## UI changes

- New component `savesync-desktop/src/components/DiscoveryView.tsx`; keep `App.tsx`
  as the shell and tab switcher.
- New API functions in `api.ts`: `discoverGames`, `enrollDiscoveredCandidate`,
  `addDiscoveredLocations`, `listLocations`.
- New types in `types.ts` for `DiscoveryCategory`, `DiscoveredGame.sources`,
  `SavePath`, `ExistingMatch`, `DiscoveryReport`, and `EnrollOutcome`.
- Broad folders render unchecked with the warning text; checking one asks for
  confirmation before the add is sent.
- Unlinked matches show **In library (unlinked)** with a **Re-link** action plus
  **Add N new folders**, instead of creating a duplicate game.
- Styles follow the existing card/chip language in `styles.css`; add
  `.discovery-*` rules rather than restructuring existing styles.
- Keyboard: space toggles the focused card checkbox; Enter opens the context menu;
  Escape closes it. Buttons have accessible labels.
- Loading, empty, error, and partial-result states are explicit. A failed scan keeps
  previous results visible with a retry action.

## Watcher contract

The save watcher currently allows one root per game (`watcher::add_root` errors when
a game is already registered) and `unwatch` removes one root. Extend it to N roots:

- `watch(game_id, root)` rejects only the exact same path, not a second root for the
  same game.
- `unwatch(game_id)` removes every root for that game and discards its tracker
  state.
- Dirty attribution already uses longest-prefix matching, so an event under one root
  still reports the right game.
- The Tauri layer starts a watch for every attached location and unwatches all of
  them on unlink.

Without this, only the first folder would get dirty badges and the rest would be
silently unwatched.

## Performance and responsiveness

- Scanning stays on `spawn_blocking`; the UI never blocks.
- The Ludusavi manifest keeps its ETag SQLite cache. A scan is only started by the
  user.
- Discovery results are capped at 1,000 candidates per scan with a visible notice;
  the cap protects the IPC payload and the DOM.
- Folder validation and enrollment are bounded by the 16-location cap.
- No per-keystroke backend calls: filtering and search run on the in-memory result
  set.

## Safety checklist

- [ ] No enrollment without an explicit add action.
- [ ] Every path validated before enrollment; missing, duplicate, nested, and
      overlapping paths never reach a snapshot.
- [ ] Broad profile roots require explicit confirmation.
- [ ] Snapshot fails closed if any configured location is missing or unreadable.
- [ ] Restore validates every slot before any live write.
- [ ] Safety backup per location, verified before the journal row is written.
- [ ] Any failure rolls back all touched locations or records `rollback_failed` with
      retained backups.
- [ ] Manifest and archive paths remain sanitized; prefixes are generated, never
      user-supplied.
- [ ] Existing single-location tests and legacy revisions keep passing.

## Testing

Core unit/integration:

- category derivation and precedence; multi-source dedupe keeps all sources;
- `match_key` merges `"Foo"` and `"Foo (Proton)"` and matches enrolled/unlinked games;
- nested-path pruning, duplicate paths, overlap with existing locations, 16-location
  cap (including the manual `add_location` path);
- broad-path flagging and default-unchecked behaviour;
- slot assignment is stable and unique per game;
- stored manifest version keeps v1 hashes unchanged; v2 round-trip and slot isolation;
- multi-location snapshot produces one revision covering every root;
- restore restores every root and rejects a missing slot before any write;
- second-location install failure rolls back the first location;
- crash recovery with `locations_json` restores all locations;
- legacy v1 restore still works after the game gains a second location;
- watcher accepts N roots per game and `unwatch` removes all;
- sync round-trip of a v2 revision leaves live files untouched.

Desktop:

- manual scan, filter counts, search, multi-select, context menu, tracked and
  unlinked-game flows, broad-folder confirmation;
- error and empty states; no console errors.

## Delivery order

1. Core discovery: `sources`, `save_paths`, `category`, `match_key`, `existing`,
   report shape, enroll payload validation, broad flagging.
2. Core multi-location: stored manifest version, slot migration, v1 backfill,
   manifest v2, snapshot/restore/journal, watcher N roots, global cap.
3. Tauri commands and watcher wiring for each location.
4. Discovery UI and API.
5. Verification: `cargo fmt`, `clippy -D warnings`, full core tests, desktop build,
   manual flow.

## Calls made by the builder (check these)

- Discovery is a tab, not a modal.
- Scanning is manual; no startup scan and no background refresh.
- Categories are best-effort heuristics; every folder stays visible.
- Emulator beats Steam when a title matches both.
- Epic games are not detected as a separate source; Ludusavi may still find them and
  they appear under Local.
- Selection clears when filters change, to avoid adding hidden games.
- 16 locations per game and 1,000 candidates per scan are hard caps, enforced in the
  engine as well as the discovery commands.
- Manifest format version is stored per manifest set so old hashes never change;
  pre-upgrade revisions are backfilled to their game's single location.
- Slots are per-device; cross-device multi-folder content may not dedupe.
- Paths overlapping an existing location are skipped with a reason, never silently
  merged or replaced.
- Broad profile roots (e.g. `drive_c/users/<user>`) stay visible but unchecked and
  need explicit confirmation; scanners are not narrowed in this project.
- Unlinked games are matched and offered a re-link instead of a duplicate row.
- Cross-device multi-location restore is blocked, not guessed.
