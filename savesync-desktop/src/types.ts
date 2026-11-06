export type AdapterType =
  | "ludusavi"
  | "lutris"
  | "steam"
  | "custom_directory"
  | "emulator"
  | "jksv";

/**
 * Which cached cover variant to ask for. Portrait is the default everywhere so
 * an omitted kind keeps every legacy code path byte-identical.
 */
export type CoverKind = "portrait" | "wide";

export interface Game {
  id: string;
  title: string;
  adapter_type: AdapterType;
  enrolled: boolean;
  created_at: string;
  updated_at: string;
  active_head_revision_id: string | null;
  cover_path: string | null;
  /** Cached 920x430 banner cover, if one was chosen. */
  wide_cover_path: string | null;
  /** Whether this game is the library's single featured (banner) game. */
  featured: boolean;
}

export interface Revision {
  id: string;
  game_id: string;
  parent_ids: string[];
  device_id: string;
  created_at: string;
  manifest_hash: string;
  blob_hash: string | null;
  message: string | null;
}

export interface Conflict {
  id: string;
  game_id: string;
  revision_ids: string[];
  detected_at: string;
  resolved_at: string | null;
  resolution_revision_id: string | null;
}

export interface SnapshotInfo {
  revision_id: string;
  manifest_hash: string;
  blob_hash: string;
  file_count: number;
  total_bytes: number;
}

export interface RestoreReport {
  restore_id: string;
  revision_id: string;
  files_restored: number;
  bytes_restored: number;
  safety_backup_manifest_hash: string;
  safety_backup_path: string;
}

export interface SyncReport {
  game_id: string;
  pulled_revisions: number;
  pushed_revisions: number;
  downloaded_blobs: number;
  uploaded_blobs: number;
  heads: number;
  conflict_ids: string[];
}

export interface GoogleStatus {
  client_id: string | null;
  connected: boolean;
  pending: boolean;
  error: string | null;
}

/** Remote provider the Cloud view is operating on. Matches the Rust enum. */
export type ProviderKind = "google" | "dropbox" | "webdav";

/**
 * Provider status shared by every backend. Carries the non-secret identifier
 * (client id, app key, endpoint) and flags only: a token or password is never
 * part of this shape.
 */
export interface ProviderStatus {
  kind: ProviderKind;
  identifier: string | null;
  configured: boolean;
  connected: boolean;
  pending: boolean;
  error: string | null;
}

/**
 * Whether a SteamGridDB API key is stored in the OS keyring. The key itself is
 * never sent to the frontend: this shape deliberately carries only a flag.
 */
export interface ArtworkStatus {
  configured: boolean;
}

/**
 * One downloadable cover candidate, exactly as `GridCandidate` serializes.
 *
 * `id` is the only value handed back when the user chooses a thumbnail; the
 * backend matches it against a freshly fetched list and never treats it as a
 * path or URL. `mime` is advisory: the cached file name follows the bytes.
 */
export interface GridCandidate {
  id: number;
  url: string;
  width: number;
  height: number;
  mime: string;
}

/**
 * A SteamGridDB match and its ranked candidates (`CoverSearch` in the core).
 * `candidates` may be empty when the matched game has no usable grids.
 */
export interface CoverSearch {
  sgdb_game_id: number;
  candidates: GridCandidate[];
}

/**
 * Where a game's metadata came from, matching the core `MetadataSource` ids.
 *
 * `screenscraper` is one word because that is the brand name and the identifier
 * the core stores; splitting it would desynchronize the stored value.
 */
export type MetadataSource = "steam" | "screenscraper" | "igdb";

/**
 * Normalized metadata for one game (`GameMetadata` in the core).
 *
 * Optional fields are `null` and lists empty when the provider has no answer,
 * never invented. `source_id` is provider-local and must be treated as opaque.
 */
export interface GameMetadata {
  source: MetadataSource;
  source_id: string;
  name: string;
  release_year: number | null;
  release_date: string | null;
  genres: string[];
  developers: string[];
  publishers: string[];
  description_short: string | null;
  description_long: string | null;
  players: string | null;
  age_rating: string | null;
  franchise: string | null;
  themes: string[];
  /** RFC 3339 timestamp of the fetch that produced this row. */
  fetched_at: string;
}

/**
 * Which ScreenScraper credential pairs are stored, exactly as
 * `ScreenScraperStatus` serializes.
 *
 * Flags only: no part of a credential — not even its length — crosses IPC.
 * `configured` is the developer pair, which is ScreenScraper's minimum; the user
 * account only raises the request quota.
 */
export interface ScreenScraperStatus {
  configured: boolean;
  dev_configured: boolean;
  user_configured: boolean;
}

/**
 * Which IGDB (Twitch application) values are stored, exactly as `IgdbStatus`
 * serializes. Flags only: neither the client id nor the secret crosses IPC.
 */
export interface IgdbStatus {
  configured: boolean;
  client_id_configured: boolean;
  client_secret_configured: boolean;
}

/**
 * The source a manual metadata fetch should use. `auto` is not stored anywhere:
 * it tells the backend to pick ScreenScraper, then IGDB, then Steam.
 */
export type MetadataSourcePreference = "auto" | MetadataSource;

export interface AppStatus {
  device: string;
  platform: string;
  startup_warning: string | null;
}

export interface OAuthStart {
  auth_url: string;
  redirect_uri: string;
}

export interface DirtyEvent {
  game_id: string;
  reason: "quiet" | "max_hold";
  generation: number;
}

export type DiscoverySource =
  | { type: "ludusavi"; title: string }
  | { type: "lutris"; slug: string }
  | { type: "steam"; appid: number }
  | { type: "proton"; appid: number };

export interface DiscoveredGame {
  title: string;
  adapter_type: AdapterType;
  save_paths: string[];
  source: DiscoverySource;
  /** Candidates dropped because they were not enrolable; never fails a scan. */
  skipped_paths: number;
}
