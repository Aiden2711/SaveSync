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
  expect(source).toContain(".tile--hero");
  CSS_RULES = parseRules(source);
  expect(CSS_RULES.length).toBeGreaterThan(0);
  // The `@media` scan must produce rules, or `mediaDeclarationsFor` below would
  // be vacuously unsatisfiable/empty and the collapse check meaningless.
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
 */
function declarationsFor(selector: string): string[] {
  return declarationsWhere(
    (rule) => rule.selector === selector && rule.media === null,
  );
}

/** Declarations for `selector` inside the `@media` block with this prelude. */
function mediaDeclarationsFor(media: string, selector: string): string[] {
  return declarationsWhere(
    (rule) => rule.selector === selector && rule.media === media,
  );
}

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
  it("renders the first enrolled game as the hero and spans two columns", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const grid = tileGrid();
    const rendered = tiles();

    expect(rendered).toHaveLength(fixtures.games.length);
    expect(grid.querySelectorAll(".tile--hero")).toHaveLength(1);

    const hero = rendered[0];
    expect(hero).toHaveClass("tile", "tile--hero");
    expect(within(hero).getByText("Hades")).toBeInTheDocument();

    /*
     * The span has to reach a grid item.
     *
     * `grid-column` does nothing on a non-grid-item, so a stylesheet that says
     * `.tile--hero { grid-column: span 2 }` while `.tile--hero` sits on the
     * button *inside* the cell renders an ordinary one-column tile. Asserting
     * that the text exists passed for exactly that broken layout, so the check
     * is now structural: the rule's class must be on the hero's grid item, that
     * item must be a direct child of `.tile-grid`, and it must be the first one.
     */
    const heroItem = hero.parentElement;
    if (!(heroItem instanceof HTMLElement)) {
      throw new Error("expected the hero tile to sit in a grid cell");
    }
    expect(heroItem.parentElement).toBe(grid);
    expect(grid.firstElementChild).toBe(heroItem);
    expect(heroItem).toHaveClass("tile-cell", "tile-cell--hero");
    expect(grid.querySelectorAll(".tile-cell")).toHaveLength(
      fixtures.games.length,
    );
    expect(grid.querySelectorAll(".tile-cell--hero")).toHaveLength(1);
    // Every rendered tile lives in its own cell, and no cell holds two tiles.
    expect(rendered.map((tile) => tile.parentElement)).toEqual(
      Array.from(grid.children),
    );

    // Exactly one top-level rule grants two tracks, and it targets the class
    // carried by that grid item — never an element nested inside it.
    expect(
      CSS_RULES.filter(
        (rule) =>
          rule.media === null &&
          rule.declarations.includes("grid-column: span 2"),
      ).map((rule) => rule.selector),
    ).toEqual([".tile-cell--hero"]);
    expect(declarationsFor(".tile-cell--hero")).toEqual(["grid-column: span 2"]);
    // `.tile--hero` is the inner button's marker for the art ratio; a span on it
    // is the inert declaration this test now exists to prevent.
    expect(declarationsFor(".tile--hero")).toEqual([]);

    // The narrow-viewport collapse must not be readable as the base rule.
    expect(
      mediaDeclarationsFor("@media (max-width: 560px)", ".tile-cell--hero"),
    ).toEqual(["grid-column: span 1"]);
    expect(declarationsFor(".tile-cell--hero")).not.toContain(
      "grid-column: span 1",
    );

    expect(declarationsFor(".tile-art")).toContain("aspect-ratio: 4 / 5");
    expect(declarationsFor(".tile--hero .tile-art")).toContain(
      "aspect-ratio: 2 / 1",
    );

    /*
     * The hero band is 2:1 while the generated cover is a 3:4 portrait, which
     * letterboxed into a small centred thumbnail on a grey field. The hero lets
     * the SVG take the band's width and keeps the band as the clip, so the art
     * covers it. (The resulting geometry is verified in Chromium; jsdom has no
     * layout engine.)
     */
    expect(declarationsFor(".tile--hero .tile-art .cover-art")).toContain(
      "display: grid",
    );
    expect(declarationsFor(".tile--hero .tile-art .cover-art svg")).toEqual([
      "width: 100%",
      "height: auto",
    ]);
    // The standard 4:5 tiles must keep the default fit, untouched.
    expect(declarationsFor(".tile-art .cover-art svg")).toEqual([]);
  });

  it("falls back to the title's mono initial for games without cover art", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    // The fixtures model a library before real cover art exists.
    expect(fixtures.games.every((game) => game.cover_path === null)).toBe(true);
    expect(tileGrid().querySelectorAll(".cover-art")).toHaveLength(0);

    // Hades, Hollow Knight, Celeste, Stardew Valley — in grid order.
    expect(
      Array.from(tileGrid().querySelectorAll(".tile-mono")).map((node) =>
        node.textContent?.trim(),
      ),
    ).toEqual(["H", "H", "C", "S"]);

    const mono = declarationsFor(".tile-mono");
    expect(mono).toContain("font-family: var(--font-text)");
    expect(mono.join("; ")).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(mono.join("; ")).not.toMatch(/\b(rgb|rgba|hsl|hsla)\(/);
  });

  it("uses --line, never the accent, for the selected tile's border and text", () => {
    const selected = declarationsFor('.tile[aria-current="true"]');

    expect(selected).toContain("border-color: var(--line)");
    // The browser resolves the token; it must land on --line, not the amber.
    expect(resolveTokens("border-color: var(--line)")).toBe(
      `border-color: ${tokenValue("--line")}`,
    );
    expect(tokenValue("--line")).not.toBe(tokenValue("--accent"));
    expect(selected.join("; ")).not.toContain("#e8a33d");
    expect(selected.join("; ")).not.toMatch(/accent/i);

    const check = declarationsFor('.tile[aria-current="true"] .tile-check');
    expect(check).toContain("border-color: var(--line)");
    expect(check).toContain("color: var(--ink-2)");
    expect(check.join("; ")).not.toMatch(/accent/i);

    // No tile rule anywhere may reach for the accent or a raw colour literal.
    expect(tileDeclarations()).not.toMatch(/accent/i);
    expect(tileDeclarations()).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(tileDeclarations()).not.toMatch(/\b(rgb|rgba|hsl|hsla)\(/);
  });

  it("keeps the hero label directly under its art (mockup dead-space fix)", () => {
    // A bottom-anchored label inside a stretched two-column cell is what left
    // ~108px between the mockup's hero art and its title.
    expect(tileDeclarations()).not.toMatch(/margin-top:\s*auto/);
    expect(declarationsFor(".tile-grid")).toContain("align-items: start");
    expect(declarationsFor(".tile")).toContain("justify-content: flex-start");

    // Hero and standard tiles share one label rule, so their art-to-title gap
    // is identical; the hero must not add spacing of its own.
    expect(declarationsFor(".tile--hero .tile-label")).toEqual([]);
    expect(declarationsFor(".tile--hero .tile-title")).toEqual([]);
    expect(declarationsFor(".tile-label")).toContain(
      "padding: var(--space-2xs) var(--space-xs) var(--space-xs)",
    );
  });

  it("keeps tile text at or above the readability floor", () => {
    expect(Number.parseFloat(tokenValue("--text-sm"))).toBeGreaterThanOrEqual(15);
    expect(Number.parseFloat(tokenValue("--text-2xs"))).toBeGreaterThanOrEqual(14);

    expect(declarationsFor(".tile-title")).toContain("font-size: var(--text-sm)");
    expect(declarationsFor(".tile-acc")).toContain("font-size: var(--text-2xs)");
  });

  it("keeps the game title in the tile button's accessible name", async () => {
    const user = userEvent.setup();
    render(<App />);

    const tile = await screen.findByRole("button", { name: /Hades/ });
    expect(tile).toHaveClass("tile", "tile--hero");
    expect(tile).toHaveAccessibleName(/Hades/);

    // Selecting a game still reaches the inline history rows that
    // App.dialogs.test.tsx drives its destructive-restore contract through.
    await user.click(tile);
    expect(
      await screen.findAllByRole("button", { name: "Exact restore" }),
    ).toHaveLength(2);
  });
});
