import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  fixtures,
  installTauriStub,
  seedCover,
  seedFeatured,
  seedWideCover,
  uninstallTauriStub,
  type InvokeCall,
} from "./test/tauri-stub";

let calls: InvokeCall[];

interface CssRule {
  selector: string;
  declarations: string[];
  /** Enclosing `@media` prelude, or null when the rule is top level. */
  media: string | null;
}

/**
 * Rules of the shipped stylesheet.
 *
 * Grid placement and computed colours cannot be observed from the DOM here:
 * Vitest stubs every CSS module to an empty string (including `?raw`, since it
 * classifies `styles.css?raw` as a CSS request) and jsdom has no layout engine.
 * Reading the real rule is therefore the honest check available at this layer,
 * and it is only honest if it reads the *right* rule: the tests below pair each
 * declaration with the DOM structure it applies to. Visual behaviour — the
 * actual hero geometry — is verified in Chromium.
 *
 * The node built-ins are imported through runtime specifiers because the
 * project intentionally has no `@types/node`; a static `import "node:fs"` would
 * fail `tsc --noEmit`. For the same reason the working directory is reached
 * through `globalThis` rather than the bare `process` global.
 *
 * The stylesheet is resolved from the process working directory, not from
 * `import.meta.url`: under Vitest's jsdom environment that URL has the `http:`
 * scheme, so `node:fs` rejects it with "The URL must be of scheme file". The
 * candidate list keeps the read working whether the runner starts in this
 * package or in the repository root.
 */
async function readStylesheet(): Promise<string> {
  const fsSpecifier = "node:fs";
  const pathSpecifier = "node:path";
  const nodeFs = (await import(/* @vite-ignore */ fsSpecifier)) as {
    existsSync: (candidate: string) => boolean;
    readFileSync: (candidate: string, encoding: string) => string;
  };
  const nodePath = (await import(/* @vite-ignore */ pathSpecifier)) as {
    resolve: (...segments: string[]) => string;
  };

  const cwd =
    (globalThis as { process?: { cwd?: () => string } }).process?.cwd?.() ?? "";
  const candidates = ["src/styles.css", "savesync-desktop/src/styles.css"].map(
    (relative) => nodePath.resolve(cwd, relative),
  );

  const resolved = candidates.find((candidate) => nodeFs.existsSync(candidate));
  if (resolved === undefined) {
    // Fail loudly. A silently empty read would turn every CSS assertion below
    // into a no-op and let the suite pass without checking anything.
    throw new Error(
      `could not locate the shipped stylesheet; tried:\n${candidates.join("\n")}`,
    );
  }

  return nodeFs.readFileSync(resolved, "utf8");
}

function normalise(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

/**
 * Rules of the shipped stylesheet, tagged with their `@media` context.
 *
 * A brace walk rather than one flat regex over the whole file. The flat scan
 * could not tell a base rule from a same-selector override inside `@media`, so
 * the `max-width: 560px` collapse could stand in for — and mask — the base
 * declaration it is meant to relax. Comments are stripped first so a `{` inside
 * one cannot open a block.
 */
function parseRules(source: string): CssRule[] {
  const css = source.replace(/\/\*[\s\S]*?\*\//g, "");
  const rules: CssRule[] = [];
  const atRules: string[] = [];
  let openRule: CssRule | null = null;
  let buffer = "";

  for (const char of css) {
    if (char === "{") {
      const prelude = normalise(buffer);
      buffer = "";
      if (prelude.startsWith("@")) {
        atRules.push(prelude);
      } else {
        const rule: CssRule = {
          selector: prelude,
          declarations: [],
          media: atRules.length > 0 ? atRules.join(" and ") : null,
        };
        rules.push(rule);
        openRule = rule;
      }
      continue;
    }

    if (char === "}") {
      if (openRule) {
        openRule.declarations = buffer
          .split(";")
          .map(normalise)
          .filter((declaration) => declaration.length > 0);
        openRule = null;
      } else {
        atRules.pop();
      }
      buffer = "";
      continue;
    }

    buffer += char;
  }

  return rules;
}

let CSS_RULES: CssRule[] = [];

beforeAll(async () => {
  const source = await readStylesheet();
  // Vitest answers every CSS request with "", so the read must be proven real
  // before any rule lookup can be believed. These two assertions are the
  // difference between a real check and a vacuous pass.
  expect(source.length).toBeGreaterThan(0);
  expect(source).toContain(".tile-grid");
  CSS_RULES = parseRules(source);
  expect(CSS_RULES.length).toBeGreaterThan(0);
  // The `@media` scan must also produce rules, or the parser has silently
  // mis-nested every block after the first at-rule.
  expect(CSS_RULES.some((rule) => rule.media !== null)).toBe(true);
});

beforeEach(() => {
  calls = installTauriStub();
});

/** Only the calls for one command, so counts stay easy to reason about. */
function callsFor(cmd: string): InvokeCall[] {
  return calls.filter((call) => call.cmd === cmd);
}

/** Opens the context menu on a tile and clicks one item by its accessible name. */
async function chooseMenuItem(
  user: ReturnType<typeof userEvent.setup>,
  tile: HTMLElement,
  name: string,
): Promise<void> {
  fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });
  const menu = await screen.findByRole("menu", { name: "Game actions" });
  await user.click(within(menu).getByRole("menuitem", { name }));
}

