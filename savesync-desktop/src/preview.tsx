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
 */
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./styles.css";
import { fixtures, installTauriStub, seedDirtyGames, seedGames } from "./test/tauri-stub";

const THEME_KEY = "savesync-theme";

function requestedTheme(): "light" | "dark" | null {
  const value = new URLSearchParams(window.location.search).get("theme");
  return value === "light" || value === "dark" ? value : null;
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

const container = document.getElementById("root");
if (!container) throw new Error("missing #root element");

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
