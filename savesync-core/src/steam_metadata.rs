//! Keyless Steam Store metadata provider.
//!
//! Two public Steam endpoints, no API key and therefore no secret anywhere:
//!
//! - search asks `store.steampowered.com/api/storesearch`, the storefront's own
//!   query endpoint. One small request per lookup returns the app ids and names
//!   of the closest matches, so a title resolves to an app id without
//!   downloading anything large;
//! - details use `store.steampowered.com/api/appdetails`, one request per app
//!   id, serialized so concurrent callers cannot stampede the endpoint.
//!
//! Everything here is optional enrichment: a failure is reported to the caller
//! and never touches save data, snapshots, or restore state.
//!
//! # The published app list is only a fallback
//!
//! `ISteamApps/GetAppList` used to be the index this provider matched against.
//! It outgrew the old 64 MiB ceiling — the response is over 100 MB today — and
//! Steam has since withdrawn the method: it answers `404` with
//! `Method 'GetAppList' not found in interface 'ISteamApps'`. Searching that
//! endpoint first therefore turned a service failure into "no metadata found".
//!
//! Storefront search answers the same question in one small response, so it is
//! the primary path. The published list stays as a last resort: it is consulted
//! only after the search endpoint fails, and a healthy search never downloads
//! it. When it is consulted it is downloaded once, streamed to disk, and cached
//! under [`APP_LIST_TTL`].
//!
//! # Bounds
//!
//! A search body is bounded by [`MAX_RESPONSE_BYTES`] (the transport's metadata
//! limit) and its candidates by [`MAX_SEARCH_RESULTS`]; the details cache is
//! capped at [`MAX_DETAILS_CACHE_ENTRIES`]. The fallback list is streamed with
//! [`MAX_APP_LIST_BYTES`] enforced while copying, and its parsed form is capped
//! at `MAX_APP_LIST_ENTRIES`, so a hostile or broken response can neither fill
//! the volume nor make the in-memory index unbounded.
//!
//! # Error truthfulness
//!
//! `storesearch` reports "no match" with `200` and an empty item list, so every
//! non-2xx answer is a service failure ([`Error::Provider`]) and never
//! [`Error::NotFound`]: a moved or refused endpoint must not be presented as
//! "this game has no metadata". An oversized body is [`Error::Network`]. When
//! both paths fail, a network or size-limit failure is what the caller is told.
//!
//! # Politeness
//!
//! Details requests are single-flight per provider: one mutex admits one
//! request at a time, and a successful result is cached per app id, so a
//! concurrent caller for the same id waits once and then reads the cache. The
//! fallback list download is single-flighted the same way.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use url::form_urlencoded;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, MAX_RESPONSE_BYTES, RetryPolicy, execute_with_retry_classified,
};
use crate::metadata::{GameMetadata, MetadataCandidate, MetadataProvider, MetadataSource};
use crate::provider_runtime::{ProviderFlavor, ensure_success, is_retryable_response};

/// Storefront search: one request per lookup, returns app ids and names.
pub const STORE_SEARCH_URL: &str = "https://store.steampowered.com/api/storesearch";

/// Full Steam app list (app ids and names only). Fallback only.
pub const APP_LIST_URL: &str = "https://api.steampowered.com/ISteamApps/GetAppList/v2/";

/// Per-app store details.
pub const APP_DETAILS_URL: &str = "https://store.steampowered.com/api/appdetails";

/// File name the app list is cached under inside the engine's metadata
/// directory.
pub const APP_LIST_CACHE_FILE: &str = "steam_app_list.json";

/// How long a cached app list is considered fresh (24 hours).
///
/// The list changes only when Steam publishes a new app, so a daily refresh is
/// plenty while keeping a fallback lookup offline.
pub const APP_LIST_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Ceiling for the streamed app-list download (256 MiB).
///
/// The published list is over 100 MB and keeps growing, so the former 64 MiB
/// cap rejected it before a single entry was matched. Streaming with this cap
/// still bounds what a broken response can write to the cache file, and
/// [`MAX_RESPONSE_BYTES`] stays where it is because it is tuned for the small
/// objects every other metadata endpoint returns.
pub const MAX_APP_LIST_BYTES: u64 = 256 * 1024 * 1024;

/// Refuse an app list with more entries than this: a trivially corrupt or
/// hostile response must not make the in-memory index unbounded.
const MAX_APP_LIST_ENTRIES: usize = 3_000_000;

/// Upper bound on the candidates one search returns.
///
/// The list crosses the IPC boundary and every candidate costs a details
/// request when picked, so the tail is capped rather than proportional to how
/// many apps share a title.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Upper bound on the in-process details cache. When it is full it is cleared;
/// a metadata entry is cheap to re-fetch and nothing depends on it surviving.
const MAX_DETAILS_CACHE_ENTRIES: usize = 256;

/// Best-effort platform label for a Steam Store app.
const PLATFORM_HINT: &str = "PC";

