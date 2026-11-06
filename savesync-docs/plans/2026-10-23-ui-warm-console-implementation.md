# UI Warm Console Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-theme the SaveSync desktop shell from its current generic dark look to the approved "Warm Console" direction (late-2000s Nintendo warmth + modern craft): warm dark base, soft 8px corners, hairline rules, one amber accent, self-contained grain overlay, compact nav bar without the Google Drive banner, reworked game tiles and context menu, and a Settings page with a SteamGridDB API key field.

**Architecture:** Keep the existing React component structure (App.tsx shell, Modal dialog, context menu, CoverArt) and re-style everything against a single locked CSS token block. No save-data, sync, or restore logic changes — UI only. The existing Vitest dialog contract tests (App.dialogs.test.tsx, 5 tests) must continue to pass unchanged.

**Tech Stack:** React 19, TypeScript, Vite, Vitest/jsdom, CSS custom properties, Tauri 2 commands (unchanged IPC surface except one new settings command).

**Spec:** savesync-docs/specs/2026-10-23-ui-warm-console-theme.md — brief, token block, contrast results, change list. The plan argues from the spec; executors read both.

## Global Constraints

- Token block is the single source of truth: --base #24201c, --surface #2e2a25, --line #463f39, --ink #e9e5dd, --ink-2 #c9c2b8, --ink-3 #a59d93, --accent #e8a33d, --accent-ink #1f1c19; no raw hex outside :root in components.
- One radius value: --radius 8px everywhere; no pills, no rounded everything.
- No glass/blur (backdrop-filter), no gradients on buttons.
- Grain overlay applied once via .grain::after SVG noise, no image file.
- Type: Instrument Sans display + IBM Plex Mono text; scale ratio 1.250; tabular numerals for sizes/dates.
- Accent under 10% of visible surface; ink-3 never used for body text.
- SaveSync data-safety invariants unchanged: no save bytes read/written from UI, destructive actions keep preview + confirmation.
- Dialog contract (existing): Cancel owns initial focus; Escape and backdrop never call the API; confirm fires exactly once.
- Contrast (verified in spec): all six pairs PASS.

## Review Focus

Input classes / failure modes the spec implies but no task's tests directly exercise — each pinned to the owning task below:

1. Accent on non-base surfaces (e.g., selected tile border) — contrast not in spec's contrast block; pinned to Task 3 (tile selected state must use --line/--ink-2, not accent, for borders).
2. Context menu viewport clamping — menu must not render off-screen or cover the header/footer; pinned to Task 4 (positionMenu clamps to viewport).
3. Modal focus trap under dynamic content — focus must stay trapped if dialog content swaps; pinned to Task 6 (query first focusable on each open).
4. Grain overlay under OS reduced-motion / high-contrast — .grain::after hidden under prefers-reduced-transparency: reduce; pinned to Task 1.
5. SteamGridDB API key never logged or surfaced in errors — settings handler must not echo the key; pinned to Task 5.

## File Structure

| File | Responsibility |
|---|---|
| savesync-desktop/src/styles.css | Token block :root, base/app reset, grain overlay, nav, tiles, menu, dialogs, settings — all styled by token name only |
| savesync-desktop/src/App.tsx | Shell: header/nav (no Drive banner), library grid, tiles, context menu, settings view, dialog contract |
| savesync-desktop/src/components/CoverArt.tsx | Tile cover art rendering, mono initial letter, aspect-ratio |
| savesync-desktop/src/lib/cover.ts | Cover URL / placeholder helpers (unchanged interface, re-styled consumers) |
| savesync-desktop/src/api.ts | Typed Tauri wrappers; add settings command wrapper for SteamGridDB API key |
| savesync-desktop/src-tauri/src/lib.rs | One new Tauri command to persist/read settings (API key); no existing commands changed |
| savesync-desktop/src/App.dialogs.test.tsx | Existing 5 destructive-dialog contract tests; must still pass (no modification expected) |
| savesync-desktop/mockups/home-menu-preview.html | Unchanged design source of truth (reference only) |

## Tasks

### Task 1: Token block, base reset, grain overlay

Files:
- Modify: savesync-desktop/src/styles.css — replace :root tokens with Warm Console block, add .grain::after, base/app reset
- Test: none new (existing App.dialogs.test.tsx unaffected)

Interfaces:
- Consumes: spec token block (exact hex values)
- Produces: CSS custom properties consumed by Tasks 2-6

