import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent, ReactNode } from "react";
import { api } from "./api";
import CoverArt from "./components/CoverArt";
import { ADAPTER_LABELS, adapterLabel } from "./lib/cover";
import { clampMenuPosition, MENU_SIZE, TOPBAR_FALLBACK_HEIGHT } from "./lib/menu";
import { isDesktop } from "./tauri";
import type {
  AppStatus,
  Conflict,
  CoverKind,
  DirtyEvent,
  Game,
  GameMetadata,
  GoogleStatus,
  GridCandidate,
  MetadataSource,
  ProviderKind,
  ProviderStatus,
  Revision,
  ScreenScraperStatus,
  SyncReport,
} from "./types";

function playCartridgeSound(name: "insert" | "eject"): void {
  if (!isDesktop()) return;
  const url = `resource://cartridge-${name}.wav`;
  new Audio(url).play().catch(() => {
    // Silently ignore playback errors (e.g., resource not available in browser)
  });
}

const ADAPTER_ORDER = [
  "custom_directory",
  "ludusavi",
  "emulator",
  "jksv",
] as const;

type Theme = "light" | "dark";
/** Top-level destination reached from the top navigation bar. */
type View = "library" | "snapshots" | "conflicts" | "cloud" | "settings";
type DialogState =
  | { kind: "restore"; revision: Revision; exactMirror: boolean }
  | { kind: "conflict"; conflict: Conflict; revisionId: string }
  | { kind: "disconnect" }
  | null;

