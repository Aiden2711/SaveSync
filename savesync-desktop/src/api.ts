import { invoke, listenEvent } from "./tauri";
import type {
  AppStatus,
  ArtworkStatus,
  Conflict,
  DirtyEvent,
  DiscoveredGame,
  Game,
  GoogleStatus,
  OAuthStart,
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

  setSteamgriddbApiKey: (api_key: string) =>
    invoke<ArtworkStatus>("set_steamgriddb_api_key", { api_key }),

  steamgriddbStatus: () => invoke<ArtworkStatus>("steamgriddb_status"),

  clearSteamgriddbApiKey: () => invoke<ArtworkStatus>("clear_steamgriddb_api_key"),

  sync: (game_id: string) => invoke<SyncReport>("sync_game", { game_id }),

  unlinkGame: (game_id: string) => invoke<Game>("unlink_game", { game_id }),

  listDirtyGames: () => invoke<string[]>("list_dirty_games"),

  listenDirtyEvent: (callback: (event: DirtyEvent) => void) => {
    return listenEvent<DirtyEvent>("dirty-event", (event) => {
      callback(event.payload);
    });
  },
};
