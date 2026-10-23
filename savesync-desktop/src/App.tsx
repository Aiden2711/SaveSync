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
  DirtyEvent,
  Game,
  GoogleStatus,
  Revision,
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
 * Initial letter of a game title, used by the tile art slot when the game has
 * no cover art yet. Skips leading punctuation/whitespace so "'-Hades'" still
 * reads as "H", and falls back to "?" for a title with no letters or digits.
 * Rendered with the mono text stack by `.tile-mono`; this helper only decides
 * the character.
 */
function tileInitial(title: string): string {
  const letter = title.match(/[\p{L}\p{N}]/u);
  return letter ? letter[0].toUpperCase() : "?";
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
  children,
}: {
  titleId: string;
  onClose: () => void;
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
      className="modal"
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
 * Shared attributes for the 18px line icons in the top navigation. The CSS
 * `.nav-icon` box is `display: grid; place-items: center`, so the glyph is
 * centred on the label's cap height rather than riding high in the button.
 */
const NAV_SVG_PROPS = {
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.8,
  strokeLinecap: "round",
  strokeLinejoin: "round",
  "aria-hidden": true,
  focusable: "false",
} as const;

/** Purely decorative glyph for a nav destination; the label carries the name. */
function NavGlyph({ view }: { view: View }) {
  return (
    <svg {...NAV_SVG_PROPS}>
      {view === "library" && (
        <>
          <rect x="3.5" y="3.5" width="7" height="7" />
          <rect x="13.5" y="3.5" width="7" height="7" />
          <rect x="3.5" y="13.5" width="7" height="7" />
          <rect x="13.5" y="13.5" width="7" height="7" />
        </>
      )}
      {view === "snapshots" && (
        <>
          <path d="M3.4 12a8.6 8.6 0 1 0 2.6-6.1" />
          <path d="M3 4.2v5h5" />
          <path d="M12 8.2v4.2l2.8 1.6" />
        </>
      )}
      {view === "conflicts" && (
        <>
          <circle cx="6" cy="6" r="2.2" />
          <circle cx="6" cy="18" r="2.2" />
          <circle cx="18" cy="12" r="2.2" />
          <path d="M6 8.2v7.6M8.2 6h4a3.4 3.4 0 0 1 3.4 3.4" />
        </>
      )}
      {view === "cloud" && (
        <path d="M7.2 18.5h9.6a3.9 3.9 0 0 0 .5-7.8 5.6 5.6 0 0 0-10.8-1.2 4.5 4.5 0 0 0 .7 9Z" />
      )}
      {view === "settings" && (
        <>
          <circle cx="12" cy="12" r="3.2" />
          <path d="M12 2.8v2.4M12 18.8v2.4M4.5 4.5l1.7 1.7M17.8 17.8l1.7 1.7M2.8 12h2.4M18.8 12h2.4M4.5 19.5l1.7-1.7M17.8 6.2l1.7-1.7" />
        </>
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
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [menuPosition, setMenuPosition] = useState<{ x: number; y: number } | null>(null);
  const [unlinkConfirming, setUnlinkConfirming] = useState(false);

  const [title, setTitle] = useState("");
  const [savePath, setSavePath] = useState("");
  const [adapter, setAdapter] = useState("custom_directory");
  const [snapshotMessage, setSnapshotMessage] = useState("");
  const [clientIdDraft, setClientIdDraft] = useState("");
  const [authUrl, setAuthUrl] = useState<string | null>(null);
  /** Flag only: the SteamGridDB key lives in the OS keyring, never in the UI. */
  const [artworkConfigured, setArtworkConfigured] = useState(false);
  const [steamGridKeyDraft, setSteamGridKeyDraft] = useState("");

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

  const loadGames = useCallback(async () => {
    try {
      const list = await api.listGames();
      setGames(list);
      setSelectedId((current) =>
        current && list.some((game) => game.id === current) ? current : null,
      );
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

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
        const [nextStatus, nextDrive] = await Promise.all([
          api.status(),
          api.googleStatus(),
        ]);
        setStatus(nextStatus);
        setDrive(nextDrive);
        setClientIdDraft(nextDrive.client_id ?? "");
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
   * SteamGridDB key state is fetched when the Settings view opens, not at
   * startup: the library and restore flows never need it, so it must not add
   * work (or a keyring read) to application startup.
   */
  useEffect(() => {
    if (!isDesktop()) return;
    if (view !== "settings") {
      // Do not keep a typed secret around in memory after leaving Settings.
      setSteamGridKeyDraft("");
      return;
    }
    let cancelled = false;
    void api
      .steamgriddbStatus()
      .then((next) => {
        if (!cancelled) setArtworkConfigured(next.configured);
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
      await loadGameDetails(gameId);
    }
  }

  async function syncGame(gameId: string) {
    const report: SyncReport | null = await run("Syncing with Google Drive", () =>
      api.sync(gameId),
    );
    if (report) {
      setNotice(
        `Sync complete: ${report.pulled_revisions} pulled, ${report.pushed_revisions} pushed, ${report.conflict_ids.length} conflict(s).`,
      );
      await loadGames();
      await loadGameDetails(gameId);
    }
  }

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

  async function syncSelected() {
    if (!selectedGame) return;
    const report: SyncReport | null = await run("Syncing with Google Drive", () =>
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

  const openMenu = useCallback(
    (event: React.MouseEvent, gameId: string) => {
      event.preventDefault();
      setSelectedId(gameId);
      setMenuGameId(gameId);
      const topbarEl = document.querySelector<HTMLElement>(".topbar");
      const topInset = topbarEl ? topbarEl.getBoundingClientRect().bottom + 8 : TOPBAR_FALLBACK_HEIGHT;
      const pos = clampMenuPosition(
        { x: event.clientX, y: event.clientY },
        MENU_SIZE,
        { width: window.innerWidth, height: window.innerHeight },
        topInset,
      );
      setMenuPosition(pos);
      // Store the triggering element so Escape can return focus to it
      menuTriggerRef.current = event.currentTarget as HTMLElement;
      // Focus first enabled menu item after render
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

  const handleUnlink = useCallback(async () => {
    if (!selectedId) return;
    const result = await run("Unlinking game", () => api.unlinkGame(selectedId));
    if (result) {
      playCartridgeSound("eject");
      setSelectedId(null);
      setMenuPosition(null);
      setUnlinkConfirming(false);
      setNotice("Removed from library. Revisions and on-disk backups are preserved.");
    }
  }, [selectedId]);

  const openUnlinkDialog = useCallback(() => {
    setUnlinkConfirming(true);
    setMenuPosition(null);
  }, []);

  const closeUnlinkDialog = useCallback(() => {
    setUnlinkConfirming(false);
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
    return (
      <section className="view" aria-labelledby="library-title">
        <header className="view-header">
          <div>
            <p className="eyebrow">Library</p>
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
            {games.map((game, index) => {
              const isSelected = game.id === selectedId;
              const isDirty = dirtyGames.has(game.id);
              // The first enrolled game fills the two-column hero slot.
              const isHero = index === 0;
              return (
                <li
                  key={game.id}
                  className={isHero ? "tile-cell tile-cell--hero" : "tile-cell"}
                >
                  <button
                    type="button"
                    className={isHero ? "tile tile--hero" : "tile"}
                    aria-current={isSelected ? "true" : undefined}
                    onClick={() => setSelectedId(game.id)}
                    onContextMenu={(event) => openMenu(event, game.id)}
                  >
                    <span className="tile-art" aria-hidden="true">
                      {game.cover_path ? (
                        <CoverArt title={game.title} adapter={game.adapter_type} />
                      ) : (
                        <span className="tile-mono">{tileInitial(game.title)}</span>
                      )}
                    </span>
                    <span className="tile-label">
                      <span className="tile-head">
                        <span className="tile-title">{game.title}</span>
                        {isDirty && (
                          <span
                            className="dirty-badge"
                            aria-label="Uncaptured save changes"
                          />
                        )}
                      </span>
                      <span className="tile-acc">
                        {adapterLabel(game.adapter_type)} ·{" "}
                        {game.active_head_revision_id
                          ? `head ${shortId(game.active_head_revision_id)}`
                          : "No snapshots"}
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
            <CoverArt title={game.title} adapter={game.adapter_type} />
          </div>
          <div className="detail-copy">
            <p className="eyebrow">Game</p>
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
              onClick={() => void syncSelected()}
              disabled={!!busy || !drive.connected}
              title={
                drive.connected
                  ? "Sync revisions with Google Drive"
                  : "Connect Google Drive first"
              }
            >
              {busy === "Syncing with Google Drive" ? "Syncing…" : "Sync now"}
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
            <p className="eyebrow">{section}</p>
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
    return (
      <section className="view" aria-labelledby="cloud-title">
        <header className="view-header">
          <div>
            <p className="eyebrow">Cloud</p>
            <h2 id="cloud-title">Cloud sync</h2>
            <p className="sub">
              Optional and manual. Local snapshots and restores work offline.
            </p>
          </div>
        </header>

        <section className="panel drive-panel" aria-labelledby="drive-panel-title">
          <div className="panel-title">
            <h2 id="drive-panel-title">Google Drive</h2>
            <span
              className={drive.connected ? "dot online" : "dot"}
              aria-hidden="true"
            />
          </div>
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
                placeholder="…apps.googleusercontent.com"
                spellCheck={false}
              />
            </label>
            <button className="ghost" type="submit" disabled={!!busy}>
              Save client id
            </button>
          </form>

          <div className="row">
            {drive.connected ? (
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
            )}
            {selectedGame ? (
              <button
                className="ghost"
                onClick={() => void syncSelected()}
                disabled={!!busy || !drive.connected}
                title={
                  drive.connected
                    ? "Sync the selected game with Google Drive"
                    : "Connect Google Drive first"
                }
              >
                {busy === "Syncing with Google Drive" ? "Syncing…" : "Sync now"}
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

          {authUrl && (
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
          {drive.error && <p className="error small">{drive.error}</p>}
        </section>
      </section>
    );
  }

  function renderSettingsView() {
    return (
      <section className="view" aria-labelledby="settings-title">
        <header className="view-header">
          <div>
            <p className="eyebrow">Settings</p>
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
              Used for cover-art lookups. The key is kept in the OS keyring and is
              never written to the SaveSync database or to logs.
            </p>
            <div className="row">
              <button className="ghost" type="submit" disabled={!!busy}>
                Save key
              </button>
              {artworkConfigured && (
                <button
                  className="ghost"
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
      </section>
    );
  }

  return (
    <div className="app grain">
      <header className="topbar">
        <div className="brand">
          <div className="brand-mark" aria-hidden="true">
            SS
          </div>
          <div className="brand-copy">
            <h1 className="brand-name">SaveSync</h1>
            <p className="brand-sub">Local-first save history</p>
          </div>
        </div>

        <nav className="nav" aria-label="Primary">
          {NAV_ITEMS.map((item) => (
            <button
              key={item.view}
              type="button"
              className="nav-btn"
              aria-current={view === item.view ? "page" : undefined}
              onClick={() => setView(item.view)}
            >
              <span className="nav-icon" aria-hidden="true">
                <NavGlyph view={item.view} />
              </span>
              {item.label}
            </button>
          ))}
        </nav>

        <div className="topbar-actions">
          <button
            className="theme-toggle"
            onClick={toggleTheme}
            aria-label={theme === "light" ? "Switch to dark theme" : "Switch to light theme"}
            aria-pressed={theme === "dark"}
          >
            {theme === "light" ? (
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                <circle cx="12" cy="12" r="5" />
                <line x1="12" y1="1" x2="12" y2="3" />
                <line x1="12" y1="21" x2="12" y2="23" />
                <line x1="4.22" y1="4.22" x2="5.64" y2="5.64" />
                <line x1="18.36" y1="18.36" x2="19.78" y2="19.78" />
                <line x1="1" y1="12" x2="3" y2="12" />
                <line x1="21" y1="12" x2="23" y2="12" />
                <line x1="4.22" y1="19.78" x2="5.64" y2="18.36" />
                <line x1="18.36" y1="5.64" x2="19.78" y2="4.22" />
              </svg>
            ) : (
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
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
                  setMenuPosition(null);
                  setMenuGameId(null);
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
                  setMenuPosition(null);
                  setMenuGameId(null);
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
                  setMenuPosition(null);
                  setMenuGameId(null);
                  if (game?.active_head_revision_id) {
                    const rev = revisions.find((r) => r.id === game.active_head_revision_id);
                    if (rev) setDialogState({ kind: "restore", revision: rev, exactMirror: false });
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
                disabled={!!busy || !drive.connected}
                title={
                  drive.connected
                    ? busy
                      ? "Syncing…"
                      : "Sync revisions with Google Drive"
                    : "Connect Google Drive first"
                }
                onClick={(event) => {
                  event.stopPropagation();
                  setMenuPosition(null);
                  setMenuGameId(null);
                  if (menuGameId) void syncGame(menuGameId);
                }}
              >
                {busy === "Syncing with Google Drive" ? "Syncing…" : "Sync now"}
              </button>
              <hr className="menu-separator" />
              <button
                type="button"
                role="menuitem"
                tabIndex={-1}
                className="menu-item menu-item-danger"
                onClick={(event) => {
                  event.stopPropagation();
                  setMenuPosition(null);
                  setMenuGameId(null);
                  openUnlinkDialog();
                }}
              >
                Remove from library…
              </button>
            </div>
          );
        })()
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

      {unlinkConfirming && selectedId ? (
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