/// Country the storefront prices and filters for. A fixed value keeps the same
/// title resolving to the same app on every machine.
const STORE_COUNTRY_CODE: &str = "US";

/// Response language for search and details.
const STORE_LANGUAGE: &str = "english";

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

/// One `storesearch` item. Only apps are useful; the endpoint also describes
/// bundles and packages, which have no `appdetails` entry of their own.
#[derive(Debug, Clone, Deserialize)]
struct StoreSearchItem {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    name: String,
}

#[derive(Default, Deserialize)]
struct StoreSearchEnvelope {
    #[serde(default)]
    items: Vec<StoreSearchItem>,
}

/// One entry of the published app list (fallback only).
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
/// `Debug`; it does not, because the caches would make the output useless.
pub struct SteamMetadataProvider {
    transport: Arc<dyn crate::http::HttpTransport>,
    policy: RetryPolicy,
    /// Where the fallback app list is persisted; `None` keeps it in memory only.
    cache_file: Option<PathBuf>,
    app_list: Mutex<Option<AppListSnapshot>>,
    /// Serializes the one-time app-list download across threads.
    app_list_gate: Mutex<()>,
    details: Mutex<HashMap<String, GameMetadata>>,
    /// Serializes details requests (single-flight per provider).
    details_gate: Mutex<()>,
}

impl SteamMetadataProvider {
    /// Provider that keeps the fallback list in memory only (tests, previews).
    pub fn new(transport: Arc<dyn crate::http::HttpTransport>) -> Self {
        Self::build(transport, None)
    }

    /// Provider that persists the fallback list under `directory` for
    /// [`APP_LIST_TTL`], so it is downloaded once across restarts.
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

    /// Ask the storefront for the apps closest to `term`.
    ///
    /// `term` is already normalized, so the query stays inside the ASCII range
    /// the endpoint handles best and trademark glyphs cannot change the result.
    fn store_search(&self, term: &str) -> Result<Vec<StoreSearchItem>> {
        let url = search_url(term);
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

        // A `404` here means the endpoint moved or was withdrawn, not that the
        // title has no Steam entry: a real no-match is a `200` with no items.
        if !(200..300).contains(&response.status) {
            return Err(Error::Provider {
                status: response.status,
                message: format!("Steam store search returned HTTP {}", response.status),
            });
        }
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Network(format!(
                "Steam store search exceeded the {MAX_RESPONSE_BYTES} byte limit"
            )));
        }

        let parsed: StoreSearchEnvelope =
            serde_json::from_slice(&response.body).map_err(|_| Error::Provider {
                status: 200,
                message: "Steam returned an unreadable search response".to_string(),
            })?;
        Ok(parsed.items)
    }

    /// Parsed fallback app list, loading (or downloading once) it when absent
    /// or stale.
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

        let url = format!("{APP_DETAILS_URL}?appids={appid}&l={STORE_LANGUAGE}");
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

    /// Match a title through the storefront's own search, falling back to the
    /// published app list only when that search fails.
    ///
    /// Exact normalized matches rank first, then prefix/whole-word matches,
    /// then substring matches; unrelated hits are dropped instead of becoming a
    /// false candidate, and the list is capped at [`MAX_SEARCH_RESULTS`].
    /// `release_year` is not available from either index and stays `None`.
    fn search(&self, title: &str) -> Result<Vec<MetadataCandidate>> {
        let target = crate::metadata::normalize_title(title);
        if target.is_empty() {
            return Ok(Vec::new());
        }

        match self.store_search(&target) {
            // A `200` with no usable item is the storefront's own "no such app".
            // The fallback list is not consulted: a healthy search is
            // authoritative, and downloading a list it cannot improve on would
            // be wasted work on every unmatched title.
            Ok(items) => Ok(rank_candidates(
                &target,
                items
                    .iter()
                    .filter(|item| item.kind.is_empty() || item.kind.eq_ignore_ascii_case("app"))
                    .map(|item| (item.id, item.name.as_str())),
            )),
            Err(primary) => match self.app_list() {
                Ok(apps) => Ok(rank_candidates(
                    &target,
                    apps.iter().map(|app| (app.appid, app.name.as_str())),
                )),
                Err(fallback) => Err(search_failure(primary, fallback)),
            },
        }
    }

    fn details(&self, source_id: &str) -> Result<GameMetadata> {
        let appid = parse_app_id(source_id)?;
        self.details_for(appid)
    }
}

/// The `storesearch` URL for one normalized title.
fn search_url(term: &str) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("term", term)
        .append_pair("l", STORE_LANGUAGE)
        .append_pair("cc", STORE_COUNTRY_CODE)
        .finish();
    format!("{STORE_SEARCH_URL}?{query}")
}