afterEach(() => {
  uninstallTauriStub();
});

/**
 * Declarations from top-level rules only.
 *
 * `@media` overrides are deliberately excluded: a rule written for narrow or
 * reduced-motion conditions must never be able to satisfy a base-layout
 * assertion, nor mask the absence of the base declaration.
 *
 * Selector groups match any of their members. Whether `svg` and `img` share a
 * rule or get one each is a stylesheet style choice, so the lookup accepts both
 * forms; it still requires the requested selector to appear verbatim in the
 * group, and every declaration of that rule still has to be present.
 */
function declarationsFor(selector: string): string[] {
  return declarationsWhere(
    (rule) =>
      rule.media === null &&
      rule.selector.split(",").some((part) => part.trim() === selector),
  );
}

/** Declarations from rules that match `match`. */
function declarationsWhere(match: (rule: CssRule) => boolean): string[] {
  return CSS_RULES.filter(match).flatMap((rule) => rule.declarations);
}

function tileDeclarations(): string {
  return CSS_RULES.filter((rule) => rule.selector.includes(".tile"))
    .flatMap((rule) => rule.declarations)
    .join("; ");
}

/** Raw value of a custom property declared in `:root`. */
function tokenValue(name: string): string {
  const root = CSS_RULES.find((rule) => rule.selector === ":root");
  const declaration = root?.declarations.find((entry) =>
    entry.startsWith(`${name}: `),
  );
  if (!declaration) throw new Error(`token ${name} is not declared in :root`);
  return declaration.slice(declaration.indexOf(": ") + 2);
}

/** Resolves `var(--token)` the way the browser would, one level deep. */
function resolveTokens(value: string): string {
  return value.replace(/var\((--[a-z0-9-]+)\)/gi, (_match, name: string) =>
    tokenValue(name),
  );
}

function tileGrid(): HTMLElement {
  const grid = document.querySelector(".tile-grid");
  if (!(grid instanceof HTMLElement)) throw new Error("expected a .tile-grid");
  return grid;
}

function tiles(): HTMLElement[] {
  return Array.from(tileGrid().querySelectorAll(":scope > li > .tile"));
}

