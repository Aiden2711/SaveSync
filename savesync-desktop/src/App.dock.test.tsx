import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import App from "./App";
import {
  fixtures,
  installTauriStub,
  seedDirtyGames,
  seedGames,
  uninstallTauriStub,
} from "./test/tauri-stub";

beforeEach(() => {
  installTauriStub();
});

afterEach(() => {
  uninstallTauriStub();
});

/** The docked band, located through the navigation landmark it contains. */
async function findDock(): Promise<HTMLElement> {
  const nav = await screen.findByRole("navigation", { name: "Primary" });
  const dock = nav.closest(".dock");
  if (!(dock instanceof HTMLElement)) {
    throw new Error("navigation landmark is not inside the docked band");
  }
  return dock;
}

function gamesWith(ids: string[]) {
  return fixtures.games.filter((game) => ids.includes(game.id));
}

describe("docked band", () => {
  it("renders the primary navigation inside a single .dock element", async () => {
    render(<App />);

    const dock = await findDock();
    const nav = within(dock).getByRole("navigation", { name: "Primary" });

    expect(dock).toContainElement(nav);
    expect(document.querySelectorAll(".dock")).toHaveLength(1);
  });

  it("reports an empty library without a state dot", async () => {
    seedGames([]);
    seedDirtyGames([]);
    render(<App />);

    const dock = await findDock();
    expect(await within(dock).findByText("No games enrolled")).toBeInTheDocument();
    expect(dock.querySelector(".dock-dot")).toBeNull();
  });

  it("singularises one enrolled game without a state dot", async () => {
    seedGames(gamesWith([fixtures.primaryGameId]));
    seedDirtyGames([]);
    render(<App />);

    const dock = await findDock();
    expect(await within(dock).findByText("1 game enrolled")).toBeInTheDocument();
    expect(dock.querySelector(".dock-dot")).toBeNull();
  });

  it("pluralises the enrolled-game count", async () => {
    seedDirtyGames([]);
    render(<App />);

    const dock = await findDock();
    expect(await within(dock).findByText("4 games enrolled")).toBeInTheDocument();
    expect(dock.querySelector(".dock-dot")).toBeNull();
  });

  it("singularises one uncaptured-change game and shows the state dot", async () => {
    seedGames(gamesWith([fixtures.secondGameId]));
    seedDirtyGames([fixtures.secondGameId]);
    render(<App />);

    const dock = await findDock();
    expect(
      await within(dock).findByText("1 game with uncaptured changes"),
    ).toBeInTheDocument();
    expect(dock.querySelector(".dock-dot")).not.toBeNull();
  });

  it("pluralises the uncaptured-change count and shows the state dot", async () => {
    seedDirtyGames([fixtures.secondGameId, fixtures.thirdGameId]);
    render(<App />);

    const dock = await findDock();
    expect(
      await within(dock).findByText("2 games with uncaptured changes"),
    ).toBeInTheDocument();
    expect(dock.querySelector(".dock-dot")).not.toBeNull();
  });

  it("counts only enrolled games that are actually dirty", async () => {
    // The dirty list may name a game that is no longer enrolled; the readout and
    // the dot must agree with what is on screen, not with the raw id list.
    seedGames(gamesWith([fixtures.primaryGameId]));
    seedDirtyGames([fixtures.secondGameId]);
    render(<App />);

    const dock = await findDock();
    expect(await within(dock).findByText("1 game enrolled")).toBeInTheDocument();
    expect(dock.querySelector(".dock-dot")).toBeNull();
  });
});
