# SaveSync — Design system

**Last updated:** 2026-11-02
**Mode:** Operate (all surfaces). The visitor is mid-task; familiarity and precision beat expression.
**Source of truth for values:** `savesync-desktop/src/styles.css` (token blocks and component
rules). `savesync-desktop/mockups/system-white-iisu.html` previews the same system as static
HTML. If this document and the stylesheet disagree, the stylesheet wins and this document is
the bug.

## The world

**iiSU-inspired light glass.** SaveSync's shell reads like a console's home screen: a cool
white field carrying a faint dot grid, white glass cards with hairline frames, a floating glass
dock, a HUD top bar, and multi-stop gradients used as accents and as badge fills. Dark mode is
the same world at night — graphite glass on a near-black field — not a second brand. Both
themes declare the same token names, so no component rule branches on theme.

References for the grammar (not the pixels): the iiSU home screen
(<https://github.com/iisu-network/iiSU>) — cited as **design inspiration only**, never as a
spec, a dependency, or a source of assets. Library behaviour references: RomM (cover-first
grid, platform meta), Onyx (context menus, cover flow).

The earlier "Console System UI, cool" brief is superseded and withdrawn: no warm surfaces, no
grain, no `SS` brand badge, no flat single-accent highlight language.

## Type

| Role | Family | Use |
| --- | --- | --- |
| Display (`--font-display`) | **Fredoka** (self-hosted, variable, OFL) | Wordmark, view headings, game titles, dialog titles |
| UI (`--font-ui`) | System humanist sans: Segoe UI Variable Text → Segoe UI → system-ui → Inter → Helvetica Neue → Arial | Body, labels, buttons, tables |
| Data (`--font-mono`) | ui-monospace / Cascadia Mono / Consolas | Hashes, revision ids, byte counts, timestamps |

Fredoka is vendored at `savesync-desktop/src/assets/fonts/fredoka-latin-var.woff2` with its
OFL licence beside it. It is the only non-system face, and it is never used for controls,
labels, or data.

Scale (fixed px, ratio ≈1.15): `14 / 15 / 15.5 / 18 / 22 / 28 / 34`. Nothing renders below
14px. No eyebrow/kicker labels above headings.

## Colour

Tokens are the only colour source. No raw hex outside `:root` / `[data-theme]`.

### Light — "System White" (default)

| Token | Value | Notes |
| --- | --- | --- |
| `--base` | `#eef3fa` | Cool app field, under the dot grid |
| `--base-2` | `#e2ebf7` | Wells, recessed rows |
| `--surface` | `rgba(255,255,255,.88)` | In-flow panels and buttons |
| `--surface-2` | `rgba(226,235,247,.94)` | Hover, current destination, tile matte |
| `--glass` | `rgba(255,255,255,.72)` | Top bar, dock, in-flow panels |
| `--glass-top` | `rgba(255,255,255,.94)` | Floating cards: context menu, dialogs |
| `--line` | `rgba(100,116,139,.25)` | Translucent separator |
| `--hairline` | `rgba(100,116,139,.22)` | The 1px frame on every glass surface |
| `--line-strong` | `#64748b` | Control boundaries (≥3:1) |
| `--ink` | `#0d1522` | Primary text |
| `--ink-2` | `#3b4a5f` | Secondary text |
| `--ink-3` | `#55657c` | Icons, non-text, disabled |
| `--ink-inverse` | `#ffffff` | Glyph knocked out of a badge |
| `--accent` | `#1259c4` | System blue: selection, focus, primary state |
| `--accent-hover` | `#2a6fd6` | |
| `--accent-ink` | `#ffffff` | Ink on accent fill |
| `--accent-text` | `#1259c4` | Accent as text |
| `--focus-ring` | `#1259c4` | |
| `--danger` / `--danger-text` / `--danger-ink` | `#b3261e` / `#b3261e` / `#ffffff` | |
| `--warn` / `--warn-ink` | `#8a5207` / `#8a5207` | Warning text |
| `--scrim` | `rgba(20,30,44,.28)` | In-flow overlay |
| `--scrim-strong` | `rgba(4,7,11,.72)` | Dialog backdrop |
| `--primary-ink` | `#ffffff` | Ink on `--grad-primary` |

### Dark — "Graphite Night"

| Token | Value | Notes |
| --- | --- | --- |
| `--base` | `#070b12` | Near-black, blue cast |
| `--base-2` | `#0d1119` | Wells |
| `--surface` | `rgba(19,26,38,.88)` | Panels, buttons |
| `--surface-2` | `rgba(34,45,62,.92)` | Hover, current destination, tile matte |
| `--glass` | `rgba(14,20,31,.7)` | Top bar, dock |
| `--glass-top` | `rgba(19,26,38,.94)` | Floating cards |
| `--line` | `rgba(124,142,166,.22)` | |
| `--hairline` | `rgba(156,168,184,.2)` | |
| `--line-strong` | `#7c8ea6` | Control boundaries |
| `--ink` | `#e9f1fb` | |
| `--ink-2` | `#b7c6da` | |
| `--ink-3` | `#97a7bd` | |
| `--ink-inverse` | `#ffffff` | |
| `--accent` / `--accent-text` | `#59c2ff` / `#7ed0ff` | |
| `--accent-hover` | `#7ed0ff` | |
| `--accent-ink` | `#05121d` | Ink on accent fill |
| `--focus-ring` | `#59c2ff` | |
| `--danger` / `--danger-text` / `--danger-ink` | `#ff6b5e` / `#ff9e95` / `#150604` | |
| `--warn` / `--warn-ink` | `#f2c36b` / `#f2c36b` | |
| `--scrim` / `--scrim-strong` | `rgba(4,7,11,.66)` / `rgba(0,0,0,.72)` | |

Shape tokens (`--radius-*`, `--shadow-*`), type, spacing, and the hue/gradient sets are
declared once in `:root` and shared; each theme re-declares only the values it changes.

### Gradients

| Token | Stops | Allowed use |
| --- | --- | --- |
| `--grad-brand` | `#2f7fe0 → #8b5cf6 → #ec4899` (light) · `#7db9ff → #a97cf0 → #f47bd8` (dark), 112deg | **Wordmark lettering only** — the single exception to "gradients never carry text" |
| `--grad-primary` | `#5b21b6 → #a21caf`, 90deg (`--grad-primary-hover` one step brighter) | Primary button fill and the settings segment indicator. The only gradient allowed under text, and only with `--primary-ink` |
| `--grad-warm` | `#5b21b6 → #a21caf → #22d3ee` | Selected-tile ring (as a 2px stroke, painted through a mask) |
| `--grad-cool` | `#0ea5e9 → #22d3ee` | Decorative accent |
| `--grad-gold` | `#f59e0b → #fbbf24` | Decorative accent |
| `--grad-teal` | `#0d9488 → #14b8a6` | Decorative accent |
| `--grad-rose` | `#e11d48 → #fb7185` | Decorative accent |

### Hue badges

Each destination owns a two-stop hue pair, used as the fill of its 28px nav badge and nothing
else. Light values are deep enough to hold a white glyph; dark values are lifted pastels with a
scrim gradient over them (`rgba(7,11,18,.1)` → `.38`) so the glyphs stay legible on graphite.

| Badge | Light | Dark |
| --- | --- | --- |
| `library` | `#6d28d9` → `#c026d3` | `#a78bfa` → `#e879f9` |
| `snapshots` | `#0e7490` → `#0369a1` | `#22d3ee` → `#60a5fa` |
| `conflicts` | `#b45309` → `#d97706` | `#fbbf24` → `#fb923c` |
| `cloud` | `#0f766e` → `#0d9488` | `#2dd4bf` → `#34d399` |
| `settings` | `#be123c` → `#db2777` | `#fb7185` → `#f472b6` |

Neutral badge pair (`--badge-neutral-1/2`): `#475569 → #64748b` light, `#334155 → #64748b`
dark, with `--badge-inset` (top sheen) and `--badge-lift` (drop shadow).

### Glass, hairline, dot grid

- `--dot-grid` is a `radial-gradient` dot every 22px, painted under `--base` on `body`:
  `rgba(38,64,102,.16)` dots in light, `rgba(150,190,255,.18)` in dark. It is a field texture,
  not a container.
- Glass surfaces pair a translucent fill with a 1px `--hairline` frame; there is **no**
  `backdrop-filter` blur anywhere — top-layer cards raise `--glass-top` opacity instead, which
  is why text over cover art stays crisp.
- In-flow panels use `--glass` + `--shadow-near`; floating cards use `--glass-top` +
  `--shadow-float`.

### Contrast commitments

Measured with WCAG 2.1 relative luminance against the token values (translucent surfaces
evaluated over their solid backdrop). Text pairs clear **4.5:1**; boundaries, focus rings, and
large text clear **3:1**.

Light: ink/white 18.3, ink/base 16.4, ink-2/base 8.1, ink-3/base 5.3, ink-3/`--surface-2` 4.9,
accent/white 6.5, accent/base 5.8, `--primary-ink` on the darkest `--grad-primary` stop 6.3,
warn-ink/base 5.7, danger-text/base 5.9, `--line-strong`/base 4.3,
`--line-strong`/`--surface-2` 4.0. Wordmark stops on the light glass bar: 3.9 / 4.1 / 3.4 —
large text (22px/700) against a 3:1 bar.

Dark: ink/base 17.3, ink-2/`--surface` 10.1, ink-3/`--surface` 7.1, accent-text/base 11.6,
`--accent-ink`/`--accent` 9.5, warn-ink/base 12.0, danger-text/base 9.9, `--line-strong`/base
5.9. Wordmark stops on the graphite glass bar: 9.2 / 6.1 / 7.8.

Badge glyphs are decoration (`aria-hidden`; the destination name is the text), so the 3:1
non-text bar applies. Measured across the scrim range they run 3.8–11.5:1 in light and
2.1–5.9:1 in dark; the dark gold and pink badges sit below 3:1 at the light end of their scrim
and are a known gap, not a sanction to lower any text pair.

The accent is state, never decoration: primary action fill, current selection, live indicator,
focus. It must not become the ordinary border of every card.

## Shape, depth, motion

- Radii: controls `8px` (`--radius-control`), tiles and panels `12px` (`--radius-panel`),
  dialogs `16px` (`--radius-dialog`); the dock takes `16px` as a floating panel. Nothing
  pill-shaped except the dock status chip, its live dot, and the dirty-state dot.
- Shadows carry an offset and a soft blur, and are named by distance:
  `--shadow-near` `0 1px 2px rgba(16,23,32,.06)`, `--shadow-far`
  `0 10px 26px rgba(16,23,32,.1)`, `--shadow-float` `0 12px 30px rgba(16,23,32,.12)`
  (dark: `rgba(0,0,0,.55)` / `.45` / `.45`). No zero-offset halos, no inner glow as depth.
  Selected tiles add a hue-tinted glow from `--hue-library-*`, never a second border.
- Motion is state feedback only: `--dur-fast` 140ms, `--dur` 200ms, `--ease`
  `cubic-bezier(.2,0,0,1)`. Tile hover lifts 1px and raises only the artwork's shadow; cover
  art scales 1.02 inside its clip; controls set buttons scale 0.985 on press; the dock dot is
  the only looping animation (1.1s pulse). Nothing animates on page load.
- `prefers-reduced-motion: reduce` drops the lift, the scale, the pulse, and control
  transitions, while keeping colour changes on hover/focus/press.

## Components

- **HUD top bar:** full-width glass strip (`--glass`) with a `--hairline` bottom edge, wordmark
  left, actions right. The wordmark is "SaveSync" in Fredoka 700 at 22px, letter-spaced
  `-0.02em`, painted with `--grad-brand` through `background-clip: text` (with the solid `--ink`
  fallback for engines without it) — the **only gradient text in the app**. No badge, no
  monogram tile, no tagline beside it.
- **Theme toggle:** a 34px neutral badge — the `--badge-neutral` pair under the badge scrim,
  inset sheen, lift shadow — holding a filled white sun or moon glyph (`--ink-inverse`). Hover
  is a `filter` lift, so no border colour can creep in. `aria-label` names the destination
  theme, `aria-pressed` reports the current one.
- **Floating glass dock:** centred, `max-width: min(920px, calc(100% - 24px))`, 12px inset,
  16px radius, `--glass` fill, `--hairline` frame, `--shadow-float`. Three parts: a status chip
  (pill, `--surface` fill, live dot + ellipsised count text), a hairline divider, and five
  destinations. Each destination is a 44px-target button: a 28px rounded-square gradient badge
  with a white glyph, plus its label in UI sans; the current destination is a `--surface-2`
  well with `--ink` text, never an accent edge.
  Shrink ladder: the chip yields first (`flex: 0 1 auto`, ellipsis), the nav island keeps its
  buttons and takes leftover space as symmetric auto margins; below 760px the divider is
  hidden, labels are visually clipped (still in the DOM and the accessible name), targets stay
  44px wide, and the chip takes the remaining width.
- **Tiles:** a bare channel, not a card — transparent fill, transparent 1px border for geometry,
  no shadow. The 2:3 artwork sits in a `--surface-2` matte with a 4px inset, one `--hairline`
  frame just inside its edge, and the cover art filling the box (never letterboxed). Title
  (Fredoka 600, 18px, two-line clamp) and adapter/snapshot meta (`--ink-2`, 14px) sit **directly
  on the dot-grid field** — there is no label plate in any state. Selection draws a 2px
  `--grad-warm` ring through a mask, keeps the `--line-strong` border, adds the hue glow, and
  reveals the check badge; keyboard focus is a separate neutral `--focus-ring` outline drawn
  outside the card. Hover raises the artwork, not the label.
- **Detail view:** cover on the left (160px, 120px under 760px), title and meta beside it,
  actions on the right, back control above. Back returns to the grid.
- **Context menu:** right-click (or the `ContextMenu` key / Shift+F10) opens a `role="menu"`
  glass card — `--glass-top` fill, `--hairline` frame, `--shadow-float`, 12px radius, no blur —
  at the pointer, clamped to the viewport below the top bar and above the dock. Items are
  `--radius-control` rows on `--surface-2` when hovered; the danger item flips to `--danger`
  fill with `--danger-ink`. It never changes the current view; "Open details" is the only item
  that navigates; Escape returns focus to the tile.
- **Dialogs:** native `<dialog>` with `--scrim-strong` on `::backdrop`, a `--glass-top` card at
  16px radius capped by the viewport. Cancel is a peer action with a real `--line-strong`
  boundary and owns initial focus; Escape and backdrop clicks never call a destructive API.
- **Cloud view:** a provider picker as a segmented control (`role="group"`, `aria-pressed`) over
  Google Drive / Dropbox / WebDAV, with a gradient indicator under the pressed segment. One
  panel below it, titled with the active provider's name, carries that provider's fields
  (Drive connect/disconnect, Dropbox app key, WebDAV endpoint/username/password), its status
  line, and the connect/sync controls. The status chip expresses state through the accent token
  (`Connected`, `Waiting for sign-in`, `Not connected`) rather than a separate success palette,
  so a red `.error` line beside it is the only colour-coded failure cue. Secrets live in forms
  read at submit time and are never copied into React state. The provider heading is reused
  verbatim in sync labels, so the panel and the action can never disagree. Drive scope follows
  `savesync-docs/ADR-0001.md` (`drive.file`).
- **Empty states** teach the next action; they do not apologise.

## Cover art

`CoverArt` prefers real artwork: when `cover_path` is present the tile and the detail view
render that cached image (served through the scoped URI scheme). Only when it is absent does it
draw generated art, deterministically per title: one hash domain picks a light or dark field
(45% light), a second picks a palette inside that field (three light, seven dark curated sets),
and a third picks the crest motif (concentric rounded squares, concentric circles, or slats), so
a shelf always mixes fields and never flattens into one colour. The artwork adds a vertical wash
(flat upper half settling toward the second field colour), a diagonal gloss that fades before
the crest, and a hairline double frame; the title supplies a one- or two-letter monogram in
Fredoka at the shared crest centre. Covers are decorative (`aria-hidden`) because the tile
button carries the accessible name.

## Do not

- Emit raw colours outside `:root` / `[data-theme]` token blocks.
- Put gradient text anywhere but the wordmark.
- Let any gradient carry text except `--grad-primary` under `--primary-ink`.
- Use the accent (or a hue) as the ordinary border of a card, row, or control.
- Put a plate, card, or shadow behind a tile label; labels sit on the field.
- Blur under text: no `backdrop-filter` behind body copy or menu items.
- Use pill shapes outside the dock status chip, the live dot, and the dirty dot.
- Reintroduce warm neutrals, sepia, amber accents, or film grain.
- Use mono for prose, labels, or navigation.
- Add eyebrow labels, icon-plus-heading-plus-text card grids, or a second "cool" flat-accent
  system layered over this one.
- Let a right-click leave the library.
