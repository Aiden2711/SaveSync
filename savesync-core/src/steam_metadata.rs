//! Keyless Steam Store metadata provider.
//!
//! Two public Steam endpoints, no API key and therefore no secret anywhere:
//!
//! - search uses `ISteamApps/GetAppList/v2/`, the full published app list. It
//!   is far too large to fetch per lookup (roughly 1.3M entries), so the raw
//!   response is downloaded **once**, written atomically to a cache file, and
//!   matched in memory until the cache is older than [`APP_LIST_TTL`];
//! - details use `store.steampowered.com/api/appdetails`, one request per app
//!   id, serialized so concurrent callers cannot stampede the endpoint.
//!
//! Everything here is optional enrichment: a failure is reported to the caller
//! and never touches save data, snapshots, or restore state.
//!
//! # Bounds
//!
//! The app list is streamed to disk with [`MAX_APP_LIST_BYTES`] enforced while
//! copying, so a hostile or broken response cannot fill the volume. A details
//! body is buffered and rejected above [`MAX_RESPONSE_BYTES`] (the transport
//! already stops there; the provider repeats the check so an injected transport
//! in tests obeys the same rule).
//!
//! # Politeness
//!
//! Details requests are single-flight per provider: one mutex admits one
//! request at a time, and a successful result is cached per app id, so a
//! concurrent caller for the same id waits once and then reads the cache. The
//! same gate pattern single-flights the one-time app-list download.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, MAX_RESPONSE_BYTES, RetryPolicy, execute_with_retry_classified,
};
use crate::metadata::{GameMetadata, MetadataCandidate, MetadataProvider, MetadataSource};
use crate::provider_runtime::{ProviderFlavor, ensure_success, is_retryable_response};

/// Full Steam app list (app ids and names only).
pub const APP_LIST_URL: &str = "https://api.steampowered.com/ISteamApps/GetAppList/v2/";

/// Per-app store details.
pub const APP_DETAILS_URL: &str = "https://store.steampowered.com/api/appdetails";

/// File name the app list is cached under inside the engine's metadata
/// directory.
pub const APP_LIST_CACHE_FILE: &str = "steam_app_list.json";

/// How long a cached app list is considered fresh (24 hours).
///
/// The list changes only when Steam publishes a new app, so a daily refresh is
/// plenty while keeping a startup lookup offline.
pub const APP_LIST_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Ceiling for the streamed app-list download (64 MiB).
///
/// The published list is tens of megabytes, so [`MAX_RESPONSE_BYTES`] (8 MiB,
/// tuned for small metadata objects) would refuse it. Streaming with this cap
/// bounds what a broken response can write to the cache file.
pub const MAX_APP_LIST_BYTES: u64 = 64 * 1024 * 1024;

/// Refuse an app list with more entries than this: a trivially corrupt or
/// hostile response must not make the in-memory index unbounded.
const MAX_APP_LIST_ENTRIES: usize = 3_000_000;

/// Upper bound on the candidates one search returns.
///
/// The list crosses the IPC boundary in a later milestone and every candidate
/// costs a details request when picked, so the tail is capped rather than
/// proportional to how many apps share a title.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Upper bound on the in-process details cache. When it is full it is cleared;
/// a metadata entry is cheap to re-fetch and nothing depends on it surviving.
const MAX_DETAILS_CACHE_ENTRIES: usize = 256;

/// Best-effort platform label for a Steam Store app.
const PLATFORM_HINT: &str = "PC";

/// Recovery from mutex poisoning.
///
/// Every lock here guards a cache, never a state machine: a panic in an
/// unrelated thread leaves the cache merely incomplete, so the next caller
/// continues instead of turning the whole provider permanently unusable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Deserialize)]
struct SteamApp {
    #[serde(default)]
    appid: u32,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct AppListEnvelope {
    #[serde(default)]
    applist: AppListBody,
}

#[derive(Default, Deserialize)]
struct AppListBody {
    #[serde(default)]
    apps: Vec<SteamApp>,
}

#[derive(Deserialize)]
struct AppDetailsEnvelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<AppDetailsData>,
}

#[derive(Deserialize)]
struct AppDetailsData {
    #[serde(default)]
    name: String,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    detailed_description: Option<String>,
    #[serde(default)]
    developers: Vec<String>,
    #[serde(default)]
    publishers: Vec<String>,
    #[serde(default)]
    genres: Vec<Genre>,
    #[serde(default)]
    release_date: Option<ReleaseDate>,
}

