import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  failArtworkKeySave,
  installTauriStub,
  seedArtworkStatus,
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

/** Renders the app and reaches the Settings view through the real nav button. */
async function openSettings(user: User): Promise<HTMLInputElement> {
  render(<App />);

  const nav = await screen.findByRole("navigation", { name: "Primary" });
  await user.click(within(nav).getByRole("button", { name: "Settings" }));

  await screen.findByRole("heading", { name: "SteamGridDB" });
  const input = screen.getByLabelText(/steamgriddb api key/i);
  if (!(input instanceof HTMLInputElement)) {
    throw new Error("expected the SteamGridDB API key field to be an input");
  }
  return input;
}

/** Only the key-mutating calls, so counts stay easy to reason about. */
function keyCalls(cmd: string): InvokeCall[] {
  return calls.filter((call) => call.cmd === cmd);
}

describe("SteamGridDB API key settings", () => {
  it("renders an enabled, labelled, secret-entry field", async () => {
    const user = userEvent.setup();
    const input = await openSettings(user);

    expect(input).toBeEnabled();
    expect(input).toHaveAttribute("type", "password");
    expect(input).toHaveAttribute("aria-describedby", "steamgriddb-note");
    expect(input).toHaveAttribute("autocomplete", "off");
  });

  it("shows Not configured until the keyring reports a stored key", async () => {
    const user = userEvent.setup();
    render(<App />);

    const nav = await screen.findByRole("navigation", { name: "Primary" });
    await user.click(within(nav).getByRole("button", { name: "Settings" }));

    // The status fetch runs when Settings opens, not at startup.
    expect(await screen.findByText("Not configured")).toBeInTheDocument();
    expect(keyCalls("steamgriddb_status")).toHaveLength(1);
    expect(screen.queryByRole("button", { name: "Remove key" })).toBeNull();
  });

  it("shows Configured when the OS keyring already holds a key", async () => {
    seedArtworkStatus(true);

    const user = userEvent.setup();
    await openSettings(user);

    expect(await screen.findByText("Configured")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Remove key" }),
    ).toBeInTheDocument();
  });

  it("saves the entered key, flips the badge, and clears the field", async () => {
    const user = userEvent.setup();
    const input = await openSettings(user);

    await screen.findByText("Not configured");
    await user.type(input, "sgsdb-test-key-1234");
    await user.click(screen.getByRole("button", { name: "Save key" }));

    expect(await screen.findByText("Configured")).toBeInTheDocument();

    const saves = keyCalls("set_steamgriddb_api_key");
    expect(saves).toHaveLength(1);
    expect(saves[0].args).toEqual({ api_key: "sgsdb-test-key-1234" });

    // The secret must not linger in the DOM once it is persisted.
    expect(screen.getByLabelText(/steamgriddb api key/i)).toHaveValue("");
  });

  it("never echoes the key back in an error banner", async () => {
    const sentinel = "sgsdb-sentinel-key-0123456789";
    failArtworkKeySave("Could not save the SteamGridDB API key.");

    const user = userEvent.setup();
    const input = await openSettings(user);

    await user.type(input, sentinel);
    await user.click(screen.getByRole("button", { name: "Save key" }));

    const banner = await screen.findByRole("alert");
    // The error banner really does render text, so the negative assertion below
    // cannot pass vacuously.
    expect(banner.textContent).toContain("Could not save the SteamGridDB API key.");
    expect(banner.textContent).not.toContain(sentinel);
    expect(screen.getByText("Not configured")).toBeInTheDocument();
    expect(document.body.textContent ?? "").not.toContain(sentinel);

    // The failing call still carried the key to the backend — and only there.
    expect(keyCalls("set_steamgriddb_api_key")).toHaveLength(1);
  });

  it("removes a stored key and returns the badge to Not configured", async () => {
    seedArtworkStatus(true);

    const user = userEvent.setup();
    await openSettings(user);

    await user.click(await screen.findByRole("button", { name: "Remove key" }));

    expect(await screen.findByText("Not configured")).toBeInTheDocument();
    expect(keyCalls("clear_steamgriddb_api_key")).toHaveLength(1);
    expect(screen.queryByRole("button", { name: "Remove key" })).toBeNull();
  });
});
