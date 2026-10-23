import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "./App";
import {
  fixtures,
  installTauriStub,
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

function restoreCalls(): InvokeCall[] {
  return calls.filter((call) => call.cmd === "restore_revision");
}

/**
 * `dialog[open]` only exists while the dialog is mounted, so it doubles as the
 * "closed" assertion after a cancel/backdrop/confirm path.
 */
function openDialog(): HTMLDialogElement {
  const element = document.querySelector("dialog[open]");
  if (!(element instanceof HTMLDialogElement)) {
    throw new Error("expected an open <dialog>");
  }
  return element;
}

/**
 * Selects the first game, opens the destructive *exact* restore dialog from the
 * history row, and returns the dialog plus its two buttons scoped to that
 * dialog. "Exact restore" is also the label of the history-row trigger, so
 * every assertion below is scoped with `within(dialog)`.
 */
async function openExactRestoreDialog(user: User) {
  await user.click(await screen.findByRole("button", { name: /Hades/ }));

  // Two revisions are listed, each with its own trigger. The dialog is not open
  // yet, so every match belongs to a history row.
  const triggers = await screen.findAllByRole("button", { name: "Exact restore" });
  await user.click(triggers[0]);

  const dialog = openDialog();
  return {
    dialog,
    cancel: within(dialog).getByRole("button", { name: "Cancel" }),
    confirm: within(dialog).getByRole("button", { name: "Exact restore" }),
  };
}

describe("restore confirmation dialog", () => {
  it("puts initial focus on Cancel", async () => {
    const user = userEvent.setup();
    render(<App />);

    const { cancel } = await openExactRestoreDialog(user);

    expect(cancel).toBe(document.activeElement);
  });

  it("closes without restoring when the dialog is cancelled (Esc path)", async () => {
    const user = userEvent.setup();
    render(<App />);

    const { dialog } = await openExactRestoreDialog(user);
    expect(restoreCalls()).toHaveLength(0);

    // Dispatch the native `cancel` event our onCancel handler listens for. The
    // browser's real Escape key (top layer) is covered separately in Chromium.
    act(() => {
      dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
    });

    expect(restoreCalls()).toHaveLength(0);
    expect(document.querySelector("dialog")).toBeNull();
  });

  it("closes without restoring when the backdrop is clicked", async () => {
    const user = userEvent.setup();
    render(<App />);

    const { dialog } = await openExactRestoreDialog(user);
    expect(restoreCalls()).toHaveLength(0);

    // Click the <dialog> element itself, not the .dialog-card inside it.
    await user.click(dialog);

    expect(restoreCalls()).toHaveLength(0);
    expect(document.querySelector("dialog")).toBeNull();
  });

  it("restores exactly once with exact_mirror=true when confirmed", async () => {
    const user = userEvent.setup();
    render(<App />);

    const { confirm } = await openExactRestoreDialog(user);
    await user.click(confirm);

    await waitFor(() => expect(restoreCalls()).toHaveLength(1));
    expect(restoreCalls()[0].args).toEqual({
      revision_id: fixtures.revisions[0].id,
      exact_mirror: true,
    });
  });

  it("does not double-restore on a double click", async () => {
    const user = userEvent.setup();
    render(<App />);

    const { confirm } = await openExactRestoreDialog(user);
    await user.click(confirm);
    await user.click(confirm);

    await waitFor(() => expect(restoreCalls()).toHaveLength(1));
    expect(restoreCalls()).toHaveLength(1);
  });
});