#[derive(Deserialize)]
struct Genre {
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct ReleaseDate {
    #[serde(default)]
    date: Option<String>,
}

struct AppListSnapshot {
    apps: Arc<Vec<SteamApp>>,
    loaded_at: SystemTime,
}

impl AppListSnapshot {
    fn is_fresh(&self) -> bool {
        SystemTime::now()
            .duration_since(self.loaded_at)
            .is_ok_and(|age| age <= APP_LIST_TTL)
    }
}

/// Steam Store metadata over a mockable [`crate::http::HttpTransport`].
///
/// The struct holds no secret, so unlike the SteamGridDB client it may derive
/// `Debug`; it does not, because the app list would make the output useless.
pub struct SteamMetadataProvider {
    transport: Arc<dyn crate::http::HttpTransport>,
    policy: RetryPolicy,
    /// Raw `GetAppList` response cache; `None` keeps the list in memory only.
    cache_file: Option<PathBuf>,
    app_list: Mutex<Option<AppListSnapshot>>,
    /// Serializes the one-time app-list download across threads.
    app_list_gate: Mutex<()>,
    details: Mutex<HashMap<String, GameMetadata>>,
    /// Serializes details requests (single-flight per provider).
    details_gate: Mutex<()>,
}

impl SteamMetadataProvider {
    /// Provider that keeps the app list in memory only (tests, previews).
    pub fn new(transport: Arc<dyn crate::http::HttpTransport>) -> Self {
        Self::build(transport, None)
    }

    /// Provider that persists the app list under `directory` for
    /// [`APP_LIST_TTL`], so the daily list is downloaded once across restarts.
    ///
    /// The directory is created on the first successful download; a failure to
    /// write it never fails the lookup (the list is still parsed from the
    /// downloaded bytes).
    pub fn with_cache_dir(
        transport: Arc<dyn crate::http::HttpTransport>,
        directory: &Path,
    ) -> Self {
        Self::build(transport, Some(directory.join(APP_LIST_CACHE_FILE)))
    }

    fn build(transport: Arc<dyn crate::http::HttpTransport>, cache_file: Option<PathBuf>) -> Self {
        Self {
            transport,
            policy: RetryPolicy::default(),
            cache_file,
            app_list: Mutex::new(None),
            app_list_gate: Mutex::new(()),
            details: Mutex::new(HashMap::new()),
            details_gate: Mutex::new(()),
        }
    }

    /// Override the retry policy (tests use a short one so retries do not
    /// sleep for the production backoff).
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Parsed app list, loading (or downloading once) it when absent or stale.
    fn app_list(&self) -> Result<Arc<Vec<SteamApp>>> {
        if let Some(snapshot) = lock(&self.app_list).as_ref()
            && snapshot.is_fresh()
        {
            return Ok(snapshot.apps.clone());
        }

        let _gate = lock(&self.app_list_gate);
        // Another thread may have loaded it while this one waited for the gate.
        if let Some(snapshot) = lock(&self.app_list).as_ref()
            && snapshot.is_fresh()
        {
            return Ok(snapshot.apps.clone());
        }

        let apps = match self.read_cached_app_list() {
            Some(apps) => apps,
            None => self.download_app_list()?,
        };

        let apps = Arc::new(apps);
        *lock(&self.app_list) = Some(AppListSnapshot {
            apps: apps.clone(),
            loaded_at: SystemTime::now(),
        });
        Ok(apps)
    }

    /// Read and parse a cached app list, if one exists and is still fresh.
    ///
    /// A corrupt cache is not an error: it is treated as absent so the next
    /// step re-downloads it.
    fn read_cached_app_list(&self) -> Option<Vec<SteamApp>> {
        let path = self.cache_file.as_deref()?;
        if !is_file_fresh(path, APP_LIST_TTL) {
            return None;
        }
        parse_app_list_file(path).ok()
    }

    /// Download the full app list once, parse it, and (when configured)
    /// atomically persist it.
    fn download_app_list(&self) -> Result<Vec<SteamApp>> {
        let part = self.part_path();
        if let Some(parent) = part.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }

        let response = execute_with_retry_classified(
            self.transport.as_ref(),
            &self.policy,
            || {
                HttpRequest::new(HttpMethod::Get, APP_LIST_URL)
                    .header("Accept", "application/json")
                    .download_to(&part)
                    .max_download_bytes(MAX_APP_LIST_BYTES)
            },
            |response| is_retryable_response(response, ProviderFlavor::SteamStore),
        );

        if let Err(error) =
            response.and_then(|response| ensure_success(response, ProviderFlavor::SteamStore))
        {
            // The transport wrote a body to `part` before the status was
            // known; never leave a rejected response behind as a cache.
            let _ = fs::remove_file(&part);
            return Err(error);
        }

        match self.finish_app_list_download(&part) {
            Ok(apps) => Ok(apps),
            Err(error) => {
                let _ = fs::remove_file(&part);
                Err(error)
            }
        }
    }

