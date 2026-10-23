import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import { installTauriStub, uninstallTauriStub } from "./test/tauri-stub";

beforeEach(() => {
  installTauriStub();
});

afterEach(() => {
  uninstallTauriStub();
});

async function findNav(): Promise<HTMLElement> {
  return screen.findByRole("navigation", { name: "Primary" });
}

describe("top navigation", () => {
  it("renders the five view destinations as buttons", async () => {
    render(<App />);

    const nav = await findNav();
    const labels = within(nav)
      .getAllByRole("button")
      .map((button) => button.textContent?.trim());

    expect(labels).toEqual(["Library", "Snapshots", "Conflicts", "Cloud", "Settings"]);
  });

  it("keeps every Google Drive control out of the navigation bar", async () => {
    render(<App />);

    const nav = await findNav();

    // The bar must not advertise the remote provider: no aria-label mention and
    // no visible text. The Drive panel lives in the Cloud view instead.
    expect(nav.querySelector('[aria-label*="Google Drive"]')).toBeNull();
    expect(within(nav).queryByText(/google drive/i)).toBeNull();
  });

  it("reaches the snapshot and conflict destinations without a separate route", async () => {
    const user = userEvent.setup();
    render(<App />);

    const nav = await findNav();

    await user.click(within(nav).getByRole("button", { name: "Snapshots" }));
    expect(
      await screen.findByRole("heading", { name: "Snapshot history" }),
    ).toBeInTheDocument();

    await user.click(within(nav).getByRole("button", { name: "Conflicts" }));
    expect(
      await screen.findByRole("heading", { name: "Conflicts" }),
    ).toBeInTheDocument();
  });

  it("reaches the relocated Google Drive panel in the Cloud view", async () => {
    const user = userEvent.setup();
    render(<App />);

    const nav = await findNav();
    await user.click(within(nav).getByRole("button", { name: "Cloud" }));

    expect(
      await screen.findByRole("heading", { name: "Google Drive" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("OAuth client id")).toBeInTheDocument();
    // Every Drive control survives the move out of the sidebar.
    expect(screen.getByRole("button", { name: "Save client id" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Connect Drive" })).toBeInTheDocument();

    // Opened straight from the nav there is no selected game, so the Cloud view
    // must not strand an inert sync control. It routes to the library instead.
    expect(screen.queryByRole("button", { name: "Sync now" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Go to library" }));
    expect(
      await screen.findByRole("heading", { name: "Game library" }),
    ).toBeInTheDocument();

    // Choosing a game makes the real sync action available again.
    await user.click(await screen.findByRole("button", { name: /Hades/ }));
    await user.click(within(nav).getByRole("button", { name: "Cloud" }));
    expect(
      await screen.findByRole("button", { name: "Sync now" }),
    ).toBeInTheDocument();
  });

  it("reserves a Settings section for the SteamGridDB API key", async () => {
    const user = userEvent.setup();
    render(<App />);

    const nav = await findNav();
    await user.click(within(nav).getByRole("button", { name: "Settings" }));

    expect(
      await screen.findByRole("heading", { name: "SteamGridDB" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "Theme" })).toBeInTheDocument();
    expect(screen.getByLabelText("SteamGridDB API key")).toBeDisabled();
  });
});
