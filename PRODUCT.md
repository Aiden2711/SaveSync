# SaveSync — Product context

**Last updated:** 2026-10-23

## What it is

SaveSync is a local-first desktop app that versions game save data. Players enroll a
save folder, take immutable snapshots, and restore any revision — safely, offline, and
without a cloud account. Google Drive sync is optional and always manual.

## Who it's for

Players who have lost saves to a crash, a reinstall, a bad mod, or a second device. They
are technical enough to find a save folder but are not managing a server. They judge the
app the way they judge a console's home menu: in two seconds, by whether their library
feels like *theirs*.

## Product truth (from AGENTS.md, not up for redesign)

- Safety and data integrity first. Nothing deletes or overwrites the only known-good save.
- Manual first. No automated backup, sync, or enrollment without explicit user action.
- History is a DAG of immutable revisions, not timestamps.
- Local backup and restore must work fully offline; remote is never a runtime requirement.
- Discovery never enrolls on its own; enrollment is always an explicit choice.
- Platform adapters isolate OS/tool specifics from the core.

## Surfaces

| Surface | Mode | Job |
| --- | --- | --- |
| Library (game grid) | Operate | Find a game, see its state, act on it in one right-click. |
| Game detail | Operate | Snapshot, review history, restore — with destructive actions gated. |
| Snapshots / Conflicts | Operate | Per-game history and unresolved conflicts. |
| Cloud | Operate | Optional Google Drive connection and manual sync. |
| Settings | Operate | Theme, integrations. Secrets never echoed. |

## Design brief (from the user, 2026-10-23)

- **Remove the "SS" badge and its lettering.** At most the brand reads "SaveSync", set
  in a fun, rounded face.
- **Light mode must be cool**, not warm: the white of the Wii / Wii U system menu.
- **Dark mode must be cool and neutral-to-deep**: dark grey, black, dark blue. No warm
  brown.
- **The library must look like a game library**: cover art on every title.
- **Right-click must open a context menu**, never navigate away from the library.
- **Style references:** Wii U, Wii, 3DS console system UI. Library-behaviour references:
  RomM, Onyx, OpenBoxGL.

## Assumptions made without an interview

The user's brief is treated as final for direction. Where it is silent:

- Typography stays functional: one humanist sans for UI, one rounded display face for
  the wordmark and content titles only.
- Cover art is generated locally per title until real art can be fetched; no network
  artwork provider is wired in the frontend.
- No new library features (search, filters, collections, list view) are invented.

## Non-goals

- Replacing the safety, sync, adapter, or database design.
- Automatic enrollment or automatic artwork download.
- Browser-based use; the app ships inside a Tauri shell.