    /// Validate, parse, and persist a streamed app-list response.
    fn finish_app_list_download(&self, part: &Path) -> Result<Vec<SteamApp>> {
        let length = fs::metadata(part)
            .map_err(|error| Error::io(part, error))?
            .len();
        if length > MAX_APP_LIST_BYTES {
            return Err(oversize_app_list());
        }

        let apps = parse_app_list_file(part)?;

        // Persisting is an optimization: a read-only data directory must not
        // turn a usable app list into a failed lookup.
        match self.cache_file.as_deref() {
            Some(path) => {
                if fs::rename(part, path).is_err() {
                    let _ = fs::remove_file(part);
                }
            }
            None => {
                let _ = fs::remove_file(part);
            }
        }

        Ok(apps)
    }

    /// Where an in-progress app-list download is staged.
    fn part_path(&self) -> PathBuf {
        match self.cache_file.as_deref() {
            Some(path) => path.with_extension("part"),
            None => std::env::temp_dir().join(format!(
                "savesync-steam-app-list-{}.part",
                uuid::Uuid::new_v4()
            )),
        }
    }

    /// Cached details, fetching them once per app id.
    fn details_for(&self, appid: u32) -> Result<GameMetadata> {
        let key = appid.to_string();
        if let Some(hit) = lock(&self.details).get(&key).cloned() {
            return Ok(hit);
        }

        let _gate = lock(&self.details_gate);
        if let Some(hit) = lock(&self.details).get(&key).cloned() {
            return Ok(hit);
        }

        let url = format!("{APP_DETAILS_URL}?appids={appid}&l=english");
        let response = execute_with_retry_classified(
            self.transport.as_ref(),
            &self.policy,
            || {
                HttpRequest::new(HttpMethod::Get, &url)
                    .header("Accept", "application/json")
                    .header("Accept-Language", "en")
            },
            |response| is_retryable_response(response, ProviderFlavor::SteamStore),
        )?;

        if response.status == 404 {
            return Err(Error::NotFound(format!("Steam app {appid}")));
        }
        let response = ensure_success(response, ProviderFlavor::SteamStore)?;
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Network(format!(
                "Steam app details exceeded the {MAX_RESPONSE_BYTES} byte limit"
            )));
        }

        let metadata = parse_app_details(appid, &response.body)?;
        let mut cache = lock(&self.details);
        if cache.len() >= MAX_DETAILS_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, metadata.clone());
        Ok(metadata)
    }
}

impl MetadataProvider for SteamMetadataProvider {
    fn source(&self) -> MetadataSource {
        MetadataSource::Steam
    }

    /// Match a title against the cached app list.
    ///
    /// Exact normalized matches rank first, then prefix/whole-word matches,
    /// then substring matches; the list is capped at [`MAX_SEARCH_RESULTS`].
    /// `release_year` is not available from `GetAppList` and stays `None`.
    fn search(&self, title: &str) -> Result<Vec<MetadataCandidate>> {
        let target = crate::metadata::normalize_title(title);
        if target.is_empty() {
            return Ok(Vec::new());
        }

        let apps = self.app_list()?;
        let mut exact = Vec::new();
        let mut prefix = Vec::new();
        let mut contains = Vec::new();
        let mut seen = HashSet::new();

        for app in apps.iter() {
            let name = crate::metadata::normalize_title(&app.name);
            if app.appid == 0 || name.is_empty() || !seen.insert(app.appid) {
                continue;
            }
            if name == target {
                exact.push(app);
            } else if name.starts_with(&target) || name.split(' ').any(|word| word == target) {
                prefix.push(app);
            } else if name.contains(&target) {
                contains.push(app);
            }
        }

        let mut candidates = Vec::new();
        for app in exact.into_iter().chain(prefix).chain(contains) {
            if candidates.len() >= MAX_SEARCH_RESULTS {
                break;
            }
            candidates.push(MetadataCandidate {
                source_id: app.appid.to_string(),
                name: app.name.trim().to_string(),
                release_year: None,
                platform_hint: Some(PLATFORM_HINT.to_string()),
            });
        }
        Ok(candidates)
    }

    fn details(&self, source_id: &str) -> Result<GameMetadata> {
        let appid = parse_app_id(source_id)?;
        self.details_for(appid)
    }
}

/// Parse a provider-supplied app id.
///
/// The id comes from a candidate or a caller, never from a path, but it still
/// has to be a positive `u32` before it is interpolated into a URL.
fn parse_app_id(source_id: &str) -> Result<u32> {
    match source_id.trim().parse::<u32>() {
        Ok(appid) if appid > 0 => Ok(appid),
        _ => Err(Error::InvalidState(
            "Steam app id must be a positive integer".to_string(),
        )),
    }
}

fn oversize_app_list() -> Error {
    Error::Network(format!(
        "Steam app list exceeded the {MAX_APP_LIST_BYTES} byte limit"
    ))
}

