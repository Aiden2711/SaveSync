//! SaveSync desktop shell.
//!
//! Thin Tauri command layer around `savesync-core`. All destructive work
//! (snapshots, restores, conflict resolution) goes through the engine's safety
//! invariants; this layer only marshals arguments, owns the OAuth loopback
//! flow, and persists the refresh token in the OS keyring.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use savesync_core::{
    AdapterType, Conflict, DiscoveredGame, Game, GoogleDriveProvider, HttpTransport,
    KeyringTokenStore, MemoryTokenStore, OAuthConfig, OAuthFlow, ReqwestTransport, RestoreReport,
    Revision, SaveSyncEngine, SaveWatcher, SnapshotInfo, SteamGridDbClient, SyncReport, TokenStore,
};
use serde::Serialize;
use std::path::Path;
use tauri::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use tauri::http::{Response, StatusCode};
use tauri::{Emitter, Manager};

const GOOGLE_CLIENT_ID_KEY: &str = "google_client_id";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(300);
/// Cover-art downloads are bounded by the transport, not left to hang.
const ARTWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// Custom URI scheme that serves cached cover art to the webview.
///
/// The webview may only name a game id, never a filesystem path: the scheme
/// handler resolves the cached file itself from the engine. This is a
/// deliberately narrower alternative to Tauri's `asset:` protocol, which stays
/// disabled.
const COVER_SCHEME: &str = "savesync-cover";
/// Bound on the game id accepted from an untrusted request path. Game ids are
/// UUIDs (36 characters); the headroom is for future id formats.
const COVER_ID_MAX_LEN: usize = 128;

/// Keyring coordinates for the SteamGridDB API key. It is a secret credential,
/// so it lives in the OS keyring (exactly like the Google refresh token) and
/// never in SQLite or application state.
const STEAMGRIDDB_SERVICE: &str = "savesync";
const STEAMGRIDDB_ACCOUNT: &str = "steamgriddb-api-key";
/// Bound on untrusted IPC input so a pathological payload cannot be pushed into
/// the OS keyring.
const STEAMGRIDDB_API_KEY_MAX_LEN: usize = 512;

struct Inner {
    engine: Mutex<SaveSyncEngine>,
    watcher: Mutex<Option<SaveWatcher>>,
    credentials: Arc<dyn TokenStore>,
    /// SteamGridDB key store. Kept separate from the Google store so the two
    /// credentials cannot overwrite each other's keyring entry.
    artwork_credentials: Arc<dyn TokenStore>,
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

/// Whether a SteamGridDB API key is present. Deliberately carries only a flag:
/// the key itself is never read into memory or returned across IPC.
///
/// `Debug` is derived so tests can call `unwrap_err()` on results; the type
/// holds no key material, so this cannot expose the credential.
#[derive(Debug, Serialize)]
struct ArtworkStatus {
    configured: bool,
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

/// Report whether an API key is stored, without ever loading the key into a
/// value that could be logged or serialized. The underlying store error is
/// discarded: it is not guaranteed key-free.
fn artwork_status(store: &dyn TokenStore) -> Result<ArtworkStatus, String> {
    let configured = store
        .load()
        .map_err(|_| "Could not read the SteamGridDB API key from the OS keyring.".to_string())?;
    Ok(ArtworkStatus {
        configured: configured.is_some(),
    })
}

/// Validate, trim, and persist an API key. Fails loudly on empty input so the
/// UI can correct the user instead of silently storing nothing, and bounds the
/// input length. Every failure maps to a generic, key-free message.
fn store_steamgriddb_api_key(
    store: &dyn TokenStore,
    api_key: &str,
) -> Result<ArtworkStatus, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err("Enter a SteamGridDB API key.".into());
    }
    if key.chars().count() > STEAMGRIDDB_API_KEY_MAX_LEN {
        return Err(format!(
            "SteamGridDB API keys may be at most {STEAMGRIDDB_API_KEY_MAX_LEN} characters."
        ));
    }
    store
        .save(key)
        .map_err(|_| "Could not save the SteamGridDB API key to the OS keyring.".to_string())?;
    Ok(ArtworkStatus { configured: true })
}

fn remove_steamgriddb_api_key(store: &dyn TokenStore) -> Result<ArtworkStatus, String> {
    store
        .delete()
        .map_err(|_| "Could not remove the SteamGridDB API key from the OS keyring.".to_string())?;
    Ok(ArtworkStatus { configured: false })
}

