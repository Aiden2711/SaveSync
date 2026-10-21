/**
 * Minimal bindings to the Tauri runtime.
 *
 * The official `@tauri-apps/api` package is not vendored in this workspace, so
 * these helpers call the stable runtime bridge directly. They throw a clear
 * error when the UI is opened in a plain browser instead of the desktop app.
 */

type TauriEvent = {
  listen: <T>(
    event: string,
    handler: (e: { payload: T }) => void,
  ) => Promise<() => void>;
};

type TauriInternals = {
  invoke: <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
  event?: TauriEvent;
};

function runtime(): TauriInternals {
  const internals = (
    window as unknown as { __TAURI_INTERNALS__?: TauriInternals }
  ).__TAURI_INTERNALS__;
  if (!internals) {
    throw new Error(
      "SaveSync must run inside the desktop shell (run `npm run tauri dev`).",
    );
  }
  return internals;
}

export function invoke<T>(
  command: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  return runtime().invoke<T>(command, args);
}

export function isDesktop(): boolean {
  return Boolean(
    (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__,
  );
}

export function listenEvent<T>(
  event: string,
  handler: (e: { payload: T }) => void,
): Promise<() => void> {
  const internals = runtime();
  if (!internals.event?.listen) {
    // Silent no-op in environments without event support (e.g., browser)
    return Promise.resolve(() => {});
  }
  return internals.event.listen<T>(event, handler);
}
