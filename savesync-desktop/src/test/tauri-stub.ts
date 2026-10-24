/**
 * Deterministic test double for the Tauri runtime bridge.
 *
 * The UI talks to Rust exclusively through `window.__TAURI_INTERNALS__`
 * (see `src/tauri.ts`). Installing this stub before rendering `App` lets the
 * real component code — including its safety dialogs — run against fixtures
 * without a desktop shell, network, filesystem, or user save data.
 *
 * Every command invocation is recorded in the returned `calls` array before
 * it is answered, so tests can assert on exactly how many mutating calls the
 * UI issued and with which arguments. Unknown commands fail loudly: a new
 * command must be added here deliberately rather than silently returning
 * `undefined`.
 */
import type {
  AppStatus,
  ArtworkStatus,
  Conflict,
  Game,
  GoogleStatus,
  RestoreReport,
  Revision,
} from "../types";

export interface InvokeCall {
  cmd: string;
  args: Record<string, unknown>;
}

interface TauriInternalsStub {
  invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
  event: {
    listen: (event: string, handler: (event: { payload: unknown }) => void) => Promise<() => void>;
  };
}

type StubWindow = { __TAURI_INTERNALS__?: TauriInternalsStub };

const DEVICE_ID = "device-test-0001";
const PRIMARY_GAME_ID = "11111111-1111-4111-8111-111111111111";
const SECOND_GAME_ID = "22222222-2222-4222-8222-222222222222";

const status: AppStatus = {
  device: "Test workstation",
  platform: "windows",
  startup_warning: null,
};

const google: GoogleStatus = {
  client_id: "test-client.apps.googleusercontent.com",
  connected: false,
  pending: false,
  error: null,
};

const games: Game[] = [
  {
    id: PRIMARY_GAME_ID,
    title: "Hades",
    adapter_type: "custom_directory",
    enrolled: true,
    created_at: "2026-01-02T10:00:00Z",
    updated_at: "2026-01-04T18:30:00Z",
    active_head_revision_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    cover_path: null,
  },
  {
    id: SECOND_GAME_ID,
    title: "Hollow Knight",
    adapter_type: "ludusavi",
    enrolled: true,
    created_at: "2026-01-02T11:00:00Z",
    updated_at: "2026-01-05T09:15:00Z",
    active_head_revision_id: null,
    cover_path: null,
  },
  {
    id: "33333333-3333-4333-8333-333333333333",
    title: "Celeste",
    adapter_type: "emulator",
    enrolled: true,
    created_at: "2026-01-03T08:00:00Z",
    updated_at: "2026-01-03T08:00:00Z",
    active_head_revision_id: null,
    cover_path: null,
  },
  {
    id: "44444444-4444-4444-8444-444444444444",
    title: "Stardew Valley",
    adapter_type: "jksv",
    enrolled: true,
    created_at: "2026-01-03T09:30:00Z",
    updated_at: "2026-01-03T09:30:00Z",
    active_head_revision_id: null,
    cover_path: null,
  },
];

const revisions: Revision[] = [
  {
    id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    game_id: PRIMARY_GAME_ID,
    parent_ids: [],
    device_id: DEVICE_ID,
    created_at: "2026-01-03T21:10:00Z",
    manifest_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111",
    blob_hash: "sha256:2222222222222222222222222222222222222222222222222222222222222222",
    message: "Before the final boss",
  },
  {
    id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    game_id: PRIMARY_GAME_ID,
    parent_ids: ["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"],
    device_id: DEVICE_ID,
    created_at: "2026-01-04T18:30:00Z",
    manifest_hash: "sha256:3333333333333333333333333333333333333333333333333333333333333333",
    blob_hash: "sha256:4444444444444444444444444444444444444444444444444444444444444444",
    message: "Epilogue check",
  },
];

const conflicts: Conflict[] = [
  {
    id: "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
    game_id: PRIMARY_GAME_ID,
    revision_ids: [revisions[0].id, revisions[1].id],
    detected_at: "2026-01-05T07:45:00Z",
    resolved_at: null,
    resolution_revision_id: null,
  },
];

const dirtyGameIds: string[] = [SECOND_GAME_ID];

/**
 * Pristine copy of the game fixtures. Handlers mutate `cover_path` and
 * `updated_at`, so every `installTauriStub` restores the shared array in place
 * to keep test order from leaking state into the next file.
 */
const baselineGames: Game[] = games.map((game) => ({ ...game }));

/** Game ids with a cached cover in the stub's fake blob store. */
const seededCovers = new Set<string>();
/** Monotonic clock so a re-fetched cover gets a distinct `updated_at`. */
let coverRevision = 0;
/** When set, the next `fetch_cover_art` rejects with this message. */
let coverFetchError: string | null = null;

/**
 * SteamGridDB key state for the stub. It tracks only whether a key is stored —
 * exactly like the Rust command surface, which never returns the key itself.
 */
let artworkStatus: ArtworkStatus = { configured: false };
/** When set, `set_steamgriddb_api_key` rejects with this generic message. */
let artworkSaveError: string | null = null;

/** Seed the stored-key flag before rendering (Settings fetches it on open). */
export function seedArtworkStatus(configured: boolean): void {
  artworkStatus = { configured };
}

