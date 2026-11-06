//! Optional SteamGridDB cover-art client.
//!
//! Artwork is a convenience feature: nothing here touches saves, snapshots, or
//! restore state. The client is deliberately narrow and paranoid because it
//! consumes URLs and bytes chosen by a third party:
//!
//! - the API key travels only in the `Authorization` header, only to the API
//!   origin, and is never stored in an error, log, or `Debug` output;
//! - upstream response bodies are never echoed into errors;
//! - download URLs must be `https` on an allowlisted SteamGridDB CDN host, with
//!   no userinfo and no non-default port, before any request is made;
//! - downloads are capped at [`MAX_GRID_BYTES`], sniffed for a real image
//!   signature, and written through a `.part` file that is renamed into place
//!   only after it is complete.
//!
//! Endpoints and payload shapes were verified against the official SteamGridDB
//! OpenAPI description (v2.10.0, <https://www.steamgriddb.com/api/v2>).

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, RetryPolicy, execute_with_retry,
};

/// Verified SteamGridDB API v2 base URL.
pub const API_BASE: &str = "https://www.steamgriddb.com/api/v2";

/// Hard cap for a downloaded cover image (8 MiB).
pub const MAX_GRID_BYTES: u64 = 8 * 1024 * 1024;

/// Hosts that serve SteamGridDB artwork.
const ALLOWED_CDN_HOSTS: [&str; 2] = ["cdn2.steamgriddb.com", "cdn.steamgriddb.com"];

/// Portrait grids are requested in the API's 2:3 dimension set.
const PORTRAIT_DIMENSIONS: &str = "600x900";

/// Aspect ratio preferred for a game tile (2:3).
const TARGET_ASPECT_RATIO: f64 = 2.0 / 3.0;

/// Wide grids are requested in the API's 920x430 banner dimension set.
const WIDE_DIMENSIONS: &str = "920x430";

/// Aspect ratio preferred for a wide banner (920:430).
const WIDE_TARGET_ASPECT_RATIO: f64 = 920.0 / 430.0;

/// Which SteamGridDB grid family a request targets.
///
/// Both families share one client, transport, retry policy, and download trust
/// boundary; only the `dimensions` filter and the aspect ratio used for ranking
/// differ. Portrait is the default so every legacy call path keeps its exact
/// previous request and ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GridKind {
    /// 600x900 game tiles (2:3).
    #[default]
    Portrait,
    /// 920x430 header/banner art.
    Wide,
}

impl GridKind {
    /// SteamGridDB `dimensions` filter for this family.
    pub const fn dimensions(self) -> &'static str {
        match self {
            Self::Portrait => PORTRAIT_DIMENSIONS,
            Self::Wide => WIDE_DIMENSIONS,
        }
    }

    /// Aspect ratio this family's candidates are ranked against.
    pub const fn target_aspect_ratio(self) -> f64 {
        match self {
            Self::Portrait => TARGET_ASPECT_RATIO,
            Self::Wide => WIDE_TARGET_ASPECT_RATIO,
        }
    }

    /// Stable identifier used across the IPC boundary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::Wide => "wide",
        }
    }

    /// Parse an IPC-supplied kind.
    ///
    /// Unknown or missing values fall back to [`GridKind::Portrait`] so a
    /// client that predates wide art keeps its existing behavior.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "wide" => Self::Wide,
            _ => Self::Portrait,
        }
    }
}

/// Upper bound on the candidates returned by [`SteamGridDbClient::list_grids`].
///
/// The picker loads one `<img>` per candidate directly from the CDN and the
/// whole list crosses the IPC boundary, so an upstream response with hundreds
/// of grids must not scale the modal's DOM or payload without limit. The cap
/// only ever drops the worst-ranked tail; the head is unaffected.
const MAX_CANDIDATES: usize = 60;

const REJECTED_URL: &str = "SteamGridDB image URL rejected.";
const OVERSIZE_IMAGE: &str = "SteamGridDB image exceeds the size limit.";
const INVALID_IMAGE: &str = "SteamGridDB image is not a supported PNG, JPEG, or WebP file.";
const UNSUCCESSFUL_RESPONSE: &str = "SteamGridDB reported an unsuccessful response.";
const MALFORMED_RESPONSE: &str = "SteamGridDB returned an unreadable response.";

/// One downloadable cover candidate returned by SteamGridDB.
///
/// Holds only public metadata; it never carries the API key. `width` and
/// `height` are advisory ranking hints, so they may be zero when the response
/// omits them.
///
/// `Serialize`/`Deserialize` exist so the candidate can cross the IPC boundary
/// with [`crate::engine::CoverSearch`]; nothing here is secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridCandidate {
    /// Upstream grid id. It is the only value a caller may hand back when
    /// choosing a specific candidate, and it is used solely as a match key
    /// against a freshly fetched list — never as a path or a URL component.
    pub id: u64,
    pub url: String,
    pub width: u32,
    pub height: u32,
    /// Advisory only: the cache file extension is derived from the downloaded
    /// bytes, never from this declared type.
    pub mime: String,
}

