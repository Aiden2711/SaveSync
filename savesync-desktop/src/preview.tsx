/**
 * DEV-ONLY preview entry.
 *
 * Renders the real production `<App/>` — the same component, `StrictMode` and
 * stylesheet import as `src/main.tsx` — against the deterministic Tauri test
 * stub, so the shipping UI can be inspected (and screenshotted) in a plain
 * browser without the desktop shell.
 *
 * Not part of the production build: Vite's default input stays `index.html`,
 * and nothing imports this module from `main.tsx`.
 *
 * `?theme=light|dark` selects the theme the same way the app does — the app
 * reads `localStorage["savesync-theme"]` in its `useState` initializer, so the
 * parameter only pre-seeds that key and the document attribute.
 *
 * `?provider=google|dropbox|webdav` selects the Cloud view's active provider and
 * `&state=connected|pending|error` seeds that provider's state, so the
 * connected/pending/error presentations can be inspected without a real remote.
 * `&error=…` overrides the seeded error text.
 *
 * `?featured=1` pins the first fixture as the library's wide banner and seeds
 * its wide cover cache, so the featured tile can be captured in both themes.
 */
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./styles.css";
import {
  fixtures,
  installTauriStub,
  seedActiveProvider,
  seedDirtyGames,
  seedFeatured,
  seedGames,
  seedGoogleStatus,
  seedProviderStatus,
  seedWideCover,
} from "./test/tauri-stub";
import type { ProviderKind } from "./types";

const THEME_KEY = "savesync-theme";
const params = new URLSearchParams(window.location.search);

function requestedTheme(): "light" | "dark" | null {
  const value = params.get("theme");
  return value === "light" || value === "dark" ? value : null;
}

function requestedProvider(): ProviderKind | null {
  const value = params.get("provider");
  return value === "google" || value === "dropbox" || value === "webdav"
    ? value
    : null;
}

/** Non-secret identifier each provider's panel would show once configured. */
const IDENTIFIERS: Record<ProviderKind, string> = {
  google: "test-client.apps.googleusercontent.com",
  dropbox: "test-app-key",
  webdav: "https://dav.example.com/savesync",
};

/** Display-only error text; `&error=` overrides it for a specific capture. */
const ERROR_TEXT: Record<ProviderKind, string> = {
  google: "Sign-in was cancelled.",
  dropbox: "The Dropbox app key was rejected.",
  webdav: "Password rejected",
};

/**
 * Seed the Cloud view's provider selection and state. Must run after
 * `installTauriStub`, which restores the pristine provider fixtures.
 */
function seedCloudState(): void {
  const kind = requestedProvider();
  if (!kind) return;
  seedActiveProvider(kind);

  const state = params.get("state");
  if (state !== "connected" && state !== "pending" && state !== "error") return;

  const shared = {
    identifier: IDENTIFIERS[kind],
    configured: true,
    connected: state === "connected",
    pending: state === "pending",
    error: state === "error" ? (params.get("error") ?? ERROR_TEXT[kind]) : null,
  };

  if (kind === "google") {
    seedGoogleStatus({
      client_id: shared.identifier,
      connected: shared.connected,
      pending: shared.pending,
      error: shared.error,
    });
  } else {
    seedProviderStatus(kind, shared);
  }
}

const theme = requestedTheme();
if (theme) {
  try {
    window.localStorage.setItem(THEME_KEY, theme);
  } catch {
    // The app falls back to the system preference on its own.
  }
  // Matches what `App` writes in its theme effect, and avoids a pre-mount flash.
  document.documentElement.dataset.theme = theme;
}

installTauriStub();
seedGames([...fixtures.games]);
// More than one dirty id so the dock dot and the tile badges are both visible.
seedDirtyGames([fixtures.secondGameId, fixtures.thirdGameId]);

/**
 * Pin the first fixture as the banner and seed its wide cache. `featured` is a
 * manual flag in the real app, so this mirrors what a user's context-menu pin
 * writes; the wide URL resolves from the seeded cache the same way the shell
 * serves it.
 */
if (params.get("featured") === "1") {
  seedFeatured(fixtures.primaryGameId);
  seedWideCover(fixtures.primaryGameId);
}

seedCloudState();

const container = document.getElementById("root");
if (!container) throw new Error("missing #root element");

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
