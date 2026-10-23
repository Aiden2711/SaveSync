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
};

export function installTauriStub(): InvokeCall[] {
  const calls: InvokeCall[] = [];

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