/// Parse a `GetAppList` envelope from a file.
fn parse_app_list_file(path: &Path) -> Result<Vec<SteamApp>> {
    let file = fs::File::open(path).map_err(|error| Error::io(path, error))?;
    let parsed: AppListEnvelope =
        serde_json::from_reader(std::io::BufReader::new(file)).map_err(|_| Error::Provider {
            status: 200,
            message: "Steam returned an unreadable app list".to_string(),
        })?;

    if parsed.applist.apps.len() > MAX_APP_LIST_ENTRIES {
        return Err(Error::Provider {
            status: 200,
            message: "Steam app list has an implausible number of entries".to_string(),
        });
    }
    Ok(parsed.applist.apps)
}

/// Map one `appdetails` response onto [`GameMetadata`].
///
/// Fields Steam does not expose (players, age rating, franchise, themes) stay
/// empty instead of being guessed.
fn parse_app_details(appid: u32, body: &[u8]) -> Result<GameMetadata> {
    if body.is_empty() {
        return Err(Error::NotFound(format!("Steam app {appid}")));
    }

    let parsed: BTreeMap<String, AppDetailsEnvelope> =
        serde_json::from_slice(body).map_err(|_| Error::Provider {
            status: 200,
            message: "Steam returned unreadable app details".to_string(),
        })?;

    let envelope = parsed
        .get(&appid.to_string())
        .or_else(|| parsed.values().next())
        .ok_or_else(|| Error::NotFound(format!("Steam app {appid}")))?;
    let Some(data) = envelope.data.as_ref().filter(|_| envelope.success) else {
        return Err(Error::NotFound(format!("Steam app {appid}")));
    };

    let name = data.name.trim().to_string();
    if name.is_empty() {
        return Err(Error::NotFound(format!("Steam app {appid}")));
    }

    let release_date = data
        .release_date
        .as_ref()
        .and_then(|release| release.date.as_deref())
        .map(str::trim)
        .filter(|date| !date.is_empty())
        .map(str::to_string);
    let release_year = release_date.as_deref().and_then(parse_release_year);

    Ok(GameMetadata {
        source: MetadataSource::Steam,
        source_id: appid.to_string(),
        name,
        release_year,
        release_date,
        genres: data
            .genres
            .iter()
            .map(|genre| genre.description.trim().to_string())
            .filter(|genre| !genre.is_empty())
            .collect(),
        developers: data
            .developers
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
        publishers: data
            .publishers
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
        description_short: non_empty(&data.short_description),
        description_long: non_empty(&data.detailed_description),
        players: None,
        age_rating: None,
        franchise: None,
        themes: Vec::new(),
        fetched_at: crate::store::now_iso(),
    })
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Extract a plausible release year from Steam's free-form date text.
///
/// Steam publishes `"3 Oct, 2018"`, `"Oct 3, 2018"`, `"2018"`, and
/// non-commitments such as `"Coming soon"` or `"TBA"`. The last standalone
/// four-digit run wins, which handles both orderings without a date parser.
fn parse_release_year(date: &str) -> Option<i32> {
    let bytes = date.as_bytes();
    let mut year = None;
    for start in 0..bytes.len().saturating_sub(3) {
        let window = &bytes[start..start + 4];
        if !window.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let bounded_before = start == 0 || !bytes[start - 1].is_ascii_digit();
        let bounded_after = start + 4 == bytes.len() || !bytes[start + 4].is_ascii_digit();
        if !(bounded_before && bounded_after) {
            continue;
        }
        if let Some(value) = std::str::from_utf8(window)
            .ok()
            .and_then(|text| text.parse::<i32>().ok())
            && (1900..=2200).contains(&value)
        {
            year = Some(value);
        }
    }
    year
}

/// Whether `path` exists and was modified within `ttl`.
///
/// A missing file, an unreadable mtime, or an mtime in the future all report
/// "stale": refetching is always safe, trusting a bad timestamp is not.
fn is_file_fresh(path: &Path, ttl: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= ttl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_release_years_from_steam_date_text() {
        assert_eq!(parse_release_year("3 Oct, 2018"), Some(2018));
        assert_eq!(parse_release_year("Oct 3, 2018"), Some(2018));
        assert_eq!(parse_release_year("2018"), Some(2018));
        assert_eq!(parse_release_year("Coming soon"), None);
        assert_eq!(parse_release_year("TBA"), None);
        assert_eq!(parse_release_year("12345"), None);
        assert_eq!(parse_release_year("1999-2001"), Some(2001));
    }

    #[test]
    fn rejects_non_numeric_app_ids() {
        for invalid in ["", "  ", "abc", "0", "-1", "12.5"] {
            assert!(parse_app_id(invalid).is_err(), "{invalid}");
        }
        assert_eq!(parse_app_id(" 1145360 ").unwrap(), 1_145_360);
    }
}
