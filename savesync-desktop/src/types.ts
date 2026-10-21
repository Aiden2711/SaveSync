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