/// A completed cover download.
///
/// `extension` comes from the image's own signature, so a wrong or missing
/// `Content-Type`/`mime` can neither misname the cached file nor make the
/// command layer serve the wrong media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadedGrid {
    /// Number of bytes written.
    pub size: u64,
    /// Extension implied by the magic bytes: `png`, `jpg`, or `webp`.
    pub extension: &'static str,
}

/// SteamGridDB client over a mockable [`HttpTransport`].
///
/// Deliberately does not derive `Debug`: the client is used with a caller-owned
/// API key, and no derive may tempt a later change into printing credentials.
pub struct SteamGridDbClient {
    transport: Arc<dyn HttpTransport>,
    policy: RetryPolicy,
}

impl SteamGridDbClient {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            policy: RetryPolicy::default(),
        }
    }

    /// Search for a game by title.
    ///
    /// Returns `Ok(None)` when the API reports no match, including a `404`.
    /// Among several results, a case-insensitive exact title match wins;
    /// otherwise the first result is used.
    pub fn search_game(&self, api_key: &str, title: &str) -> Result<Option<u64>> {
        let url = autocomplete_url(title)?;
        let response = execute_with_retry(self.transport.as_ref(), &self.policy, || {
            authorized(HttpRequest::new(HttpMethod::Get, &url), api_key)
                .header("Accept", "application/json")
        })?;

        if response.status == 404 {
            return Ok(None);
        }
        check_status(&response)?;

        let parsed: AutocompleteResponse =
            serde_json::from_slice(&response.body).map_err(|_| malformed_response())?;
        if !parsed.success {
            return Err(unsuccessful_response());
        }

        let target = title.trim().to_lowercase();
        if let Some(exact) = parsed
            .data
            .iter()
            .find(|game| game.name.trim().to_lowercase() == target)
        {
            return Ok(Some(exact.id));
        }
        Ok(parsed.data.first().map(|game| game.id))
    }

    /// Pick the best portrait cover for a SteamGridDB game id.
    ///
    /// Equivalent to the first entry of [`Self::list_grids`], so an automatic
    /// pick and a manual choice can never rank candidates differently.
    ///
    /// Returns `Ok(None)` only when the response contains no usable entry, or
    /// the API reports a `404`.
    pub fn pick_grid(&self, api_key: &str, sgdb_game_id: u64) -> Result<Option<GridCandidate>> {
        self.pick_grid_kind(api_key, sgdb_game_id, GridKind::Portrait)
    }

    /// Pick the best cover of `kind` for a SteamGridDB game id.
    ///
    /// Equivalent to the first entry of [`Self::list_grids_kind`].
    pub fn pick_grid_kind(
        &self,
        api_key: &str,
        sgdb_game_id: u64,
        kind: GridKind,
    ) -> Result<Option<GridCandidate>> {
        Ok(self
            .list_grids_kind(api_key, sgdb_game_id, kind)?
            .into_iter()
            .next())
    }

    /// List every usable portrait cover for a SteamGridDB game id, best first.
    ///
    /// Same request as [`Self::pick_grid`]; this only keeps the whole ranked
    /// list instead of the head. Entries are dropped when the URL is empty or
    /// fails the download trust boundary ([`validate_grid_url`]) *before* they
    /// reach the UI, so a thumbnail can never point at a host the download path
    /// would refuse.
    ///
    /// Dimensions are advisory. Entries that report them rank first, closest
    /// to 2:3 first and larger area breaking ties; entries without dimensions
    /// follow, ranked by upstream `score` and then the largest `id`. The sort is
    /// stable, so the head is byte-for-byte the entry [`Self::pick_grid`] has
    /// always chosen.
    ///
    /// The returned list is capped at [`MAX_CANDIDATES`]: the picker renders
    /// thumbnails straight from the CDN, so the payload is bounded rather than
    /// proportional to however many grids upstream publishes.
    pub fn list_grids(&self, api_key: &str, sgdb_game_id: u64) -> Result<Vec<GridCandidate>> {
        self.list_grids_kind(api_key, sgdb_game_id, GridKind::Portrait)
    }

    /// List every usable cover of `kind` for a SteamGridDB game id, best first.
    ///
    /// Identical to [`Self::list_grids`] except for the `dimensions` filter and
    /// the aspect ratio used to rank dimensioned entries.
    pub fn list_grids_kind(
        &self,
        api_key: &str,
        sgdb_game_id: u64,
        kind: GridKind,
    ) -> Result<Vec<GridCandidate>> {
        let url = grids_url(sgdb_game_id, kind)?;
        let response = execute_with_retry(self.transport.as_ref(), &self.policy, || {
            authorized(HttpRequest::new(HttpMethod::Get, &url), api_key)
                .header("Accept", "application/json")
        })?;

        if response.status == 404 {
            return Ok(Vec::new());
        }
        check_status(&response)?;

        let parsed: GridsResponse =
            serde_json::from_slice(&response.body).map_err(|_| malformed_response())?;
        if !parsed.success {
            return Err(unsuccessful_response());
        }
        Ok(ranked_grids(&parsed.data, kind))
    }

    /// Download a cover to `dest` atomically, returning the bytes written and
    /// the kind detected from the image signature.
    ///
    /// The destination's parent directory is created if needed. The image is
    /// streamed to `<dest>.part`, size-checked, signature-checked, flushed,
    /// and only then renamed over `dest`. Any failure removes the `.part` file
    /// and never leaves a partial image at `dest`.
    ///
    /// The API key is deliberately *not* attached to this request: the CDN
    /// serves public image URLs and is a different origin from the API, so
    /// sending the bearer token there would only widen credential exposure.
    /// The write is bounded by [`MAX_GRID_BYTES`] while streaming, so an
    /// oversized response is cut off before it fills the disk.
    pub fn download_grid(
        &self,
        api_key: &str,
        candidate: &GridCandidate,
        dest: &Path,
    ) -> Result<DownloadedGrid> {
        // Accepted for call-site symmetry with `search_game` and `pick_grid`,
        // but deliberately unused: the image request goes to the CDN, which
        // must never receive the credential.
        let _ = api_key;

        validate_grid_url(&candidate.url)?;

        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }

        let part = part_path(dest);
        let mut guard = PartGuard::new(part.clone());

        let response = execute_with_retry(self.transport.as_ref(), &self.policy, || {
            // No `authorized(...)` here: the bearer token is scoped to the API
            // origin. A compromised or spoofed CDN host must not learn it.
            HttpRequest::new(HttpMethod::Get, &candidate.url)
                .header("Accept", "image/*")
                .max_download_bytes(MAX_GRID_BYTES)
                .download_to(&part)
        })?;
        check_status(&response)?;

        if let Some(declared) = response
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok())
            && declared > MAX_GRID_BYTES
        {
            return Err(oversize_image());
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&part)
            .map_err(|error| Error::io(&part, error))?;

        let size = file
            .metadata()
            .map_err(|error| Error::io(&part, error))?
            .len();
        if size > MAX_GRID_BYTES {
            return Err(oversize_image());
        }
        if size == 0 {
            return Err(invalid_image());
        }

        let mut header = [0u8; 12];
        let read = file
            .read(&mut header)
            .map_err(|error| Error::io(&part, error))?;
        let Some(kind) = image_kind(&header[..read]) else {
            return Err(invalid_image());
        };

        file.sync_all().map_err(|error| Error::io(&part, error))?;
        drop(file);

        rename_over(&part, dest)?;
        guard.commit();
        Ok(DownloadedGrid {
            size,
            extension: kind.extension(),
        })
    }
}