/// Rank `(app id, name)` pairs for `target` (an already normalized title).
///
/// The storefront orders by its own notion of relevance — a search for `portal`
/// answers `Portal 2` first — so the caller's stored title decides: an exact
/// normalized match always wins, and a hit unrelated to the query is dropped
/// rather than offered as a candidate.
fn rank_candidates<'a>(
    target: &str,
    items: impl Iterator<Item = (u32, &'a str)>,
) -> Vec<MetadataCandidate> {
    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    let mut seen = HashSet::new();

    for (id, name) in items {
        if id == 0 || !seen.insert(id) {
            continue;
        }
        let name = name.trim();
        let normalized = crate::metadata::normalize_title(name);
        if normalized.is_empty() {
            continue;
        }

        let candidate = MetadataCandidate {
            source_id: id.to_string(),
            name: name.to_string(),
            release_year: None,
            platform_hint: Some(PLATFORM_HINT.to_string()),
        };
        if normalized == target {
            exact.push(candidate);
        } else if normalized.starts_with(target) || normalized.split(' ').any(|word| word == target)
        {
            prefix.push(candidate);
        } else if normalized.contains(target) {
            contains.push(candidate);
        }
    }

    exact
        .into_iter()
        .chain(prefix)
        .chain(contains)
        .take(MAX_SEARCH_RESULTS)
        .collect()
}

/// The failure to report when the primary search and the fallback both fail.
///
/// A transport or size-limit failure is the more actionable message: the
/// service either could not be reached or answered something unusable. Only a
/// successful search with no usable item means "no match", so this never
/// produces [`Error::NotFound`].
fn search_failure(primary: Error, fallback: Error) -> Error {
    match (&primary, &fallback) {
        (Error::Network(_), _) => primary,
        (_, Error::Network(_)) => fallback,
        _ => primary,
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

    /// The storefront answers a `portal` search with `Portal 2` first; the
    /// caller's stored title must win over that ordering.
    #[test]
    fn ranking_prefers_an_exact_normalized_title() {
        let items = [
            (620, "Portal 2"),
            (400, "Portal"),
            (374_040, "Portal Knights"),
            (0, "Portal"),
            (400, "Portal™"),
            (9, "Unrelated"),
        ];
        let candidates = rank_candidates("portal", items.iter().map(|(id, name)| (*id, *name)));

        let ids: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.source_id.as_str())
            .collect();
        assert_eq!(ids, vec!["400", "620", "374040"]);
        assert_eq!(candidates[0].name, "Portal");
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.platform_hint.as_deref() == Some(PLATFORM_HINT))
        );
    }

    /// The storefront search URL carries the normalized title, language, and a
    /// fixed country so the same title resolves to the same app everywhere.
    #[test]
    fn search_url_is_built_from_the_normalized_title() {
        assert_eq!(
            search_url("half life 2"),
            "https://store.steampowered.com/api/storesearch?term=half+life+2&l=english&cc=US"
        );
    }

    /// The published list outgrew the old 64 MiB cap; the download bound must
    /// stay above the real size while remaining a hard ceiling.
    #[test]
    fn the_app_list_cap_admits_the_published_list() {
        // Static bounds, checked at compile time: a cap below the published
        // list would reject it outright, and a non-page-aligned cap is a typo.
        const _: () = assert!(MAX_APP_LIST_BYTES >= 128 * 1024 * 1024);
        const _: () = assert!(MAX_APP_LIST_BYTES.is_multiple_of(1024));
    }

    /// A streamed response above the cap is a network failure, never "no match".
    ///
    /// The staging file is given its length without writing 256 MiB, which is
    /// exactly what the transport's streaming bound prevents in production.
    #[test]
    fn an_oversized_app_list_is_a_network_error() {
        struct NoTransport;

        impl crate::http::HttpTransport for NoTransport {
            fn execute(&self, _request: &HttpRequest) -> Result<crate::http::HttpResponse> {
                Err(Error::Network("no transport in this test".to_string()))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("steam_app_list.part");
        fs::File::create(&part)
            .unwrap()
            .set_len(MAX_APP_LIST_BYTES + 1)
            .unwrap();

        let provider = SteamMetadataProvider::with_cache_dir(Arc::new(NoTransport), dir.path());
        let error = provider.finish_app_list_download(&part).unwrap_err();
        assert!(matches!(error, Error::Network(_)), "{error:?}");
        assert!(error.to_string().contains("byte limit"), "{error}");
        assert!(!matches!(error, Error::NotFound(_)), "{error:?}");
    }

    /// A size limit or transport failure must survive both paths, never become
    /// "no match".
    #[test]
    fn search_failure_prefers_the_network_class() {
        assert!(matches!(
            search_failure(
                Error::Network("down".into()),
                Error::Provider {
                    status: 503,
                    message: "busy".into()
                }
            ),
            Error::Network(_)
        ));
        assert!(matches!(
            search_failure(
                Error::Provider {
                    status: 503,
                    message: "busy".into()
                },
                Error::Network("too large".into())
            ),
            Error::Network(_)
        ));
        assert!(matches!(
            search_failure(
                Error::Provider {
                    status: 503,
                    message: "busy".into()
                },
                Error::Provider {
                    status: 404,
                    message: "gone".into()
                }
            ),
            Error::Provider { status: 503, .. }
        ));
    }
}
