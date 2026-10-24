import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import { fixtures, installTauriStub, uninstallTauriStub } from "./test/tauri-stub";

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
  installTauriStub();
});

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
     * Every tile is one channel. A promoted "first game" hero tile made the
     * first row ragged and contradicted the console grid this design follows,
     * so no tile may span two tracks.
     */
    expect(grid.querySelectorAll(".tile-cell")).toHaveLength(
      fixtures.games.length,
    );
    expect(grid.querySelectorAll(".tile-cell--hero")).toHaveLength(0);
    expect(grid.querySelectorAll(".tile--hero")).toHaveLength(0);
    // Every rendered tile lives in its own cell, and no cell holds two tiles.
    expect(rendered.map((tile) => tile.parentElement)).toEqual(
      Array.from(grid.children),
    );
    expect(
      CSS_RULES.filter(
        (rule) =>
          rule.media === null &&
          rule.declarations.some((declaration) =>
            declaration.startsWith("grid-column: span"),
          ),
      ),
    ).toEqual([]);

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

  it("uses a neutral outline, never the accent, for the selected tile", () => {
    const selected = declarationsFor('.tile[aria-current="true"]');

    expect(selected).toContain("border-color: var(--line-strong)");
    expect(selected).toContain("outline: 2px solid var(--focus-ring)");
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
