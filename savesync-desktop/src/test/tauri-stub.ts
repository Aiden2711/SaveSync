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
  CoverSearch,
  Game,
  GameMetadata,
  GoogleStatus,
  MetadataSource,
  ProviderKind,
  ProviderStatus,
  RestoreReport,
  Revision,
  ScreenScraperStatus,
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
const THIRD_GAME_ID = "33333333-3333-4333-8333-333333333333";
const FOURTH_GAME_ID = "44444444-4444-4444-8444-444444444444";

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

/**
 * Pristine Google status. `seedGoogleStatus` mutates the object in place, so
 * every `installTauriStub` restores this baseline to keep one file's
 * connected/pending/error choice from leaking into the next.
 */
const baselineGoogle: GoogleStatus = { ...google };

/**
 * Provider state for the Cloud view. Reset by `installTauriStub` so one file's
 * choices never leak into another's assertions.
 */
function defaultDropboxStatus(): ProviderStatus {
  return {
    kind: "dropbox",
    identifier: null,
    configured: false,
    connected: false,
    pending: false,
    error: null,
  };
}

function defaultWebdavStatus(): ProviderStatus {
  return {
    kind: "webdav",
    identifier: null,
    configured: false,
    connected: false,
    pending: false,
    error: null,
  };
}

let activeProvider: ProviderKind = "google";
let dropboxStatus: ProviderStatus = defaultDropboxStatus();
let webdavStatus: ProviderStatus = defaultWebdavStatus();

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
    wide_cover_path: null,
    featured: false,
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
    wide_cover_path: null,
    featured: false,
  },
  {
    id: THIRD_GAME_ID,
    title: "Celeste",
    adapter_type: "emulator",
    enrolled: true,
    created_at: "2026-01-03T08:00:00Z",
    updated_at: "2026-01-03T08:00:00Z",
    active_head_revision_id: null,
    cover_path: null,
    wide_cover_path: null,
    featured: false,
  },
  {
    id: FOURTH_GAME_ID,
    title: "Stardew Valley",
    adapter_type: "jksv",
    enrolled: true,
    created_at: "2026-01-03T09:30:00Z",
    updated_at: "2026-01-03T09:30:00Z",
    active_head_revision_id: null,
    cover_path: null,
    wide_cover_path: null,
    featured: false,
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
 * Pristine copy of the dirty-id fixture. `seedDirtyGames` replaces the array in
 * place, so every `installTauriStub` puts the default back.
 */
const baselineDirtyGameIds: string[] = [SECOND_GAME_ID];

/**
 * Pristine copy of the game fixtures. Handlers mutate `cover_path` and
 * `updated_at`, so every `installTauriStub` restores the shared array in place
 * to keep test order from leaking state into the next file.
 */
const baselineGames: Game[] = games.map((game) => ({ ...game }));

/** Game ids with a cached portrait cover in the stub's fake blob store. */
const seededCovers = new Set<string>();
/** Game ids with a cached wide cover; kept separate, exactly like the engine. */
const seededWideCovers = new Set<string>();
/** Monotonic clock so a re-fetched cover gets a distinct `updated_at`. */
let coverRevision = 0;
/** When set, the next `fetch_cover_art` rejects with this message. */
let coverFetchError: string | null = null;

/**
 * What `search_cover_candidates` returns. Reset to the two-candidate default
 * on every install so tests never observe a previous file's fixtures.
 */
function defaultCoverSearch(): CoverSearch {
  return {
    sgdb_game_id: 7,
    candidates: [
      {
        id: 101,
        url: "https://cdn2.steamgriddb.com/grid/101.png",
        width: 600,
        height: 900,
        mime: "image/png",
      },
      {
        id: 202,
        url: "https://cdn2.steamgriddb.com/grid/202.png",
        width: 600,
        height: 900,
        mime: "image/png",
      },
    ],
  };
}

let coverSearchResponse: CoverSearch | null = defaultCoverSearch();
/** When set, the next `search_cover_candidates` rejects with this message. */
let coverSearchError: string | null = null;
/** When set, the next search waits for it to resolve (loading-state tests). */
let coverSearchGate: Promise<void> | null = null;

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
 * ScreenScraper credential flags for the stub.
 *
 * Flags only, exactly like the Rust `ScreenScraperStatus`: the stub never holds
 * a credential, so a test that reads state here cannot see one.
 */
let screenscraperStatus: ScreenScraperStatus = defaultScreenscraperStatus();
/** When set, the next credential save rejects with this generic message. */
let screenscraperSaveError: string | null = null;

/** Cached metadata per game id; `fetch_game_metadata` fills it. */
const metadataCache = new Map<string, GameMetadata>();
/** When set, the next `fetch_game_metadata` rejects with this message. */
let metadataFetchError: string | null = null;

/** Seed which ScreenScraper pairs the settings form should show as saved. */
export function seedScreenscraperStatus(patch: Partial<ScreenScraperStatus>): void {
  screenscraperStatus = { ...screenscraperStatus, ...patch };
}

/** Makes the next `set_screenscraper_credentials` reject with this message. */
export function failScreenscraperCredentialSave(message: string): void {
  screenscraperSaveError = message;
}

/** Makes the next `fetch_game_metadata` reject with this message, then clear it. */
export function failMetadataFetch(message: string): void {
  metadataFetchError = message;
}

/**
 * Seed a cached metadata row before rendering.
 *
 * Mirrors `game_metadata`: the detail view reads this without a network call,
 * so a test can assert the offline path. Returns the stored row.
 */
export function seedGameMetadata(
  gameId: string,
  patch: Partial<GameMetadata> = {},
): GameMetadata {
  const metadata: GameMetadata = { ...metadataFor(gameId, "steam"), ...patch };
  metadataCache.set(gameId, metadata);
  return metadata;
}

/** Mark Dropbox connected, as the real flow does once sign-in completes. */
export function seedDropboxConnected(identifier = "test-app-key"): void {
  dropboxStatus = {
    kind: "dropbox",
    identifier,
    configured: true,
    connected: true,
    pending: false,
    error: null,
  };
}

/**
 * Seed a non-Google provider's Cloud-view state before rendering: `pending`
 * for the OAuth wait, `error` for a rejected credential, `connected` with an
 * identifier. Google carries its own shape, so it uses `seedGoogleStatus`.
 */
export function seedProviderStatus(
  kind: "dropbox" | "webdav",
  patch: Partial<Omit<ProviderStatus, "kind">>,
): void {
  const base = kind === "dropbox" ? dropboxStatus : webdavStatus;
  const next: ProviderStatus = { ...base, ...patch, kind };
  if (kind === "dropbox") dropboxStatus = next;
  else webdavStatus = next;
}

/** Seed Google's Cloud-view state (connected, pending, or a surfaced error). */
export function seedGoogleStatus(patch: Partial<GoogleStatus>): void {
  Object.assign(google, patch);
}

/** Choose the provider the stub reports as active before rendering. */
export function seedActiveProvider(kind: ProviderKind): void {
  activeProvider = kind;
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

/**
 * Seed a cached wide cover, mirroring `wide_cover_path` on the Rust model.
 *
 * Portrait and wide caches are independent, so this never makes
 * `cover_url({ game_id })` return anything.
 */
export function seedWideCover(gameId: string): void {
  seededWideCovers.add(gameId);
  setWideCoverPath(gameId, `covers/${gameId}-wide.png`);
}

/** Game id marked as the library's featured banner, if any. */
export function seedFeatured(gameId: string): void {
  for (const game of games) game.featured = game.id === gameId;
}

/** Makes the next `fetch_cover_art` reject with this message, then clear it. */
export function failCoverFetch(message: string): void {
  coverFetchError = message;
}

/** Replace the candidate list returned by `search_cover_candidates`. */
export function seedCoverCandidates(candidates: CoverSearch["candidates"]): void {
  coverSearchResponse = { sgdb_game_id: 7, candidates };
}

/** Make the next search report "no SteamGridDB match" (`null`). */
export function hideCoverCandidates(): void {
  coverSearchResponse = null;
}

/** Makes the next `search_cover_candidates` reject with this message. */
export function failCoverSearch(message: string): void {
  coverSearchError = message;
}

/**
 * Hold the next `search_cover_candidates` open until the returned function is
 * called, so a test can observe the modal's loading state deterministically.
 */
export function stallNextCoverSearch(): () => void {
  let release!: () => void;
  coverSearchGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  return () => release();
}

/**
 * Replace the enrolled game list before rendering.
 *
 * Call after `installTauriStub`, which restores the default fixtures. Tests use
 * this for the empty-library and single-game status readouts.
 */
export function seedGames(next: Game[]): void {
  games.length = 0;
  games.push(...next.map((game) => ({ ...game })));
}

/** Replace the ids `list_dirty_games` reports before rendering. */
export function seedDirtyGames(gameIds: string[]): void {
  dirtyGameIds.length = 0;
  dirtyGameIds.push(...gameIds);
}

/** Fixtures are exported so tests can assert against the exact ids returned. */
export const fixtures = {
  primaryGameId: PRIMARY_GAME_ID,
  secondGameId: SECOND_GAME_ID,
  thirdGameId: THIRD_GAME_ID,
  games,
  revisions,
  conflicts,
  coverCandidates: defaultCoverSearch().candidates,
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
  const game = findGame(gameId);
  coverRevision += 1;
  game.cover_path = coverPath;
  game.updated_at = `2026-01-06T00:00:${String(coverRevision % 60).padStart(2, "0")}Z`;
  return { ...game };
}

function findGame(gameId: string): Game {
  const game = games.find((candidate) => candidate.id === gameId);
  if (!game) {
    throw new Error(`Tauri stub: unknown game_id ${gameId}`);
  }
  return game;
}

/** Wide counterpart of {@link setCoverPath}; leaves `cover_path` untouched. */
function setWideCoverPath(gameId: string, wideCoverPath: string | null): Game {
  const game = findGame(gameId);
  coverRevision += 1;
  game.wide_cover_path = wideCoverPath;
  game.updated_at = `2026-01-06T00:00:${String(coverRevision % 60).padStart(2, "0")}Z`;
  return { ...game };
}

type Handler = (args: Record<string, unknown>) => unknown;

/** All-false credential flags; also the state `clear_...` returns to. */
function defaultScreenscraperStatus(): ScreenScraperStatus {
  return { configured: false, dev_configured: false, user_configured: false };
}

/**
 * Plausible metadata for a fixture game.
 *
 * Built from the game so a test never has to hardcode a title twice, and never
 * carries anything credential-shaped.
 */
function metadataFor(gameId: string, source: MetadataSource): GameMetadata {
  const game = findGame(gameId);
  return {
    source,
    source_id: `stub-${gameId.slice(0, 8)}`,
    name: game.title,
    release_year: 2020,
    release_date: "17 Sep, 2020",
    genres: ["Action", "Roguelike"],
    developers: ["Supergiant Games"],
    publishers: ["Supergiant Games"],
    description_short: "A rogue-like dungeon crawler.",
    description_long: "Defy the god of the dead as you hack and slash out of the Underworld.",
    players: "1",
    age_rating: "T",
    franchise: game.title,
    themes: ["Mythology"],
    fetched_at: "2026-01-06T00:00:00Z",
  };
}

/** Status of the provider a command would report, matching the Rust shapes. */
function providerStatus(kind: ProviderKind): ProviderStatus {
  if (kind === "dropbox") return dropboxStatus;
  if (kind === "webdav") return webdavStatus;
  return {
    kind: "google",
    identifier: google.client_id,
    configured: google.client_id !== null,
    connected: google.connected,
    pending: google.pending,
    error: google.error,
  };
}

const handlers: Record<string, Handler> = {
  get_status: () => status,
  google_status: () => google,
  active_provider: () => activeProvider,
  set_active_provider: (args) => {
    const next = args.provider;
    if (next !== "google" && next !== "dropbox" && next !== "webdav") {
      throw new Error(`Tauri stub: unknown provider "${String(next)}"`);
    }
    activeProvider = next;
    return providerStatus(next);
  },
  dropbox_status: () => dropboxStatus,
  set_dropbox_app_key: (args) => {
    const key = typeof args.app_key === "string" ? args.app_key.trim() : "";
    if (!key) {
      throw new Error("Dropbox app key may not be empty");
    }
    // Mirrors the command: a different app key invalidates the old token.
    const tokenSurvives = dropboxStatus.identifier === key;
    dropboxStatus = {
      kind: "dropbox",
      identifier: key,
      configured: true,
      connected: tokenSurvives && dropboxStatus.connected,
      pending: false,
      error: null,
    };
    return dropboxStatus;
  },
  begin_dropbox_oauth: () => ({
    auth_url: "https://www.dropbox.com/oauth2/authorize?client_id=test-app-key",
    redirect_uri: "http://127.0.0.1:53682/callback",
  }),
  // The manual consent-page fallback. The command validates the URL in Rust, so
  // the stub stays a no-op and the call itself is what tests assert on.
  open_auth_url: () => undefined,
  // Documentation links: same contract — Rust owns the allowlist.
  open_docs_url: () => undefined,
  disconnect_dropbox: () => {
    dropboxStatus = { ...dropboxStatus, connected: false, pending: false, error: null };
    return dropboxStatus;
  },
  webdav_status: () => webdavStatus,
  set_webdav_config: (args) => {
    const endpoint = typeof args.endpoint === "string" ? args.endpoint.trim() : "";
    if (!endpoint) {
      throw new Error("WebDAV endpoint may not be empty");
    }
    const username = typeof args.username === "string" ? args.username.trim() : "";
    const password = typeof args.password === "string" ? args.password : "";
    if (!username && !webdavStatus.configured) {
      throw new Error("WebDAV username may not be empty");
    }
    if (!password && !webdavStatus.connected) {
      throw new Error("Enter the WebDAV password.");
    }
    webdavStatus = {
      kind: "webdav",
      identifier: endpoint,
      configured: true,
      connected: true,
      pending: false,
      error: null,
    };
    return webdavStatus;
  },
  disconnect_webdav: () => {
    webdavStatus = { ...webdavStatus, connected: false, error: null };
    return webdavStatus;
  },
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
  screenscraper_status: () => screenscraperStatus,
  set_screenscraper_credentials: (args) => {
    if (screenscraperSaveError) {
      // One-shot, so a retry can be asserted independently.
      const message = screenscraperSaveError;
      screenscraperSaveError = null;
      throw new Error(message);
    }
    const field = (value: unknown) => (typeof value === "string" ? value.trim() : "");
    // Blank keeps the stored value, exactly like the command.
    const devIdSet = field(args.dev_id) !== "" || screenscraperStatus.dev_configured;
    const devPasswordSet =
      field(args.dev_password) !== "" || screenscraperStatus.dev_configured;
    if (!devIdSet || !devPasswordSet) {
      throw new Error("Enter the ScreenScraper dev id and dev password.");
    }
    const ssidSet = field(args.ssid) !== "" || screenscraperStatus.user_configured;
    const sspasswordSet =
      field(args.sspassword) !== "" || screenscraperStatus.user_configured;
    if (ssidSet !== sspasswordSet) {
      throw new Error("Enter both the ScreenScraper user SSID and user SS password.");
    }
    screenscraperStatus = {
      configured: true,
      dev_configured: true,
      user_configured: ssidSet,
    };
    return screenscraperStatus;
  },
  clear_screenscraper_credentials: () => {
    screenscraperStatus = defaultScreenscraperStatus();
    return screenscraperStatus;
  },
  /**
   * Fetch metadata. An omitted source follows the command: ScreenScraper when
   * credentials are stored, the keyless Steam Store otherwise.
   */
  fetch_game_metadata: (args) => {
    assertKnownGame(args);
    const gameId = String(args.game_id);
    if (metadataFetchError) {
      const message = metadataFetchError;
      metadataFetchError = null;
      throw new Error(message);
    }
    const requested = typeof args.source === "string" ? args.source : undefined;
    if (
      requested !== undefined &&
      requested !== "steam" &&
      requested !== "screenscraper" &&
      requested !== "igdb"
    ) {
      throw new Error(`Tauri stub: unknown metadata source "${requested}"`);
    }
    const source: MetadataSource =
      requested ?? (screenscraperStatus.configured ? "screenscraper" : "steam");
    if (source === "screenscraper" && !screenscraperStatus.configured) {
      // The command maps core's `Auth` to this generic text, never a value.
      throw new Error(
        "ScreenScraper rejected the saved credentials. Check the developer credentials and, if one is saved, the user account.",
      );
    }
    if (source === "igdb") {
      throw new Error("Could not fetch metadata for this game.");
    }
    const metadata = metadataFor(gameId, source);
    metadataCache.set(gameId, metadata);
    return metadata;
  },
  game_metadata: (args) => metadataCache.get(String(args.game_id)) ?? null,
  cover_url: (args) => {
    const gameId = String(args.game_id);
    // The Rust command defaults to portrait, so a missing kind must too.
    if (args.kind === "wide") {
      return seededWideCovers.has(gameId)
        ? `http://savesync-cover.localhost/${gameId}?kind=wide`
        : null;
    }
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
  search_cover_candidates: async (args) => {
    assertKnownGame(args);
    if (!artworkStatus.configured) {
      throw new Error("Configure the SteamGridDB API key in Settings first.");
    }
    // Consumed before awaiting so a second search cannot stall on the same gate.
    const gate = coverSearchGate;
    coverSearchGate = null;
    if (gate) await gate;
    if (coverSearchError) {
      // One-shot, so a test can assert the retry path independently.
      const message = coverSearchError;
      coverSearchError = null;
      throw new Error(message);
    }
    return coverSearchResponse;
  },
  set_cover_grid: (args) => {
    assertKnownGame(args);
    const gameId = String(args.game_id);
    if (!artworkStatus.configured) {
      throw new Error("Configure the SteamGridDB API key in Settings first.");
    }
    const gridId = Number(args.grid_id);
    const available = coverSearchResponse?.candidates ?? [];
    if (!available.some((candidate) => candidate.id === gridId)) {
      throw new Error("That cover is no longer available. Pick another.");
    }
    // Wide writes the wide column only, mirroring `set_cover_grid_kind`.
    if (args.kind === "wide") {
      seededWideCovers.add(gameId);
      return setWideCoverPath(gameId, `covers/${gameId}-wide.png`);
    }
    seededCovers.add(gameId);
    return setCoverPath(gameId, `covers/${gameId}.png`);
  },
  set_featured: (args) => {
    const game = findGame(String(args.game_id));
    for (const candidate of games) candidate.featured = candidate.id === game.id;
    return { ...game };
  },
  clear_featured: () => {
    for (const candidate of games) candidate.featured = false;
    return null;
  },
};

export function installTauriStub(): InvokeCall[] {
  const calls: InvokeCall[] = [];
  artworkStatus = { configured: false };
  artworkSaveError = null;
  Object.assign(google, baselineGoogle);
  activeProvider = "google";
  dropboxStatus = defaultDropboxStatus();
  webdavStatus = defaultWebdavStatus();

  // Handlers mutate the shared fixture array, so reset it in place from the
  // pristine copy: tests must not observe a previous file's cover state.
  games.length = 0;
  games.push(...baselineGames.map((game) => ({ ...game })));
  dirtyGameIds.length = 0;
  dirtyGameIds.push(...baselineDirtyGameIds);
  seededCovers.clear();
  seededWideCovers.clear();
  coverFetchError = null;
  coverSearchResponse = defaultCoverSearch();
  coverSearchError = null;
  coverSearchGate = null;
  coverRevision = 0;
  screenscraperStatus = defaultScreenscraperStatus();
  screenscraperSaveError = null;
  metadataCache.clear();
  metadataFetchError = null;

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
