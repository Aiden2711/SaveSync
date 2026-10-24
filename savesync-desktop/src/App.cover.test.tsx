import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  failCoverFetch,
  fixtures,
  installTauriStub,
  seedArtworkStatus,
  seedCover,
  uninstallTauriStub,
  type InvokeCall,
} from "./test/tauri-stub";

type User = ReturnType<typeof userEvent.setup>;

let calls: InvokeCall[];

beforeEach(() => {
  calls = installTauriStub();
});

afterEach(() => {
  uninstallTauriStub();
});

/** Only the calls for one command, so counts stay easy to reason about. */
function callsFor(cmd: string): InvokeCall[] {
  return calls.filter((call) => call.cmd === cmd);
}

/** Renders the app and waits for the library, returning the Hades tile. */
async function renderLibrary(): Promise<HTMLButtonElement> {
  render(<App />);
  const tile = await screen.findByRole("button", { name: /Hades/ });
  if (!(tile instanceof HTMLButtonElement)) {
    throw new Error("expected the game tile to be a button");
  }
  return tile;
}

/** The `aria-hidden` cover box inside a tile. */
function coverBox(tile: HTMLElement): HTMLElement {
  const box = tile.querySelector<HTMLElement>(".cover-art");
  if (!box) {
    throw new Error("expected the tile to render a cover art box");
  }
  return box;
}

/** Current tile element; re-queried because a cover swap re-renders children. */
function hadesTile(): HTMLButtonElement {
  const tile = screen.getByRole("button", { name: /Hades/ });
  if (!(tile instanceof HTMLButtonElement)) {
    throw new Error("expected the game tile to be a button");
  }
  return tile;
}

/** Opens the context menu on a tile and clicks one item by its accessible name. */
async function chooseMenuItem(user: User, tile: HTMLElement, name: string): Promise<void> {
  fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });
  const menu = await screen.findByRole("menu", { name: "Game actions" });
  await user.click(within(menu).getByRole("menuitem", { name }));
}

describe("cover art", () => {
  it("draws the generated monogram and asks for no URL when no cover is cached", async () => {
    const tile = await renderLibrary();

    const box = coverBox(tile);
    expect(box.querySelector("img")).toBeNull();
    expect(box.querySelector("svg")).not.toBeNull();
    // Non-vacuous: the library has rendered, so the cover lookup already ran.
    expect(callsFor("list_games")).toHaveLength(1);
    expect(callsFor("cover_url")).toHaveLength(0);
  });

  it("renders the cached cover image for a game whose cover_path is set", async () => {
    seedCover(fixtures.primaryGameId);

    await renderLibrary();

    await waitFor(() => {
      expect(coverBox(hadesTile()).querySelector("img")).not.toBeNull();
    });
    const image = coverBox(hadesTile()).querySelector("img");
    if (!(image instanceof HTMLImageElement)) {
      throw new Error("expected an <img> in the cover box");
    }
    expect(image.getAttribute("src")).toContain(fixtures.primaryGameId);
    expect(image).toHaveAttribute("aria-hidden", "true");

    const lookups = callsFor("cover_url");
    expect(lookups).toHaveLength(1);
    expect(lookups[0].args).toEqual({ game_id: fixtures.primaryGameId });
  });

  it("falls back to the monogram when the cached image fails to load", async () => {
    seedCover(fixtures.primaryGameId);

    await renderLibrary();
    await waitFor(() => {
      expect(coverBox(hadesTile()).querySelector("img")).not.toBeNull();
    });

    const image = coverBox(hadesTile()).querySelector("img");
    if (!(image instanceof HTMLImageElement)) {
      throw new Error("expected an <img> in the cover box");
    }
    fireEvent.error(image);

    await waitFor(() => {
      expect(coverBox(hadesTile()).querySelector("img")).toBeNull();
    });
    const box = coverBox(hadesTile());
    // The box is never left empty: the generated artwork takes over.
    expect(box.querySelector("svg")).not.toBeNull();
    expect(box.querySelector("text")?.textContent ?? "").not.toBe("");
  });

  it("surfaces the backend configuration error when no API key is stored", async () => {
    seedArtworkStatus(false);
    const user = userEvent.setup();
    const tile = await renderLibrary();

    await chooseMenuItem(user, tile, "Fetch cover art");

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain(
      "Configure the SteamGridDB API key in Settings first.",
    );

    const fetches = callsFor("fetch_cover_art");
    expect(fetches).toHaveLength(1);
    expect(fetches[0].args).toEqual({ game_id: fixtures.primaryGameId });

    // A failed fetch changes nothing: monogram stays, no URL is requested.
    expect(coverBox(hadesTile()).querySelector("svg")).not.toBeNull();
    expect(coverBox(hadesTile()).querySelector("img")).toBeNull();
    expect(callsFor("cover_url")).toHaveLength(0);
  });

  it("shows the fetched cover and closes the menu", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const tile = await renderLibrary();

    await chooseMenuItem(user, tile, "Fetch cover art");

    expect(screen.queryByRole("menu")).toBeNull();
    expect(callsFor("fetch_cover_art")).toHaveLength(1);

    await waitFor(() => {
      expect(coverBox(hadesTile()).querySelector("img")).not.toBeNull();
    });
    const image = coverBox(hadesTile()).querySelector("img");
    if (!(image instanceof HTMLImageElement)) {
      throw new Error("expected an <img> in the cover box");
    }
    expect(image.getAttribute("src")).toContain(fixtures.primaryGameId);
  });

  it("keeps the monogram when the provider finds no cover", async () => {
    seedArtworkStatus(true);
    failCoverFetch("No cover art found for this game.");
    const user = userEvent.setup();
    const tile = await renderLibrary();

    await chooseMenuItem(user, tile, "Fetch cover art");

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain("No cover art found for this game.");
    expect(coverBox(hadesTile()).querySelector("svg")).not.toBeNull();
    expect(coverBox(hadesTile()).querySelector("img")).toBeNull();
    expect(callsFor("cover_url")).toHaveLength(0);
  });

  it("issues exactly one fetch per click", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const tile = await renderLibrary();

    await chooseMenuItem(user, tile, "Fetch cover art");

    await waitFor(() => {
      expect(coverBox(hadesTile()).querySelector("img")).not.toBeNull();
    });
    const fetches = callsFor("fetch_cover_art");
    expect(fetches).toHaveLength(1);
    expect(fetches.filter((call) => call.args.game_id === fixtures.primaryGameId)).toHaveLength(1);
    // One lookup per successful fetch, and none for the other tiles.
    expect(callsFor("cover_url")).toHaveLength(1);
  });
});
