import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

/**
 * jsdom has no top layer and does not implement `HTMLDialogElement.showModal()`
 * (it throws "Not implemented"), so the shims below are installed
 * unconditionally instead of behind a `??=` guard. Toggling the `open`
 * attribute is enough for React and for `dialog[open]` queries.
 *
 * jsdom also refuses to focus any element inside a *closed* `<dialog>`.
 *
 * These shims make OUR handlers testable only: `onCancel`, the backdrop
 * `onClick` on the `<dialog>` element itself, and the open/closed state.
 * Browser behaviour — the real focus trap, Escape closing the top layer, and
 * `::backdrop` hit-testing — is verified separately in Chromium.
 */
HTMLDialogElement.prototype.showModal = function (this: HTMLDialogElement) {
  this.setAttribute("open", "");
};
HTMLDialogElement.prototype.close = function (this: HTMLDialogElement) {
  this.removeAttribute("open");
};

window.matchMedia ??= ((query: string) => ({
  matches: false,
  media: query,
  onchange: null,
  addEventListener() {},
  removeEventListener() {},
})) as unknown as typeof window.matchMedia;

// App may construct Audio for the cartridge sound; jsdom cannot play it.
window.Audio ??= class {
  play() {
    return Promise.resolve();
  }
} as unknown as typeof Audio;

afterEach(() => {
  cleanup();
});