/** Makes every key save reject with this generic message until reinstalled. */
export function failArtworkKeySave(message: string): void {
  artworkSaveError = message;
}

/**
 * Seed a cached cover for one game before rendering.
 *
 * `cover_path` is set as well: `App` only asks for a URL when the path is
 * non-null, so without it no image would ever appear.
 */
export function seedCover(gameId: string): void {
  seededCovers.add(gameId);
  setCoverPath(gameId, `covers/${gameId}.png`);
}

/** Makes the next `fetch_cover_art` reject with this message, then clear it. */
export function failCoverFetch(message: string): void {
  coverFetchError = message;
}

/** Fixtures are exported so tests can assert against the exact ids returned. */
export const fixtures = {
  primaryGameId: PRIMARY_GAME_ID,
  games,
  revisions,
  conflicts,
} as const;

function restoreReport(revisionId: string): RestoreReport {
  return {
    restore_id: "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
    revision_id: revisionId,
    files_restored: 3,
    bytes_restored: 48213,
    safety_backup_manifest_hash:
      "sha256:5555555555555555555555555555555555555555555555555555555555555555",
    safety_backup_path: "C:\\savesync-test\\safety-backups\\dddddddd",
  };
}

function assertKnownGame(args: Record<string, unknown>): void {
  if (!games.some((game) => game.id === args.game_id)) {
    throw new Error(`Tauri stub: unknown game_id ${String(args.game_id)}`);
  }
}

/**
 * Point one game at a cached cover (or clear it) and bump its `updated_at`,
 * mirroring the engine, which bumps the timestamp on every cover mutation.
 * Returns a copy so callers cannot alias the mutable fixture array.
 */
function setCoverPath(gameId: string, coverPath: string | null): Game {
  const game = games.find((candidate) => candidate.id === gameId);
  if (!game) {
    throw new Error(`Tauri stub: unknown game_id ${gameId}`);
  }
  coverRevision += 1;
  game.cover_path = coverPath;
  game.updated_at = `2026-01-06T00:00:${String(coverRevision % 60).padStart(2, "0")}Z`;
  return { ...game };
}

type Handler = (args: Record<string, unknown>) => unknown;

const handlers: Record<string, Handler> = {
  get_status: () => status,
  google_status: () => google,
  list_games: () => games,
  list_dirty_games: () => dirtyGameIds,
  history: (args) => {
    assertKnownGame(args);
    return args.game_id === PRIMARY_GAME_ID ? revisions : [];
  },
  list_conflicts: (args) => {
    assertKnownGame(args);
    return args.game_id === PRIMARY_GAME_ID ? conflicts : [];
  },
  restore_revision: (args) =>
    restoreReport(typeof args.revision_id === "string" ? args.revision_id : ""),
  steamgriddb_status: () => artworkStatus,
  set_steamgriddb_api_key: (args) => {
    if (artworkSaveError) {
      throw new Error(artworkSaveError);
    }
    const key = typeof args.api_key === "string" ? args.api_key.trim() : "";
    if (!key) {
      throw new Error("SteamGridDB API key is required.");
    }
    artworkStatus = { configured: true };
    return artworkStatus;
  },
  clear_steamgriddb_api_key: () => {
    artworkStatus = { configured: false };
    return artworkStatus;
  },
  cover_url: (args) => {
    const gameId = String(args.game_id);
    return seededCovers.has(gameId) ? `http://savesync-cover.localhost/${gameId}` : null;
  },
  fetch_cover_art: (args) => {
    assertKnownGame(args);
    const gameId = String(args.game_id);
    if (coverFetchError) {
      // One-shot, so a test can assert the retry path independently.
      const message = coverFetchError;
      coverFetchError = null;
      throw new Error(message);
    }
    // Mirrors the Rust command: the keyring is read inside the command and the
    // caller only ever sees a generic message.
    if (!artworkStatus.configured) {
      throw new Error("Configure the SteamGridDB API key in Settings first.");
    }
    seededCovers.add(gameId);
    return setCoverPath(gameId, `covers/${gameId}.png`);
  },
  clear_cover_art: (args) => {
    assertKnownGame(args);
    const gameId = String(args.game_id);
    seededCovers.delete(gameId);
    return setCoverPath(gameId, null);
  },
};

export function installTauriStub(): InvokeCall[] {
  const calls: InvokeCall[] = [];
  artworkStatus = { configured: false };
  artworkSaveError = null;

  // Handlers mutate the shared fixture array, so reset it in place from the
  // pristine copy: tests must not observe a previous file's cover state.
  games.length = 0;
  games.push(...baselineGames.map((game) => ({ ...game })));
  seededCovers.clear();
  coverFetchError = null;
  coverRevision = 0;

  const internals: TauriInternalsStub = {
    invoke: async (cmd, args = {}) => {
      calls.push({ cmd, args });
      const handler = handlers[cmd];
      if (!handler) {
        throw new Error(`Tauri stub: unexpected command "${cmd}"`);
      }
      return handler(args);
    },
    event: {
      listen: async () => () => {},
    },
  };

  (window as unknown as StubWindow).__TAURI_INTERNALS__ = internals;
  return calls;
}

export function uninstallTauriStub(): void {
  delete (window as unknown as StubWindow).__TAURI_INTERNALS__;
}