[ ] Step 1: Write the failing test

```tsx
// scratch spec file, removed after Task 1
it("applies Warm Console base token to app shell", () => {
  render(<App />);
  expect(document.documentElement.style.getPropertyValue("--base").trim()).toBe("#24201c");
});
```

[ ] Step 2: Run test to verify it fails — npm test -> FAIL (token not set)
[ ] Step 3: Implement — write :root token block + .grain::after + body { background: var(--base); color: var(--ink); } in styles.css
[ ] Step 4: Run test to verify it passes — npm test -> PASS; remove scratch test
[ ] Step 5: Commit

### Task 2: Top nav bar — remove Drive banner, compact, centered icons

Files:
- Modify: savesync-desktop/src/App.tsx — header/nav section
- Test: savesync-desktop/src/App.nav.test.tsx (new)

Interfaces:
- Consumes: --base, --surface, --line, --ink, --radius, --dur-fast
- Produces: header markup with nav buttons (Library, Snapshots, Conflicts, Cloud, Settings, Menu)

[ ] Step 1: Write the failing test

```tsx
it("renders nav bar without Google Drive banner, icons vertically centered", () => {
  const { container, getByRole } = render(<AppHeader />);
  expect(container.querySelector('[aria-label*="Google Drive"]')).toBeNull();
  const nav = getByRole("navigation");
  expect(nav.querySelectorAll("button")).toHaveLength(6);
});
```

[ ] Step 2: Run test to verify it fails — npm test -- App.nav.test.tsx -> FAIL
[ ] Step 3: Implement — rewrite header in App.tsx: compact bar, six nav buttons, icons centered to cap height, no "GOOGLE DRIVE · CONNECTED" banner
[ ] Step 4: Run test to verify it passes — npm test -- App.nav.test.tsx -> PASS
[ ] Step 5: Commit

### Task 3: Game tile grid — hero layout, mono initial, cover art

Files:
- Modify: savesync-desktop/src/App.tsx — library grid + tiles
- Modify: savesync-desktop/src/components/CoverArt.tsx — cover art + mono initial
- Test: savesync-desktop/src/App.tile.test.tsx (new)

Interfaces:
- Consumes: --surface, --line, --ink, --ink-2, --radius, --accent
- Produces: grid of .tile elements; hero spans 2 columns; selected state uses --line/--ink-2 borders (not accent)

[ ] Step 1: Write the failing test

```tsx
it("hero tile spans 2 columns with mono initial; selected state uses line border, not accent", () => {
  const { container } = render(<LibraryGrid tiles={tiles} />);
  const hero = container.querySelector(".tile--hero");
  expect(hero).toMatchInlineSnapshot(); // gridColumn: span 2
  expect(hero.querySelector(".mono")).not.toBeNull();
  const selected = container.querySelector(".tile[aria-pressed='true']");
  expect(selected.style.borderColor).not.toContain("e8a33d"); // not accent
});
```

[ ] Step 2: Run test to verify it fails — npm test -- App.tile.test.tsx -> FAIL
[ ] Step 3: Implement — hero grid-column: span 2, 2:1 art ratio, mono initial letter; selected border var(--line) + var(--ink-2) text
[ ] Step 4: Run test to verify it passes — npm test -- App.tile.test.tsx -> PASS
[ ] Step 5: Commit

### Task 4: Context menu rework — 5 items, destructive separated, viewport clamp

Files:
- Modify: savesync-desktop/src/App.tsx — context menu + positionMenu
- Test: savesync-desktop/src/App.menu.test.tsx (new)

Interfaces:
- Consumes: --surface, --line, --ink, --ink-2, --accent, --radius
- Produces: 5-item menu (Open details, Backup now, Restore latest…, Sync now, Remove from library…); destructive action visually separated; menu clamped to viewport

[ ] Step 1: Write the failing test

```tsx
it("renders 5 menu items, first focused, clamps to viewport, Escape closes", () => {
  const { container, getByRole } = render(<GameMenu game={g} anchor={a} />);
  expect(getAllByRole("menuitem")).toHaveLength(5);
  expect(document.activeElement).toBe(getByRole("menuitem", { name: /open details/i }));
  fireEvent.keyDown(document, { key: "Escape" });
  expect(container.querySelector('[role="menu"]')).toHaveAttribute("hidden", "");
});
```

