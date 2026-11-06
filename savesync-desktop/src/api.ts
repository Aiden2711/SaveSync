import { invoke, listenEvent } from "./tauri";
import type {
  AppStatus,
  ArtworkStatus,
  Conflict,
  CoverKind,
  CoverSearch,
  DirtyEvent,
  DiscoveredGame,
  Game,
  GoogleStatus,
  OAuthStart,
  ProviderKind,
  ProviderStatus,
  RestoreReport,
  Revision,
  SnapshotInfo,
  SyncReport,
} from "./types";

export const api = {
  status: () => invoke<AppStatus>("get_status"),

  listGames: () => invoke<Game[]>("list_games"),

  discoverGames: () => invoke<DiscoveredGame[]>("discover_games"),

  enrollGame: (title: string, adapter: string, path: string) =>
    invoke<Game>("enroll_game", { title, adapter, path }),

  addLocation: (game_id: string, path: string) =>
    invoke<void>("add_location", { game_id, path }),

  snapshot: (game_id: string, message?: string) =>
    invoke<SnapshotInfo>("snapshot_game", {
      game_id,
      message: message?.trim() ? message.trim() : null,
    }),

  history: (game_id: string) => invoke<Revision[]>("history", { game_id }),

  restore: (revision_id: string, exact_mirror: boolean) =>
    invoke<RestoreReport>("restore_revision", { revision_id, exact_mirror }),

  conflicts: (game_id: string) =>
    invoke<Conflict[]>("list_conflicts", { game_id }),

  resolve: (conflict_id: string, revision_id: string) =>
    invoke<Revision>("resolve_conflict", { conflict_id, revision_id }),

  setGoogleClientId: (client_id: string) =>
    invoke<GoogleStatus>("set_google_client_id", { client_id }),

  googleStatus: () => invoke<GoogleStatus>("google_status"),

  beginGoogleOAuth: () => invoke<OAuthStart>("begin_google_oauth"),

  openAuthUrl: (url: string) => invoke<void>("open_auth_url", { url }),

  disconnectGoogle: () => invoke<GoogleStatus>("disconnect_google"),

  setDropboxAppKey: (app_key: string) =>
    invoke<ProviderStatus>("set_dropbox_app_key", { app_key }),

  dropboxStatus: () => invoke<ProviderStatus>("dropbox_status"),

  beginDropboxOAuth: () => invoke<OAuthStart>("begin_dropbox_oauth"),

  disconnectDropbox: () => invoke<ProviderStatus>("disconnect_dropbox"),

  /** Stores the endpoint and username, and the password in the OS keyring. */
  setWebdavConfig: (endpoint: string, username: string, password: string) =>
    invoke<ProviderStatus>("set_webdav_config", { endpoint, username, password }),

  webdavStatus: () => invoke<ProviderStatus>("webdav_status"),

  disconnectWebdav: () => invoke<ProviderStatus>("disconnect_webdav"),

  setActiveProvider: (provider: ProviderKind) =>
    invoke<ProviderStatus>("set_active_provider", { provider }),

  activeProvider: () => invoke<ProviderKind>("active_provider"),

  setSteamgriddbApiKey: (api_key: string) =>
    invoke<ArtworkStatus>("set_steamgriddb_api_key", { api_key }),

  steamgriddbStatus: () => invoke<ArtworkStatus>("steamgriddb_status"),

  clearSteamgriddbApiKey: () => invoke<ArtworkStatus>("clear_steamgriddb_api_key"),

  fetchCoverArt: (game_id: string) => invoke<Game>("fetch_cover_art", { game_id }),

  /**
   * Ranked cover candidates, or `null` when SteamGridDB has no match at all.
   *
   * `kind` is omitted from the IPC payload for portrait so an existing call
   * site sends exactly the args it always did.
   */
  searchCoverCandidates: (game_id: string, kind: CoverKind = "portrait") =>
    kind === "portrait"
      ? invoke<CoverSearch | null>("search_cover_candidates", { game_id })
      : invoke<CoverSearch | null>("search_cover_candidates", { game_id, kind }),

  setCoverGrid: (game_id: string, grid_id: number, kind: CoverKind = "portrait") =>
    kind === "portrait"
      ? invoke<Game>("set_cover_grid", { game_id, grid_id })
      : invoke<Game>("set_cover_grid", { game_id, grid_id, kind }),

  clearCoverArt: (game_id: string) => invoke<Game>("clear_cover_art", { game_id }),

  coverUrl: (game_id: string, kind: CoverKind = "portrait") =>
    kind === "portrait"
      ? invoke<string | null>("cover_url", { game_id })
      : invoke<string | null>("cover_url", { game_id, kind }),

  setFeatured: (game_id: string) => invoke<Game>("set_featured", { game_id }),

  clearFeatured: () => invoke<null>("clear_featured"),

  sync: (game_id: string) => invoke<SyncReport>("sync_game", { game_id }),

  unlinkGame: (game_id: string) => invoke<Game>("unlink_game", { game_id }),

  listDirtyGames: () => invoke<string[]>("list_dirty_games"),

  listenDirtyEvent: (callback: (event: DirtyEvent) => void) => {
    return listenEvent<DirtyEvent>("dirty-event", (event) => {
      callback(event.payload);
    });
  },
};
