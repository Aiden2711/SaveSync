# SaveSync — Design system

**Last updated:** 2026-10-23
**Mode:** Operate (all surfaces). The visitor is mid-task; familiarity and precision beat expression.

## The world

**Console System UI, cool.** SaveSync's shell behaves like a console's home menu rather
than a desktop utility: a light, cool, slightly blue-tinted system surface with white
tiles, a single system-blue highlight, soft depth, and art-first game tiles. Dark mode is
the same system at night — near-black with a blue cast, graphite panels, and a brighter
blue highlight. Nothing warm, nothing sepia, no grain, no skeuomorphic plastic.

References for the grammar (not the pixels): Wii / Wii U / 3DS system software. Library
behaviour references: RomM (cover-first grid, platform meta), Onyx (context menus, cover
flow), OpenBoxGL (console-density detail panels).

Light and dark are two states of one system, not two brands. Every token exists in both.

## Type

| Role | Family | Use |
| --- | --- | --- |
| Display (`--font-display`) | **Fredoka** (self-hosted, variable, OFL) | Wordmark, view headings, game titles |
| UI (`--font-ui`) | System humanist sans: Segoe UI Variable Text → Segoe UI → system-ui → Inter → Helvetica Neue → Arial | Body, labels, buttons, tables |
| Data (`--font-mono`) | ui-monospace / Cascadia Mono / Consolas | Hashes, revision ids, byte counts, timestamps |

Fredoka is vendored at `savesync-desktop/src/assets/fonts/fredoka-latin-var.woff2` with its
OFL licence beside it. It is the only non-system face, and it is never used for controls,
labels, or data.

Scale (fixed px, ratio ≈1.15): `14 / 15 / 15.5 / 18 / 22 / 28 / 34`.
Nothing renders below 14px. No eyebrow/kicker labels above headings.

## Colour

Tokens are the only colour source. No raw hex outside `:root` / `[data-theme]`.

### Light — "System White"

| Token | Value | Notes |
| --- | --- | --- |
| `--base` | `#eef3f9` | Cool app field |
| `--base-2` | `#e6edf6` | Toolbar / rail |
| `--surface` | `#ffffff` | Panels, tiles |
| `--surface-2` | `#e4ebf4` | Hover, selected, wells |
| `--line` | `#ccd8e6` | Hairline separators |
| `--line-strong` | `#6d8098` | Control boundaries (≥3:1) |
| `--ink` | `#101720` | Primary text |
| `--ink-2` | `#465468` | Secondary text |
| `--ink-3` | `#5f6d80` | Icons, non-text |
| `--accent` | `#0a6db0` | System blue |
| `--accent-hover` | `#095f99` | |
| `--accent-ink` | `#ffffff` | Ink on accent fill |
| `--focus-ring` | `#0a6db0` | |
| `--danger` | `#b3261e` | |
| `--danger-ink` | `#ffffff` | |
| `--warn-ink` | `#8a5207` | Warning text |

### Dark — "Graphite Night"

| Token | Value | Notes |
| --- | --- | --- |
| `--base` | `#0b0f15` | Near-black, blue cast |
| `--base-2` | `#0a0d12` | Toolbar / rail |
| `--surface` | `#151b24` | Panels, tiles |
| `--surface-2` | `#1e2733` | Hover, selected, wells |
| `--line` | `#2b3746` | Hairline separators |
| `--line-strong` | `#62778f` | Control boundaries (≥3:1) |
| `--ink` | `#e8eff8` | |
| `--ink-2` | `#aab9ca` | |
| `--ink-3` | `#8b99ab` | |
| `--accent` | `#5cbaff` | |
| `--accent-hover` | `#82cbff` | |
| `--accent-ink` | `#04121d` | Ink on accent fill |
| `--focus-ring` | `#5cbaff` | |
| `--danger` | `#ff6b5e` | |
| `--danger-ink` | `#150604` | |
| `--warn-ink` | `#f2c36b` | |

