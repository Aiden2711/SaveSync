import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  failIgdbCredentialSave,
  failMetadataFetch,
  failScreenscraperCredentialSave,
  fixtures,
  installTauriStub,
  seedGameMetadata,
  seedIgdbStatus,
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

/** The single input a settings label or aria-label points at. */
function inputByLabel(label: RegExp): HTMLInputElement {
  const input = screen.getByLabelText(label);
  if (!(input instanceof HTMLInputElement)) {
    throw new Error(`expected the ${String(label)} field to be an input`);
  }
  return input;
}

/** The settings section whose heading gives the region its accessible name. */
function settingsPanel(name: string): HTMLElement {
  return screen.getByRole("region", { name });
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
  it("shows only the account pair and keeps the developer pair collapsed", async () => {
    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("ScreenScraper");
    // The simple form is the player's own account, so it needs no setup hint.
    expect(within(panel).getByText("Not set up")).toBeInTheDocument();

    const username = inputByLabel(/screenscraper username/i);
    const password = inputByLabel(/screenscraper password/i);
    expect(username).toHaveValue("");
    expect(password).toHaveValue("");
    expect(username).toHaveAttribute("autocomplete", "off");
    expect(password).toHaveAttribute("type", "password");
    expect(password).toHaveAttribute("autocomplete", "off");

    const advanced = within(panel).getByText("Advanced").closest("details");
    expect(advanced).not.toBeNull();
    // Collapsed by default: the application's developer pair is not a first step.
    expect(advanced!.open).toBe(false);
    expect(advanced!.contains(inputByLabel(/screenscraper dev id/i))).toBe(true);
    expect(advanced!.contains(inputByLabel(/screenscraper dev password/i))).toBe(
      true,
    );
    expect(advanced!.contains(username)).toBe(false);
    expect(advanced!.contains(password)).toBe(false);
    expect(advanced!.contains(document.getElementById("screenscraper-dev-note")!)).toBe(
      true,
    );

    await user.click(within(panel).getByText("Advanced"));

    expect(advanced!.open).toBe(true);
    expect(inputByLabel(/screenscraper dev id/i)).toBeEnabled();
  });

  it("never renders a stored value, only keyring placeholders", async () => {
    seedScreenscraperStatus({
      configured: true,
      dev_configured: true,
      user_configured: true,
    });

    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("ScreenScraper");
    const devId = inputByLabel(/screenscraper dev id/i);
    const devPassword = inputByLabel(/screenscraper dev password/i);
    const username = inputByLabel(/screenscraper username/i);
    const password = inputByLabel(/screenscraper password/i);

    for (const input of [devId, devPassword, username, password]) {
      expect(input).toHaveValue("");
      expect(input).toHaveAttribute("placeholder", "Stored in the OS keyring");
    }
    // Both passwords are secret-entry fields, and autofill never fills them.
    expect(devPassword).toHaveAttribute("type", "password");
    expect(password).toHaveAttribute("type", "password");
    expect(devPassword).toHaveAttribute("autocomplete", "off");
    expect(password).toHaveAttribute("autocomplete", "off");

    expect(cmdCalls("screenscraper_status")).toHaveLength(1);
    expect(await within(panel).findByText("Signed in")).toBeInTheDocument();
    // Stored values never re-open the Advanced block on their own.
    expect(within(panel).getByText("Advanced").closest("details")!.open).toBe(false);
  });

  it("reports the developer pair as available before an account is added", async () => {
    seedScreenscraperStatus({ configured: true, dev_configured: true });

    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("ScreenScraper");
    expect(await within(panel).findByText("Available")).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: "Clear" })).toBeEnabled();
    // The account placeholder asks for the user's login, not a keyring hint.
    expect(inputByLabel(/screenscraper username/i)).toHaveAttribute(
      "placeholder",
      "ScreenScraper username",
    );
  });

  it("sends the typed account pair and leaves blank developer fields alone", async () => {
    seedScreenscraperStatus({ configured: true, dev_configured: true });

    const user = userEvent.setup();
    await openSettings(user);

    const sentinel = "ss-user-pw-sentinel";
    await user.type(inputByLabel(/screenscraper username/i), "someone");
    await user.type(inputByLabel(/screenscraper password/i), sentinel);
    await user.click(screen.getByRole("button", { name: "Save credentials" }));

    const saves = cmdCalls("set_screenscraper_credentials");
    expect(saves).toHaveLength(1);
    // Blank fields travel as empty strings, which the command reads as "keep".
    expect(saves[0].args).toEqual({
      dev_id: "",
      dev_password: "",
      ssid: "someone",
      sspassword: sentinel,
    });
    expect(screen.queryByRole("alert")).toBeNull();
    expect(
      await screen.findByText("ScreenScraper credentials saved in the OS keyring."),
    ).toBeInTheDocument();

    expect(
      within(settingsPanel("ScreenScraper")).getByText("Signed in"),
    ).toBeInTheDocument();
    // Nothing typed back into state survives the save.
    expect(inputByLabel(/screenscraper username/i)).toHaveValue("");
    expect(inputByLabel(/screenscraper password/i)).toHaveValue("");
    expect(document.body.textContent ?? "").not.toContain(sentinel);
  });

  it("sends empty strings so a blank field keeps the stored value", async () => {
    seedScreenscraperStatus({
      configured: true,
      dev_configured: true,
      user_configured: true,
    });

    const user = userEvent.setup();
    await openSettings(user);

    await user.click(screen.getByRole("button", { name: "Save credentials" }));

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
      await screen.findByText("ScreenScraper credentials saved in the OS keyring."),
    ).toBeInTheDocument();
  });

  it("shows the backend's guidance for an incomplete credential pair", async () => {
    seedScreenscraperStatus({ configured: true, dev_configured: true });

    const user = userEvent.setup();
    await openSettings(user);

    await user.type(inputByLabel(/screenscraper username/i), "someone");
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

    const panel = settingsPanel("ScreenScraper");
    await user.click(within(panel).getByText("Advanced"));
    await user.type(inputByLabel(/screenscraper dev id/i), "dev-id-sentinel");
    await user.type(inputByLabel(/screenscraper dev password/i), "dev-pw-sentinel");
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

    expect(inputByLabel(/screenscraper dev id/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-dev-note",
    );
    expect(inputByLabel(/screenscraper dev password/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-dev-note",
    );
    expect(inputByLabel(/screenscraper username/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-user-note",
    );
    expect(inputByLabel(/screenscraper password/i)).toHaveAttribute(
      "aria-describedby",
      "screenscraper-user-note",
    );
  });
});

describe("IGDB credential settings", () => {
  it("renders the client pair, the console note, and an unconfigured chip", async () => {
    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("IGDB");
    expect(await within(panel).findByText("Not set up")).toBeInTheDocument();

    const clientId = inputByLabel(/igdb client id/i);
    const clientSecret = inputByLabel(/igdb client secret/i);
    expect(clientId).toHaveValue("");
    expect(clientId).toHaveAttribute(
      "placeholder",
      "Twitch application client id",
    );
    expect(clientId).toHaveAttribute("autocomplete", "off");
    expect(clientSecret).toHaveValue("");
    expect(clientSecret).toHaveAttribute("type", "password");
    expect(clientSecret).toHaveAttribute("autocomplete", "off");

    expect(clientId).toHaveAttribute("aria-describedby", "igdb-console-note");
    expect(clientSecret).toHaveAttribute("aria-describedby", "igdb-console-note");

    const note = document.getElementById("igdb-console-note");
    expect(note).not.toBeNull();
    expect(note!.querySelector("a")).toHaveAttribute(
      "href",
      "https://dev.twitch.tv/console/apps/create",
    );

    // The read is a flag-only status call: neither value ever comes back.
    expect(cmdCalls("igdb_status")).toHaveLength(1);
  });

  it("never renders a stored value, only keyring placeholders", async () => {
    seedIgdbStatus({
      configured: true,
      client_id_configured: true,
      client_secret_configured: true,
    });

    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("IGDB");
    expect(await within(panel).findByText("Configured")).toBeInTheDocument();

    for (const label of [/igdb client id/i, /igdb client secret/i]) {
      const input = inputByLabel(label);
      expect(input).toHaveValue("");
      expect(input).toHaveAttribute("placeholder", "Stored in the OS keyring");
    }
    expect(within(panel).getByRole("button", { name: "Clear" })).toBeEnabled();
  });

  it("sends empty strings so a blank save keeps the stored pair", async () => {
    seedIgdbStatus({
      configured: true,
      client_id_configured: true,
      client_secret_configured: true,
    });

    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("IGDB");
    await user.click(within(panel).getByRole("button", { name: "Save" }));

    const saves = cmdCalls("set_igdb_credentials");
    expect(saves).toHaveLength(1);
    expect(saves[0].args).toEqual({ client_id: "", client_secret: "" });
    expect(screen.queryByRole("alert")).toBeNull();
    expect(
      await screen.findByText("IGDB credentials saved in the OS keyring."),
    ).toBeInTheDocument();
  });

  it("saves a typed pair, flips the chip, and clears both fields", async () => {
    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("IGDB");
    // Wait for the flags read so the chip starts from the seeded state.
    expect(await within(panel).findByText("Not set up")).toBeInTheDocument();

    const secret = "igdb-secret-sentinel";
    await user.type(inputByLabel(/igdb client id/i), "twitch-client-id");
    await user.type(inputByLabel(/igdb client secret/i), secret);
    await user.click(within(panel).getByRole("button", { name: "Save" }));

    expect(await within(panel).findByText("Configured")).toBeInTheDocument();

    const saves = cmdCalls("set_igdb_credentials");
    expect(saves).toHaveLength(1);
    expect(saves[0].args).toEqual({
      client_id: "twitch-client-id",
      client_secret: secret,
    });

    // The secret must not linger in the DOM once it is persisted.
    expect(inputByLabel(/igdb client id/i)).toHaveValue("");
    expect(inputByLabel(/igdb client secret/i)).toHaveValue("");
    expect(document.body.textContent ?? "").not.toContain(secret);
  });

  it("shows the backend's guidance for a lone client id", async () => {
    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("IGDB");
    await user.type(inputByLabel(/igdb client id/i), "lone-id-sentinel");
    await user.click(within(panel).getByRole("button", { name: "Save" }));

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain(
      "Enter both the IGDB client id and client secret.",
    );
    expect(cmdCalls("set_igdb_credentials")).toHaveLength(1);
    expect(document.body.textContent ?? "").not.toContain("lone-id-sentinel");
  });

  it("never echoes a rejected save into the banner", async () => {
    failIgdbCredentialSave(
      "Could not save the IGDB credentials to the OS keyring.",
    );

    const user = userEvent.setup();
    await openSettings(user);

    const panel = settingsPanel("IGDB");
    await user.type(inputByLabel(/igdb client id/i), "igdb-id-sentinel");
    await user.type(inputByLabel(/igdb client secret/i), "igdb-pw-sentinel");
    await user.click(within(panel).getByRole("button", { name: "Save" }));

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain(
      "Could not save the IGDB credentials to the OS keyring.",
    );
    expect(banner.textContent).not.toContain("igdb-id-sentinel");
    expect(banner.textContent).not.toContain("igdb-pw-sentinel");
  });
});

describe("metadata source picker", () => {
  it("defaults to Auto and omits the source from the fetch payload", async () => {
    const user = userEvent.setup();
    const panel = await openDetail(user);

    const picker = within(panel).getByRole("combobox", {
      name: "Metadata source",
    });
    expect(picker).toHaveValue("auto");
    expect(
      within(picker)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual(["Auto", "Steam", "ScreenScraper", "IGDB"]);

    await user.click(within(panel).getByRole("button", { name: "Fetch metadata" }));

    const fetches = cmdCalls("fetch_game_metadata");
    expect(fetches).toHaveLength(1);
    // Auto leaves the order to the backend: ScreenScraper, IGDB, then Steam.
    expect(fetches[0].args).toEqual({ game_id: fixtures.primaryGameId });
    expect(fetches[0].args).not.toHaveProperty("source");
    expect(await within(panel).findByText("via Steam")).toBeInTheDocument();
  });

  it("sends the chosen source and labels the result with it", async () => {
    seedIgdbStatus({
      configured: true,
      client_id_configured: true,
      client_secret_configured: true,
    });

    const user = userEvent.setup();
    const panel = await openDetail(user);

    const picker = within(panel).getByRole("combobox", {
      name: "Metadata source",
    });
    await user.selectOptions(picker, "igdb");
    expect(picker).toHaveValue("igdb");

    await user.click(within(panel).getByRole("button", { name: "Fetch metadata" }));

    const fetches = cmdCalls("fetch_game_metadata");
    expect(fetches).toHaveLength(1);
    expect(fetches[0].args).toEqual({
      game_id: fixtures.primaryGameId,
      source: "igdb",
    });
    expect(await within(panel).findByText("via IGDB")).toBeInTheDocument();
  });
});