function initialTheme(): Theme {
  try {
    const stored = window.localStorage.getItem("savesync-theme");
    if (stored === "light" || stored === "dark") return stored;
  } catch {
    // Use the system preference when storage is unavailable.
  }
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function formatDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (value >= 1024 && unit < units.length - 1);
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`;
}

function shortId(id: string): string {
  return id.slice(0, 8);
}

/**
 * "What to do" half of a provider error line.
 *
 * The backend owns what happened — those strings are surfaced verbatim and are
 * never rewritten here — so the next step is added at the presentation layer.
 * A message that already carries advice is left alone, so the line never reads
 * as two competing instructions.
 */
const PROVIDER_ERROR_ADVICE: Record<ProviderKind, string> = {
  google: "Check the OAuth client id, then connect again.",
  dropbox: "Check the app key, then connect again.",
  webdav: "Check the endpoint, username, and password, then try again.",
};

function providerErrorText(kind: ProviderKind, message: string): string {
  if (/(try again|connect again|reconnect|check )/i.test(message)) {
    return message;
  }
  const whatHappened = message.trim().replace(/[.!?]+$/, "");
  return `${whatHappened}. ${PROVIDER_ERROR_ADVICE[kind]}`;
}

/**
 * Display name of each metadata provider, for the detail panel's source chip.
 *
 * The ids come from core (`MetadataSource`); only a `screenscraper` id is
 * unspaced, so the label map is the one place the brand spelling differs.
 */
const METADATA_SOURCE_LABELS: Record<MetadataSource, string> = {
  steam: "Steam",
  screenscraper: "ScreenScraper",
  igdb: "IGDB",
};

/** Busy line for a manual metadata fetch; the detail button reuses the label. */
const METADATA_BUSY = "Fetching metadata";

/** State of the "Choose cover art" modal. */
type CoverPickerState = {
  gameId: string;
  title: string;
  /**
   * Which cover shape the grid is showing. Portrait is the default, so the
   * dialog opens exactly as it did before wide art existed.
   */
  kind: CoverKind;
  status: "loading" | "ready" | "error";
  candidates: GridCandidate[];
  /** Friendly text only; never a raw backend message. */
  error: string | null;
  /** Grid id currently downloading, so every other thumbnail can be disabled. */
  pickingId: number | null;
  /**
   * Grid ids whose CDN thumbnail failed to load. They are shown as unavailable
   * and cannot be picked: selecting one would download from a URL the webview
   * has just proven it cannot fetch. Per tile, never a dialog-wide error.
   */
  failedIds: number[];
};

/**
 * User-facing text for a failed picker operation.
 *
 * Backend messages are matched against this allowlist instead of being
 * rendered. An unexpected IPC, transport, or keyring error must never put a
 * raw `error.message` — which can carry URLs or paths — into the dialog.
 */
function coverPickerMessage(cause: unknown): string {
  const raw = cause instanceof Error ? cause.message : "";
  if (raw === "Configure the SteamGridDB API key in Settings first.") {
    return "Add a SteamGridDB API key in Settings to choose cover art.";
  }
  if (raw === "No cover art found for this game.") {
    return "No cover art found for this game.";
  }
  if (raw === "That cover is no longer available. Pick another.") {
    return "That cover is no longer available. Pick another.";
  }
  return "Could not load or save cover art. Check your connection and try again.";
}

/**
 * First focusable control inside a dialog, in DOM order. Keyboard users land on
 * Cancel, which is rendered first in every confirmation dialog.
 */
const DIALOG_FOCUSABLE =
  'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [href], [tabindex]:not([tabindex="-1"])';

/**
 * Modal dialog built on the native <dialog> element, which provides focus
 * trapping, Escape handling, an inert background, and focus restoration.
 */
function Modal({
  titleId,
  onClose,
  wide = false,
  children,
}: {
  titleId: string;
  onClose: () => void;
  /** Ask for the wider geometry; every other dialog keeps its narrow card. */
  wide?: boolean;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el || el.open) return;
    const previous = document.activeElement as HTMLElement | null;
    el.showModal();
    // React applies `autoFocus` during commit, while the dialog is still
    // `display: none`, so browsers drop that focus call and `showModal()` falls
    // back to focusing the dialog box itself. Once the dialog is in the top
    // layer, move focus to the first control (Cancel in every dialog) unless a
    // descendant already has it.
    if (!el.contains(document.activeElement) || document.activeElement === el) {
      el.querySelector<HTMLElement>(DIALOG_FOCUSABLE)?.focus();
    }
    return () => {
      if (el.open) el.close();
      previous?.focus?.();
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className={wide ? "modal modal-wide" : "modal"}
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
      onClick={(event) => {
        if (event.target === ref.current) onClose();
      }}
    >
      {children}
    </dialog>
  );
}

/**
 * A documentation link inside a helper note.
 *
 * The desktop webview drops `target="_blank"` requests (the platform shell has
 * no new-window handler), so the app's existing convention applies: hand the
 * URL to a Rust command that opens the system browser, after checking it
 * against the allowlist of documented pages. The `href` still carries the real
 * URL so the link reads and copies correctly in a plain browser.
 */
function DocsLink({
  href,
  onError,
  children,
}: {
  href: string;
  onError: (message: string) => void;
  children: ReactNode;
}) {
  return (
    <a
      className="docs-link"
      href={href}
      target="_blank"
      rel="noreferrer"
      onClick={(event) => {
        event.preventDefault();
        void api.openDocsUrl(href).catch((cause) => onError(String(cause)));
      }}
    >
      {children}
    </a>
  );
}

/**
 * Shared attributes for the filled nav glyphs. Each glyph paints in
 * `currentColor` — white, taken from the gradient badge `.nav-icon` draws
 * behind it — so the SVG carries no hue of its own. The badge's
 * `display: grid; place-items: center` centres the 18px box.
 */
const NAV_SVG_PROPS = {
  viewBox: "0 0 24 24",
  "aria-hidden": true,
  focusable: "false",
} as const;

/**
 * Purely decorative filled glyph for a nav destination; the label carries the
 * name. Every shape is a flat `currentColor` fill, so the glyph takes the white
 * ink of the gradient badge `.nav-icon` draws behind it. The destination hue
 * lives in CSS (`[data-view]` on the badge), which keeps the theme tokens as
 * the single source of colour and lets one glyph set serve both themes.
 */
function NavGlyph({ view }: { view: View }) {
  return (
    <svg {...NAV_SVG_PROPS} fill="currentColor">
      {view === "library" && (
        <>
          <rect x="3" y="3" width="8" height="8" rx="2.2" fill="currentColor" />
          <rect x="13" y="3" width="8" height="8" rx="2.2" fill="currentColor" />
          <rect x="3" y="13" width="8" height="8" rx="2.2" fill="currentColor" />
          <rect x="13" y="13" width="8" height="8" rx="2.2" fill="currentColor" />
        </>
      )}
      {view === "snapshots" && (
        <path
          fill="currentColor"
          fillRule="evenodd"
          clipRule="evenodd"
          d="M12 2.6a9.4 9.4 0 1 0 0 18.8 9.4 9.4 0 0 0 0-18.8Zm-.85 4.3h1.7v5.2l3.5 2.05-.85 1.45-4.35-2.55Z"
        />
      )}
      {view === "conflicts" && (
        <>
          {/* Two divergent revisions converging into one local head: a merge,
              not a trophy. Stroked connectors at a 2.4 weight keep the ink as
              solid as the filled glyphs beside it and stay crisp at the 18px
              badge size, where hand-built filled diagonals turn to mush. */}
          <g
            fill="none"
            stroke="currentColor"
            strokeWidth="2.4"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M6.5 6.0 12 11.6 17.5 6.0" />
            <path d="M12 11.6v5.1" />
          </g>
          <circle cx="6.5" cy="4.5" r="2.5" fill="currentColor" />
          <circle cx="17.5" cy="4.5" r="2.5" fill="currentColor" />
          <circle cx="12" cy="19.3" r="2.8" fill="currentColor" />
        </>
      )}
      {view === "cloud" && (
        <path
          fill="currentColor"
          d="M7.2 18.4h9.6a3.9 3.9 0 0 0 .5-7.8 5.6 5.6 0 0 0-10.8-1.2 4.5 4.5 0 0 0 .7 9Z"
        />
      )}
      {view === "settings" && (
        <path
          fill="currentColor"
          fillRule="evenodd"
          clipRule="evenodd"
          d="M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58c.18-.14.23-.41.12-.61l-1.92-3.32c-.12-.22-.37-.29-.59-.22l-2.39.96c-.5-.38-1.03-.7-1.62-.94L14.4 2.81c-.04-.24-.24-.41-.48-.41h-3.84c-.24 0-.43.17-.47.41L9.25 5.35c-.59.24-1.13.57-1.62.94l-2.39-.96c-.22-.08-.47 0-.59.22L2.74 8.87c-.12.21-.08.47.12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58c-.18.14-.23.41-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32c.12-.22.07-.47-.12-.61l-2.01-1.58ZM12 15.6a3.6 3.6 0 1 1 0-7.2 3.6 3.6 0 0 1 0 7.2Z"
        />
      )}
    </svg>
  );
}

/** Nav destinations, in display order. Every one reaches real content. */
const NAV_ITEMS: readonly { view: View; label: string }[] = [
  { view: "library", label: "Library" },
  { view: "snapshots", label: "Snapshots" },
  { view: "conflicts", label: "Conflicts" },
  { view: "cloud", label: "Cloud" },
  { view: "settings", label: "Settings" },
];

export default function App() {
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [games, setGames] = useState<Game[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [revisions, setRevisions] = useState<Revision[]>([]);
  const [conflicts, setConflicts] = useState<Conflict[]>([]);
  const [drive, setDrive] = useState<GoogleStatus>({
    client_id: null,
    connected: false,
    pending: false,
    error: null,
  });
  /**
   * Active remote provider. Google stays the default so the pre-existing Drive
   * flow is unchanged for anyone who never opens the provider picker.
   */
  const [provider, setProvider] = useState<ProviderKind>("google");
  const [dropbox, setDropbox] = useState<ProviderStatus>({
    kind: "dropbox",
    identifier: null,
    configured: false,
    connected: false,
    pending: false,
    error: null,
  });
  const [webdav, setWebdav] = useState<ProviderStatus>({
    kind: "webdav",
    identifier: null,
    configured: false,
    connected: false,
    pending: false,
    error: null,
  });
  const [dropboxAppKeyDraft, setDropboxAppKeyDraft] = useState("");
  const [webdavEndpointDraft, setWebdavEndpointDraft] = useState("");
  const [webdavUsernameDraft, setWebdavUsernameDraft] = useState("");
  /**
   * The WebDAV password is read off the DOM at submit time and never copied
   * into React state, so it cannot end up in a render, an error, or a log.
   */
  const webdavPasswordRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [menuPosition, setMenuPosition] = useState<{ x: number; y: number } | null>(null);
  const [unlinkConfirming, setUnlinkConfirming] = useState(false);
  /** Game awaiting "Remove from library" confirmation. */
  const [pendingUnlinkId, setPendingUnlinkId] = useState<string | null>(null);

  const [title, setTitle] = useState("");
  const [savePath, setSavePath] = useState("");
  const [adapter, setAdapter] = useState("custom_directory");
  const [snapshotMessage, setSnapshotMessage] = useState("");
  const [clientIdDraft, setClientIdDraft] = useState("");
  const [authUrl, setAuthUrl] = useState<string | null>(null);
  /**
   * The Dropbox consent URL from the last start response. Kept so the manual
   * fallback is reachable even when the automatic browser launch failed
   * silently in Rust.
   */
  const [dropboxAuthUrl, setDropboxAuthUrl] = useState<string | null>(null);
  /** Flag only: the SteamGridDB key lives in the OS keyring, never in the UI. */
  const [artworkConfigured, setArtworkConfigured] = useState(false);
  const [steamGridKeyDraft, setSteamGridKeyDraft] = useState("");
  /**
   * ScreenScraper credential *presence*, never a value. The panel prefills from
   * these three flags alone, so a stored credential cannot reach the DOM.
   */
  const [screenscraper, setScreenscraper] = useState<ScreenScraperStatus>({
    configured: false,
    dev_configured: false,
    user_configured: false,
  });
  /** Non-secret identifiers, held in state the same way a username is. */
  const [ssDevIdDraft, setSsDevIdDraft] = useState("");
  const [ssSsidDraft, setSsSsidDraft] = useState("");
  /**
   * Both ScreenScraper passwords are read off the DOM at submit time and never
   * copied into React state, so they cannot be rendered, logged, or surfaced in
   * an error line.
   */
  const ssDevPasswordRef = useRef<HTMLInputElement>(null);
  const ssUserPasswordRef = useRef<HTMLInputElement>(null);
  /** Cached metadata for the open game; null means none has been fetched yet. */
  const [metadata, setMetadata] = useState<GameMetadata | null>(null);
  /** Whether the long description is expanded in the detail panel. */
  const [metadataExpanded, setMetadataExpanded] = useState(false);
  /**
   * Resolved `savesync-cover:` URLs, keyed by game id. Only games with a cached
   * cover appear here; everything else keeps the generated monogram.
   */
  const [coverUrls, setCoverUrls] = useState<Record<string, string>>({});
  /**
   * Resolved wide (`?kind=wide`) cover URLs, kept apart from the portrait map
   * because the featured tile never falls back from one shape to the other.
   */
  const [wideCoverUrls, setWideCoverUrls] = useState<Record<string, string>>({});
  /** Open "Choose cover art" modal, or null when it is closed. */
  const [coverPicker, setCoverPicker] = useState<CoverPickerState | null>(null);
  /**
   * Guards against a second `set_cover_grid` while the first is in flight: a
   * state update is asynchronous, so two clicks in one tick would otherwise
   * both pass the disabled check and download twice.
   */
  const coverPickInFlight = useRef(false);
  /** The thumbnail grid, so a pick can keep keyboard focus inside the dialog. */
  const coverGridRef = useRef<HTMLDivElement>(null);

  const titleInputRef = useRef<HTMLInputElement>(null);
  const [dirtyGames, setDirtyGames] = useState<Set<string>>(new Set());

  const [theme, setTheme] = useState<Theme>(initialTheme());
  const [dialogState, setDialogState] = useState<DialogState>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuTriggerRef = useRef<HTMLElement | null>(null);
  const [menuFocusIndex, setMenuFocusIndex] = useState<number>(0);
  const [menuGameId, setMenuGameId] = useState<string | null>(null);
  const [view, setView] = useState<View>("library");
  const [enrollOpen, setEnrollOpen] = useState(false);
  /**
   * Selection at the moment the enroll dialog opened. `enroll` selects the new
   * game on success (and leaves the selection alone on failure), so a change
   * here is the signal that the dialog can close.
   */
  const selectionAtEnrollOpen = useRef<string | null>(null);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      window.localStorage.setItem("savesync-theme", theme);
    } catch {
      // Silently ignore storage errors
    }
  }, [theme]);

  const toggleTheme = useCallback(() => {
    setTheme((current) => current === "light" ? "dark" : "light");
  }, []);

  const handleMenuKeyDown = useCallback((event: React.KeyboardEvent) => {
    if (!menuPosition) return;

    const items = menuRef.current?.querySelectorAll<HTMLButtonElement>("[role='menuitem']") ?? [];
    const count = items.length;

    if (event.key === "Escape") {
      setMenuPosition(null);
      setMenuGameId(null);
      menuTriggerRef.current?.focus();
      menuTriggerRef.current = null;
      return;
    }

    // Tabbing out of a menu dismisses it, as native menus do. Focus is left to
    // the browser so it lands on the next control in the page order.
    if (event.key === "Tab") {
      setMenuPosition(null);
      setMenuGameId(null);
      menuTriggerRef.current = null;
      return;
    }

    const getNextEnabled = (start: number, dir: 1 | -1): number => {
      let i = start;
      for (let step = 0; step < count; step++) {
        i = (i + dir + count) % count;
        if (!items[i].disabled) return i;
      }
      return start; // all disabled
    };

    if (event.key === "ArrowDown") {
      event.preventDefault();
      setMenuFocusIndex((current) => getNextEnabled(current, 1));
    }

    if (event.key === "ArrowUp") {
      event.preventDefault();
      setMenuFocusIndex((current) => getNextEnabled(current, -1));
    }

    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      (items[menuFocusIndex] as HTMLElement)?.click();
    }
  }, [menuPosition]);

  useEffect(() => {
    if (menuPosition) {
      const items = menuRef.current?.querySelectorAll("[role='menuitem']") ?? [];
      (items[menuFocusIndex] as HTMLElement)?.focus();
    }
  }, [menuPosition, menuFocusIndex]);

  // Click-outside dismissal
  useEffect(() => {
    if (!menuPosition) return;
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuPosition(null);
        setMenuGameId(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [menuPosition]);

  const selectedGame = useMemo(
    () => games.find((game) => game.id === selectedId) ?? null,
    [games, selectedId],
  );

  /** Display name of the active provider, used verbatim in labels and titles. */
  const providerHeading =
    provider === "google" ? "Google Drive" : provider === "dropbox" ? "Dropbox" : "WebDAV";
  /**
   * Whether sync can run. Local snapshots and restores never consult this:
   * cloud sync is optional and manual.
   */
  const activeConnected =
    provider === "google"
      ? drive.connected
      : provider === "dropbox"
        ? dropbox.connected
        : webdav.connected;
  const connectHint = `Connect ${providerHeading} first`;
  const syncLabel = `Syncing with ${providerHeading}`;

  const run = useCallback(
    async <T,>(label: string, action: () => Promise<T>): Promise<T | null> => {
      setBusy(label);
      setError(null);
      setNotice(null);
      try {
        return await action();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
        return null;
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  /**
   * Resolve cover URLs for games that actually have a cached cover.
   *
   * `cover_url` is never invoked for a game whose `cover_path` is null, so the
   * common case (no artwork fetched yet) costs zero IPC. Wide URLs are resolved
   * only for the featured game with a cached wide cover. A per-game failure is
   * swallowed: a missing cover is not worth an error banner, and the tile
   * simply keeps its generated monogram.
   *
   * The cache-busting query is derived from `updated_at`, which the engine bumps
   * on every cover mutation, so a re-fetched cover gets a new URL. It is
   * appended with `&` when the URL already carries `?kind=wide`, because the
   * scheme handler parses the first `?` only.
   */
  const loadCoverUrls = useCallback(async (list: Game[]) => {
    const withPortrait = list.filter((game) => game.cover_path);
    const withWide = list.filter((game) => game.featured && game.wide_cover_path);
    const resolve = async (entries: Game[], kind: CoverKind = "portrait") => {
      const resolved = await Promise.all(
        entries.map(async (game) => {
          try {
            const url = await api.coverUrl(game.id, kind);
            if (!url) return null;
            const separator = url.includes("?") ? "&" : "?";
            return [
              game.id,
              `${url}${separator}v=${encodeURIComponent(game.updated_at)}`,
            ] as const;
          } catch {
            return null;
          }
        }),
      );
      const next: Record<string, string> = {};
      for (const entry of resolved) {
        if (entry) next[entry[0]] = entry[1];
      }
      return next;
    };

    const [portrait, wide] = await Promise.all([
      resolve(withPortrait),
      resolve(withWide, "wide"),
    ]);
    setCoverUrls(portrait);
    setWideCoverUrls(wide);
  }, []);

  const loadGames = useCallback(async () => {
    try {
      const list = await api.listGames();
      setGames(list);
      setSelectedId((current) =>
        current && list.some((game) => game.id === current) ? current : null,
      );
      // Not awaited: a cover URL never gates the library rendering.
      void loadCoverUrls(list);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [loadCoverUrls]);

  const loadGameDetails = useCallback(async (gameId: string) => {
    try {
      const [nextRevisions, nextConflicts] = await Promise.all([
        api.history(gameId),
        api.conflicts(gameId),
      ]);
      setRevisions(nextRevisions);
      setConflicts(nextConflicts);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  useEffect(() => {
    if (!isDesktop()) {
      setError(
        "This page is running in a browser. Start the desktop shell with `npm run tauri dev`.",
      );
      return;
    }
    void (async () => {
      try {
        const [nextStatus, nextDrive, nextProvider, nextDropbox, nextWebdav] =
          await Promise.all([
            api.status(),
            api.googleStatus(),
            api.activeProvider(),
            api.dropboxStatus(),
            api.webdavStatus(),
          ]);
        setStatus(nextStatus);
        setDrive(nextDrive);
        setClientIdDraft(nextDrive.client_id ?? "");
        // The persisted provider choice is authoritative: `sync_game` on the
        // Rust side dispatches on the same value.
        setProvider(nextProvider);
        setDropbox(nextDropbox);
        setDropboxAppKeyDraft(nextDropbox.identifier ?? "");
        setWebdav(nextWebdav);
        setWebdavEndpointDraft(nextWebdav.identifier ?? "");
        await loadGames();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    })();
  }, [loadGames]);

  useEffect(() => {
    if (selectedId) void loadGameDetails(selectedId);
    else {
      setRevisions([]);
      setConflicts([]);
    }
  }, [selectedId, loadGameDetails]);

  /**
   * Read the cached metadata when a game's detail view opens.
   *
   * `game_metadata` never makes a network call, so this stays offline-safe and
   * cannot delay the snapshot, restore, or sync panels. A read failure is
   * deliberately swallowed: metadata is optional chrome, and leaving the panel
   * empty keeps the manual fetch button (and its error banner) as the retry.
   */
  useEffect(() => {
    setMetadataExpanded(false);
    // A different game must never show the previous game's catalogue entry, not
    // even in the frame before the cached read resolves.
    setMetadata(null);
    if (!selectedId) return;
    let cancelled = false;
    void api
      .gameMetadata(selectedId)
      .then((next) => {
        if (!cancelled) setMetadata(next);
      })
      .catch(() => {
        if (!cancelled) setMetadata(null);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedId]);

  useEffect(() => {
    if (!drive.pending) return;
    const timer = window.setInterval(() => {
      void api
        .googleStatus()
        .then((next) => {
          setDrive(next);
          if (next.connected && !next.pending) {
            setNotice("Google Drive connected.");
            setAuthUrl(null);
          }
        })
        .catch((cause) => setError(String(cause)));
    }, 1200);
    return () => window.clearInterval(timer);
  }, [drive.pending]);

  useEffect(() => {
    if (!dropbox.pending) return;
    const timer = window.setInterval(() => {
      void api
        .dropboxStatus()
        .then((next) => {
          setDropbox(next);
          if (next.connected && !next.pending) {
            setNotice("Dropbox connected.");
          }
        })
        .catch((cause) => setError(String(cause)));
    }, 1200);
    return () => window.clearInterval(timer);
  }, [dropbox.pending]);

  useEffect(() => {
    function handleDocumentClick() {
      setMenuPosition(null);
    }
    document.addEventListener("click", handleDocumentClick);
    return () => document.removeEventListener("click", handleDocumentClick);
  }, []);

  useEffect(() => {
    if (!isDesktop()) return;

    let unlisten: (() => void) | null = null;

    async function init() {
      try {
        const initial = await api.listDirtyGames();
        setDirtyGames(new Set(initial));

        unlisten = await api.listenDirtyEvent((event: DirtyEvent) => {
          setDirtyGames((prev) => {
            const next = new Set(prev);
            next.add(event.game_id);
            return next;
          });
        });
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    }

    void init();

    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  /**
   * Credential presence for both metadata/artwork providers is fetched when the
   * Settings view opens, not at startup: the library and restore flows never
   * need it, so it must not add work (or a keyring read) to application startup.
   */
  useEffect(() => {
    if (!isDesktop()) return;
    if (view !== "settings") {
      // Do not keep a typed secret around in memory after leaving Settings. The
      // fields themselves unmount with this view, so the DOM-held passwords go
      // with them; the drafts are cleared so the next open starts blank too.
      setSteamGridKeyDraft("");
      setSsDevIdDraft("");
      setSsSsidDraft("");
      return;
    }
    let cancelled = false;
    void Promise.all([api.steamgriddbStatus(), api.screenscraperStatus()])
      .then(([artwork, credentials]) => {
        if (cancelled) return;
        setArtworkConfigured(artwork.configured);
        setScreenscraper(credentials);
      })
      .catch((cause) => {
        if (!cancelled) {
          setError(cause instanceof Error ? cause.message : String(cause));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [view]);

  async function enroll(event: FormEvent) {
    event.preventDefault();
    if (!title.trim() || !savePath.trim()) {
      setError("Enter a title and an absolute save folder path.");
      return;
    }
    const game = await run("Enrolling game", () =>
      api.enrollGame(title.trim(), adapter, savePath.trim()),
    );
    if (game) {
      playCartridgeSound("insert");
      setTitle("");
      setSavePath("");
      setNotice(`Enrolled “${game.title}”. Create its first snapshot to begin versioning.`);
      await loadGames();
      setSelectedId(game.id);
    }
  }

  async function takeSnapshot(event: FormEvent) {
    event.preventDefault();
    if (!selectedGame) return;
    const result = await run("Creating snapshot", () =>
      api.snapshot(selectedGame.id, snapshotMessage),
    );
    if (result) {
      setSnapshotMessage("");
      setNotice(
        `Snapshot saved: ${result.file_count} file(s), ${formatBytes(result.total_bytes)}.`,
      );
      await loadGames();
      await loadGameDetails(selectedGame.id);
    }
  }

  async function takeSnapshotFromMenu(gameId: string) {
    const result = await run("Creating snapshot", () =>
      api.snapshot(gameId, ""),
    );
    if (result) {
      setNotice(
        `Snapshot saved: ${result.file_count} file(s), ${formatBytes(result.total_bytes)}.`,
      );
      await loadGames();
      // Only the open detail view owns `revisions`; refreshing it for a game the
      // user has not opened would show one game's history under another's name.
      if (selectedId === gameId) await loadGameDetails(gameId);
    }
  }

  async function syncGame(gameId: string) {
    const report: SyncReport | null = await run(syncLabel, () =>
      api.sync(gameId),
    );
    if (report) {
      setNotice(
        `Sync complete: ${report.pulled_revisions} pulled, ${report.pushed_revisions} pushed, ${report.conflict_ids.length} conflict(s).`,
      );
      await loadGames();
      if (selectedId === gameId) await loadGameDetails(gameId);
    }
  }

  /**
   * Fetch and cache cover art for one game, then show it immediately.
   *
   * A failed fetch leaves `games` and `coverUrls` untouched, so the existing
   * monogram (or a previously cached cover) stays in place and the error is
   * reported through `run`. The follow-up `cover_url` call is best-effort: a
   * failure there must not escape the click handler.
   */
  const fetchCoverArt = useCallback(
    async (gameId: string) => {
      const updated = await run("Fetching cover art", () => api.fetchCoverArt(gameId));
      if (!updated) return;
      setGames((prev) => prev.map((game) => (game.id === updated.id ? updated : game)));
      let url: string | null = null;
      try {
        url = await api.coverUrl(updated.id);
      } catch {
        url = null;
      }
      if (url) {
        setCoverUrls((prev) => ({ ...prev, [updated.id]: `${url}?v=${Date.now()}` }));
      }
    },
    [run],
  );

  /**
   * Load (or reload) SteamGridDB candidates for one game and shape.
   *
   * Exactly one search per call and nothing is downloaded. The response is
   * dropped unless it still matches both the dialog's game and its current
   * shape, so a slow request can never resurrect a dismissed modal or overwrite
   * the other variant's grid.
   */
  const loadCoverCandidates = useCallback(async (gameId: string, kind: CoverKind) => {
    try {
      const search = await api.searchCoverCandidates(gameId, kind);
      setCoverPicker((current) =>
        current && current.gameId === gameId && current.kind === kind
          ? {
              ...current,
              status: "ready",
              candidates: search?.candidates ?? [],
              error: null,
            }
          : current,
      );
    } catch (cause) {
      setCoverPicker((current) =>
        current && current.gameId === gameId && current.kind === kind
          ? {
              ...current,
              status: "error",
              candidates: [],
              error: coverPickerMessage(cause),
            }
          : current,
      );
    }
  }, []);

  /**
   * Open the cover picker for one game.
   *
   * Portrait is the default so the menu entry behaves exactly as before; the
   * dialog's Portrait | Wide toggle switches shape afterwards.
   */
  const openCoverPicker = useCallback(
    async (gameId: string, kind: CoverKind = "portrait") => {
      const game = games.find((candidate) => candidate.id === gameId);
      setCoverPicker({
        gameId,
        title: game?.title ?? "This game",
        kind,
        status: "loading",
        candidates: [],
        error: null,
        pickingId: null,
        failedIds: [],
      });
      await loadCoverCandidates(gameId, kind);
    },
    [games, loadCoverCandidates],
  );

  /**
   * Switch the grid between portrait and wide art without closing the dialog.
   *
   * A pick in flight wins: the toggle is disabled meanwhile, and this guard
   * keeps a programmatic call from discarding the candidate being downloaded.
   */
  const switchCoverKind = useCallback(
    (kind: CoverKind) => {
      const picker = coverPicker;
      if (!picker || picker.kind === kind || picker.pickingId !== null) return;
      setCoverPicker({
        ...picker,
        kind,
        status: "loading",
        candidates: [],
        error: null,
        pickingId: null,
        failedIds: [],
      });
      void loadCoverCandidates(picker.gameId, kind);
    },
    [coverPicker, loadCoverCandidates],
  );

  const retryCoverSearch = useCallback(() => {
    if (coverPicker) void openCoverPicker(coverPicker.gameId, coverPicker.kind);
  }, [coverPicker, openCoverPicker]);

  const closeCoverPicker = useCallback(() => {
    coverPickInFlight.current = false;
    setCoverPicker(null);
  }, []);

  /**
   * Download and record one chosen candidate, refresh that game's cover URL,
   * then close the modal.
   *
   * Only the chosen grid is downloaded; thumbnails are CDN links rendered by
   * the webview. A failure leaves the picker open with a friendly message and
   * offers a fresh search rather than retrying a grid that may be gone.
   */
  const chooseCoverGrid = useCallback(
    async (candidate: GridCandidate) => {
      const picker = coverPicker;
      if (!picker || picker.status !== "ready" || coverPickInFlight.current) return;
      // A tile whose image failed is not pickable, even if something invokes
      // this directly instead of going through its disabled button.
      if (picker.failedIds.includes(candidate.id)) return;
      coverPickInFlight.current = true;
      setCoverPicker((current) =>
        current ? { ...current, pickingId: candidate.id, error: null } : current,
      );
      try {
        const updated = await api.setCoverGrid(picker.gameId, candidate.id, picker.kind);
        setGames((prev) =>
          prev.map((game) => (game.id === updated.id ? updated : game)),
        );
        let url: string | null = null;
        // Only the portrait cache feeds the library tile's 2:3 art box, so a
        // wide pick must not replace it with a banner-shaped image.
        if (picker.kind === "portrait") {
          try {
            url = await api.coverUrl(updated.id);
          } catch {
            url = null;
          }
        }
        if (url) {
          setCoverUrls((prev) => ({ ...prev, [updated.id]: `${url}?v=${Date.now()}` }));
        }
        setCoverPicker(null);
      } catch (cause) {
        setCoverPicker((current) =>
          current
            ? {
                ...current,
                status: "error",
                candidates: [],
                pickingId: null,
                error: coverPickerMessage(cause),
              }
            : current,
        );
      } finally {
        coverPickInFlight.current = false;
      }
    },
    [coverPicker],
  );

  const pickingCoverId = coverPicker?.pickingId ?? null;

  /**
   * Keep keyboard focus inside the dialog while a pick is in flight.
   *
   * Every thumbnail except the one being picked is disabled, so focus sitting
   * on a sibling (possible through programmatic focus, or a browser that does
   * not focus a button it was clicked on) would otherwise drop to
   * `document.body` and strand keyboard users. The picking tile stays enabled
   * and already carries the busy state, and `coverPickInFlight` makes a second
   * activation a no-op, so it is safe for it to hold focus.
   */
  useEffect(() => {
    if (pickingCoverId === null) return;
    coverGridRef.current
      ?.querySelector<HTMLButtonElement>(`[data-cover-id="${pickingCoverId}"]`)
      ?.focus();
  }, [pickingCoverId]);

  async function requestRestore(revision: Revision, exactMirror: boolean) {
    setDialogState({ kind: "restore", revision, exactMirror });
  }

  async function requestResolve(conflict: Conflict, revisionId: string) {
    setDialogState({ kind: "conflict", conflict, revisionId });
  }

  async function confirmRestore(revision: Revision, exactMirror: boolean) {
    const report = await run("Restoring snapshot", () =>
      api.restore(revision.id, exactMirror),
    );
    if (report) {
      setNotice(
        `Restored ${report.files_restored} file(s). Safety backup: ${report.safety_backup_path}`,
      );
      if (selectedGame) await loadGames();
    }
  }

  async function confirmResolve(conflict: Conflict, revisionId: string) {
    const resolution = await run("Resolving conflict", () =>
      api.resolve(conflict.id, revisionId),
    );
    if (resolution) {
      setNotice(
        `Conflict resolved into new revision ${shortId(resolution.id)}. Restore it to apply it locally.`,
      );
      if (selectedGame) {
        await loadGames();
        await loadGameDetails(selectedGame.id);
      }
    }
  }

  async function requestDisconnect() {
    setDialogState({ kind: "disconnect" });
  }

  async function confirmDisconnect() {
    const result = await run("Disconnecting Google Drive", () =>
      api.disconnectGoogle(),
    );
    if (result) {
      setDrive(result);
      setAuthUrl(null);
      setNotice("Google Drive disconnected.");
    }
  }

  async function saveClientId(event: FormEvent) {
    event.preventDefault();
    const result = await run("Saving client id", () =>
      api.setGoogleClientId(clientIdDraft),
    );
    if (result) {
      setDrive(result);
      setNotice("Google OAuth client id saved.");
    }
  }

  /**
   * Persist the SteamGridDB API key through the engine. Neither the key nor any
   * fragment of it is ever placed in an error, notice, or log line: the backend
   * returns a generic message and the success text is a fixed string.
   */
  async function saveSteamGridKey(event: FormEvent) {
    event.preventDefault();
    const key = steamGridKeyDraft.trim();
    if (!key) {
      setError("Enter a SteamGridDB API key.");
      return;
    }
    const next = await run("Saving SteamGridDB API key", () =>
      api.setSteamgriddbApiKey(key),
    );
    if (next) {
      setArtworkConfigured(next.configured);
      // Drop the secret from component state as soon as it is persisted.
      setSteamGridKeyDraft("");
      setNotice(
        "SteamGridDB API key saved in the OS keyring. It is used for cover-art lookups only.",
      );
    }
  }

  async function removeSteamGridKey() {
    const next = await run("Removing SteamGridDB API key", () =>
      api.clearSteamgriddbApiKey(),
    );
    if (next) {
      setArtworkConfigured(next.configured);
      setSteamGridKeyDraft("");
      setNotice("SteamGridDB API key removed from the OS keyring.");
    }
  }

  /**
   * Store the four ScreenScraper values in the OS keyring.
   *
   * A blank field keeps the stored value, which is what lets someone change one
   * credential without retyping the rest. The backend owns the friendly text for
   * an incomplete pair ("enter both the user SSID and …"), so nothing is
   * re-implemented and no value is echoed into the banner.
   */
  async function saveScreenscraperCredentials(event: FormEvent) {
    event.preventDefault();
    const next = await run("Saving ScreenScraper credentials", () =>
      api.setScreenscraperCredentials(
        ssDevIdDraft.trim(),
        ssDevPasswordRef.current?.value ?? "",
        ssSsidDraft.trim(),
        ssUserPasswordRef.current?.value ?? "",
      ),
    );
    if (next) {
      setScreenscraper(next);
      // Presence flags now describe the stored set, so the drafts go back to
      // placeholders rather than showing a value the UI never received.
      setSsDevIdDraft("");
      setSsSsidDraft("");
      if (ssDevPasswordRef.current) ssDevPasswordRef.current.value = "";
      if (ssUserPasswordRef.current) ssUserPasswordRef.current.value = "";
      setNotice("ScreenScraper credentials saved in the OS keyring.");
    }
  }

  async function clearScreenscraperCredentials() {
    const next = await run("Clearing ScreenScraper credentials", () =>
      api.clearScreenscraperCredentials(),
    );
    if (next) {
      setScreenscraper(next);
      setSsDevIdDraft("");
      setSsSsidDraft("");
      if (ssDevPasswordRef.current) ssDevPasswordRef.current.value = "";
      if (ssUserPasswordRef.current) ssUserPasswordRef.current.value = "";
      setNotice("ScreenScraper credentials removed from the OS keyring.");
    }
  }

  /**
   * Fetch metadata for the open game. Manual only: this is never called from an
   * effect, so opening a detail view cannot touch the network, and a failure
   * leaves the cached panel exactly as it was.
   */
  async function fetchGameMetadata(game: Game) {
    const next = await run(METADATA_BUSY, () => api.fetchGameMetadata(game.id));
    if (next) {
      setMetadata(next);
      setMetadataExpanded(false);
      setNotice(`Metadata fetched from ${METADATA_SOURCE_LABELS[next.source]}.`);
    }
  }

  async function connectDrive() {
    const started = await run("Starting Google sign-in", () =>
      api.beginGoogleOAuth(),
    );
    if (started) {
      setAuthUrl(started.auth_url);
      setNotice(
        "A browser window was opened. Finish sign-in there; SaveSync will update automatically.",
      );
      try {
        setDrive(await api.googleStatus());
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    }
  }

  async function disconnectDrive() {
    void requestDisconnect();
  }

  /**
   * Switch the provider both in the UI and in Rust.
   *
   * The UI follows the persisted value rather than optimistically diverging:
   * `sync_game` dispatches on the Rust-side choice, so a failed switch must not
   * leave the panel claiming a provider that sync will not use.
   */
  async function chooseProvider(kind: ProviderKind) {
    if (kind === provider) return;
    const next = await run("Saving cloud provider", () =>
      api.setActiveProvider(kind),
    );
    if (next) setProvider(next.kind);
  }

  async function saveDropboxAppKey(event: FormEvent) {
    event.preventDefault();
    const next = await run("Saving Dropbox app key", () =>
      api.setDropboxAppKey(dropboxAppKeyDraft),
    );
    if (next) {
      setDropbox(next);
      setNotice("Dropbox app key saved.");
    }
  }

  async function connectDropbox() {
    const started = await run("Starting Dropbox sign-in", () =>
      api.beginDropboxOAuth(),
    );
    if (started) {
      setDropboxAuthUrl(started.auth_url);
      setNotice(
        "A browser window was opened. Finish Dropbox sign-in there; SaveSync will update automatically.",
      );
      try {
        setDropbox(await api.dropboxStatus());
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    }
  }

  async function disconnectDropboxProvider() {
    const next = await run("Disconnecting Dropbox", () =>
      api.disconnectDropbox(),
    );
    if (next) {
      setDropbox(next);
      setNotice("Dropbox disconnected.");
    }
  }

  /**
   * Persist the WebDAV endpoint and username, and hand the password straight to
   * the command. The password is read from the DOM and never stored in React
   * state, so it cannot be rendered, logged, or carried into a notice.
   */
  async function saveWebdavConfig(event: FormEvent) {
    event.preventDefault();
    const endpoint = webdavEndpointDraft.trim();
    if (!endpoint) {
      setError("Enter the WebDAV endpoint.");
      return;
    }
    const password = webdavPasswordRef.current?.value ?? "";
    const next = await run("Saving WebDAV settings", () =>
      api.setWebdavConfig(endpoint, webdavUsernameDraft.trim(), password),
    );
    if (next) {
      setWebdav(next);
      setWebdavEndpointDraft(next.identifier ?? endpoint);
      // Drop the secret from the DOM as soon as the command has accepted it.
      if (webdavPasswordRef.current) webdavPasswordRef.current.value = "";
      setNotice(
        next.connected
          ? "WebDAV connected. The password is stored in the OS keyring."
          : "WebDAV settings saved.",
      );
    }
  }

  async function disconnectWebdavProvider() {
    const next = await run("Disconnecting WebDAV", () =>
      api.disconnectWebdav(),
    );
    if (next) {
      setWebdav(next);
      setNotice("WebDAV disconnected.");
    }
  }

  async function syncSelected() {
    if (!selectedGame) return;
    const report: SyncReport | null = await run(syncLabel, () =>
      api.sync(selectedGame.id),
    );
    if (report) {
      setNotice(
        `Sync complete: ${report.pulled_revisions} pulled, ${report.pushed_revisions} pushed, ${report.conflict_ids.length} conflict(s).`,
      );
      await loadGames();
      await loadGameDetails(selectedGame.id);
    }
  }

  /**
   * Open the game context menu at a point, without navigating.
   *
   * Right-clicking a tile is a request for that game's actions, not a request
   * to leave the library: `selectedId` (which drives the detail view) is left
   * untouched here and only changes when "Open details" is chosen.
   */
  const openMenuAt = useCallback(
    (point: { x: number; y: number }, gameId: string, trigger: HTMLElement) => {
      setMenuGameId(gameId);
      const topbarEl = document.querySelector<HTMLElement>(".topbar");
      const topInset = topbarEl
        ? topbarEl.getBoundingClientRect().bottom + 8
        : TOPBAR_FALLBACK_HEIGHT;
      // The docked band owns the bottom edge, so the usable area ends at its top
      // edge. It is passed as a bottom inset, not a shorter viewport: on a short
      // window a shrunken viewport collapses the floor and ceiling onto the top
      // bar and pushes the menu through the band. An unmeasured layout (jsdom, or
      // a band that has not laid out yet) reports 0 and contributes no inset.
      const dockTop =
        document.querySelector<HTMLElement>(".dock")?.getBoundingClientRect().top ?? 0;
      const bottomInset = dockTop > 0 ? window.innerHeight - dockTop : 0;
      const pos = clampMenuPosition(
        point,
        MENU_SIZE,
        { width: window.innerWidth, height: window.innerHeight },
        topInset,
        bottomInset,
      );
      setMenuPosition(pos);
      // Store the triggering element so Escape can return focus to it.
      menuTriggerRef.current = trigger;
      // Focus the first enabled menu item after render.
      requestAnimationFrame(() => {
        const items = menuRef.current?.querySelectorAll<HTMLButtonElement>("[role='menuitem']");
        if (!items) return;
        const first = Array.from(items).findIndex((el) => !el.disabled);
        if (first >= 0) {
          items[first].focus();
          setMenuFocusIndex(first);
        }
      });
    },
    [],
  );

  const openMenu = useCallback(
    (event: React.MouseEvent, gameId: string) => {
      event.preventDefault();
      openMenuAt(
        { x: event.clientX, y: event.clientY },
        gameId,
        event.currentTarget as HTMLElement,
      );
    },
    [openMenuAt],
  );

  /** Keyboard equivalent of right-click: ContextMenu key or Shift+F10. */
  const openMenuFromKeyboard = useCallback(
    (event: React.KeyboardEvent<HTMLElement>, gameId: string) => {
      const isContextKey =
        event.key === "ContextMenu" || (event.shiftKey && event.key === "F10");
      if (!isContextKey) return;
      event.preventDefault();
      const rect = event.currentTarget.getBoundingClientRect();
      openMenuAt(
        { x: rect.left + Math.min(rect.width, 48), y: rect.top + 32 },
        gameId,
        event.currentTarget,
      );
    },
    [openMenuAt],
  );

  const closeMenu = useCallback(() => {
    setMenuPosition(null);
    setMenuGameId(null);
  }, []);

  /**
   * "Restore latest…" from the context menu. The menu can target a game whose
   * history is not loaded (it no longer changes the selection), so fetch that
   * game's revisions on demand instead of reading another game's state.
   */
  const restoreLatest = useCallback(
    async (gameId: string, headRevisionId: string) => {
      const history = await run("Loading snapshots", () => api.history(gameId));
      const revision = history?.find((item) => item.id === headRevisionId);
      if (revision) {
        setDialogState({ kind: "restore", revision, exactMirror: false });
      }
    },
    [run],
  );

  /**
   * Unlink targets the game the menu was opened on, which is not necessarily
   * the selected one: right-clicking never changes the library's selection.
   */
  const handleUnlink = useCallback(async () => {
    const gameId = pendingUnlinkId;
    if (!gameId) return;
    const result = await run("Unlinking game", () => api.unlinkGame(gameId));
    if (result) {
      playCartridgeSound("eject");
      setSelectedId((current) => (current === gameId ? null : current));
      setUnlinkConfirming(false);
      setPendingUnlinkId(null);
      setNotice("Removed from library. Revisions and on-disk backups are preserved.");
      await loadGames();
    }
  }, [loadGames, pendingUnlinkId, run]);

  /**
   * Pin or unpin the library's single featured game.
   *
   * Manual only: enrollment and discovery never promote a game on their own.
   * The banner moves optimistically, then the authoritative list is re-read —
   * which also restores the previous pin if the write fails — and a failure
   * surfaces through the existing error banner (`run`).
   */
  const setFeaturedGame = useCallback(
    async (gameId: string, featured: boolean) => {
      setGames((current) =>
        current.map((game) => ({
          ...game,
          featured: featured && game.id === gameId,
        })),
      );
      const ok = await run("Updating featured game", async () => {
        if (featured) {
          await api.setFeatured(gameId);
        } else {
          await api.clearFeatured();
        }
        return true;
      });
      await loadGames();
      if (ok) {
        setNotice(featured ? "Featured game updated." : "Featured game cleared.");
      }
    },
    [loadGames, run],
  );

  const openUnlinkDialog = useCallback((gameId: string) => {
    setPendingUnlinkId(gameId);
    setUnlinkConfirming(true);
    setMenuPosition(null);
    setMenuGameId(null);
  }, []);

  const closeUnlinkDialog = useCallback(() => {
    setUnlinkConfirming(false);
    setPendingUnlinkId(null);
  }, []);

  const openEnrollDialog = useCallback(() => {
    selectionAtEnrollOpen.current = selectedId;
    setEnrollOpen(true);
  }, [selectedId]);

  const closeEnrollDialog = useCallback(() => {
    setEnrollOpen(false);
  }, []);

  // `enroll` selects the freshly created game on success and does nothing on a
  // failed call, so the dialog closes only once a new selection appears.
  useEffect(() => {
    if (
      enrollOpen &&
      selectedId !== null &&
      selectedId !== selectionAtEnrollOpen.current
    ) {
      setEnrollOpen(false);
    }
  }, [enrollOpen, selectedId]);

  function renderLibraryView() {
    /*
     * The featured game leads the wall as a wide banner; the rest keep their
     * original order and 2:3 shape. `featured` is a manual pin, and the schema
     * enforces at most one, so at most one tile is promoted.
     */
    const featuredGame = games.find((game) => game.featured) ?? null;
    const orderedGames = featuredGame
      ? [featuredGame, ...games.filter((game) => game.id !== featuredGame.id)]
      : games;

    return (
      <section className="view" aria-labelledby="library-title">
        <header className="view-header">
          <div>
            <h2 id="library-title">Game library</h2>
            <p className="sub">
              {games.length} game{games.length === 1 ? "" : "s"} enrolled
            </p>
          </div>
          <button className="primary" type="button" onClick={openEnrollDialog}>
            Enroll software
          </button>
        </header>

        {games.length === 0 ? (
          <div className="empty">
            <h3>No games enrolled yet</h3>
            <p className="muted">
              Use “Enroll software” to add a save folder and create your first
              immutable snapshot.
            </p>
          </div>
        ) : (
          <ul className="tile-grid">
            {orderedGames.map((game) => {
              const isSelected = game.id === selectedId;
              const isDirty = dirtyGames.has(game.id);
              const isFeatured = game.featured;
              return (
                <li
                  key={game.id}
                  className={
                    isFeatured ? "tile-cell tile-cell--featured" : "tile-cell"
                  }
                >
                  <button
                    type="button"
                    className={isFeatured ? "tile tile--featured" : "tile"}
                    aria-current={isSelected ? "true" : undefined}
                    onClick={() => setSelectedId(game.id)}
                    onContextMenu={(event) => openMenu(event, game.id)}
                    onKeyDown={(event) => openMenuFromKeyboard(event, game.id)}
                  >
                    <span className="tile-art">
                      <CoverArt
                        title={game.title}
                        adapter={game.adapter_type}
                        coverUrl={
                          isFeatured
                            ? wideCoverUrls[game.id]
                            : coverUrls[game.id]
                        }
                        wide={isFeatured}
                      />
                    </span>
                    <span className="tile-label">
                      <span className="tile-head">
                        <span className="tile-title">{game.title}</span>
                        {isDirty && (
                          <span
                            className="dirty-badge"
                            aria-label="Uncaptured save changes"
                            title="Uncaptured save changes"
                          />
                        )}
                      </span>
                      <span className="tile-acc">
                        {adapterLabel(game.adapter_type)}
                      </span>
                      <span className="tile-acc">
                        {game.active_head_revision_id
                          ? "Backed up"
                          : "No snapshots yet"}
                      </span>
                    </span>
                    <span className="tile-check" aria-hidden="true">
                      <svg
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="2.4"
                        strokeLinecap="round"
                        strokeLinejoin="round"
                      >
                        <path d="M5 12.8 9.6 17.4 19 8" />
                      </svg>
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </section>
    );
  }

  function renderDetailView(game: Game) {
    /*
     * Catalogue facts, in the order the panel reads them. Only what the provider
     * actually answered is listed: an absent year or an empty credits list is
     * omitted rather than rendered as a placeholder.
     */
    const metadataFacts: { label: string; value: string }[] = [];
    if (metadata) {
      if (metadata.release_year !== null) {
        metadataFacts.push({
          label: "Year",
          value: String(metadata.release_year),
        });
      }
      if (metadata.genres.length > 0) {
        metadataFacts.push({ label: "Genres", value: metadata.genres.join(", ") });
      }
      if (metadata.developers.length > 0) {
        metadataFacts.push({
          label: "Developers",
          value: metadata.developers.join(", "),
        });
      }
      if (metadata.publishers.length > 0) {
        metadataFacts.push({
          label: "Publishers",
          value: metadata.publishers.join(", "),
        });
      }
    }
    const shortDescription = metadata?.description_short?.trim() || null;
    const longDescription = metadata?.description_long?.trim() || null;
    /*
     * The short form is what the panel shows by default. A toggle is only honest
     * when the long form says more than the paragraph already on screen; with two
     * identical strings, or no long form, it would be a control that does
     * nothing.
     */
    const canExpand =
      shortDescription !== null &&
      longDescription !== null &&
      longDescription.length > shortDescription.length;
    const description =
      metadataExpanded && longDescription
        ? longDescription
        : (shortDescription ?? longDescription);

    return (
      <section className="view" aria-labelledby="game-title">
        <button
          className="ghost back"
          type="button"
          onClick={() => {
            setSelectedId(null);
            setView("library");
          }}
        >
          <span aria-hidden="true">←</span> Back to library
        </button>

        <header className="detail-header">
          <div className="detail-cover">
            <CoverArt
              title={game.title}
              adapter={game.adapter_type}
              coverUrl={coverUrls[game.id]}
            />
          </div>
          <div className="detail-copy">
            <h2 id="game-title">{game.title}</h2>
            {dirtyGames.has(game.id) && (
              <span
                className="dirty-badge"
                aria-label="Uncaptured save changes"
                style={{ marginTop: 4 }}
              />
            )}
            <p className="detail-meta">
              {adapterLabel(game.adapter_type)} · {revisions.length} snapshot
              {revisions.length === 1 ? "" : "s"} ·{" "}
              {game.active_head_revision_id
                ? `head ${shortId(game.active_head_revision_id)}`
                : "no active snapshot"}
            </p>
          </div>
          <div className="detail-actions">
            <button
              className="ghost"
              type="button"
              disabled={!!busy}
              title={
                game.featured
                  ? "Remove the wide banner from the library"
                  : "Pin this game as the library's wide banner"
              }
              onClick={() => void setFeaturedGame(game.id, !game.featured)}
            >
              {game.featured ? "Remove from featured" : "Feature on library"}
            </button>
            <button
              className="ghost"
              onClick={() => void syncSelected()}
              disabled={!!busy || !activeConnected}
              title={
                activeConnected
                  ? `Sync revisions with ${providerHeading}`
                  : connectHint
              }
            >
              {busy === syncLabel ? "Syncing…" : "Sync now"}
            </button>
          </div>
        </header>

        <div className="detail-body">
          {conflicts.length > 0 && (
            <section className="panel conflicts">
              <h2>Unresolved conflicts</h2>
              <p className="muted small">
                Two devices advanced from the same snapshot. Nothing is merged
                automatically — choose the snapshot to keep, then restore it if
                you want it applied here.
              </p>
              {conflicts.map((conflict) => (
                <div className="conflict" key={conflict.id}>
                  <p className="tiny muted">
                    Detected {formatDate(conflict.detected_at)}
                  </p>
                  <ul className="conflict-options">
                    {conflict.revision_ids.map((revisionId) => {
                      const revision = revisions.find(
                        (item) => item.id === revisionId,
                      );
                      return (
                        <li key={revisionId}>
                          <div>
                            <strong>{revision?.message || "Untitled snapshot"}</strong>
                            <span className="muted small block">
                              {revision
                                ? formatDate(revision.created_at)
                                : "Unknown revision"}{" "}
                              · {shortId(revisionId)}
                            </span>
                          </div>
                          <button
                            className="primary small"
                            disabled={!!busy}
                            onClick={() => void requestResolve(conflict, revisionId)}
                          >
                            Choose this
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </div>
              ))}
            </section>
          )}

          <section className="panel" aria-labelledby="metadata-title">
            <div className="panel-title">
              <h2 id="metadata-title">Metadata</h2>
              {metadata && (
                <span className="metadata-source">
                  via {METADATA_SOURCE_LABELS[metadata.source]}
                </span>
              )}
            </div>
            {metadata ? (
              <>
                {metadataFacts.length > 0 && (
                  <dl className="metadata-facts">
                    {metadataFacts.map((fact) => (
                      <div className="metadata-fact" key={fact.label}>
                        <dt>{fact.label}</dt>
                        <dd>{fact.value}</dd>
                      </div>
                    ))}
                  </dl>
                )}
                {description && (
                  <div className="metadata-description">
                    <p>{description}</p>
                    {canExpand && (
                      <button
                        className="ghost tiny"
                        type="button"
                        aria-expanded={metadataExpanded}
                        onClick={() => setMetadataExpanded((current) => !current)}
                      >
                        {metadataExpanded ? "Show less" : "Read more"}
                      </button>
                    )}
                  </div>
                )}
              </>
            ) : (
              <p className="muted">
                No metadata yet. Fetch it to add the year, genres, credits, and
                description shown on the store page.
              </p>
            )}
            <div className="row">
              <button
                className="ghost"
                type="button"
                disabled={!!busy}
                title="Look the game up with the configured metadata source"
                onClick={() => void fetchGameMetadata(game)}
              >
                {busy === METADATA_BUSY ? "Fetching…" : "Fetch metadata"}
              </button>
            </div>
          </section>

          <section className="panel">
            <div className="panel-title">
              <h2>New snapshot</h2>
              <span className="muted tiny">SHA-256 verified</span>
            </div>
            <form className="snapshot-form" onSubmit={takeSnapshot}>
              <input
                aria-label="Snapshot label"
                value={snapshotMessage}
                onChange={(event) => setSnapshotMessage(event.target.value)}
                placeholder="Optional label, e.g. before boss fight"
              />
              <button className="primary" type="submit" disabled={!!busy}>
                {busy === "Creating snapshot" ? "Hashing…" : "Create snapshot"}
              </button>
            </form>
          </section>

          <section className="panel">
            <div className="panel-title">
              <h2>History</h2>
              <span className="muted tiny">Newest first</span>
            </div>
            {revisions.length === 0 ? (
              <p className="muted">No snapshots yet.</p>
            ) : (
              <ol className="history">
                {revisions.map((revision) => {
                  const isHead = game.active_head_revision_id === revision.id;
                  return (
                    <li
                      key={revision.id}
                      className={isHead ? "history-item head" : "history-item"}
                    >
                      <span className="history-dot" aria-hidden="true" />
                      <div className="history-body">
                        <div className="row space-between">
                          <div>
                            <h3>{revision.message || "Untitled snapshot"}</h3>
                            <p className="muted small">
                              {formatDate(revision.created_at)} ·{" "}
                              {shortId(revision.id)}
                              {isHead ? " · active head" : ""}
                            </p>
                          </div>
                          <div className="row">
                            <button
                              className="ghost small"
                              disabled={!!busy}
                              onClick={() => void requestRestore(revision, false)}
                              title="Write snapshot files; keep unrelated live files"
                            >
                              Restore
                            </button>
                            <button
                              className="ghost small danger"
                              disabled={!!busy}
                              onClick={() => void requestRestore(revision, true)}
                              title="Make the live folder match this snapshot exactly"
                            >
                              Exact restore
                            </button>
                          </div>
                        </div>
                      </div>
                    </li>
                  );
                })}
              </ol>
            )}
          </section>
        </div>
      </section>
    );
  }

  function renderPickGameView(section: "Snapshots" | "Conflicts") {
    const heading = section === "Snapshots" ? "Snapshot history" : "Conflicts";
    const subject =
      section === "Snapshots" ? "snapshots and history" : "unresolved conflicts";
    return (
      <section className="view" aria-labelledby="pick-game-title">
        <header className="view-header">
          <div>
            <h2 id="pick-game-title">{heading}</h2>
            <p className="sub">
              Open a game from the library to review its {subject}.
            </p>
          </div>
          <button className="primary" type="button" onClick={() => setView("library")}>
            Go to library
          </button>
        </header>
        <div className="empty">
          <h3>No game selected</h3>
          <p className="muted">
            {section} are shown per game, together with its history. Pick one from
            the library.
          </p>
        </div>
      </section>
    );
  }

  function renderCloudView() {
    const activePending =
      provider === "google"
        ? drive.pending
        : provider === "dropbox"
          ? dropbox.pending
          : false;
    const providerError =
      provider === "google"
        ? drive.error
        : provider === "dropbox"
          ? dropbox.error
          : webdav.error;
    return (
      <section className="view" aria-labelledby="cloud-title">
        <header className="view-header">
          <div>
            <h2 id="cloud-title">Cloud sync</h2>
            <p className="sub">
              Optional and manual. Local snapshots and restores work offline.
            </p>
          </div>
        </header>

        <section className="panel drive-panel" aria-labelledby="drive-panel-title">
          <div className="panel-title">
            <h2 id="drive-panel-title">{providerHeading}</h2>
            <span className={activeConnected ? "status on" : "status"}>
              {activeConnected
                ? "Connected"
                : activePending
                  ? "Waiting for sign-in"
                  : "Not connected"}
            </span>
          </div>

          <div className="seg" role="group" aria-label="Cloud provider">
            <button
              type="button"
              aria-pressed={provider === "google"}
              onClick={() => void chooseProvider("google")}
              disabled={!!busy}
            >
              Google Drive
            </button>
            <button
              type="button"
              aria-pressed={provider === "dropbox"}
              onClick={() => void chooseProvider("dropbox")}
              disabled={!!busy}
            >
              Dropbox
            </button>
            <button
              type="button"
              aria-pressed={provider === "webdav"}
              onClick={() => void chooseProvider("webdav")}
              disabled={!!busy}
            >
              WebDAV
            </button>
          </div>

          {provider === "google" && (
            <>
              <p className="muted small">
                {drive.connected
                  ? "Connected. Sync is manual and never overwrites saves."
                  : drive.pending
                    ? "Waiting for the browser sign-in…"
                    : "Optional. Requests only the drive.file scope."}
              </p>

              <form className="stack" onSubmit={saveClientId}>
                <label>
                  OAuth client id
                  <input
                    value={clientIdDraft}
                    onChange={(event) => setClientIdDraft(event.target.value)}
                    aria-describedby="google-client-id-note"
                    placeholder="…apps.googleusercontent.com"
                    spellCheck={false}
                  />
                </label>
                <p className="muted small" id="google-client-id-note">
                  A desktop-app client id comes from the{" "}
                  <DocsLink
                    href="https://console.cloud.google.com/apis/credentials"
                    onError={setError}
                  >
                    Google Cloud console
                  </DocsLink>
                  , and an OAuth consent screen has to be configured there
                  before one can be created. Requests only the drive.file
                  scope.
                </p>
                <button type="submit" disabled={!!busy}>
                  Save client id
                </button>
              </form>
            </>
          )}

          {provider === "dropbox" && (
            <>
              <p className="muted small">
                {dropbox.connected
                  ? "Connected. Sync is manual and never overwrites saves."
                  : dropbox.pending
                    ? "Waiting for the browser sign-in…"
                    : "Optional. Uses app-folder access and a PKCE app key."}
              </p>

              <form className="stack" onSubmit={saveDropboxAppKey}>
                <label>
                  App key
                  <input
                    value={dropboxAppKeyDraft}
                    onChange={(event) => setDropboxAppKeyDraft(event.target.value)}
                    aria-describedby="dropbox-app-key-note"
                    placeholder="Dropbox app key"
                    autoComplete="off"
                    spellCheck={false}
                  />
                </label>
                <p className="muted small" id="dropbox-app-key-note">
                  Create an app in the{" "}
                  <DocsLink
                    href="https://www.dropbox.com/developers/apps"
                    onError={setError}
                  >
                    Dropbox app console
                  </DocsLink>
                  : choose scoped access with the files.content.write and
                  files.content.read permissions, and app-folder access so
                  SaveSync only ever sees its own folder.
                </p>
                <button type="submit" disabled={!!busy}>
                  Save app key
                </button>
              </form>
            </>
          )}

          {provider === "webdav" && (
            <>
              <p className="muted small">
                {webdav.connected
                  ? "Connected. Sync is manual and never overwrites saves."
                  : "Optional. Any HTTPS WebDAV server with Basic authentication."}
              </p>

              <form className="stack" onSubmit={saveWebdavConfig}>
                <label>
                  Endpoint
                  <input
                    value={webdavEndpointDraft}
                    onChange={(event) => setWebdavEndpointDraft(event.target.value)}
                    aria-describedby="webdav-endpoint-note"
                    placeholder="https://dav.example.com/savesync"
                    autoComplete="off"
                    spellCheck={false}
                  />
                </label>
                <p className="muted small" id="webdav-endpoint-note">
                  Any HTTPS WebDAV server with Basic authentication works. With
                  two-factor sign-in enabled on Nextcloud or ownCloud, use an{" "}
                  <DocsLink
                    href="https://docs.nextcloud.com/server/latest/user_manual/en/session_management.html#managing-devices"
                    onError={setError}
                  >
                    app password
                  </DocsLink>
                  .
                </p>
                <label>
                  Username
                  <input
                    value={webdavUsernameDraft}
                    onChange={(event) => setWebdavUsernameDraft(event.target.value)}
                    placeholder={
                      webdav.configured ? "Stored username" : "WebDAV username"
                    }
                    autoComplete="username"
                    spellCheck={false}
                  />
                </label>
                <label>
                  Password
                  {/*
                    Uncontrolled on purpose: the secret is read from the DOM at
                    submit time and never enters React state, so it cannot be
                    rendered, logged, or shown in an error.
                  */}
                  <input
                    ref={webdavPasswordRef}
                    type="password"
                    defaultValue=""
                    aria-describedby="webdav-password-note"
                    placeholder={
                      webdav.configured ? "Stored in the OS keyring" : "WebDAV password"
                    }
                    autoComplete="current-password"
                    spellCheck={false}
                  />
                </label>
                <p className="muted small" id="webdav-password-note">
                  The password is kept in the OS keyring and never written to the
                  SaveSync database or to logs. Leaving the username or password
                  blank keeps the stored value.
                </p>
                <button type="submit" disabled={!!busy}>
                  Save and connect
                </button>
              </form>
            </>
          )}

          {/* Directly under this provider's fields: what failed and what to do
              next, announced as one line. */}
          {providerError && (
            <p className="error small" role="alert">
              {providerErrorText(provider, providerError)}
            </p>
          )}

          <div className="row">
            {provider === "google" &&
              (drive.connected ? (
                <button
                  className="ghost"
                  onClick={() => void disconnectDrive()}
                  disabled={!!busy}
                >
                  Disconnect
                </button>
              ) : (
                <button
                  className="primary"
                  onClick={() => void connectDrive()}
                  disabled={!!busy || drive.pending || !drive.client_id}
                >
                  {drive.pending ? "Waiting…" : "Connect Drive"}
                </button>
              ))}
            {provider === "dropbox" &&
              (dropbox.connected ? (
                <button
                  className="ghost"
                  onClick={() => void disconnectDropboxProvider()}
                  disabled={!!busy}
                >
                  Disconnect
                </button>
              ) : (
                <button
                  className="primary"
                  onClick={() => void connectDropbox()}
                  disabled={!!busy || dropbox.pending || !dropbox.configured}
                >
                  {dropbox.pending ? "Waiting…" : "Connect Dropbox"}
                </button>
              ))}
            {provider === "webdav" && webdav.connected && (
              <button
                className="ghost"
                onClick={() => void disconnectWebdavProvider()}
                disabled={!!busy}
              >
                Disconnect
              </button>
            )}
            {selectedGame ? (
              <button
                className="ghost"
                onClick={() => void syncSelected()}
                disabled={!!busy || !activeConnected}
                title={
                  activeConnected
                    ? `Sync the selected game with ${providerHeading}`
                    : connectHint
                }
              >
                {busy === syncLabel ? "Syncing…" : "Sync now"}
              </button>
            ) : (
              <>
                <span className="muted small">No game selected.</span>
                <button
                  className="ghost"
                  type="button"
                  onClick={() => setView("library")}
                >
                  Go to library
                </button>
              </>
            )}
          </div>

          {provider === "google" && authUrl && (
            <p className="muted tiny break">
              Browser did not open?{" "}
              <button
                className="ghost tiny"
                onClick={() =>
                  void api
                    .openAuthUrl(authUrl)
                    .catch((cause) => setError(String(cause)))
                }
              >
                Open the consent page
              </button>
            </p>
          )}
          {provider === "dropbox" && dropboxAuthUrl && (
            <p className="muted tiny break">
              Browser did not open?{" "}
              <button
                className="ghost tiny"
                onClick={() =>
                  void api
                    .openAuthUrl(dropboxAuthUrl)
                    .catch((cause) => setError(String(cause)))
                }
              >
                Open the consent page
              </button>
            </p>
          )}
        </section>
      </section>
    );
  }

  function renderSettingsView() {
    return (
      <section className="view" aria-labelledby="settings-title">
        <header className="view-header">
          <div>
            <h2 id="settings-title">Settings</h2>
            <p className="sub">
              Appearance and integrations. Cloud sync stays optional.
            </p>
          </div>
        </header>

        <section className="panel" aria-labelledby="settings-appearance">
          <div className="panel-title">
            <h2 id="settings-appearance">Appearance</h2>
          </div>
          <div className="settings-rows">
            <div className="setting-row">
              <div>
                <strong>Theme</strong>
                <span className="muted">
                  Switch the console between the dark and light palettes.
                </span>
              </div>
              <div className="seg" role="group" aria-label="Theme">
                <button
                  type="button"
                  aria-pressed={theme === "light"}
                  onClick={() => setTheme("light")}
                >
                  Light
                </button>
                <button
                  type="button"
                  aria-pressed={theme === "dark"}
                  onClick={() => setTheme("dark")}
                >
                  Dark
                </button>
              </div>
            </div>
          </div>
        </section>

        <section className="panel" aria-labelledby="settings-artwork">
          <div className="panel-title">
            <h2 id="settings-artwork">SteamGridDB</h2>
            <span className="muted tiny">
              {artworkConfigured ? "Configured" : "Not configured"}
            </span>
          </div>
          <form className="stack" onSubmit={saveSteamGridKey}>
            <label>
              API key
              <input
                type="password"
                value={steamGridKeyDraft}
                onChange={(event) => setSteamGridKeyDraft(event.target.value)}
                aria-label="SteamGridDB API key"
                aria-describedby="steamgriddb-note"
                placeholder={
                  artworkConfigured
                    ? "Stored in the OS keyring"
                    : "Paste your SteamGridDB API key"
                }
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            <p className="muted small" id="steamgriddb-note">
              Used for cover-art lookups. Get your API key at{" "}
              <DocsLink
                href="https://www.steamgriddb.com/profile/preferences/api"
                onError={setError}
              >
                steamgriddb.com/profile/preferences/api
              </DocsLink>
              . The key is kept in the OS keyring and is never written to the
              SaveSync database or to logs.
            </p>
            <div className="row">
              <button className="primary" type="submit" disabled={!!busy}>
                Save key
              </button>
              {artworkConfigured && (
                <button
                  type="button"
                  onClick={() => void removeSteamGridKey()}
                  disabled={!!busy}
                >
                  Remove key
                </button>
              )}
            </div>
          </form>
        </section>

        <section className="panel" aria-labelledby="settings-screenscraper">
          <div className="panel-title">
            <h2 id="settings-screenscraper">ScreenScraper</h2>
            <span
              className={screenscraper.dev_configured ? "status on" : "status"}
            >
              {screenscraper.dev_configured
                ? screenscraper.user_configured
                  ? "Developer and user saved"
                  : "Developer credentials saved"
                : "Not set up"}
            </span>
          </div>
          <p className="muted small">
            Alternative metadata source, used first once it is configured.
            ScreenScraper needs a developer pair; a user account only raises the
            request quota.
          </p>
          <form className="stack" onSubmit={saveScreenscraperCredentials}>
            <label>
              Dev ID
              <input
                value={ssDevIdDraft}
                onChange={(event) => setSsDevIdDraft(event.target.value)}
                aria-label="ScreenScraper dev id"
                aria-describedby="screenscraper-dev-note"
                placeholder={
                  screenscraper.dev_configured
                    ? "Stored in the OS keyring"
                    : "ScreenScraper developer id"
                }
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            <label>
              Dev password
              <input
                type="password"
                ref={ssDevPasswordRef}
                aria-label="ScreenScraper dev password"
                aria-describedby="screenscraper-dev-note"
                placeholder={
                  screenscraper.dev_configured
                    ? "Stored in the OS keyring"
                    : "Developer password"
                }
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            <p className="muted small" id="screenscraper-dev-note">
              Required. Request a developer account in the{" "}
              <DocsLink
                href="https://www.screenscraper.fr/forumsujets.php?frub=12"
                onError={setError}
              >
                ScreenScraper developer forum
              </DocsLink>
              . Leave a field blank to keep the value already stored. All four
              values live in the OS keyring, never in the SaveSync database or
              logs.
            </p>
            <label>
              User SSID
              <input
                value={ssSsidDraft}
                onChange={(event) => setSsSsidDraft(event.target.value)}
                aria-label="ScreenScraper user SSID"
                aria-describedby="screenscraper-user-note"
                placeholder={
                  screenscraper.user_configured
                    ? "Stored in the OS keyring"
                    : "ScreenScraper user SSID (optional)"
                }
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            <label>
              User SS password
              <input
                type="password"
                ref={ssUserPasswordRef}
                aria-label="ScreenScraper user SS password"
                aria-describedby="screenscraper-user-note"
                placeholder={
                  screenscraper.user_configured
                    ? "Stored in the OS keyring"
                    : "User SS password (optional)"
                }
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            <p className="muted small" id="screenscraper-user-note">
              Optional. A ScreenScraper user account raises the request quota;
              create one at{" "}
              <DocsLink
                href="https://www.screenscraper.fr/membreinscription.php"
                onError={setError}
              >
                screenscraper.fr/membreinscription.php
              </DocsLink>
              .
            </p>
            <div className="row">
              <button className="primary" type="submit" disabled={!!busy}>
                Save credentials
              </button>
              <button
                type="button"
                onClick={() => void clearScreenscraperCredentials()}
                disabled={!!busy || !screenscraper.configured}
                title="Remove the ScreenScraper credentials from the OS keyring"
              >
                Clear
              </button>
            </div>
          </form>
        </section>
      </section>
    );
  }

  const dirtyCount = games.filter((game) => dirtyGames.has(game.id)).length;
  const statusText =
    games.length === 0
      ? "No games enrolled"
      : dirtyCount > 0
        ? `${dirtyCount} game${dirtyCount === 1 ? "" : "s"} with uncaptured changes`
        : `${games.length} game${games.length === 1 ? "" : "s"} enrolled`;

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <h1 className="brand-name">SaveSync</h1>
        </div>

        <div className="topbar-actions">
          <button
            className="theme-toggle"
            onClick={toggleTheme}
            aria-label={theme === "light" ? "Switch to dark theme" : "Switch to light theme"}
            aria-pressed={theme === "dark"}
          >
            {theme === "light" ? (
              <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
                <g fill="currentColor">
                  <circle cx="12" cy="12" r="4.7" />
                  {[0, 45, 90, 135, 180, 225, 270, 315].map((angle) => (
                    <rect
                      key={angle}
                      x="11.05"
                      y="1.7"
                      width="1.9"
                      height="3.2"
                      rx="0.95"
                      transform={`rotate(${angle} 12 12)`}
                    />
                  ))}
                </g>
              </svg>
            ) : (
              <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
                <path
                  fill="currentColor"
                  d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79Z"
                />
              </svg>
            )}
          </button>
        </div>
      </header>

      <main className="content">
        <div className="messages">
          {(error || notice) && (
            <div
              className={error ? "banner error" : "banner"}
              role={error ? "alert" : "status"}
            >
              <span>{error ?? notice}</span>
              <button
                className="ghost tiny"
                onClick={() => {
                  setError(null);
                  setNotice(null);
                }}
              >
                Dismiss
              </button>
            </div>
          )}

          {status?.startup_warning && (
            <div className="banner warning" role="status">
              {status.startup_warning}
            </div>
          )}

          {busy && (
            <p className="busy-line" role="status">
              <span className="busy-dot" aria-hidden="true" />
              {busy}…
            </p>
          )}
        </div>

        {view === "library" &&
          (selectedGame ? renderDetailView(selectedGame) : renderLibraryView())}

        {(view === "snapshots" || view === "conflicts") &&
          (selectedGame
            ? renderDetailView(selectedGame)
            : renderPickGameView(view === "snapshots" ? "Snapshots" : "Conflicts"))}

        {view === "cloud" && renderCloudView()}

        {view === "settings" && renderSettingsView()}
      </main>

      {/*
        One docked band owns every primary destination. The status readout is
        local state only — enrollment count and uncaptured-change count — never a
        remote-provider claim, because local backup works offline.
      */}
      <div className="dock">
        <p className="dock-status">
          {/*
            State, not decoration: the dot exists only while something is
            uncaptured, driven by the same count as the text beside it, so the
            two can never disagree. When it is absent the text reflows into the
            space rather than a placeholder reserving it.
          */}
          {dirtyCount > 0 && <span className="dock-dot" aria-hidden="true" />}
          <span className="dock-status-text">{statusText}</span>
        </p>

        <span className="dock-divider" aria-hidden="true" />

        <nav className="nav" aria-label="Primary">
          {NAV_ITEMS.map((item) => (
            <button
              key={item.view}
              type="button"
              className="nav-btn"
              aria-current={view === item.view ? "page" : undefined}
              onClick={() => setView(item.view)}
            >
              <span className="nav-icon" aria-hidden="true" data-view={item.view}>
                <NavGlyph view={item.view} />
              </span>
              <span className="nav-label">{item.label}</span>
            </button>
          ))}
        </nav>
      </div>

      {menuPosition && menuGameId ? (
        (() => {
          const game = games.find((g) => g.id === menuGameId);
          const hasHead = game?.active_head_revision_id !== null && game?.active_head_revision_id !== undefined;
          return (
            <div
              ref={menuRef}
              role="menu"
              aria-label="Game actions"
              onKeyDown={handleMenuKeyDown}
              style={{
                position: "fixed",
                left: menuPosition.x,
                top: menuPosition.y,
                minWidth: "232px",
              }}
              className="game-menu"
              onClick={(event) => event.stopPropagation()}
            >
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  setSelectedId(menuGameId);
                }}
              >
                Open details
              </button>
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                disabled={busy === "Creating snapshot"}
                title={busy === "Creating snapshot" ? "Creating snapshot…" : "Create a new snapshot of the current save state"}
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (menuGameId) void takeSnapshotFromMenu(menuGameId);
                }}
              >
                {busy === "Creating snapshot" ? "Creating snapshot…" : "Backup now"}
              </button>
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                disabled={!hasHead}
                title={hasHead ? "Restore the latest snapshot" : "No snapshot exists yet"}
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (game?.active_head_revision_id) {
                    void restoreLatest(game.id, game.active_head_revision_id);
                  }
                }}
              >
                Restore latest…
              </button>
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                disabled={!!busy || !activeConnected}
                title={
                  activeConnected
                    ? busy
                      ? "Syncing…"
                      : `Sync revisions with ${providerHeading}`
                    : connectHint
                }
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (menuGameId) void syncGame(menuGameId);
                }}
              >
                {busy === syncLabel ? "Syncing…" : "Sync now"}
              </button>
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                disabled={!!busy}
                title={
                  busy === "Fetching cover art"
                    ? "Fetching cover art…"
                    : "Search SteamGridDB for box art"
                }
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (menuGameId) void fetchCoverArt(menuGameId);
                }}
              >
                {busy === "Fetching cover art" ? "Fetching cover art…" : "Fetch cover art"}
              </button>
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                disabled={!!busy}
                title="Pick a specific cover from SteamGridDB"
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (menuGameId) void openCoverPicker(menuGameId);
                }}
              >
                Choose cover art…
              </button>
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item"
                disabled={!!busy}
                title={
                  game?.featured
                    ? "Remove the wide banner from the library"
                    : "Pin this game as the library's wide banner"
                }
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (menuGameId) {
                    void setFeaturedGame(menuGameId, !game?.featured);
                  }
                }}
              >
                {game?.featured ? "Remove from featured" : "Feature on library"}
              </button>
              <hr className="menu-separator" />
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item menu-item-danger"
                onClick={(event) => {
                  event.stopPropagation();
                  closeMenu();
                  if (menuGameId) openUnlinkDialog(menuGameId);
                }}
              >
                Remove from library…
              </button>
            </div>
          );
        })()
      ) : null}

      {coverPicker ? (
        <Modal titleId="cover-picker-title" onClose={closeCoverPicker} wide>
          <div className="dialog-card cover-picker-card" onClick={(event) => event.stopPropagation()}>
            <div className="cover-picker-head">
              <h3 id="cover-picker-title" style={{ margin: 0 }}>
                Choose cover art
              </h3>
              {/* Rendered before every other control so it is the dialog's first
                  focusable element: the shared Modal focuses it on open. */}
              <button type="button" className="ghost" onClick={closeCoverPicker} autoFocus>
                Cancel
              </button>
            </div>
            <div className="cover-picker-subhead">
              <p className="muted small" style={{ margin: 0 }}>
                {coverPicker.title}
              </p>
              {/* Shape toggle. Portrait is the default, so opening the dialog
                  still searches exactly what it always did. */}
              <div className="seg" role="group" aria-label="Cover shape">
                <button
                  type="button"
                  aria-pressed={coverPicker.kind === "portrait"}
                  onClick={() => switchCoverKind("portrait")}
                  disabled={coverPicker.pickingId !== null}
                >
                  Portrait
                </button>
                <button
                  type="button"
                  aria-pressed={coverPicker.kind === "wide"}
                  onClick={() => switchCoverKind("wide")}
                  disabled={coverPicker.pickingId !== null}
                >
                  Wide
                </button>
              </div>
            </div>

            {coverPicker.status === "loading" ? (
              <p role="status" style={{ marginTop: "16px" }}>
                Loading cover art…
              </p>
            ) : null}

            {coverPicker.status === "error" ? (
              <div style={{ marginTop: "12px" }}>
                <p role="alert">{coverPicker.error}</p>
                <div style={{ display: "flex", justifyContent: "flex-end", marginTop: "12px" }}>
                  <button type="button" className="primary" onClick={retryCoverSearch}>
                    Retry
                  </button>
                </div>
              </div>
            ) : null}

            {coverPicker.status === "ready" && coverPicker.candidates.length === 0 ? (
              <div style={{ marginTop: "16px" }}>
                <p role="status">No cover art found</p>
                <p className="muted small">
                  SteamGridDB has no usable artwork for this game. Your current cover is
                  unchanged.
                </p>
              </div>
            ) : null}

            {coverPicker.status === "ready" && coverPicker.candidates.length > 0 ? (
              <div
                ref={coverGridRef}
                role="group"
                aria-label={
                  coverPicker.kind === "wide"
                    ? "Wide cover art candidates"
                    : "Cover art candidates"
                }
                className={
                  coverPicker.kind === "wide" ? "cover-grid cover-grid-wide" : "cover-grid"
                }
              >
                {coverPicker.candidates.map((candidate, index) => {
                  const unavailable = coverPicker.failedIds.includes(candidate.id);
                  const picking = coverPicker.pickingId === candidate.id;
                  const busy = coverPicker.pickingId !== null;
                  // The tile in flight stays enabled so it can keep keyboard
                  // focus (`coverPickInFlight` blocks a second activation); an
                  // unavailable tile can never be picked.
                  const disabled = unavailable || (busy && !picking);
                  return (
                    <button
                      type="button"
                      key={candidate.id}
                      data-cover-id={candidate.id}
                      disabled={disabled}
                      aria-busy={picking || undefined}
                      aria-label={
                        unavailable
                          ? `Cover ${index + 1} unavailable`
                          : `Use cover ${index + 1}${
                              candidate.width && candidate.height
                                ? ` (${candidate.width}×${candidate.height})`
                                : ""
                            }`
                      }
                      onClick={() => void chooseCoverGrid(candidate)}
                      className={
                        coverPicker.kind === "wide" ? "cover-tile cover-tile-wide" : "cover-tile"
                      }
                      style={{
                        cursor: unavailable ? "not-allowed" : busy ? "progress" : "pointer",
                        opacity: unavailable ? 0.4 : busy && !picking ? 0.55 : 1,
                      }}
                    >
                      <img
                        src={candidate.url}
                        alt=""
                        loading="lazy"
                        // A silent empty box hides the fact that the tile can no
                        // longer be used; mark just this candidate instead of
                        // failing the whole dialog.
                        onError={() =>
                          setCoverPicker((current) =>
                            current &&
                            current.gameId === coverPicker.gameId &&
                            !current.failedIds.includes(candidate.id)
                              ? { ...current, failedIds: [...current.failedIds, candidate.id] }
                              : current,
                          )
                        }
                        style={{
                          width: "100%",
                          height: "100%",
                          objectFit: "cover",
                          display: "block",
                        }}
                      />
                      {unavailable ? (
                        <span
                          style={{
                            position: "absolute",
                            inset: 0,
                            display: "grid",
                            placeItems: "center",
                            padding: "4px",
                            background: "var(--scrim-strong)",
                            color: "var(--ink-inverse)",
                            fontSize: "0.68rem",
                            lineHeight: 1.2,
                            textAlign: "center",
                          }}
                        >
                          Image unavailable
                        </span>
                      ) : null}
                      {picking ? (
                        <span
                          style={{
                            position: "absolute",
                            inset: 0,
                            display: "grid",
                            placeItems: "center",
                            background: "var(--scrim-strong)",
                            color: "var(--ink-inverse)",
                            fontSize: "0.8rem",
                          }}
                        >
                          Saving…
                        </span>
                      ) : null}
                    </button>
                  );
                })}
              </div>
            ) : null}
          </div>
        </Modal>
      ) : null}

      {enrollOpen ? (
        <Modal titleId="enroll-dialog-title" onClose={closeEnrollDialog}>
          <div className="dialog-card" onClick={(event) => event.stopPropagation()}>
            <h3 id="enroll-dialog-title">Enroll software</h3>
            <p className="muted small">
              Point SaveSync at a save folder. Discovery never enrolls on its own.
            </p>
            <form className="stack" onSubmit={enroll}>
              <label>
                Title
                <input
                  ref={titleInputRef}
                  value={title}
                  onChange={(event) => setTitle(event.target.value)}
                  placeholder="Hades"
                />
              </label>
              <label>
                Save folder (absolute path)
                <input
                  value={savePath}
                  onChange={(event) => setSavePath(event.target.value)}
                  placeholder="/home/me/.local/share/Hades/Saved"
                  spellCheck={false}
                />
              </label>
              <label>
                Adapter
                <select value={adapter} onChange={(event) => setAdapter(event.target.value)}>
                  {ADAPTER_ORDER.map((value) => (
                    <option key={value} value={value}>
                      {ADAPTER_LABELS[value]}
                    </option>
                  ))}
                </select>
              </label>
              <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px", marginTop: "8px" }}>
                <button type="button" className="ghost" onClick={closeEnrollDialog}>
                  Cancel
                </button>
                <button className="primary" type="submit" disabled={!!busy}>
                  {busy === "Enrolling game" ? "Enrolling…" : "Enroll game"}
                </button>
              </div>
            </form>
          </div>
        </Modal>
      ) : null}

      {unlinkConfirming && pendingUnlinkId ? (
        <Modal titleId="unlink-dialog-title" onClose={closeUnlinkDialog}>
          <div className="dialog-card" onClick={(event) => event.stopPropagation()}>
            <h3 id="unlink-dialog-title">Remove from library</h3>
            <p>Removing from library keeps all revisions and on-disk backups intact.</p>
            <p>No saved games or history will be deleted. You can add it again at any time.</p>
            <p>Deleting history is a separate action and is never the default.</p>
            <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px", marginTop: "8px" }}>
              <button
                type="button"
                className="ghost"
                onClick={closeUnlinkDialog}
                autoFocus
              >
                Cancel
              </button>
              <button type="button" className="danger" onClick={handleUnlink}>
                Remove from library
              </button>
            </div>
          </div>
        </Modal>
      ) : null}

      {dialogState?.kind === "restore" ? (
        <Modal titleId="restore-dialog-title" onClose={() => setDialogState(null)}>
          <div className="dialog-card" onClick={(event) => event.stopPropagation()}>
            <h3 id="restore-dialog-title">Restore snapshot</h3>
            {dialogState.exactMirror ? (
              <>
                <p style={{ fontWeight: 600, color: "var(--danger)", marginBottom: "8px" }}>
                  Warning: Exact restore will delete files not in this snapshot.
                </p>
                <p>
                  The live folder will be made to match this snapshot exactly. Files that exist
                  in the live folder but not in the snapshot will be permanently deleted. A verified
                  safety backup is created before the operation.
                </p>
              </>
            ) : (
              <p>
                Snapshot files will be written over the live folder. Unrelated files already
                present in the live folder will be kept. A verified safety backup is created
                before the operation.
              </p>
            )}
            <p className="muted small">
              Snapshot from {formatDate(dialogState.revision.created_at)} ·{" "}
              {shortId(dialogState.revision.id)}
            </p>
            <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px", marginTop: "16px" }}>
              <button
                type="button"
                className="ghost"
                onClick={() => setDialogState(null)}
                autoFocus
              >
                Cancel
              </button>
              <button
                type="button"
                className={dialogState.exactMirror ? "danger" : "primary"}
                onClick={() => {
                  const { revision, exactMirror } = dialogState;
                  setDialogState(null);
                  void confirmRestore(revision, exactMirror);
                }}
              >
                {dialogState.exactMirror ? "Exact restore" : "Restore"}
              </button>
            </div>
          </div>
        </Modal>
      ) : null}

      {dialogState?.kind === "conflict" ? (
        <Modal titleId="conflict-dialog-title" onClose={() => setDialogState(null)}>
          <div className="dialog-card" onClick={(event) => event.stopPropagation()}>
            <h3 id="conflict-dialog-title">Resolve conflict</h3>
            <p>
              Two devices advanced from the same snapshot. Choose which snapshot to keep.
              Live save files are not modified until you restore the chosen snapshot.
            </p>
            <p className="muted small">
              Detected {formatDate(dialogState.conflict.detected_at)}
            </p>
            <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px", marginTop: "16px" }}>
              <button
                type="button"
                className="ghost"
                onClick={() => setDialogState(null)}
                autoFocus
              >
                Cancel
              </button>
              <button
                type="button"
                className="primary"
                onClick={() => {
                  const { conflict, revisionId } = dialogState;
                  setDialogState(null);
                  void confirmResolve(conflict, revisionId);
                }}
              >
                Choose this snapshot
              </button>
            </div>
          </div>
        </Modal>
      ) : null}

      {dialogState?.kind === "disconnect" ? (
        <Modal titleId="disconnect-dialog-title" onClose={() => setDialogState(null)}>
          <div className="dialog-card" onClick={(event) => event.stopPropagation()}>
            <h3 id="disconnect-dialog-title">Disconnect Google Drive</h3>
            <p>This will disconnect Google Drive and remove the local refresh token.</p>
            <p className="muted small">You will need to sign in again to use Google Drive sync.</p>
            <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px", marginTop: "16px" }}>
              <button
                type="button"
                className="ghost"
                onClick={() => setDialogState(null)}
                autoFocus
              >
                Cancel
              </button>
              <button
                type="button"
                className="danger"
                onClick={() => {
                  setDialogState(null);
                  void confirmDisconnect();
                }}
              >
                Disconnect
              </button>
            </div>
          </div>
        </Modal>
      ) : null}
    </div>
  );
}
