import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  failMetadataFetch,
  failScreenscraperCredentialSave,
  fixtures,
  installTauriStub,
  seedGameMetadata,
  seedScreenscraperStatus,
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
function cmdCalls(cmd: string): InvokeCall[] {
  return calls.filter((call) => call.cmd === cmd);
}

/** Renders the app and opens the first fixture's detail view by clicking its tile. */
async function openDetail(user: User): Promise<HTMLElement> {
  render(<App />);

  const tile = await screen.findByRole("button", { name: /Hades/ });
  await user.click(tile);
  await screen.findByRole("button", { name: /Back to library/ });

  return screen.getByRole("region", { name: "Metadata" });
}

/** Renders the app and reaches the Settings view through the real nav button. */
async function openSettings(user: User): Promise<void> {
  render(<App />);

  const nav = await screen.findByRole("navigation", { name: "Primary" });
  await user.click(within(nav).getByRole("button", { name: "Settings" }));

  await screen.findByRole("heading", { name: "ScreenScraper" });
}

function screenScraperInput(label: RegExp): HTMLInputElement {
  const input = screen.getByLabelText(label);
  if (!(input instanceof HTMLInputElement)) {
    throw new Error(`expected the ${String(label)} field to be an input`);
  }
  return input;
}

describe("detail view metadata panel", () => {
  it("renders the cached facts, description, and provider chip", async () => {
    seedGameMetadata(fixtures.primaryGameId, {
      source: "steam",
      description_short: "A rogue-like dungeon crawler.",
      description_long: "Defy the god of the dead as you hack and slash out of the Underworld.",
    });

    const user = userEvent.setup();
    const panel = await openDetail(user);

    // The cached read happens once on open and never calls the network command.
    expect(cmdCalls("game_metadata")).toHaveLength(1);
    expect(cmdCalls("fetch_game_metadata")).toHaveLength(0);

    expect(within(panel).getByText("via Steam")).toBeInTheDocument();
    expect(within(panel).getByText("2020")).toBeInTheDocument();
    expect(within(panel).getByText("Action, Roguelike")).toBeInTheDocument();
    // Developer and publisher are the same studio here, so the value appears twice.
    expect(within(panel).getAllByText("Supergiant Games")).toHaveLength(2);
    expect(
      within(panel).getByText("A rogue-like dungeon crawler."),
    ).toBeInTheDocument();
  });

  it("shows the empty state and only fetches when asked", async () => {
    const user = userEvent.setup();
    const panel = await openDetail(user);

    expect(cmdCalls("fetch_game_metadata")).toHaveLength(0);
    expect(within(panel).getByText(/No metadata yet/i)).toBeInTheDocument();

    await user.click(within(panel).getByRole("button", { name: "Fetch metadata" }));

    const fetches = cmdCalls("fetch_game_metadata");
    expect(fetches).toHaveLength(1);
    // No source is sent, so the backend keeps choosing the default provider.
    expect(fetches[0].args).toEqual({ game_id: fixtures.primaryGameId });

    expect(await within(panel).findByText("via Steam")).toBeInTheDocument();
    expect(within(panel).queryByText(/No metadata yet/i)).toBeNull();
    expect(within(panel).getByText("2020")).toBeInTheDocument();
  });

  it("surfaces a fetch failure in the banner without losing the panel", async () => {
    const message = "Could not fetch metadata for this game.";
    failMetadataFetch(message);

    const user = userEvent.setup();
    const panel = await openDetail(user);

    await user.click(within(panel).getByRole("button", { name: "Fetch metadata" }));

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain(message);
    expect(within(panel).getByText(/No metadata yet/i)).toBeInTheDocument();
    expect(within(panel).queryByText("via Steam")).toBeNull();
  });

  it("expands and collapses the long description", async () => {
    const short = "A rogue-like dungeon crawler.";
    const long =
      "Defy the god of the dead as you hack and slash out of the Underworld.";
    seedGameMetadata(fixtures.primaryGameId, {
      description_short: short,
      description_long: long,
    });

    const user = userEvent.setup();
    const panel = await openDetail(user);

    const readMore = within(panel).getByRole("button", { name: "Read more" });
    expect(readMore).toHaveAttribute("aria-expanded", "false");
    expect(within(panel).getByText(short)).toBeInTheDocument();
    expect(within(panel).queryByText(long)).toBeNull();

    await user.click(readMore);

    const showLess = within(panel).getByRole("button", { name: "Show less" });
    expect(showLess).toHaveAttribute("aria-expanded", "true");
    expect(within(panel).getByText(long)).toBeInTheDocument();
    expect(within(panel).queryByText(short)).toBeNull();

    await user.click(showLess);

    expect(
      within(panel).getByRole("button", { name: "Read more" }),
    ).toHaveAttribute("aria-expanded", "false");
    expect(within(panel).getByText(short)).toBeInTheDocument();
  });
});