#[derive(Deserialize)]
struct AutocompleteResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Vec<AutocompleteGame>,
}

#[derive(Deserialize)]
struct AutocompleteGame {
    id: u64,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct GridsResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Vec<GridEntry>,
}

/// Tolerant of entries with missing metadata: unknown fields are ignored, a
/// missing URL is the only disqualifier, and dimensions, score, and MIME type
/// are all optional ranking hints.
#[derive(Deserialize)]
struct GridEntry {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    url: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    score: f64,
    #[serde(default)]
    mime: String,
}

fn autocomplete_url(title: &str) -> Result<String> {
    let mut url = api_base_url()?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| misconfigured_endpoint())?;
        segments.push("search").push("autocomplete").push(title);
    }
    Ok(url.to_string())
}

fn grids_url(sgdb_game_id: u64, kind: GridKind) -> Result<String> {
    let mut url = api_base_url()?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| misconfigured_endpoint())?;
        let id = sgdb_game_id.to_string();
        segments.push("grids").push("game").push(&id);
    }
    url.query_pairs_mut()
        .append_pair("dimensions", kind.dimensions());
    Ok(url.to_string())
}

fn api_base_url() -> Result<Url> {
    Url::parse(API_BASE).map_err(|_| misconfigured_endpoint())
}

fn authorized(request: HttpRequest, api_key: &str) -> HttpRequest {
    request.header("Authorization", format!("Bearer {api_key}"))
}

/// Map a SteamGridDB API response to a crate error without ever including the
/// response body or request headers.
fn check_status(response: &HttpResponse) -> Result<()> {
    if response.status == 401 || response.status == 403 {
        return Err(Error::Auth("SteamGridDB rejected the API key.".to_string()));
    }
    if !(200..300).contains(&response.status) {
        return Err(Error::Provider {
            status: response.status,
            message: "SteamGridDB request failed.".to_string(),
        });
    }
    Ok(())
}

/// Rank candidate grids best-first, preferring the aspect ratio closest to the
/// family's target (`2:3` portrait, `920:430` wide).
///
/// Any entry with a URL is a usable fallback: dimensions are advisory upstream
/// metadata, so an entry that omits them is still downloadable. Entries whose
/// URL fails [`validate_grid_url`] are removed here rather than offered to the
/// UI or picked automatically: URL safety stays enforced at the download
/// boundary, and the picker can never render a thumbnail for a rejected host.
///
/// The comparator is deliberately *not* a total order: entries that tie on the
/// ranking keys keep their upstream order, and `sort_by` is stable, so
/// `ranked[0]` is exactly the entry the pre-picker `best_grid` selected.
fn ranked_grids(entries: &[GridEntry], kind: GridKind) -> Vec<GridCandidate> {
    let mut ranked: Vec<&GridEntry> = entries
        .iter()
        .filter(|entry| !entry.url.is_empty() && validate_grid_url(&entry.url).is_ok())
        .collect();

    ranked.sort_by(|left, right| {
        match (aspect_distance(left, kind), aspect_distance(right, kind)) {
            (Some(left_ratio), Some(right_ratio)) => left_ratio
                .total_cmp(&right_ratio)
                .then_with(|| area(right).cmp(&area(left))),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => right
                .score
                .total_cmp(&left.score)
                .then_with(|| right.id.cmp(&left.id)),
        }
    });

    ranked
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|entry| GridCandidate {
            id: entry.id,
            url: entry.url.clone(),
            width: entry.width,
            height: entry.height,
            mime: entry.mime.clone(),
        })
        .collect()
}

