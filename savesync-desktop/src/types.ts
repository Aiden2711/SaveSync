export type AdapterType =
  | "ludusavi"
  | "custom_directory"
  | "emulator"
  | "jksv";

export interface Game {
  id: string;
  title: string;
  adapter_type: AdapterType;
  enrolled: boolean;
  created_at: string;
  updated_at: string;
  active_head_revision_id: string | null;
  cover_path: string | null;
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
}