describe("ScreenScraper credential settings", () => {
  it("never renders a stored value, only keyring placeholders", async () => {
    seedScreenscraperStatus({
      configured: true,
      dev_configured: true,
      user_configured: true,
    });

    const user = userEvent.setup();
    await openSettings(user);

    const devId = screenScraperInput(/screenscraper dev id/i);
    const devPassword = screenScraperInput(/screenscraper dev password/i);
    const ssid = screenScraperInput(/screenscraper user ssid/i);
    const userPassword = screenScraperInput(/screenscraper user ss password/i);

    for (const input of [devId, devPassword, ssid, userPassword]) {
      expect(input).toHaveValue("");
      expect(input).toHaveAttribute("placeholder", "Stored in the OS keyring");
    }
    // Both passwords are secret-entry fields, and autofill never fills them.
    expect(devPassword).toHaveAttribute("type", "password");
    expect(userPassword).toHaveAttribute("type", "password");
    expect(devPassword).toHaveAttribute("autocomplete", "off");
    expect(userPassword).toHaveAttribute("autocomplete", "off");

    expect(cmdCalls("screenscraper_status")).toHaveLength(1);
    expect(await screen.findByText("Developer and user saved")).toBeInTheDocument();
  });

  it("sends empty strings so a blank field keeps the stored value", async () => {
    seedScreenscraperStatus({
      configured: true,
      dev_configured: true,
      user_configured: true,
    });

    const user = userEvent.setup();
    await openSettings(user);

    await user.click(
      screen.getByRole("button", { name: "Save credentials" }),
    );

    const saves = cmdCalls("set_screenscraper_credentials");
    expect(saves).toHaveLength(1);
    expect(saves[0].args).toEqual({
      dev_id: "",
      dev_password: "",
      ssid: "",
      sspassword: "",
    });
    expect(screen.queryByRole("alert")).toBeNull();
    expect(
      await screen.findByText(/credentials saved in the OS keyring/i),
    ).toBeInTheDocument();
  });

  it("shows the backend's guidance for an incomplete credential pair", async () => {
    seedScreenscraperStatus({ configured: true, dev_configured: true });

    const user = userEvent.setup();
    await openSettings(user);

    await user.type(screenScraperInput(/screenscraper user ssid/i), "someone");
    await user.click(screen.getByRole("button", { name: "Save credentials" }));

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain(
      "Enter both the ScreenScraper user SSID and user SS password.",
    );
    expect(cmdCalls("set_screenscraper_credentials")).toHaveLength(1);
    // No credential value may leak into the banner or the page.
    expect(document.body.textContent ?? "").not.toContain("someone");
  });

  it("never echoes a rejected save into the banner", async () => {
    failScreenscraperCredentialSave(
      "Could not save the ScreenScraper credentials to the OS keyring.",
    );

    const user = userEvent.setup();
    await openSettings(user);

    await user.type(screenScraperInput(/screenscraper dev id/i), "dev-id-sentinel");
    await user.type(
      screenScraperInput(/screenscraper dev password/i),
      "dev-pw-sentinel",
    );
    await user.click(screen.getByRole("button", { name: "Save credentials" }));

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain(
      "Could not save the ScreenScraper credentials to the OS keyring.",
    );
    expect(banner.textContent).not.toContain("dev-id-sentinel");
    expect(banner.textContent).not.toContain("dev-pw-sentinel");
  });

  it("links both help notes to their documented pages", async () => {
    const user = userEvent.setup();
    await openSettings(user);

    const devNote = document.getElementById("screenscraper-dev-note");
    const userNote = document.getElementById("screenscraper-user-note");
    expect(devNote).not.toBeNull();
    expect(userNote).not.toBeNull();

    expect(devNote!.querySelector("a")).toHaveAttribute(
      "href",
      "https://www.screenscraper.fr/forumsujets.php?frub=12",
    );
    expect(userNote!.querySelector("a")).toHaveAttribute(
      "href",
      "https://www.screenscraper.fr/membreinscription.php",
    );

    expect(screenScraperInput(/screenscraper dev id/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-dev-note",
    );
    expect(screenScraperInput(/screenscraper dev password/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-dev-note",
    );
    expect(screenScraperInput(/screenscraper user ssid/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-user-note",
    );
    expect(screenScraperInput(/screenscraper user ss password/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-user-note",
    );
  });
});