#[tauri::command(rename_all = "snake_case")]
fn set_steamgriddb_api_key(
    state: tauri::State<'_, AppState>,
    api_key: String,
) -> Result<ArtworkStatus, String> {
    store_steamgriddb_api_key(state.inner.artwork_credentials.as_ref(), &api_key)
}

#[tauri::command(rename_all = "snake_case")]
fn steamgriddb_status(state: tauri::State<'_, AppState>) -> Result<ArtworkStatus, String> {
    artwork_status(state.inner.artwork_credentials.as_ref())
}

#[tauri::command(rename_all = "snake_case")]
fn clear_steamgriddb_api_key(state: tauri::State<'_, AppState>) -> Result<ArtworkStatus, String> {
    remove_steamgriddb_api_key(state.inner.artwork_credentials.as_ref())
}

/// Fetch and cache cover art for a game, then return the updated record.
///
/// The API key is read from the keyring inside the blocking task and is never
/// cloned into shared state, logged, or put into an error.
#[tauri::command(rename_all = "snake_case")]
async fn fetch_cover_art(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<Game, String> {
    let inner = state.inner.clone();
    tauri::async_runtime::spawn_blocking(move || inner.fetch_cover_art(&game_id))
        .await
        .map_err(|error| format!("artwork task failed: {error}"))?
}

#[tauri::command(rename_all = "snake_case")]
async fn clear_cover_art(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<Game, String> {
    let inner = state.inner.clone();
    tauri::async_runtime::spawn_blocking(move || inner.clear_cover_art(&game_id))
        .await
        .map_err(|error| format!("artwork task failed: {error}"))?
}

/// Resolve the URL the webview should use for a game's cached cover, if any.
///
/// The URL is built from the game id alone. No filesystem path is ever sent to
/// the webview, and cache-busting is left to the caller.
#[tauri::command(rename_all = "snake_case")]
fn cover_url(state: tauri::State<'_, AppState>, game_id: String) -> Result<Option<String>, String> {
    let engine = state.inner.engine.lock().map_err(lock_error)?;
    let cached = engine
        .cover_file(&game_id)
        // Deliberately generic: the underlying error can name a cache path.
        .map_err(|_| "Could not resolve the cover art for this game.".to_string())?;
    Ok(cached.map(|_| cover_scheme_url(&game_id)))
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

    /// Download and record cover art for one game.
    ///
    /// `fetch_cover_art` performs network I/O and hashes/writes an image, so the
    /// command layer calls this from a blocking task.
    fn fetch_cover_art(&self, game_id: &str) -> Result<Game, String> {
        let mut api_key = self
            .artwork_credentials
            .load()
            // The keyring error is not guaranteed key-free, so it is discarded.
            .map_err(|_| "Could not read the SteamGridDB API key from the OS keyring.".to_string())?
            .ok_or_else(|| "Configure the SteamGridDB API key in Settings first.".to_string())?;

        let result = (|| -> Result<Game, String> {
            let transport = ReqwestTransport::without_redirects(ARTWORK_TIMEOUT)
                .map_err(|_| "Could not prepare the artwork download.".to_string())?;
            let client = SteamGridDbClient::new(Arc::new(transport));
            let mut engine = self.engine.lock().map_err(lock_error)?;
            engine
                .fetch_cover_art(&client, &api_key, game_id)
                .map_err(|error| cover_fetch_error(&error))
        })();

        // Best-effort scrub: the key's allocation is overwritten before it is
        // freed. This is defence in depth, not a guarantee about freed memory.
        scrub_secret(&mut api_key);

        result
    }

    fn clear_cover_art(&self, game_id: &str) -> Result<Game, String> {
        let mut engine = self.engine.lock().map_err(lock_error)?;
        engine
            .clear_cover_art(game_id)
            // Generic on purpose: the underlying error can name a cache path.
            .map_err(|_| "Could not clear the cover art for this game.".to_string())
    }
}

/// Overwrite a secret's bytes while keeping its allocation, so a plain `drop`
/// does not leave the credential in a freed buffer.
fn scrub_secret(secret: &mut String) {
    if secret.is_empty() {
        return;
    }
    secret.replace_range(.., &"*".repeat(secret.len()));
}

/// Map an artwork error to a user-facing message without forwarding internals.
///
/// The error `Display` implementation is never used: it can carry CDN URLs and
/// response bodies. Only the class of failure is exposed.
fn cover_fetch_error(error: &savesync_core::Error) -> String {
    match error {
        savesync_core::Error::NotFound(_) => "No cover art found for this game.".to_string(),
        _ => "Could not fetch cover art for this game.".to_string(),
    }
}

fn lock_error<T>(error: std::sync::PoisonError<T>) -> String {
    format!("internal state lock failed: {error}")
}

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Platform-correct URL for a game's cached cover.
///
/// Tauri serves app-defined schemes as `http://<scheme>.localhost/<path>` on
/// Windows and Android, and as `<scheme>://localhost/<path>` everywhere else.
fn cover_scheme_url(game_id: &str) -> String {
    if cfg!(windows) || cfg!(target_os = "android") {
        format!("http://{COVER_SCHEME}.localhost/{game_id}")
    } else {
        format!("{COVER_SCHEME}://localhost/{game_id}")
    }
}

/// Accept only ids that can safely become a lookup key: no separators, no dot
/// segments, no NUL, and bounded in length.
fn is_valid_cover_id(game_id: &str) -> bool {
    !game_id.is_empty()
        && game_id.len() <= COVER_ID_MAX_LEN
        && game_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Validate a request path from the cover scheme and return the game id in it.
///
/// This is the only place an untrusted request path is interpreted. Everything
/// it rejects becomes a `404`.
fn cover_request_id(path: &str) -> Option<&str> {
    let game_id = path.strip_prefix('/')?;
    // Checked explicitly before the charset test so the intent is obvious: a
    // path separator or dot segment must never reach the engine.
    if game_id.contains("..") {
        return None;
    }
    is_valid_cover_id(game_id).then_some(game_id)
}

/// Map a cached cover's file extension to the only MIME types the cache holds.
fn cover_mime_for_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn cover_mime(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    cover_mime_for_extension(extension)
}

/// Empty `404`. No path, error string, or other internal detail is ever echoed.
fn cover_not_found() -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cover_image(bytes: Vec<u8>, mime: &'static str) -> Response<Vec<u8>> {
    let mut response = Response::new(bytes);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Read a cached cover, refusing anything that is not a known image type or
/// that exceeds the download bound.
///
/// The read is bounded by `take` rather than a prior `stat`, so a file that
/// grew after being recorded cannot blow past the limit or exhaust memory.
fn read_cover_bounded(path: &std::path::Path) -> Option<(Vec<u8>, &'static str)> {
    let mime = cover_mime(path)?;
    let limit = savesync_core::MAX_GRID_BYTES;
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    Some((bytes, mime))
}

/// Resolve a game id to its cached cover path.
///
/// The engine lock is held only for this lookup; the caller reads the file
/// after the guard is released, so a slow disk cannot block other commands.
fn resolve_cover(app: &tauri::AppHandle, game_id: &str) -> Option<PathBuf> {
    let state = app.state::<AppState>();
    let engine = state.inner.engine.lock().ok()?;
    engine.cover_file(game_id).ok().flatten()
}

/// Build the response for one cover request. Every rejection path returns the
/// same empty `404`, so the caller cannot distinguish "no such id" from "no
/// cached file".
fn cover_response(app: &tauri::AppHandle, request_path: &str) -> Response<Vec<u8>> {
    let Some(game_id) = cover_request_id(request_path) else {
        return cover_not_found();
    };
    let Some(cached) = resolve_cover(app, game_id) else {
        return cover_not_found();
    };
    let Some((bytes, mime)) = read_cover_bounded(&cached) else {
        return cover_not_found();
    };
    cover_image(bytes, mime)
}

fn data_directory(app: &tauri::AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("SAVESYNC_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(app.path().app_data_dir()?.join("savesync"))
}

pub fn run() {
    tauri::Builder::default()
        // Serves cached cover art to the webview. The webview can only name a
        // game id; the handler resolves the file itself, and `asset:` stays
        // disabled.
        .register_asynchronous_uri_scheme_protocol(COVER_SCHEME, |ctx, request, responder| {
            // Only the path is inspected, so a `?v=...` cache-busting query
            // from the UI is ignored rather than becoming part of the id.
            let path = request.uri().path().to_string();
            let app = ctx.app_handle().clone();
            // Bounded, but still disk I/O: keep it off the protocol thread.
            // Every branch of `cover_response` produces a response, so the
            // responder is resolved exactly once.
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(cover_response(&app, &path));
            });
        })
        .setup(|app| {
            let data_dir = data_directory(app.handle())?;
            std::fs::create_dir_all(&data_dir)?;

            let mut engine = SaveSyncEngine::open(&data_dir)?;
            let startup_warning = engine
                .recover()
                .err()
                .map(|error| format!("Recovery on startup failed: {error}"));

            let client_id = engine.setting(GOOGLE_CLIENT_ID_KEY)?;
            let memory_tokens = std::env::var("SAVESYNC_MEMORY_TOKENS").is_ok_and(|value| value == "1");
            let credentials: Arc<dyn TokenStore> = if memory_tokens {
                Arc::new(MemoryTokenStore::default())
            } else {
                Arc::new(KeyringTokenStore::default())
            };
            // The SteamGridDB API key is a secret credential too, so it follows
            // the same keyring-or-memory policy as the Google refresh token.
            let artwork_credentials: Arc<dyn TokenStore> = if memory_tokens {
                Arc::new(MemoryTokenStore::default())
            } else {
                Arc::new(KeyringTokenStore::new(
                    STEAMGRIDDB_SERVICE,
                    STEAMGRIDDB_ACCOUNT,
                ))
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
                    artwork_credentials,
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
            set_steamgriddb_api_key,
            steamgriddb_status,
            clear_steamgriddb_api_key,
            fetch_cover_art,
            clear_cover_art,
            cover_url,
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

    /// A store whose every operation fails with a message that embeds a
    /// secret-shaped payload. Proves the command layer never forwards the
    /// underlying keyring error to the UI.
    struct FailingStore;

    impl TokenStore for FailingStore {
        fn load(&self) -> savesync_core::Result<Option<String>> {
            Err(savesync_core::Error::Auth("keyring payload: leaked".into()))
        }
        fn save(&self, _token: &str) -> savesync_core::Result<()> {
            Err(savesync_core::Error::Auth("keyring payload: leaked".into()))
        }
        fn delete(&self) -> savesync_core::Result<()> {
            Err(savesync_core::Error::Auth("keyring payload: leaked".into()))
        }
    }

    const STEAMGRIDDB_SENTINEL_KEY: &str = "sgsdb-sentinel-key-0123456789";

    #[test]
    fn steamgriddb_key_round_trips_without_being_echoed() {
        let store: Arc<dyn TokenStore> = Arc::new(MemoryTokenStore::default());

        // Starts unconfigured.
        assert!(!artwork_status(store.as_ref()).unwrap().configured);

        // Empty / whitespace-only input is refused, and the error is key-free.
        let empty_error = store_steamgriddb_api_key(store.as_ref(), "   ").unwrap_err();
        assert!(!empty_error.contains(STEAMGRIDDB_SENTINEL_KEY));
        assert!(!artwork_status(store.as_ref()).unwrap().configured);

        // Surrounding whitespace is trimmed before the key reaches the keyring.
        let padded = format!("  {STEAMGRIDDB_SENTINEL_KEY}  ");
        let stored = store_steamgriddb_api_key(store.as_ref(), &padded).unwrap();
        assert!(stored.configured);
        assert!(artwork_status(store.as_ref()).unwrap().configured);
        assert_eq!(
            store.load().unwrap().as_deref(),
            Some(STEAMGRIDDB_SENTINEL_KEY)
        );

        // Removal returns to the unconfigured state.
        let cleared = remove_steamgriddb_api_key(store.as_ref()).unwrap();
        assert!(!cleared.configured);
        assert!(!artwork_status(store.as_ref()).unwrap().configured);
    }

    #[test]
    fn steamgriddb_key_rejects_oversized_input() {
        let store: Arc<dyn TokenStore> = Arc::new(MemoryTokenStore::default());
        let too_long = "k".repeat(STEAMGRIDDB_API_KEY_MAX_LEN + 1);

        let error = store_steamgriddb_api_key(store.as_ref(), &too_long).unwrap_err();

        // The length error must not embed any part of the rejected value.
        assert!(!error.contains("kkkk"));
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn steamgriddb_errors_never_forward_the_keyring_payload() {
        let store = FailingStore;

        assert_eq!(
            store_steamgriddb_api_key(&store, STEAMGRIDDB_SENTINEL_KEY).unwrap_err(),
            "Could not save the SteamGridDB API key to the OS keyring."
        );
        assert_eq!(
            artwork_status(&store).unwrap_err(),
            "Could not read the SteamGridDB API key from the OS keyring."
        );
        assert_eq!(
            remove_steamgriddb_api_key(&store).unwrap_err(),
            "Could not remove the SteamGridDB API key from the OS keyring."
        );
    }

    const COVER_TEST_ID: &str = "a1b2c3d4-1111-4222-8333-444455556666";

    #[test]
    fn cover_scheme_url_is_built_from_the_game_id_only() {
        let url = cover_scheme_url(COVER_TEST_ID);

        assert!(url.contains(COVER_TEST_ID));
        assert!(url.contains(COVER_SCHEME));
        assert!(url.contains("localhost"));
        // The platform form must match Tauri's serving behaviour exactly.
        let expected = if cfg!(windows) || cfg!(target_os = "android") {
            format!("http://savesync-cover.localhost/{COVER_TEST_ID}")
        } else {
            format!("savesync-cover://localhost/{COVER_TEST_ID}")
        };
        assert_eq!(url, expected);
    }

    #[test]
    fn cover_ids_accept_uuids_and_reject_path_like_input() {
        assert!(is_valid_cover_id(COVER_TEST_ID));

        assert!(!is_valid_cover_id(""));
        assert!(!is_valid_cover_id("../evil"));
        assert!(!is_valid_cover_id("a/b"));
        assert!(!is_valid_cover_id("a\\b"));
        assert!(!is_valid_cover_id("a.b"));
        assert!(!is_valid_cover_id("a\0b"));
        assert!(!is_valid_cover_id(&"a".repeat(COVER_ID_MAX_LEN + 1)));

        // The bound itself is allowed.
        assert!(is_valid_cover_id(&"a".repeat(COVER_ID_MAX_LEN)));
    }

    #[test]
    fn cover_request_path_supplies_the_bare_game_id() {
        // Leading slash is stripped, so only the id reaches the engine.
        assert_eq!(
            cover_request_id(&format!("/{COVER_TEST_ID}")),
            Some(COVER_TEST_ID)
        );

        // A path with more than one segment is refused outright.
        assert_eq!(cover_request_id("/a/b"), None);
        assert_eq!(cover_request_id("/..%2fevil"), None);
        assert_eq!(cover_request_id("/"), None);
        // A path without a leading slash is not a valid request path.
        assert_eq!(cover_request_id(COVER_TEST_ID), None);
        assert_eq!(cover_request_id("/a/b/../c"), None);
    }

    #[test]
    fn cover_mime_only_accepts_the_cached_formats() {
        assert_eq!(cover_mime_for_extension("png"), Some("image/png"));
        assert_eq!(cover_mime_for_extension("jpg"), Some("image/jpeg"));
        assert_eq!(cover_mime_for_extension("webp"), Some("image/webp"));
        // Case-insensitive, matching how the extension is read off disk.
        assert_eq!(cover_mime_for_extension("PNG"), Some("image/png"));

        assert_eq!(cover_mime_for_extension("gif"), None);
        assert_eq!(cover_mime_for_extension("exe"), None);
        assert_eq!(cover_mime_for_extension(""), None);
        assert_eq!(
            cover_mime(std::path::Path::new("/tmp/cover")),
            None,
            "a file without an extension is not served"
        );
    }

    #[test]
    fn scrub_secret_overwrites_the_key_bytes() {
        let mut secret = STEAMGRIDDB_SENTINEL_KEY.to_string();
        let capacity = secret.capacity();
        scrub_secret(&mut secret);

        assert!(!secret.contains(STEAMGRIDDB_SENTINEL_KEY));
        assert_eq!(secret.len(), STEAMGRIDDB_SENTINEL_KEY.len());
        // Same allocation, so the secret is not merely dropped somewhere else.
        assert_eq!(secret.capacity(), capacity);

        // An empty secret is untouched and does not panic.
        let mut empty = String::new();
        scrub_secret(&mut empty);
        assert!(empty.is_empty());
    }
}