describe("library tile grid", () => {
  it("renders one equal channel per enrolled game", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const grid = tileGrid();
    const rendered = tiles();

    expect(rendered).toHaveLength(fixtures.games.length);
    expect(within(rendered[0]).getByText("Hades")).toBeInTheDocument();

    /*
     * At most one promoted banner, and only when the user pins it: no fixture
     * is featured here, so nothing spans and every cell stays one track. The
     * promotion rule is manual (context menu / detail view), never an automatic
     * "first game" hero.
     */
    expect(grid.querySelectorAll(".tile-cell")).toHaveLength(
      fixtures.games.length,
    );
    expect(grid.querySelectorAll(".tile-cell--featured")).toHaveLength(0);
    expect(grid.querySelectorAll(".tile--featured")).toHaveLength(0);
    // Every rendered tile lives in its own cell, and no cell holds two tiles.
    expect(rendered.map((tile) => tile.parentElement)).toEqual(
      Array.from(grid.children),
    );
    /*
     * Track spanning exists for exactly one purpose — the featured banner — and
     * only as those two rules: a full row while the grid is narrow, three
     * tracks from 760px so the wide capsule does not stretch to a poster.
     */
    const spanningRules = CSS_RULES.filter(
      (rule) =>
        rule.selector.includes(".tile") &&
        rule.declarations.some((declaration) =>
          declaration.startsWith("grid-column"),
        ),
    );
    expect(spanningRules.map((rule) => rule.selector)).toEqual([
      ".tile-cell--featured",
      ".tile-cell--featured",
    ]);
    expect(spanningRules.map((rule) => rule.media)).toEqual([
      null,
      "@media (min-width: 760px)",
    ]);
    expect(declarationsFor(".tile-cell--featured")).toContain(
      "grid-column: 1 / -1",
    );

    // Uniform 2:3 art, and the art box is the clip the art fills.
    expect(declarationsFor(".tile-art")).toContain("aspect-ratio: 2 / 3");
    expect(declarationsFor(".tile-art .cover-art")).toContain(
      "position: absolute",
    );
    expect(declarationsFor(".tile-art .cover-art")).toContain("inset: 0");
    expect(declarationsFor(".tile-art .cover-art svg")).toContain("width: 100%");
    expect(declarationsFor(".tile-art .cover-art svg")).toContain("height: 100%");
  });

  it("gives every game generated cover art instead of a bare initial", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    // The fixtures model a library before real cover art exists; the grid must
    // still read as a game library, so art is generated rather than skipped.
    expect(fixtures.games.every((game) => game.cover_path === null)).toBe(true);
    const covers = Array.from(tileGrid().querySelectorAll(".cover-art"));
    expect(covers).toHaveLength(fixtures.games.length);

    // Hades, Hollow Knight, Celeste, Stardew Valley — in grid order.
    expect(covers.map((cover) => cover.querySelector("text")?.textContent)).toEqual([
      "H",
      "HK",
      "C",
      "SV",
    ]);

    // The art is decoration; the tile button carries the accessible name.
    for (const cover of covers) {
      expect(cover).toHaveAttribute("aria-hidden", "true");
    }
    expect(tileGrid().querySelectorAll(".tile-mono")).toHaveLength(0);

    // No tile rule may reach for a raw colour literal: generated art is the
    // only place that carries its own palette, and it is drawn by SVG fills.
    expect(tileDeclarations()).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(tileDeclarations()).not.toMatch(/\b(rgb|rgba|hsl|hsla)\(/);
  });

  it("rings the selected tile with the gradient, never the accent", () => {
    const selected = declarationsFor('.tile[aria-current="true"]');

    expect(selected).toContain("border-color: var(--line-strong)");
    // The keyboard ring moved to its own rule; selection is a gradient ring on
    // a masked pseudo-element, so the two states never read as the same thing.
    expect(declarationsFor(".tile:focus-visible")).toContain(
      "outline: 2px solid var(--focus-ring)",
    );

    const ring = declarationsFor('.tile[aria-current="true"]::after');
    expect(ring).toContain("background: var(--grad-warm)");
    expect(ring).toContain("pointer-events: none");

    // The glow is the library hue, not the accent fill.
    expect(selected.join("; ")).toContain("var(--hue-library-1)");
    // The browser resolves the token; it must land on a neutral, not the blue.
    expect(resolveTokens("border-color: var(--line-strong)")).toBe(
      `border-color: ${tokenValue("--line-strong")}`,
    );
    expect(tokenValue("--line-strong")).not.toBe(tokenValue("--accent"));
    expect(selected.join("; ")).not.toMatch(/accent/i);

    const check = declarationsFor('.tile[aria-current="true"] .tile-check');
    expect(check).toContain("border-color: var(--line-strong)");
    expect(check).toContain("color: var(--ink)");
    expect(check.join("; ")).not.toMatch(/accent/i);

    // No tile rule anywhere may reach for the accent fill.
    expect(tileDeclarations()).not.toMatch(/var\(--accent/);
  });

  it("keeps every label directly under its own art", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    // A bottom-anchored label inside a stretched cell is what once left ~108px
    // between art and title. Cells top-align and tiles are top-anchored columns.
    expect(tileDeclarations()).not.toMatch(/margin-top:\s*auto/);
    expect(declarationsFor(".tile-grid")).toContain("align-items: start");
    expect(declarationsFor(".tile")).toContain("justify-content: flex-start");
    expect(declarationsFor(".tile-label")).toContain(
      "padding: var(--space-2xs) var(--space-xs) var(--space-xs)",
    );

    // One label rule for every tile: art first, label second, in that order.
    const tile = tiles()[0];
    expect(tile.children[0]).toHaveClass("tile-art");
    expect(tile.children[1]).toHaveClass("tile-label");
  });

  it("keeps tile text at or above the readability floor", () => {
    expect(Number.parseFloat(tokenValue("--text-sm"))).toBeGreaterThanOrEqual(15);
    expect(Number.parseFloat(tokenValue("--text-2xs"))).toBeGreaterThanOrEqual(14);

    expect(declarationsFor(".tile-title")).toContain("font-size: var(--text-md)");
    expect(declarationsFor(".tile-acc")).toContain("font-size: var(--text-2xs)");
    // The title is content, so it takes the display face; the meta line stays
    // in the UI face and never drops to mono or below 14px.
    expect(declarationsFor(".tile-title")).toContain(
      "font-family: var(--font-display)",
    );
    expect(declarationsFor(".tile-acc")).not.toContain(
      "font-family: var(--font-mono)",
    );
  });

  it("keeps the game title in the tile button's accessible name", async () => {
    const user = userEvent.setup();
    render(<App />);

    const tile = await screen.findByRole("button", { name: /Hades/ });
    expect(tile).toHaveClass("tile");
    expect(tile).toHaveAccessibleName(/Hades/);

    // Selecting a game still reaches the inline history rows that
    // App.dialogs.test.tsx drives its destructive-restore contract through.
    await user.click(tile);
    expect(
      await screen.findAllByRole("button", { name: "Exact restore" }),
    ).toHaveLength(2);
  });
});

