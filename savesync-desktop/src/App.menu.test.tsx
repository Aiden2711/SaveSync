import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import { installTauriStub, uninstallTauriStub } from "./test/tauri-stub";
import { clampMenuPosition, MENU_SIZE } from "./lib/menu";

/**
 * `dialog[open]` only exists while the dialog is mounted. jsdom doesn't
 * properly implement the dialog role, so we query by the open attribute.
 */
function openDialog(): HTMLDialogElement {
  const element = document.querySelector("dialog[open]");
  if (!(element instanceof HTMLDialogElement)) {
    throw new Error("expected an open <dialog>");
  }
  return element;
}

beforeEach(() => {
  installTauriStub();
});

afterEach(() => {
  uninstallTauriStub();
});

describe("game tile context menu", () => {
  it("opens on contextmenu event on a tile", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    expect(menu).toBeInTheDocument();
  });

  it("opens the menu without navigating away from the library", async () => {
    render(<App />);
    const tile = await screen.findByRole("button", { name: /Hades/ });

    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });
    await screen.findByRole("menu", { name: "Game actions" });

    // The library is still the current view and the tile is not selected:
    // only "Open details" may change the view.
    expect(
      screen.getByRole("heading", { name: "Game library" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Back to library/ })).toBeNull();
    expect(tile).not.toHaveAttribute("aria-current");
  });

  it("opens the menu from the keyboard without selecting the game", async () => {
    render(<App />);
    const tile = await screen.findByRole("button", { name: /Hades/ });

    fireEvent.keyDown(tile, { key: "F10", shiftKey: true });

    expect(
      await screen.findByRole("menu", { name: "Game actions" }),
    ).toBeInTheDocument();
    expect(tile).not.toHaveAttribute("aria-current");
  });

  it("loads the targeted game's history before restoring its latest snapshot", async () => {
    const user = userEvent.setup();
    render(<App />);
    const tile = await screen.findByRole("button", { name: /Hades/ });

    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });
    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const restoreItem = within(menu).getByRole("menuitem", {
      name: "Restore latest…",
    });

    await user.click(restoreItem);

    // The menu may target a game whose history is not loaded, so the revision
    // is fetched on demand rather than read from another game's state.
    const dialog = await waitFor(() => openDialog());
    expect(within(dialog).getByText("Restore snapshot")).toBeInTheDocument();
    expect(
      within(dialog).getByText(/Snapshot from/),
    ).toBeInTheDocument();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("shows exactly 6 menu items with correct labels in order", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const items = Array.from(
      menu.querySelectorAll<HTMLButtonElement>('[role="menuitem"]'),
    );

    expect(items).toHaveLength(6);
    expect(items.map((item) => item.textContent)).toEqual([
      "Open details",
      "Backup now",
      "Restore latest…",
      "Sync now",
      "Fetch cover art",
      "Remove from library…",
    ]);
  });

  it("applies danger styling to the remove item and includes a separator", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const removeItem = within(menu).getByRole("menuitem", {
      name: "Remove from library…",
    });

    // Last item has danger class
    expect(removeItem).toHaveClass("menu-item-danger");

    // Separator exists
    const separator = menu.querySelector(".menu-separator");
    expect(separator).toBeInTheDocument();
  });

  it("disables 'Sync now' when Google Drive is not connected", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const syncItem = within(menu).getByRole("menuitem", { name: "Sync now" });

    expect(syncItem).toBeDisabled();
    expect(syncItem).toHaveAttribute("disabled", "");
    expect(syncItem).toHaveAttribute("title", "Connect Google Drive first");
  });

  it("disables 'Restore latest…' on Hollow Knight which has no active head revision", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    // Open menu on Hollow Knight (second game, no active_head_revision_id)
    const hollowKnightTile = screen.getByRole("button", { name: /Hollow Knight/ });
    fireEvent.contextMenu(hollowKnightTile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const restoreItem = within(menu).getByRole("menuitem", {
      name: "Restore latest…",
    });

    expect(restoreItem).toBeDisabled();
    expect(restoreItem).toHaveAttribute("disabled", "");
    expect(restoreItem).toHaveAttribute("title", "No snapshot exists yet");
  });

  it("closes menu when Escape key is pressed", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    expect(menu).toBeInTheDocument();

    // Press Escape on the menu
    fireEvent.keyDown(menu, { key: "Escape" });

    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes menu when Tab moves focus out of it", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    fireEvent.keyDown(menu, { key: "Tab" });

    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes menu when clicking outside", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    expect(menu).toBeInTheDocument();

    // Click outside on document body
    fireEvent.mouseDown(document.body, { clientX: 0, clientY: 0 });

    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("opens unlink confirmation dialog when 'Remove from library…' is clicked", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const removeItem = within(menu).getByRole("menuitem", {
      name: "Remove from library…",
    });

    await userEvent.click(removeItem);

    // Menu should close
    expect(screen.queryByRole("menu")).toBeNull();

    // Unlink confirmation dialog should appear
    const dialog = await waitFor(() => openDialog());
    expect(dialog).toBeInTheDocument();
    expect(within(dialog).getByText("Removing from library keeps all revisions and on-disk backups intact.")).toBeInTheDocument();
  });

  it("selects the game and shows detail view when 'Open details' is clicked", async () => {
    render(<App />);
    await screen.findByRole("button", { name: /Hades/ });

    const tile = screen.getByRole("button", { name: /Hades/ });
    fireEvent.contextMenu(tile, { clientX: 100, clientY: 100 });

    const menu = await screen.findByRole("menu", { name: "Game actions" });
    const openDetailsItem = within(menu).getByRole("menuitem", {
      name: "Open details",
    });

    await userEvent.click(openDetailsItem);

    // Menu should close
    expect(screen.queryByRole("menu")).toBeNull();

    // Detail view should be displayed (Back button and history panel present)
    expect(await screen.findByRole("button", { name: /Back to library/ })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "History" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "New snapshot" })).toBeInTheDocument();
  });
});

describe("clampMenuPosition", () => {
  it("keeps menu coordinates within viewport bounds for extreme coordinates", () => {
    const viewport = { width: 1920, height: 1080 };
    const topInset = 52;

    // Extreme coordinates far outside viewport
    const result = clampMenuPosition(
      { x: 9999, y: 9999 },
      MENU_SIZE,
      viewport,
      topInset,
    );

    // Menu should be clamped to bottom-right of viewport with margins
    expect(result.x).toBeLessThanOrEqual(viewport.width - MENU_SIZE.width - 8);
    expect(result.y).toBeLessThanOrEqual(viewport.height - MENU_SIZE.height - 8);
    expect(result.x).toBeGreaterThanOrEqual(8);
    expect(result.y).toBeGreaterThanOrEqual(topInset);
  });

  it("keeps menu above topInset when coordinates are too high", () => {
    const viewport = { width: 1920, height: 1080 };
    const topInset = 52;

    const result = clampMenuPosition(
      { x: 100, y: 0 },
      MENU_SIZE,
      viewport,
      topInset,
    );

    expect(result.y).toBe(topInset);
  });

  it("keeps menu within left margin when coordinates are negative", () => {
    const viewport = { width: 1920, height: 1080 };
    const topInset = 52;

    const result = clampMenuPosition(
      { x: -500, y: 100 },
      MENU_SIZE,
      viewport,
      topInset,
    );

    expect(result.x).toBe(8);
  });
});