### Verified pairs (WCAG 2.1 relative luminance)

Light: ink/base 16.15, ink/surface 18.02, ink-2/surface 7.70, ink-2/surface-2 6.41,
ink-3/surface 5.27, accent/surface 5.49, accent/base 4.92, accent-ink/accent 5.49,
focus/surface-2 4.57, line-strong/surface-2 3.37, danger-ink/danger 6.54,
danger-text/surface 6.54, warn-ink/surface 6.38.

Dark: ink/base 16.58, ink/surface 14.94, ink-2/surface 8.65, ink-2/surface-2 7.54,
ink-3/surface 5.97, accent/surface 8.17, accent/base 9.08, accent-ink/accent 8.94,
focus/surface-2 7.12, line-strong/surface-2 3.27, danger-ink/danger 7.09,
danger-text/surface 8.53, warn-ink/surface 10.54.

The accent is state, never decoration: primary action fill, current selection, live
indicator, focus. It must not become the ordinary border of every card.

## Shape, depth, motion

- Radii: controls `8px`, tiles/panels `12px`, dialogs `16px`. Nothing pill-shaped except
  the dirty-state dot.
- Shadows carry an offset and a soft blur. Light: `0 1px 2px rgba(16,23,32,.06),
  0 10px 26px rgba(16,23,32,.10)`. Dark: `0 1px 2px rgba(0,0,0,.55),
  0 12px 30px rgba(0,0,0,.45)`. No zero-offset halos.
- Motion is state feedback only: 140–200ms, `cubic-bezier(.2,0,0,1)`. Tile hover lifts
  1px and raises its shadow; cover art scales 1.02 inside its clip. Nothing animates on
  page load. `prefers-reduced-motion` removes the lift and scale.
- Grain, blur, and gradient text do not exist in this world.

## Components

- **Wordmark:** "SaveSync" in Fredoka 600 with tight tracking. No badge, no monogram tile,
  no tagline next to it.
- **Top bar:** one row, white surface over `--base`, hairline bottom border. Nav buttons
  are quiet text+icon, current destination gets a `--surface-2` well and `--ink`.
  Theme toggle at the far right.
- **Tiles:** cover art occupies the tile's upper field, edge to edge, `2:3`. Label block
  below on `--surface`: title in Fredoka 600, then adapter and snapshot state in UI sans
  `--ink-2` at 14px. Every game gets the same channel — no promoted "first game" hero.
  Dirty state is a single accent dot with a text name. Selected tile uses `--surface`
  plus a `--line-strong` border, a focus-ring outline and the check badge — never the
  accent as a border.
- **Context menu:** right-click (or the `ContextMenu` key / Shift+F10) opens a `role="menu"`
  at the pointer, clamped to the viewport and below the top bar. It never changes the
  current view. "Open details" is the only item that navigates. Escape returns focus to
  the tile.
- **Detail view:** cover on the left (`160px`), title and meta beside it, actions at the
  right. Back returns to the library grid.
- **Dialogs:** native `<dialog>`, Cancel owns initial focus, Escape and backdrop never
  call a destructive API.
- **Empty states** teach the next action; they do not apologise.

## Cover art

Real artwork is not wired to a provider yet. Until it is, `CoverArt` draws a deterministic
portrait cover per title: a cool two-tone field, one clipped rounded-square motif, and the
title's monogram in Fredoka. Palettes are hash-derived and include both light-field and
dark-field variants, so a library never turns into one flat colour. Covers are decorative
(`aria-hidden`); the tile button carries the accessible name. When `cover_path` is present
the component will render that image instead of the generated field.

## Do not

- Reintroduce warm neutrals, sepia, amber accents, or film grain.
- Use mono for prose, labels, or navigation.
- Add eyebrow labels, gradient text, glass effects, icon-plus-heading-plus-text card grids.
- Let the accent colour ordinary borders.
- Let a right-click leave the library.