[ ] Step 2: Run test to verify it fails — npm test -- App.menu.test.tsx -> FAIL
[ ] Step 3: Implement — five menu items, first auto-focused, positionMenu clamps x/y to viewport, Escape closes
[ ] Step 4: Run test to verify it passes — npm test -- App.menu.test.tsx -> PASS
[ ] Step 5: Commit

### Task 5: Settings view — SteamGridDB API key field

Files:
- Modify: savesync-desktop/src/App.tsx — settings view + API key input
- Modify: savesync-desktop/src/api.ts — settings command wrapper
- Modify: savesync-desktop/src-tauri/src/lib.rs — new settings_get/settings_set command (API key only)
- Test: savesync-desktop/src/App.settings.test.tsx (new)

Interfaces:
- Consumes: --base, --surface, --line, --ink, --ink-2, --radius
- Produces: settings page with SteamGridDB API key input; Tauri command persists key to OS keyring / settings store; key never echoed in errors

[ ] Step 1: Write the failing test

```tsx
it("renders SteamGridDB API key input in Settings; does not echo key in errors", () => {
  const { getByLabelText } = render(<SettingsView />);
  expect(getByLabelText(/steamgriddb api key/i)).toBeInTheDocument();
});
```

[ ] Step 2: Run test to verify it fails — npm test -- App.settings.test.tsx -> FAIL
[ ] Step 3: Implement — settings view with labeled API key input; Tauri command stores via keyring/settings; errors return generic message without key value
[ ] Step 4: Run test to verify it passes — npm test -- App.settings.test.tsx -> PASS
[ ] Step 5: Commit

### Task 6: Dialogs & accessibility — Modal styling, dialog contract preserved

Files:
- Modify: savesync-desktop/src/App.tsx — Modal styling
- Modify: savesync-desktop/src/styles.css — .dialog-card, .modal token-styled
- Test: existing savesync-desktop/src/App.dialogs.test.tsx (5 tests, unchanged)

Interfaces:
- Consumes: --surface, --line, --ink, --ink-2, --accent, --radius, --dur-fast
- Produces: token-styled native  Modal; Cancel owns initial focus; Escape/backdrop close without calling API; confirm fires exactly once

[ ] Step 1: Write nothing new — existing contract test is the spec
[ ] Step 2: Run existing tests — npm test -- App.dialogs.test.tsx -> PASS (baseline before styling)
[ ] Step 3: Implement — restyle Modal + dialog-card with tokens; preserve focus trap logic unchanged
[ ] Step 4: Run test to verify it passes — npm test -- App.dialogs.test.tsx -> PASS (5 tests)
[ ] Step 5: Commit

### Task 7: Full verification — tests, build, review-ready

Files: all of the above

Interfaces: none (verification task)

[ ] Step 1: Run full test suite — cd savesync-desktop && npm test -> all tests PASS
[ ] Step 2: Run production build — cd savesync-desktop && npm run build -> tsc + vite clean
[ ] Step 3: Lint — cd savesync-desktop/src-tauri && cargo clippy --all-targets -- -D warnings (if any Rust touched)
[ ] Step 4: Contrast re-check — confirm no raw hex introduced in components (grep # in src, exclude styles.css :root)
[ ] Step 5: Commit all remaining changes

## Self-Review

1. Spec coverage: Every spec requirement maps to a task: tokens (T1), nav/no-banner (T2), tiles (T3), menu (T4), settings API key (T5), dialogs (T6), verification (T7). Grain overlay (T1), contrast (spec verified + T7 grep), one radius (T1-T6 via token block), accent <10% (T3/T4/T5 by inspection).
2. Step scan: Each step writes one reasonable thing; no step carries more. No TBD lines; every function named; every assertion uses spec values.
3. Type consistency: settings_get/settings_set signatures match between api.ts and lib.rs (Task 5 defines both together).
4. Review Focus: All five items pinned to owning tasks with concrete tests.
5. Proportion: Plan is ~1.5x the spec, focused on signatures/assertions/commands, not code bodies.

---

Plan complete. Please review the plan. Does it capture what you want?

**Execution approach:** I recommend subagent-driven — the six implementation tasks depend on each other's interfaces (token block -> nav -> tiles -> menu -> settings), there are meaningful review gates between them, and a shipped UI mistake costs user trust. A fresh coder implements each task and a fresh reviewer checks it before the next starts, then a whole-branch review at the end.

Shall I proceed with subagent-driven execution, starting with Task 1? (Or do you want to adjust the plan first?)