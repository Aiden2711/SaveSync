import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { api } from "./api";
import CoverArt from "./components/CoverArt";
import { ADAPTER_LABELS, adapterLabel } from "./lib/cover";
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

  const titleInputRef = useRef<HTMLInputElement>(null);
  const [dirtyGames, setDirtyGames] = useState<Set<string>>(new Set());

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

  async function restore(revision: Revision, exactMirror: boolean) {
    const detail = exactMirror
      ? "The live folder will match this snapshot exactly; files not in it will be deleted. A verified backup is kept."
      : "Snapshot files will be written over the live folder. Unrelated files will be kept. A verified backup is kept.";
    if (
      !window.confirm(
        `Restore snapshot from ${formatDate(revision.created_at)}?\n\n${detail}`,
      )
    ) {
      return;
    }
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

  async function resolve(conflict: Conflict, revisionId: string) {
    if (
      !window.confirm(
        "Resolve this conflict by choosing this snapshot? Live saves are not changed until you restore the chosen snapshot.",
      )
    ) {
      return;
    }
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
    if (!window.confirm("Disconnect Google Drive and remove the local refresh token?")) {
      return;
    }
    const result = await run("Disconnecting Google Drive", () =>
      api.disconnectGoogle(),
    );
    if (result) {
      setDrive(result);
      setAuthUrl(null);
      setNotice("Google Drive disconnected.");
    }
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
      setMenuPosition({ x: event.clientX, y: event.clientY });
      setSelectedId(gameId);
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
      setNotice("Quitado de la biblioteca. Las revisiones y copias de seguridad en disco se conservan.");
    }
  }, [selectedId]);

  return (
    <div className="app">
      <aside className="sidebar" aria-label="Enrollment and storage controls">
        <div className="brand">
          <div className="brand-mark" aria-hidden="true">
            SS
          </div>
          <div className="brand-copy">
            <h1>SaveSync</h1>
            <p>Local-first save history</p>
          </div>
        </div>

        <section className="panel">
          <h2>Enroll a folder</h2>
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
            <button className="primary" type="submit" disabled={!!busy}>
              {busy === "Enrolling game" ? "Enrolling…" : "Enroll game"}
            </button>
          </form>
        </section>

        <section className="panel drive-panel">
          <div className="panel-title">
            <h2>Google Drive</h2>
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
              <button className="ghost" onClick={() => void disconnectDrive()} disabled={!!busy}>
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

        <footer className="sidebar-footer">
          {status ? `${status.device} · ${status.platform}` : "Starting…"}
        </footer>
      </aside>

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

        {!selectedGame ? (
          <section className="library" aria-labelledby="library-title">
            <header className="library-header">
              <div>
                <p className="eyebrow">Library</p>
                <h2 id="library-title">Game library</h2>
                <p className="library-count">
                  {games.length} game{games.length === 1 ? "" : "s"} enrolled
                </p>
              </div>
              <button
                className="primary"
                type="button"
                onClick={() => titleInputRef.current?.focus()}
              >
                Add game
              </button>
            </header>

            {games.length === 0 ? (
              <div className="empty">
                <h3>No games enrolled yet</h3>
                <p className="muted">
                  Enroll a save folder in the sidebar to create your first
                  immutable snapshot.
                </p>
              </div>
            ) : (
              <ul className="cover-grid">
                {games.map((game) => {
                  const isSelected = game.id === selectedId;
                  const isDirty = dirtyGames.has(game.id);
                  return (
                    <li key={game.id}>
                      <button
                        type="button"
                        className="cover-card"
                        aria-current={isSelected ? "true" : undefined}
                        onClick={() => setSelectedId(game.id)}
                        onContextMenu={(event) => openMenu(event, game.id)}
                      >
                        <CoverArt title={game.title} adapter={game.adapter_type} />
                        <span className="cover-label">
                          <span className="cover-title">{game.title}</span>
                          {isDirty && (
                            <span className="dirty-badge" aria-label="Cambios sin capturar" />
                          )}
                          <span className="cover-meta">
                            {adapterLabel(game.adapter_type)} ·{" "}
                            {game.active_head_revision_id
                              ? `head ${shortId(game.active_head_revision_id)}`
                              : "No snapshots"}
                          </span>
                        </span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
        ) : (
          <>
            <button
              className="ghost back"
              type="button"
              onClick={() => setSelectedId(null)}
            >
              <span aria-hidden="true">←</span> Back to library
            </button>

            <header className="detail-header">
              <div className="detail-cover">
                <CoverArt
                  title={selectedGame.title}
                  adapter={selectedGame.adapter_type}
                />
              </div>
              <div className="detail-copy">
                <p className="eyebrow">Game</p>
                <h2>{selectedGame.title}</h2>
                {dirtyGames.has(selectedGame.id) && (
                  <span className="dirty-badge" aria-label="Cambios sin capturar" style={{ marginTop: 4 }} />
                )}
                <p className="detail-meta">
                  {adapterLabel(selectedGame.adapter_type)} · {revisions.length}{" "}
                  snapshot{revisions.length === 1 ? "" : "s"} ·{" "}
                  {selectedGame.active_head_revision_id
                    ? `head ${shortId(selectedGame.active_head_revision_id)}`
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
                                onClick={() => void resolve(conflict, revisionId)}
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
                      const isHead =
                        selectedGame.active_head_revision_id === revision.id;
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
                                  onClick={() => void restore(revision, false)}
                                  title="Write snapshot files; keep unrelated live files"
                                >
                                  Restore
                                </button>
                                <button
                                  className="ghost small danger"
                                  disabled={!!busy}
                                  onClick={() => void restore(revision, true)}
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
          </>
        )}
      </main>

      {menuPosition && selectedId ? (
        <div
          role="menu"
          aria-label="Acciones del juego"
          style={{
            position: "absolute",
            left: menuPosition.x,
            top: menuPosition.y,
            minWidth: "180px",
          }}
          className="game-menu"
          onClick={(event) => event.stopPropagation()}
        >
          <button
            type="button"
            className="menu-item"
            disabled={!!busy || !drive.connected}
            title={
              drive.connected
                ? busy ? "Sincronizando…" : "Sincronizar revisiones con Google Drive"
                : "Conecta Google Drive primero"
            }
            onClick={(event) => {
              event.stopPropagation();
              setMenuPosition(null);
              void syncSelected();
            }}
          >
            {busy === "Syncing with Google Drive" ? "Sincronizando…" : "Sincronizar"}
          </button>
          <button
            type="button"
            className="menu-item menu-item-danger"
            onClick={(event) => {
              event.stopPropagation();
              setMenuPosition(null);
              setUnlinkConfirming(true);
            }}
          >
            Quitar de la biblioteca
          </button>
        </div>
      ) : null}

      {unlinkConfirming && selectedId ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="unlink-dialog-title"
          style={{
            position: "fixed",
            inset: 0,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            background: "rgba(0, 0, 0, 0.35)",
            zIndex: 50,
          }}
          className="unlink-dialog"
          onClick={(event) => event.stopPropagation()}
        >
          <div
            style={{
              background: "var(--surface)",
              border: "1px solid var(--line)",
              borderRadius: "var(--radius-panel)",
              padding: "24px",
              maxWidth: "360px",
              width: "90vw",
              display: "flex",
              flexDirection: "column",
              gap: "16px",
            }}
            onClick={(event) => event.stopPropagation()}
          >
            <h3 id="unlink-dialog-title" style={{ margin: "0", fontSize: "17px" }}>
              Quitar de la biblioteca
            </h3>
            <p style={{ margin: "0", fontSize: "13px" }}>
              Quitar de la biblioteca mantendrá todas las revisiones y copias de seguridad en disco.
            </p>
            <p style={{ margin: "0", fontSize: "13px" }}>
              No se borra ninguna partida guardada ni historial. Puede añadirlo de nuevo en cualquier momento.
            </p>
            <p style={{ margin: "0", fontSize: "13px" }}>
              Borrar el historial es una acción separada y nunca está por defecto.
            </p>
            <div style={{ display: "flex", justifyContent: "flex-end", gap: "10px", marginTop: "8px" }}>
              <button
                type="button"
                className="ghost"
                onClick={() => setUnlinkConfirming(false)}
              >
                Cancelar
              </button>
              <button
                type="button"
                className="danger"
                onClick={handleUnlink}
              >
                Quitar de la biblioteca
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
