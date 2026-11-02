import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  installTauriStub,
  seedActiveProvider,
  seedDropboxConnected,
  uninstallTauriStub,
} from "./test/tauri-stub";
import type { InvokeCall } from "./test/tauri-stub";

let calls: InvokeCall[] = [];

beforeEach(() => {
  calls = installTauriStub();
});

afterEach(() => {
  uninstallTauriStub();
});

type User = ReturnType<typeof userEvent.setup>;

async function openCloud(user: User): Promise<HTMLElement> {
  const nav = await screen.findByRole("navigation", { name: "Primary" });
  await user.click(within(nav).getByRole("button", { name: "Cloud" }));
  return screen.findByRole("group", { name: "Cloud provider" });
}

describe("cloud provider picker", () => {
  it("offers all three providers and opens on Google Drive", async () => {
    const user = userEvent.setup();
    render(<App />);

    const picker = await openCloud(user);

    expect(
      within(picker)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Google Drive", "Dropbox", "WebDAV"]);
    expect(
      within(picker).getByRole("button", { name: "Google Drive" }),
    ).toHaveAttribute("aria-pressed", "true");

    // The Drive panel keeps its original heading and label.
    expect(
      screen.getByRole("heading", { name: "Google Drive" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("OAuth client id")).toBeInTheDocument();
    expect(screen.getByText("Not connected")).toBeInTheDocument();
  });

  it("swaps the panel and persists the choice when the provider changes", async () => {
    const user = userEvent.setup();
    render(<App />);

    const picker = await openCloud(user);
    await user.click(within(picker).getByRole("button", { name: "Dropbox" }));

    expect(
      await screen.findByRole("heading", { name: "Dropbox" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("App key")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Save app key" }),
    ).toBeInTheDocument();
    // The Drive controls are gone until Google Drive is selected again.
    expect(screen.queryByLabelText("OAuth client id")).toBeNull();
    expect(
      calls.some(
        (call) =>
          call.cmd === "set_active_provider" &&
          call.args.provider === "dropbox",
      ),
    ).toBe(true);

    await user.click(within(picker).getByRole("button", { name: "WebDAV" }));

    expect(
      await screen.findByRole("heading", { name: "WebDAV" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("Endpoint")).toBeInTheDocument();
    expect(screen.getByLabelText("Username")).toBeInTheDocument();
    expect(screen.getByLabelText("Password")).toBeInTheDocument();
  });
});

describe("WebDAV settings", () => {
  it("keeps the password out of the DOM and sends it only with the command", async () => {
    const user = userEvent.setup();
    render(<App />);

    const picker = await openCloud(user);
    await user.click(within(picker).getByRole("button", { name: "WebDAV" }));

    const password = await screen.findByLabelText("Password");
    // A secret field, never a text field.
    expect(password).toHaveAttribute("type", "password");

    await user.type(
      screen.getByLabelText("Endpoint"),
      "https://dav.example.com/savesync",
    );
    await user.type(screen.getByLabelText("Username"), "alice");
    await user.type(password, "pa55word-sentinel");
    await user.click(screen.getByRole("button", { name: "Save and connect" }));

    const call = calls.find((entry) => entry.cmd === "set_webdav_config");
    expect(call?.args).toEqual({
      endpoint: "https://dav.example.com/savesync",
      username: "alice",
      password: "pa55word-sentinel",
    });

    // The field is cleared once the command has accepted the secret, and the
    // notice never repeats it.
    expect((password as HTMLInputElement).value).toBe("");
    expect(
      await screen.findByText(
        "WebDAV connected. The password is stored in the OS keyring.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/pa55word-sentinel/)).toBeNull();
  });
});

describe("Dropbox settings", () => {
  it("requires a saved app key before sign-in can start", async () => {
    const user = userEvent.setup();
    render(<App />);

    const picker = await openCloud(user);
    await user.click(within(picker).getByRole("button", { name: "Dropbox" }));

    expect(
      await screen.findByRole("button", { name: "Connect Dropbox" }),
    ).toBeDisabled();

    await user.type(screen.getByLabelText("App key"), "test-app-key");
    await user.click(screen.getByRole("button", { name: "Save app key" }));

    expect(
      await screen.findByRole("button", { name: "Connect Dropbox" }),
    ).toBeEnabled();
    await user.click(screen.getByRole("button", { name: "Connect Dropbox" }));

    expect(
      calls.some((call) => call.cmd === "begin_dropbox_oauth"),
    ).toBe(true);
  });
});

describe("sync gating", () => {
  it("follows the active provider, not Google Drive", async () => {
    seedActiveProvider("dropbox");
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: /Hades/ }));
    await openCloud(user);

    const sync = await screen.findByRole("button", { name: "Sync now" });
    expect(sync).toBeDisabled();
    expect(sync).toHaveAttribute("title", "Connect Dropbox first");
  });

  it("enables sync once the active provider is connected", async () => {
    seedActiveProvider("dropbox");
    seedDropboxConnected();
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: /Hades/ }));
    await openCloud(user);

    expect(
      await screen.findByRole("heading", { name: "Dropbox" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Sync now" })).toBeEnabled();
    expect(screen.getByText("Connected")).toBeInTheDocument();
  });
});
