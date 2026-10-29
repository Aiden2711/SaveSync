import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  failCoverSearch,
  fixtures,
  hideCoverCandidates,
  installTauriStub,
  seedArtworkStatus,
  stallNextCoverSearch,
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

/** `dialog[open]` only exists while the dialog is mounted. */
function openDialog(): HTMLDialogElement {
  const element = document.querySelector("dialog[open]");
  if (!(element instanceof HTMLDialogElement)) {
    throw new Error("expected an open <dialog>");
  }
  return element;
}

/**
 * Opens the picker from the context menu and returns the dialog once it has
 * finished loading. Callers that need the loading state open it directly.
 */
async function openPicker(user: User): Promise<HTMLDialogElement> {
  const tile = await renderLibrary();
  fireEvent.contextMenu(tile, { clientX: 120, clientY: 120 });
  const menu = await screen.findByRole("menu", { name: "Game actions" });
  await user.click(within(menu).getByRole("menuitem", { name: "Choose cover art…" }));
  const dialog = await waitFor(() => openDialog());
  await within(dialog).findByRole("button", { name: /Use cover 1/ });
  return dialog;
}

describe("cover art picker", () => {
  it("opens from the menu, names the game, and lists candidates", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();

    const dialog = await openPicker(user);

    expect(within(dialog).getByRole("heading", { name: "Choose cover art" })).toBeInTheDocument();
    expect(within(dialog).getByText("Hades")).toBeInTheDocument();

    const options = within(dialog).getAllByRole("button", { name: /Use cover/ });
    expect(options).toHaveLength(fixtures.coverCandidates.length);
    const firstImage = options[0].querySelector("img");
    if (!(firstImage instanceof HTMLImageElement)) {
      throw new Error("expected each candidate to render an <img>");
    }
    expect(firstImage.getAttribute("src")).toBe(fixtures.coverCandidates[0].url);

    // One search per open, and opening the picker never triggers a download.
    expect(callsFor("search_cover_candidates")).toHaveLength(1);
    expect(callsFor("search_cover_candidates")[0].args).toEqual({
      game_id: fixtures.primaryGameId,
    });
    expect(callsFor("fetch_cover_art")).toHaveLength(0);
    expect(callsFor("set_cover_grid")).toHaveLength(0);
  });

  it("puts initial focus on Cancel", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();

    const dialog = await openPicker(user);

    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBe(document.activeElement);
  });

  it("closes on Escape without picking anything", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const dialog = await openPicker(user);

    // Dispatch the native `cancel` event our Modal listens for.
    act(() => {
      dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
    });

    expect(document.querySelector("dialog")).toBeNull();
    expect(callsFor("set_cover_grid")).toHaveLength(0);
  });

  it("closes on a backdrop click without picking anything", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const dialog = await openPicker(user);

    // Click the <dialog> element itself, not the .dialog-card inside it.
    await user.click(dialog);

    expect(document.querySelector("dialog")).toBeNull();
    expect(callsFor("set_cover_grid")).toHaveLength(0);
  });

  it("renders a loading state until the search resolves", async () => {
    seedArtworkStatus(true);
    const release = stallNextCoverSearch();
    const user = userEvent.setup();
    const tile = await renderLibrary();

    fireEvent.contextMenu(tile, { clientX: 120, clientY: 120 });
    const menu = await screen.findByRole("menu", { name: "Game actions" });
    await user.click(within(menu).getByRole("menuitem", { name: "Choose cover art…" }));

    expect(await screen.findByText("Loading cover art…")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Use cover/ })).toBeNull();

    await act(async () => {
      release();
    });

    await waitFor(() => {
      expect(screen.getAllByRole("button", { name: /Use cover/ })).toHaveLength(
        fixtures.coverCandidates.length,
      );
    });
  });

  it("picks exactly the chosen grid and refreshes the tile's cover", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const dialog = await openPicker(user);
    const chosen = fixtures.coverCandidates[1];

    // A double click must still produce exactly one download.
    await user.dblClick(within(dialog).getByRole("button", { name: /Use cover 2/ }));

    await waitFor(() => expect(callsFor("set_cover_grid")).toHaveLength(1));
    expect(callsFor("set_cover_grid")[0].args).toEqual({
      game_id: fixtures.primaryGameId,
      grid_id: chosen.id,
    });
    expect(callsFor("set_cover_grid")).toHaveLength(1);

    // The modal closes and the game's cover is refreshed from the new record.
    await waitFor(() => expect(document.querySelector("dialog")).toBeNull());
    expect(callsFor("cover_url")).toHaveLength(1);
    expect(callsFor("cover_url")[0].args).toEqual({ game_id: fixtures.primaryGameId });

    await waitFor(() => {
      const image = screen
        .getByRole("button", { name: /Hades/ })
        .querySelector<HTMLImageElement>(".cover-art img");
      expect(image?.getAttribute("src") ?? "").toContain(fixtures.primaryGameId);
    });
  });

  it("keeps keyboard focus on the thumbnail being picked", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const dialog = await openPicker(user);
    const tiles = within(dialog).getAllByRole("button", { name: /Use cover/ });

    // Focus sits on a sibling — the situation a pick created inside the grid
    // must survive — then the first tile is activated. `click` is the DOM
    // action Enter produces on a button, and dispatching it synchronously lets
    // the assertions below run while the pick is still in flight.
    tiles[1].focus();
    fireEvent.click(tiles[0]);

    expect(document.querySelector("dialog")).not.toBeNull();
    expect(tiles[0]).toBe(document.activeElement);
    // The tile in flight stays enabled and busy; only its siblings disable.
    expect(tiles[0]).not.toBeDisabled();
    expect(tiles[0]).toHaveAttribute("aria-busy", "true");
    expect(tiles[1]).toBeDisabled();

    // Escape still closes the dialog while the pick is in flight.
    act(() => {
      openDialog().dispatchEvent(new Event("cancel", { cancelable: true }));
    });
    expect(document.querySelector("dialog")).toBeNull();

    // The pick still completed exactly once and left the library consistent.
    await waitFor(() => expect(callsFor("set_cover_grid")).toHaveLength(1));
    expect(callsFor("set_cover_grid")[0].args).toEqual({
      game_id: fixtures.primaryGameId,
      grid_id: fixtures.coverCandidates[0].id,
    });
  });

  it("marks a thumbnail whose image fails and keeps its siblings pickable", async () => {
    seedArtworkStatus(true);
    const user = userEvent.setup();
    const dialog = await openPicker(user);
    const tiles = within(dialog).getAllByRole("button", { name: /Use cover/ });
    const failed = tiles[0];
    const sibling = tiles[1];
    const image = failed.querySelector("img");
    if (!(image instanceof HTMLImageElement)) {
      throw new Error("expected each candidate to render an <img>");
    }

    // A CDN thumbnail that cannot load must not look like a usable empty box.
    fireEvent.error(image);

    expect(failed).toBeDisabled();
    expect(failed).toHaveAccessibleName(/unavailable/i);
    expect(within(dialog).getByText("Image unavailable")).toBeInTheDocument();
    expect(sibling).not.toBeDisabled();

    // Clicking the failed tile is a no-op: it never reaches the API.
    fireEvent.click(failed);
    expect(callsFor("set_cover_grid")).toHaveLength(0);

    // The sibling is unaffected and still picks its own grid.
    await user.click(sibling);
    await waitFor(() => expect(callsFor("set_cover_grid")).toHaveLength(1));
    expect(callsFor("set_cover_grid")[0].args).toEqual({
      game_id: fixtures.primaryGameId,
      grid_id: fixtures.coverCandidates[1].id,
    });
  });

  it("shows an empty state when SteamGridDB has no candidates", async () => {
    seedArtworkStatus(true);
    hideCoverCandidates();
    const user = userEvent.setup();

    const tile = await renderLibrary();
    fireEvent.contextMenu(tile, { clientX: 120, clientY: 120 });
    const menu = await screen.findByRole("menu", { name: "Game actions" });
    await user.click(within(menu).getByRole("menuitem", { name: "Choose cover art…" }));

    const dialog = await waitFor(() => openDialog());
    expect(await within(dialog).findByText("No cover art found")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: /Use cover/ })).toBeNull();
  });

  it("keeps the dialog open on error and retries the search", async () => {
    seedArtworkStatus(true);
    failCoverSearch("Could not fetch cover art for this game.");
    const user = userEvent.setup();

    const tile = await renderLibrary();
    fireEvent.contextMenu(tile, { clientX: 120, clientY: 120 });
    const menu = await screen.findByRole("menu", { name: "Game actions" });
    await user.click(within(menu).getByRole("menuitem", { name: "Choose cover art…" }));

    const dialog = await waitFor(() => openDialog());
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toBe(
      "Could not load or save cover art. Check your connection and try again.",
    );

    const retry = within(dialog).getByRole("button", { name: "Retry" });
    await user.click(retry);

    await waitFor(() => {
      expect(within(dialog).getAllByRole("button", { name: /Use cover/ })).toHaveLength(
        fixtures.coverCandidates.length,
      );
    });
    expect(callsFor("search_cover_candidates")).toHaveLength(2);
    // The retry never downloads anything by itself.
    expect(callsFor("set_cover_grid")).toHaveLength(0);
  });

  it("translates a missing API key into friendly text", async () => {
    const user = userEvent.setup();

    const tile = await renderLibrary();
    fireEvent.contextMenu(tile, { clientX: 120, clientY: 120 });
    const menu = await screen.findByRole("menu", { name: "Game actions" });
    await user.click(within(menu).getByRole("menuitem", { name: "Choose cover art…" }));

    const dialog = await waitFor(() => openDialog());
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toBe(
      "Add a SteamGridDB API key in Settings to choose cover art.",
    );
    expect(alert.textContent).not.toContain("keyring");
  });
});