/** Tile titles in grid order; re-queried because a pin re-renders the list. */
function tileTitles(): (string | null)[] {
  return Array.from(tileGrid().querySelectorAll(".tile-title")).map(
    (node) => node.textContent,
  );
}

/**
 * The featured banner.
 *
 * The promotion rule is manual and singular: the user pins one game from the
 * context menu or the detail view, it leads the wall with 920x430 art, and
 * every other tile keeps the untouched 2:3 channel. These tests replace the
 * superseded "no promoted hero" assertions in the grid suite above; they do not
 * relax the regular-tile invariants, which still run there.
 */
describe("featured tile", () => {
  it("leads the wall with the pinned game and keeps the rest in order", async () => {
    seedFeatured(fixtures.secondGameId);
    render(<App />);
    await screen.findByRole("button", { name: /Hollow Knight/ });

    // Pinned game first, then the untouched library order.
    expect(tileTitles()).toEqual([
      "Hollow Knight",
      "Hades",
      "Celeste",
      "Stardew Valley",
    ]);

    const rendered = tiles();
    expect(rendered).toHaveLength(fixtures.games.length);
    // Exactly one promoted cell, and it holds the first tile.
    const featuredCells = tileGrid().querySelectorAll(".tile-cell--featured");
    expect(featuredCells).toHaveLength(1);
    expect(featuredCells[0]).toContainElement(rendered[0]);
    expect(rendered[0]).toHaveClass("tile--featured");
    expect(rendered[0]).toHaveAccessibleName(/Hollow Knight/);
    // Only the banner is promoted; the rest keep the plain tile class.
    for (const tile of rendered.slice(1)) {
      expect(tile).not.toHaveClass("tile--featured");
    }
  });

  it("gives the banner a wide art slot and leaves every other tile 2:3", () => {
    // The wide aspect is scoped to the featured tile alone, so the portrait
    // default below cannot be relaxed for the rest of the wall.
    expect(declarationsFor(".tile-art")).toContain("aspect-ratio: 2 / 3");
    expect(declarationsFor(".tile--featured .tile-art")).toContain(
      "aspect-ratio: 920 / 430",
    );
  });

  it("resolves the cached wide cover for the banner only", async () => {
    seedFeatured(fixtures.primaryGameId);
    seedWideCover(fixtures.primaryGameId);
    seedCover(fixtures.secondGameId);
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    await waitFor(() => {
      expect(tiles()[0].querySelector("img")).not.toBeNull();
    });
    const image = tiles()[0].querySelector("img");
    if (!(image instanceof HTMLImageElement)) {
      throw new Error("expected the banner to render cached wide art");
    }
    const src = image.getAttribute("src") ?? "";
    expect(src).toContain(`/${fixtures.primaryGameId}?kind=wide`);
    // The cache-bust appends with `&`: the scheme handler parses one `?` only.
    expect(src).toContain("?kind=wide&v=");

    // The bare portrait URL is never used for the banner, so no cross-shape
    // fallback can stretch 2:3 art across the wide slot.
    expect(src).not.toContain(`/${fixtures.primaryGameId}?v=`);

    // Wide lookups are limited to the pinned game with a cached wide cover.
    const wideLookups = callsFor("cover_url").filter(
      (call) => call.args.kind === "wide",
    );
    expect(wideLookups.map((call) => call.args.game_id)).toEqual([
      fixtures.primaryGameId,
    ]);

    // A regular tile's cached portrait art is untouched: still no `kind`.
    const portrait = callsFor("cover_url").filter(
      (call) => call.args.game_id === fixtures.secondGameId,
    );
    expect(portrait).toHaveLength(1);
    expect(portrait[0].args).not.toHaveProperty("kind");
  });

  it("generates 920x430 art when no wide cover is cached", async () => {
    // A portrait cover alone must not fill the banner: the two cache shapes are
    // independent, so the featured slot falls straight through to generated art.
    seedFeatured(fixtures.primaryGameId);
    seedCover(fixtures.primaryGameId);
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const banner = tiles()[0];
    expect(banner).toHaveClass("tile--featured");
    expect(banner.querySelector("img")).toBeNull();
    expect(
      banner.querySelector(".cover-art svg")?.getAttribute("viewBox"),
    ).toBe("0 0 920 430");
    // Same palette/monogram selection as the portrait cover, at banner scale.
    expect(banner.querySelector(".cover-art text")?.textContent).toBe("H");

    // The rest of the wall keeps the original 600x900 portrait artwork.
    for (const tile of tiles().slice(1)) {
      expect(
        tile.querySelector(".cover-art svg")?.getAttribute("viewBox"),
      ).toBe("0 0 600 900");
    }
  });

  it("pins and unpins the banner through the wired actions", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    await chooseMenuItem(user, screen.getByRole("button", { name: /Hades/ }), "Feature on library");

    await waitFor(() => {
      expect(tiles()[0]).toHaveClass("tile--featured");
    });
    expect(tileTitles()[0]).toBe("Hades");
    expect(callsFor("set_featured").map((call) => call.args.game_id)).toEqual([
      fixtures.primaryGameId,
    ]);
    expect(tileGrid().querySelectorAll(".tile-cell--featured")).toHaveLength(1);

    // The banner's own menu offers the inverse action, and unpinning restores
    // the uniform 2:3 grid with nothing spanning.
    await chooseMenuItem(user, tiles()[0], "Remove from featured");

    await waitFor(() => {
      expect(tileGrid().querySelectorAll(".tile-cell--featured")).toHaveLength(0);
    });
    expect(callsFor("clear_featured")).toHaveLength(1);
    expect(tileTitles()).toEqual([
      "Hades",
      "Hollow Knight",
      "Celeste",
      "Stardew Valley",
    ]);
    for (const tile of tiles()) {
      expect(tile).not.toHaveClass("tile--featured");
      expect(
        tile.querySelector(".cover-art svg")?.getAttribute("viewBox"),
      ).toBe("0 0 600 900");
    }
  });
});

