//! SaveSync desktop shell.
//!
//! Thin Tauri command layer around `savesync-core`. All destructive work
//! (snapshots, restores, conflict resolution) goes through the engine's safety
//! invariants; this layer only marshals arguments, owns the OAuth loopback
//! flow, and persists the refresh token in the OS keyring.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use savesync_core::{
    AdapterType, Conflict, DiscoveredGame, Game, GoogleDriveProvider, HttpTransport,
    KeyringTokenStore, MemoryTokenStore, OAuthConfig, OAuthFlow, ReqwestTransport, RestoreReport,
    Revision, SaveSyncEngine, SaveWatcher, SnapshotInfo, SyncReport, TokenStore,
};
use serde::Serialize;
use std::path::Path;
use tauri::{Emitter, Manager};

const GOOGLE_CLIENT_ID_KEY: &str = "google_client_id";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(300);

struct Inner {
    engine: Mutex<SaveSyncEngine>,
    watcher: Mutex<Option<SaveWatcher>>,
    credentials: Arc<dyn TokenStore>,
    transport: Arc<dyn HttpTransport>,
    client_id: Mutex<Option<String>>,
    oauth: Mutex<OAuthRuntime>,
    startup_warning: Mutex<Option<String>>,
}

#[derive(Default)]
struct OAuthRuntime {
    pending: bool,
    connected: bool,
    error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    inner: Arc<Inner>,
}

#[derive(Serialize)]
struct AppStatus {
    device: String,
    platform: String,
    startup_warning: Option<String>,
}