/// Distance between an entry's aspect ratio and the family's target, or `None`
/// when the entry reports no usable dimensions.
fn aspect_distance(entry: &GridEntry, kind: GridKind) -> Option<f64> {
    if entry.width == 0 || entry.height == 0 {
        return None;
    }
    Some((f64::from(entry.width) / f64::from(entry.height) - kind.target_aspect_ratio()).abs())
}

fn area(entry: &GridEntry) -> u64 {
    u64::from(entry.width) * u64::from(entry.height)
}

/// Enforce the download trust boundary before a request is made.
fn validate_grid_url(raw: &str) -> Result<()> {
    let url = Url::parse(raw).map_err(|_| rejected_url())?;
    if url.scheme() != "https" {
        return Err(rejected_url());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(rejected_url());
    }
    if !matches!(url.port(), None | Some(443)) {
        return Err(rejected_url());
    }
    match url.host_str() {
        Some(host) if ALLOWED_CDN_HOSTS.contains(&host) => Ok(()),
        _ => Err(rejected_url()),
    }
}

/// Image container detected from the file signature alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    Png,
    Jpeg,
    WebP,
}

impl ImageKind {
    /// Cache file extension for this kind.
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
        }
    }
}

/// Detect a supported image from its magic bytes.
///
/// The bytes are authoritative; a declared MIME type is not consulted.
fn image_kind(bytes: &[u8]) -> Option<ImageKind> {
    const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(&PNG) {
        return Some(ImageKind::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageKind::Jpeg);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageKind::WebP);
    }
    None
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Replace `dest` without ever exposing a partially written file.
///
/// `rename` replaces an existing target on Unix. Windows refuses to rename over
/// an existing file, so the destination is removed first; the window in which
/// no file exists is acceptable for a cache, whereas a partial image is not.
///
/// Shared with the engine, which downloads to a staging name and then moves the
/// completed file to the extension implied by its bytes.
pub(crate) fn rename_over(part: &Path, dest: &Path) -> Result<()> {
    match fs::rename(part, dest) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(dest).map_err(|error| Error::io(dest, error))?;
            fs::rename(part, dest).map_err(|error| Error::io(dest, error))
        }
        Err(error) => Err(Error::io(dest, error)),
    }
}

/// Removes the staging file unless the download was committed.
struct PartGuard {
    path: PathBuf,
    keep: bool,
}

impl PartGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn commit(&mut self) {
        self.keep = true;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn rejected_url() -> Error {
    Error::InvalidState(REJECTED_URL.to_string())
}

fn oversize_image() -> Error {
    Error::InvalidState(OVERSIZE_IMAGE.to_string())
}

fn invalid_image() -> Error {
    Error::InvalidState(INVALID_IMAGE.to_string())
}

fn unsuccessful_response() -> Error {
    Error::InvalidState(UNSUCCESSFUL_RESPONSE.to_string())
}

fn misconfigured_endpoint() -> Error {
    Error::InvalidState("SteamGridDB endpoint is misconfigured.".to_string())
}

fn malformed_response() -> Error {
    Error::InvalidState(MALFORMED_RESPONSE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    /// A real 1x1 PNG; the client validates the signature, not the pixels.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    const SENTINEL_KEY: &str = "sgdb-test-key-should-never-be-echoed";

    struct ScriptedTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        calls: Mutex<usize>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(0),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }

        fn urls(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.url.clone())
                .collect()
        }