#[derive(Serialize)]
struct GoogleStatus {
    client_id: Option<String>,
    connected: bool,
    pending: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct OAuthStart {
    auth_url: String,
    redirect_uri: String,
}

#[tauri::command(rename_all = "snake_case")]
fn get_status(state: tauri::State<'_, AppState>) -> Result<AppStatus, String> {
    let engine = state.inner.engine.lock().map_err(lock_error)?;
    Ok(AppStatus {
        device: engine.device().name.clone(),
        platform: engine.device().platform.clone(),
        startup_warning: state
            .inner
            .startup_warning
            .lock()
            .map_err(lock_error)?
            .clone(),
    })
}

#[tauri::command(rename_all = "snake_case")]
fn list_games(state: tauri::State<'_, AppState>) -> Result<Vec<Game>, String> {
    let engine = state.inner.engine.lock().map_err(lock_error)?;
    // The library only shows enrolled games. Unlinked games remain stored and
    // recoverable, but must not appear in the library view.
    engine.list_games(true).map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn enroll_game(
    state: tauri::State<'_, AppState>,
    title: String,
    adapter: String,
    path: String,
) -> Result<Game, String> {
    let adapter =
        AdapterType::parse(&adapter).ok_or_else(|| format!("unknown adapter type: {adapter}"))?;
    // Validate before creating the game, so a bad path cannot leave a
    // half-enrolled title behind.
    let root = savesync_core::paths::validate_root(&path).map_err(stringify)?;

    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    let game = engine.enroll_game(&title, adapter).map_err(stringify)?;
    engine
        .add_directory_location(&game.id, &root)
        .map_err(stringify)?;

    // Start watching the enrolled game's save directory
    {
        let mut watcher_guard = state.inner.watcher.lock().map_err(lock_error)?;
        if let Some(watcher) = watcher_guard.as_mut()
            && let Err(e) = watcher.watch(&game.id, &root)
        {
            eprintln!(
                "Failed to watch game {} at {}: {}",
                game.id,
                root.display(),
                e
            );
        }
    }

    Ok(game)
}

#[tauri::command(rename_all = "snake_case")]
fn enroll_discovered(state: tauri::State<'_, AppState>, game_id: String) -> Result<Game, String> {
    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    engine.enroll(&game_id).map_err(stringify)
}

/// Discover save game candidates across Ludusavi, Lutris, and Steam sources.
///
/// This respects zero auto-enrollment: it returns candidates only and never
/// registers or enrolls any game.
#[tauri::command(rename_all = "snake_case")]
async fn discover_games(state: tauri::State<'_, AppState>) -> Result<Vec<DiscoveredGame>, String> {
    let inner = state.inner.clone();
    tauri::async_runtime::spawn_blocking(move || inner.discover_games())
        .await
        .map_err(|error| format!("discovery task failed: {error}"))?
}

#[tauri::command(rename_all = "snake_case")]
fn unlink_game(state: tauri::State<'_, AppState>, game_id: String) -> Result<Game, String> {
    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    let game = engine.unlink(&game_id).map_err(stringify)?;

    // Stop watching the unlinked game's save directory
    {
        let mut watcher_guard = state.inner.watcher.lock().map_err(lock_error)?;
        if let Some(watcher) = watcher_guard.as_mut()
            && let Err(e) = watcher.unwatch(&game_id)
        {
            eprintln!("Failed to unwatch game {}: {}", game_id, e);
        }
    }

    Ok(game)
}

#[tauri::command(rename_all = "snake_case")]
fn set_cover(
    state: tauri::State<'_, AppState>,
    game_id: String,
    cover_path: Option<String>,
) -> Result<Game, String> {
    if let Some(ref path_str) = cover_path {
        let path = Path::new(path_str);
        if !path.exists() {
            return Err(format!("cover file does not exist: {path_str}"));
        }
        if !path.is_file() {
            return Err(format!("cover path is not a file: {path_str}"));
        }
        // Basic image extension check
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        let is_image = matches!(
            ext.as_deref(),
            Some("png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tiff" | "ico")
        );
        if !is_image {
            return Err(format!(
                "cover file must be an image (png, jpg, jpeg, webp, bmp, gif, tiff, ico): {path_str}"
            ));
        }
    }

    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    engine
        .set_cover_path(&game_id, cover_path.as_deref())
        .map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn add_location(
    state: tauri::State<'_, AppState>,
    game_id: String,
    path: String,
) -> Result<(), String> {
    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    engine
        .add_directory_location(&game_id, &path)
        .map(|_| ())
        .map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn snapshot_game(
    state: tauri::State<'_, AppState>,
    game_id: String,
    message: Option<String>,
) -> Result<SnapshotInfo, String> {
    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    engine
        .snapshot_game(&game_id, message.as_deref())
        .map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn history(state: tauri::State<'_, AppState>, game_id: String) -> Result<Vec<Revision>, String> {
    let engine = state.inner.engine.lock().map_err(lock_error)?;
    engine.history(&game_id).map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn restore_revision(
    state: tauri::State<'_, AppState>,
    revision_id: String,
    exact_mirror: bool,
) -> Result<RestoreReport, String> {
    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    let mode = if exact_mirror {
        savesync_core::RestoreMode::ExactMirror
    } else {
        savesync_core::RestoreMode::Overlay
    };
    engine
        .restore_with_mode(&revision_id, mode)
        .map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn list_conflicts(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<Vec<Conflict>, String> {
    let engine = state.inner.engine.lock().map_err(lock_error)?;
    engine.open_conflicts(&game_id).map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn resolve_conflict(
    state: tauri::State<'_, AppState>,
    conflict_id: String,
    revision_id: String,
) -> Result<Revision, String> {
    let mut engine = state.inner.engine.lock().map_err(lock_error)?;
    engine
        .resolve_conflict_with(&conflict_id, &revision_id)
        .map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn set_google_client_id(
    state: tauri::State<'_, AppState>,
    client_id: String,
) -> Result<GoogleStatus, String> {
    let client_id = client_id.trim().to_string();
    if client_id.is_empty() {
        return Err("Google OAuth client id may not be empty".into());
    }
    if state.inner.oauth.lock().map_err(lock_error)?.pending {
        return Err(
            "Wait for the current Google sign-in to finish before changing the client id".into(),
        );
    }

    let previous = state.inner.client_id.lock().map_err(lock_error)?.clone();
    if previous.as_deref() != Some(client_id.as_str()) {
        // A token issued to a different client id is unusable.
        let _ = state.inner.credentials.delete();
        let mut oauth = state.inner.oauth.lock().map_err(lock_error)?;
        oauth.connected = false;
    }

    {
        let mut engine = state.inner.engine.lock().map_err(lock_error)?;
        engine
            .set_setting(GOOGLE_CLIENT_ID_KEY, &client_id)
            .map_err(stringify)?;
    }
    *state.inner.client_id.lock().map_err(lock_error)? = Some(client_id.clone());

    Ok(GoogleStatus {
        client_id: Some(client_id),
        connected: state.inner.oauth.lock().map_err(lock_error)?.connected,
        pending: state.inner.oauth.lock().map_err(lock_error)?.pending,
        error: state.inner.oauth.lock().map_err(lock_error)?.error.clone(),
    })
}

#[tauri::command(rename_all = "snake_case")]
fn google_status(state: tauri::State<'_, AppState>) -> Result<GoogleStatus, String> {
    let oauth = state.inner.oauth.lock().map_err(lock_error)?;
    Ok(GoogleStatus {
        client_id: state.inner.client_id.lock().map_err(lock_error)?.clone(),
        connected: oauth.connected,
        pending: oauth.pending,
        error: oauth.error.clone(),
    })
}

#[tauri::command(rename_all = "snake_case")]
fn begin_google_oauth(state: tauri::State<'_, AppState>) -> Result<OAuthStart, String> {
    let client_id = state
        .inner
        .client_id
        .lock()
        .map_err(lock_error)?
        .clone()
        .ok_or_else(|| "Save a Google OAuth client id first".to_string())?;

    {
        let oauth = state.inner.oauth.lock().map_err(lock_error)?;
        if oauth.pending {
            return Err("A Google sign-in is already in progress".into());
        }
    }

    let mut config = OAuthConfig::google(&client_id);
    if let Ok(secret) = std::env::var("SAVESYNC_GOOGLE_CLIENT_SECRET")
        && !secret.trim().is_empty()
    {
        config = config.with_client_secret(secret);
    }

    let flow = OAuthFlow::start(config).map_err(stringify)?;
    let auth_url = flow.authorization_url().map_err(stringify)?;
    let redirect_uri = flow.redirect_uri().to_string();

    {
        let mut oauth = state.inner.oauth.lock().map_err(lock_error)?;
        oauth.pending = true;
        oauth.error = None;
    }

    if let Err(error) = open::that(&auth_url) {
        eprintln!("could not open the system browser: {error}");
    }

    let inner = state.inner.clone();
    std::thread::spawn(move || {
        let result = flow.finish(inner.transport.as_ref(), OAUTH_TIMEOUT);
        let mut oauth = inner.oauth.lock().expect("oauth mutex");
        oauth.pending = false;
        match result {
            Ok(tokens) => match tokens.refresh_token {
                Some(refresh) => match inner.credentials.save(&refresh) {
                    Ok(()) => {
                        oauth.connected = true;
                        oauth.error = None;
                    }
                    Err(error) => {
                        oauth.error = Some(format!(
                            "Google sign-in succeeded, but the refresh token could not be saved: {error}"
                        ));
                    }
                },
                None => {
                    oauth.error = Some(
                        "Google did not return a refresh token. Remove SaveSync from your Google account permissions and try again."
                            .into(),
                    );
                }
            },
            Err(error) => {
                oauth.error = Some(error.to_string());
            }
        }
    });

    Ok(OAuthStart {
        auth_url,
        redirect_uri,
    })
}

#[tauri::command(rename_all = "snake_case")]
fn open_auth_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://accounts.google.com/") {
        return Err("refusing to open a non-Google authorization URL".into());
    }
    open::that(url).map_err(|error| format!("could not open the system browser: {error}"))
}

#[tauri::command(rename_all = "snake_case")]
fn disconnect_google(state: tauri::State<'_, AppState>) -> Result<GoogleStatus, String> {
    {
        let oauth = state.inner.oauth.lock().map_err(lock_error)?;
        if oauth.pending {
            return Err(
                "Wait for the current Google sign-in to finish before disconnecting".into(),
            );
        }
    }
    state.inner.credentials.delete().map_err(stringify)?;
    let client_id = state.inner.client_id.lock().map_err(lock_error)?.clone();
    let mut oauth = state.inner.oauth.lock().map_err(lock_error)?;
    oauth.connected = false;
    oauth.error = None;
    Ok(GoogleStatus {
        client_id,
        connected: false,
        pending: oauth.pending,
        error: None,
    })
}

#[tauri::command(rename_all = "snake_case")]
async fn sync_game(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<SyncReport, String> {
    let inner = state.inner.clone();
    tauri::async_runtime::spawn_blocking(move || inner.sync_game(&game_id))
        .await
        .map_err(|error| format!("sync task failed: {error}"))?
}

#[tauri::command(rename_all = "snake_case")]
fn watch_game(
    state: tauri::State<'_, AppState>,
    game_id: String,
    path: String,
) -> Result<(), String> {
    let mut watcher_guard = state.inner.watcher.lock().map_err(lock_error)?;
    let Some(watcher) = watcher_guard.as_mut() else {
        return Err("save watcher is not running".into());
    };
    let root = savesync_core::paths::validate_root(&path).map_err(stringify)?;
    watcher.watch(&game_id, &root).map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn unwatch_game(state: tauri::State<'_, AppState>, game_id: String) -> Result<(), String> {
    let mut watcher_guard = state.inner.watcher.lock().map_err(lock_error)?;
    let Some(watcher) = watcher_guard.as_mut() else {
        return Err("save watcher is not running".into());
    };
    watcher.unwatch(&game_id).map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn clear_dirty(
    state: tauri::State<'_, AppState>,
    game_id: String,
    generation: u64,
) -> Result<(), String> {
    let mut watcher_guard = state.inner.watcher.lock().map_err(lock_error)?;
    let Some(watcher) = watcher_guard.as_mut() else {
        return Err("save watcher is not running".into());
    };
    watcher.clear(&game_id, generation).map_err(stringify)
}

#[tauri::command(rename_all = "snake_case")]
fn list_dirty_games(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    let watcher_guard = state.inner.watcher.lock().map_err(lock_error)?;
    let Some(watcher) = watcher_guard.as_ref() else {
        return Ok(Vec::new());
    };
    Ok(watcher.pending_games())
}

impl Inner {
    fn discover_games(&self) -> Result<Vec<DiscoveredGame>, String> {
        let mut engine = self.engine.lock().map_err(lock_error)?;
        engine.discover_games().map_err(stringify)
    }

    fn sync_game(&self, game_id: &str) -> Result<SyncReport, String> {
        let client_id = self
            .client_id
            .lock()
            .map_err(lock_error)?
            .clone()
            .ok_or_else(|| "Save a Google OAuth client id first".to_string())?;
        let refresh_token = self
            .credentials
            .load()
            .map_err(stringify)?
            .ok_or_else(|| "Connect Google Drive first".to_string())?;
        let provider =
            GoogleDriveProvider::production(&client_id, &refresh_token).map_err(stringify)?;
        let mut engine = self.engine.lock().map_err(lock_error)?;
        engine.sync(&provider, game_id).map_err(stringify)
    }
}

fn lock_error<T>(error: std::sync::PoisonError<T>) -> String {
    format!("internal state lock failed: {error}")
}

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn data_directory(app: &tauri::AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("SAVESYNC_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(app.path().app_data_dir()?.join("savesync"))
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = data_directory(app.handle())?;
            std::fs::create_dir_all(&data_dir)?;

            let mut engine = SaveSyncEngine::open(&data_dir)?;
            let startup_warning = engine
                .recover()
                .err()
                .map(|error| format!("Recovery on startup failed: {error}"));

            let client_id = engine.setting(GOOGLE_CLIENT_ID_KEY)?;
            let credentials: Arc<dyn TokenStore> =
                if std::env::var("SAVESYNC_MEMORY_TOKENS").is_ok_and(|value| value == "1") {
                    Arc::new(MemoryTokenStore::default())
                } else {
                    Arc::new(KeyringTokenStore::default())
                };
            let connected = credentials.load().unwrap_or(None).is_some();
            let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestTransport::default());

            // Start the save watcher
            let (watcher, watcher_rx) = match SaveWatcher::start() {
                Ok(pair) => (Some(pair.0), Some(pair.1)),
                Err(error) => {
                    let warning = format!("Save watcher failed to start: {error}. File change detection will be unavailable.");
                    eprintln!("{warning}");
                    (None, None)
                }
            };

            let app_handle = app.handle().clone();
            if let Some(rx) = watcher_rx {
                std::thread::Builder::new()
                    .name("savesync-watcher-events".into())
                    .spawn(move || {
                        for event in rx {
                            if let Err(e) = app_handle.emit("dirty-event", &event) {
                                eprintln!("Failed to emit dirty-event: {e}");
                            }
                        }
                    })
                    .expect("failed to spawn watcher event thread");
            }

            app.manage(AppState {
                inner: Arc::new(Inner {
                    engine: Mutex::new(engine),
                    watcher: Mutex::new(watcher),
                    credentials,
                    transport,
                    client_id: Mutex::new(client_id),
                    oauth: Mutex::new(OAuthRuntime {
                        pending: false,
                        connected,
                        error: None,
                    }),
                    startup_warning: Mutex::new(startup_warning),
                }),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            list_games,
            enroll_game,
            enroll_discovered,
            unlink_game,
            set_cover,
            add_location,
            snapshot_game,
            history,
            restore_revision,
            list_conflicts,
            resolve_conflict,
            set_google_client_id,
            google_status,
            begin_google_oauth,
            open_auth_url,
            disconnect_google,
            sync_game,
            watch_game,
            unwatch_game,
            clear_dirty,
            list_dirty_games,
            discover_games,
        ])
        .run(tauri::generate_context!())
        .expect("error while running SaveSync");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_are_explicit() {
        assert_eq!(
            AdapterType::parse("custom_directory"),
            Some(AdapterType::CustomDirectory)
        );
        assert!(AdapterType::parse("whatever").is_none());
    }

    #[test]
    fn memory_credentials_never_store_empty_tokens() {
        let store: Arc<dyn TokenStore> = Arc::new(MemoryTokenStore::default());
        assert!(store.save("").is_err());
        store.save("refresh").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("refresh"));
    }
}