        /// Every request the client attempted, in order, so tests can assert on
        /// the headers actually sent to each origin.
        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
            *self.calls.lock().unwrap() += 1;
            self.requests.lock().unwrap().push(request.clone());

            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Network("no scripted response".into()))?;

            // Mirror `ReqwestTransport`, which streams to `download_to`.
            if let Some(path) = &request.download_to {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(path, &response.body).unwrap();
            }

            Ok(response)
        }
    }

    fn client(responses: Vec<HttpResponse>) -> (SteamGridDbClient, Arc<ScriptedTransport>) {
        let transport = Arc::new(ScriptedTransport::new(responses));
        (SteamGridDbClient::new(transport.clone()), transport)
    }

    fn json_response(status: u16, value: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn binary_response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }

    fn candidate(url: &str) -> GridCandidate {
        GridCandidate {
            id: 1,
            url: url.to_string(),
            width: 600,
            height: 900,
            mime: "image/png".to_string(),
        }
    }

    #[test]
    fn search_prefers_case_insensitive_exact_match() {
        let (client, transport) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [
                    {"id": 11, "name": "Hades II"},
                    {"id": 22, "name": "HADES"},
                    {"id": 33, "name": "Hades III"}
                ]
            }),
        )]);

        let found = client.search_game("key", "Hades").unwrap();
        assert_eq!(found, Some(22));
        assert_eq!(transport.calls(), 1);
        assert!(transport.urls()[0].contains("/search/autocomplete/Hades"));
    }

    #[test]
    fn search_falls_back_to_first_result() {
        let (client, _) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [
                    {"id": 11, "name": "Hades II"},
                    {"id": 33, "name": "Hades III"}
                ]
            }),
        )]);

        assert_eq!(client.search_game("key", "Hades").unwrap(), Some(11));
    }

    #[test]
    fn search_returns_none_for_empty_results() {
        let (client, _) = client(vec![json_response(
            200,
            json!({"success": true, "data": []}),
        )]);

        assert_eq!(client.search_game("key", "Nothing").unwrap(), None);
    }

    #[test]
    fn unauthorized_search_does_not_leak_the_api_key() {
        let (client, _) = client(vec![json_response(
            401,
            json!({"success": false, "errors": [SENTINEL_KEY]}),
        )]);

        let error = client.search_game(SENTINEL_KEY, "Hades").unwrap_err();
        assert!(matches!(error, Error::Auth(_)));
        assert!(!format!("{error}").contains(SENTINEL_KEY));
        assert!(!format!("{error:?}").contains(SENTINEL_KEY));
    }

    #[test]
    fn malformed_response_does_not_leak_the_api_key() {
        // A hostile body that echoes the key in a field with the wrong type
        // must not be surfaced through a serde error message.
        let body =
            format!(r#"{{"success":true,"data":[{{"id":"{SENTINEL_KEY}","name":"Hades"}}]}}"#);
        let (client, _) = client(vec![binary_response(200, &[], body.as_bytes())]);

        let error = client.search_game(SENTINEL_KEY, "Hades").unwrap_err();
        assert!(matches!(error, Error::InvalidState(_)));
        assert!(!format!("{error}").contains(SENTINEL_KEY));
        assert!(!format!("{error:?}").contains(SENTINEL_KEY));
    }

    #[test]
    fn pick_grid_prefers_two_by_three_aspect_ratio() {
        let (client, _) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [
                    {
                        "url": "https://cdn2.steamgriddb.com/grid/wide.png",
                        "width": 1920,
                        "height": 1080,
                        "mime": "image/png"
                    },
                    {
                        "url": "https://cdn2.steamgriddb.com/grid/portrait.png",
                        "width": 600,
                        "height": 900,
                        "mime": "image/png"
                    }
                ]
            }),
        )]);

        let picked = client.pick_grid("key", 7).unwrap().unwrap();
        assert_eq!(picked.url, "https://cdn2.steamgriddb.com/grid/portrait.png");
        assert_eq!((picked.width, picked.height), (600, 900));
    }

    #[test]
    fn pick_grid_returns_none_for_empty_results() {
        let (client, _) = client(vec![json_response(
            200,
            json!({"success": true, "data": []}),
        )]);

        assert!(client.pick_grid("key", 7).unwrap().is_none());
    }

    /// Older SteamGridDB payloads omit `width`, `height`, and `mime`. They must
    /// still yield a downloadable candidate instead of being filtered away.
    #[test]
    fn pick_grid_accepts_entries_without_dimensions() {
        let (client, _) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [{
                    "id": 120,
                    "score": 1,
                    "url": "https://cdn2.steamgriddb.com/grid/x.png"
                }]
            }),
        )]);

        let picked = client
            .pick_grid("key", 7)
            .unwrap()
            .expect("an entry with a URL is still usable");
        assert_eq!(picked.url, "https://cdn2.steamgriddb.com/grid/x.png");
        assert_eq!((picked.width, picked.height), (0, 0));
    }

    /// Without dimensions, ranking degrades to score and then to the largest
    /// id, so the same response always picks the same grid.
    #[test]
    fn pick_grid_without_dimensions_ranks_by_score_then_id() {
        let (client, _) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [
                    {
                        "id": 5,
                        "score": 0.2,
                        "url": "https://cdn2.steamgriddb.com/grid/low.png"
                    },
                    {
                        "id": 9,
                        "score": 0.8,
                        "url": "https://cdn2.steamgriddb.com/grid/tie-low-id.png"
                    },
                    {
                        "id": 12,
                        "score": 0.8,
                        "url": "https://cdn2.steamgriddb.com/grid/tie-high-id.png"
                    }
                ]
            }),
        )]);

        let picked = client.pick_grid("key", 7).unwrap().unwrap();
        assert_eq!(
            picked.url,
            "https://cdn2.steamgriddb.com/grid/tie-high-id.png"
        );
        assert_eq!((picked.width, picked.height), (0, 0));
    }

    /// A ranked list keeps every usable entry, best first, and carries the
    /// upstream id so the picker can hand a specific choice back.
    #[test]
    fn list_grids_ranks_best_first_with_ids() {
        let (client, _) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [
                    {
                        "id": 1,
                        "url": "https://cdn2.steamgriddb.com/grid/wide.png",
                        "width": 1920,
                        "height": 1080,
                        "mime": "image/png"
                    },
                    {
                        "id": 2,
                        "url": "https://cdn2.steamgriddb.com/grid/portrait.png",
                        "width": 600,
                        "height": 900,
                        "mime": "image/png"
                    },
                    {
                        "id": 3,
                        "url": "https://cdn2.steamgriddb.com/grid/near.png",
                        "width": 700,
                        "height": 900,
                        "mime": "image/png"
                    },
                    {
                        "id": 4,
                        "url": "https://cdn2.steamgriddb.com/grid/unknown.png",
                        "score": 0.9
                    }
                ]
            }),
        )]);

        let listed = client.list_grids("key", 7).unwrap();
        let ids: Vec<u64> = listed.iter().map(|candidate| candidate.id).collect();
        assert_eq!(ids, vec![2, 3, 1, 4]);
        assert_eq!(
            listed[0].url,
            "https://cdn2.steamgriddb.com/grid/portrait.png"
        );
        // Ranking metadata survives the conversion to the IPC shape.
        assert_eq!((listed[0].width, listed[0].height), (600, 900));
        assert_eq!(listed[0].mime, "image/png");
        assert_eq!((listed[3].width, listed[3].height), (0, 0));
    }

    /// A candidate the download path would refuse must never reach the picker,
    /// where it would render as a broken or attacker-chosen thumbnail.
    #[test]
    fn list_grids_drops_empty_and_rejected_urls() {
        let (client, _) = client(vec![json_response(
            200,
            json!({
                "success": true,
                "data": [
                    {
                        "id": 1,
                        "url": "https://evil.example.com/grid/cover.png",
                        "width": 600,
                        "height": 900
                    },
                    {
                        "id": 2,
                        "url": "https://cdn2.steamgriddb.com/grid/cover.png",
                        "width": 600,
                        "height": 900
                    },
                    {
                        "id": 3,
                        "url": "",
                        "width": 600,
                        "height": 900
                    },
                    {
                        "id": 4,
                        "url": "http://cdn2.steamgriddb.com/grid/plain-http.png",
                        "width": 600,
                        "height": 900
                    }
                ]
            }),
        )]);

        let listed = client.list_grids("key", 7).unwrap();
        let ids: Vec<u64> = listed.iter().map(|candidate| candidate.id).collect();
        assert_eq!(ids, vec![2], "only the allowlisted https URL may survive");
    }

    /// The modal renders one `<img>` and one IPC record per candidate, so the
    /// list is bounded rather than proportional to whatever upstream publishes.
    #[test]
    fn list_grids_is_capped_at_the_candidate_limit() {
        let overflow = MAX_CANDIDATES + 5;
        let entries: Vec<Value> = (0..overflow)
            .map(|index| {
                json!({
                    "id": index,
                    "url": format!("https://cdn2.steamgriddb.com/grid/{index}.png"),
                    // Every step moves further from 2:3, so the ranking order is
                    // the input order and the dropped tail is unambiguous.
                    "width": 600 + index as u32,
                    "height": 900,
                    "mime": "image/png"
                })
            })
            .collect();
        let (client, _) = client(vec![json_response(
            200,
            json!({"success": true, "data": entries}),
        )]);

        let listed = client.list_grids("key", 7).unwrap();
        assert_eq!(listed.len(), MAX_CANDIDATES);
        assert_eq!(listed.first().map(|candidate| candidate.id), Some(0));
        assert_eq!(
            listed.last().map(|candidate| candidate.id),
            Some(MAX_CANDIDATES as u64 - 1)
        );
    }

    /// A game with no grids is an empty picker, not an error.
    #[test]
    fn list_grids_returns_empty_on_404() {
        let (client, _) = client(vec![json_response(
            404,
            json!({"success": false, "errors": ["not found"]}),
        )]);

        assert!(client.list_grids("key", 7).unwrap().is_empty());
    }

    /// The automatic pick must stay byte-for-byte the head of the manual list,
    /// or the picker would silently disagree with "Fetch cover art".
    #[test]
    fn pick_grid_is_the_head_of_list_grids() {
        let payload = json!({
            "success": true,
            "data": [
                {
                    "id": 5,
                    "url": "https://cdn2.steamgriddb.com/grid/low.png",
                    "score": 0.2
                },
                {
                    "id": 9,
                    "url": "https://cdn2.steamgriddb.com/grid/tie-low-id.png",
                    "score": 0.8
                },
                {
                    "id": 12,
                    "url": "https://cdn2.steamgriddb.com/grid/tie-high-id.png",
                    "score": 0.8
                }
            ]
        });
        let (client, _) = client(vec![
            json_response(200, payload.clone()),
            json_response(200, payload),
        ]);

        let head = client
            .list_grids("key", 7)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let picked = client.pick_grid("key", 7).unwrap().unwrap();
        assert_eq!(picked, head);
    }

    /// The kind round-trips across the IPC boundary, and anything unrecognised
    /// falls back to portrait so an older client keeps its exact behavior.
    #[test]
    fn grid_kind_parses_and_carries_its_dimensions() {
        assert_eq!(GridKind::default(), GridKind::Portrait);
        assert_eq!(GridKind::Portrait.as_str(), "portrait");
        assert_eq!(GridKind::Wide.as_str(), "wide");

        assert_eq!(GridKind::parse("portrait"), GridKind::Portrait);
        assert_eq!(GridKind::parse("wide"), GridKind::Wide);
        assert_eq!(GridKind::parse(" wide "), GridKind::Wide);
        assert_eq!(GridKind::parse("WIDE"), GridKind::Wide);
        assert_eq!(GridKind::parse(""), GridKind::Portrait);
        assert_eq!(GridKind::parse("capsule"), GridKind::Portrait);

        // Verified SteamGridDB dimension strings for each family.
        assert_eq!(GridKind::Portrait.dimensions(), "600x900");
        assert_eq!(GridKind::Wide.dimensions(), "920x430");
        assert!((GridKind::Portrait.target_aspect_ratio() - 2.0 / 3.0).abs() < f64::EPSILON);
        assert!((GridKind::Wide.target_aspect_ratio() - 920.0 / 430.0).abs() < f64::EPSILON);
    }

    /// Wide requests filter upstream by 920x430; portrait requests are unchanged.
    #[test]
    fn list_grids_kind_requests_the_family_dimensions() {
        let payload = json!({"success": true, "data": []});
        let (client, transport) = client(vec![
            json_response(200, payload.clone()),
            json_response(200, payload),
        ]);

        client.list_grids_kind("key", 7, GridKind::Wide).unwrap();
        client.list_grids("key", 7).unwrap();

        let urls = transport.urls();
        assert!(
            urls[0].contains("dimensions=920x430"),
            "wide list must filter on 920x430: {}",
            urls[0]
        );
        assert!(
            urls[1].contains("dimensions=600x900"),
            "portrait list must stay byte-identical: {}",
            urls[1]
        );
    }

    /// Wide ranking prefers 920:430 instead of 2:3, so the same payload picks a
    /// different (correct) head for each family.
    #[test]
    fn pick_grid_kind_ranks_against_the_wide_aspect_ratio() {
        let payload = json!({
            "success": true,
            "data": [
                {
                    "id": 1,
                    "url": "https://cdn2.steamgriddb.com/grid/portrait.png",
                    "width": 600,
                    "height": 900,
                    "mime": "image/png"
                },
                {
                    "id": 2,
                    "url": "https://cdn2.steamgriddb.com/grid/banner.png",
                    "width": 920,
                    "height": 430,
                    "mime": "image/png"
                }
            ]
        });
        let (client, _) = client(vec![
            json_response(200, payload.clone()),
            json_response(200, payload),
        ]);

        let wide = client
            .pick_grid_kind("key", 7, GridKind::Wide)
            .unwrap()
            .unwrap();
        assert_eq!(wide.url, "https://cdn2.steamgriddb.com/grid/banner.png");

        let portrait = client.pick_grid("key", 7).unwrap().unwrap();
        assert_eq!(
            portrait.url,
            "https://cdn2.steamgriddb.com/grid/portrait.png"
        );
    }

    /// As with the portrait pair, a wide automatic pick must be the head of the
    /// wide manual list so the button and the picker never disagree.
    #[test]
    fn pick_grid_kind_is_the_head_of_list_grids_kind() {
        let payload = json!({
            "success": true,
            "data": [
                {
                    "id": 3,
                    "url": "https://cdn2.steamgriddb.com/grid/square.png",
                    "width": 512,
                    "height": 512
                },
                {
                    "id": 4,
                    "url": "https://cdn2.steamgriddb.com/grid/banner.png",
                    "width": 920,
                    "height": 430
                }
            ]
        });
        let (client, _) = client(vec![
            json_response(200, payload.clone()),
            json_response(200, payload),
        ]);

        let head = client
            .list_grids_kind("key", 7, GridKind::Wide)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let picked = client
            .pick_grid_kind("key", 7, GridKind::Wide)
            .unwrap()
            .unwrap();
        assert_eq!(picked, head);
        assert_eq!(picked.id, 4);
    }

    #[test]
    fn download_rejects_non_allowlisted_host_without_a_request() {
        let (client, transport) = client(Vec::new());

        let error = client
            .download_grid(
                "key",
                &candidate("https://evil.example.com/grid.png"),
                Path::new("unused.png"),
            )
            .unwrap_err();

        assert!(matches!(error, Error::InvalidState(_)));
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn download_rejects_plain_http_without_a_request() {
        let (client, transport) = client(Vec::new());

        let error = client
            .download_grid(
                "key",
                &candidate("http://cdn2.steamgriddb.com/grid/cover.png"),
                Path::new("unused.png"),
            )
            .unwrap_err();

        assert!(matches!(error, Error::InvalidState(_)));
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn download_rejects_userinfo_without_a_request() {
        let (client, transport) = client(Vec::new());

        let error = client
            .download_grid(
                "key",
                &candidate("https://user:pass@cdn2.steamgriddb.com/grid/cover.png"),
                Path::new("unused.png"),
            )
            .unwrap_err();

        assert!(matches!(error, Error::InvalidState(_)));
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn download_rejects_oversize_content_length_and_writes_nothing() {
        let oversized = (MAX_GRID_BYTES + 1).to_string();
        let (client, _) = client(vec![binary_response(
            200,
            &[("Content-Length", oversized.as_str())],
            PNG,
        )]);

        let dir = tempdir().unwrap();
        let dest = dir.path().join("cover.png");

        let error = client
            .download_grid(
                "key",
                &candidate("https://cdn2.steamgriddb.com/grid/cover.png"),
                &dest,
            )
            .unwrap_err();

        assert!(matches!(error, Error::InvalidState(_)));
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn download_rejects_non_image_bytes_and_cleans_up() {
        let payload = b"<html>definitely not an image</html>";
        let (client, _) = client(vec![binary_response(200, &[], payload)]);

        let dir = tempdir().unwrap();
        let dest = dir.path().join("nested").join("cover.png");

        let error = client
            .download_grid(
                "key",
                &candidate("https://cdn2.steamgriddb.com/grid/cover.png"),
                &dest,
            )
            .unwrap_err();

        assert!(matches!(error, Error::InvalidState(_)));
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn download_writes_png_atomically() {
        let (client, transport) = client(vec![binary_response(
            200,
            &[("Content-Type", "image/png")],
            PNG,
        )]);

        let dir = tempdir().unwrap();
        let dest = dir.path().join("covers").join("cover.png");

        let written = client
            .download_grid(
                "key",
                &candidate("https://cdn2.steamgriddb.com/grid/cover.png"),
                &dest,
            )
            .unwrap();

        assert_eq!(written.size, PNG.len() as u64);
        assert_eq!(written.extension, "png");
        assert_eq!(fs::read(&dest).unwrap(), PNG);
        assert!(!part_path(&dest).exists());
        assert_eq!(transport.calls(), 1);
    }

    /// The cache file name must follow the bytes, not a declared type that can
    /// disagree with them.
    #[test]
    fn download_reports_the_extension_implied_by_the_bytes() {
        const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        const WEBP: &[u8] = &[
            b'R', b'I', b'F', b'F', 0x24, 0x00, 0x00, 0x00, b'W', b'E', b'B', b'P',
        ];
        let (client, _) = client(vec![
            binary_response(200, &[("Content-Type", "image/png")], JPEG),
            binary_response(200, &[("Content-Type", "image/png")], WEBP),
        ]);

        let dir = tempdir().unwrap();

        let jpeg = client
            .download_grid(
                "key",
                &candidate("https://cdn2.steamgriddb.com/grid/cover.png"),
                &dir.path().join("jpeg.jpg"),
            )
            .unwrap();
        assert_eq!(jpeg.extension, "jpg");

        let webp = client
            .download_grid(
                "key",
                &candidate("https://cdn2.steamgriddb.com/grid/cover.png"),
                &dir.path().join("webp.webp"),
            )
            .unwrap();
        assert_eq!(webp.extension, "webp");
    }

    /// The API key is a credential for `www.steamgriddb.com` only. The image
    /// download goes to a different origin (the CDN), which must never receive
    /// it: a spoofed, compromised, or misconfigured CDN host would otherwise
    /// learn the bearer token.
    #[test]
    fn api_key_is_sent_to_the_api_but_never_to_the_cdn() {
        let (client, transport) = client(vec![
            json_response(
                200,
                json!({"success": true, "data": [{"id": 7, "name": "Hades"}]}),
            ),
            json_response(
                200,
                json!({
                    "success": true,
                    "data": [{
                        "url": "https://cdn2.steamgriddb.com/grid/cover.png",
                        "width": 600,
                        "height": 900,
                        "mime": "image/png"
                    }]
                }),
            ),
            binary_response(200, &[("Content-Type", "image/png")], PNG),
        ]);

        assert_eq!(
            client.search_game(SENTINEL_KEY, "Hades").unwrap(),
            Some(7),
            "the scripted search response should match"
        );
        let picked = client.pick_grid(SENTINEL_KEY, 7).unwrap().unwrap();

        let dir = tempdir().unwrap();
        let dest = dir.path().join("cover.png");
        client.download_grid(SENTINEL_KEY, &picked, &dest).unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 3, "search, pick, then download");

        let bearer = format!("Bearer {SENTINEL_KEY}");
        for api_request in &requests[..2] {
            assert_eq!(
                api_request.header_value("Authorization"),
                Some(bearer.as_str()),
                "API requests must stay authenticated: {}",
                api_request.url
            );
        }

        let download = &requests[2];
        assert!(
            download.url.starts_with("https://cdn2.steamgriddb.com/"),
            "unexpected download origin: {}",
            download.url
        );
        assert_eq!(
            download.header_value("Authorization"),
            None,
            "the CDN download must not carry the API key"
        );
        assert_eq!(download.header_value("Accept"), Some("image/*"));
        // The write is capped while streaming, not only after the fact.
        assert_eq!(download.max_download_bytes, Some(MAX_GRID_BYTES));
    }
}
