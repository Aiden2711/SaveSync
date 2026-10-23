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

use serde::Deserialize;
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

const REJECTED_URL: &str = "SteamGridDB image URL rejected.";
const OVERSIZE_IMAGE: &str = "SteamGridDB image exceeds the size limit.";
const INVALID_IMAGE: &str = "SteamGridDB image is not a supported PNG, JPEG, or WebP file.";
const UNSUCCESSFUL_RESPONSE: &str = "SteamGridDB reported an unsuccessful response.";
const MALFORMED_RESPONSE: &str = "SteamGridDB returned an unreadable response.";

/// One downloadable cover candidate returned by SteamGridDB.
///
/// Holds only public metadata; it never carries the API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCandidate {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub mime: String,
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
    /// Prefers the aspect ratio closest to 2:3, breaking ties by larger area.
    /// Returns `Ok(None)` when the game has no usable grids.
    pub fn pick_grid(&self, api_key: &str, sgdb_game_id: u64) -> Result<Option<GridCandidate>> {
        let url = grids_url(sgdb_game_id)?;
        let response = execute_with_retry(self.transport.as_ref(), &self.policy, || {
            authorized(HttpRequest::new(HttpMethod::Get, &url), api_key)
                .header("Accept", "application/json")
        })?;

        if response.status == 404 {
            return Ok(None);
        }
        check_status(&response)?;

        let parsed: GridsResponse =
            serde_json::from_slice(&response.body).map_err(|_| malformed_response())?;
        if !parsed.success {
            return Err(unsuccessful_response());
        }
        Ok(best_grid(&parsed.data))
    }

    /// Download a cover to `dest` atomically, returning the bytes written.
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
    ) -> Result<u64> {
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
        if !is_supported_image(&header[..read]) {
            return Err(invalid_image());
        }

        file.sync_all().map_err(|error| Error::io(&part, error))?;
        drop(file);

        rename_over(&part, dest)?;
        guard.commit();
        Ok(size)
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

/// Tolerant of entries with missing metadata: unknown fields are ignored and
/// entries without usable dimensions or a URL are skipped rather than failing
/// the whole response.
#[derive(Deserialize)]
struct GridEntry {
    #[serde(default)]
    url: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
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

fn grids_url(sgdb_game_id: u64) -> Result<String> {
    let mut url = api_base_url()?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| misconfigured_endpoint())?;
        let id = sgdb_game_id.to_string();
        segments.push("grids").push("game").push(&id);
    }
    url.query_pairs_mut()
        .append_pair("dimensions", PORTRAIT_DIMENSIONS);
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

fn best_grid(entries: &[GridEntry]) -> Option<GridCandidate> {
    entries
        .iter()
        .filter(|entry| entry.width > 0 && entry.height > 0 && !entry.url.is_empty())
        .min_by(|left, right| {
            let left_ratio =
                (f64::from(left.width) / f64::from(left.height) - TARGET_ASPECT_RATIO).abs();
            let right_ratio =
                (f64::from(right.width) / f64::from(right.height) - TARGET_ASPECT_RATIO).abs();
            left_ratio.total_cmp(&right_ratio).then_with(|| {
                let left_area = u64::from(left.width) * u64::from(left.height);
                let right_area = u64::from(right.width) * u64::from(right.height);
                right_area.cmp(&left_area)
            })
        })
        .map(|entry| GridCandidate {
            url: entry.url.clone(),
            width: entry.width,
            height: entry.height,
            mime: entry.mime.clone(),
        })
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

fn is_supported_image(bytes: &[u8]) -> bool {
    const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(&PNG) {
        return true;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
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
fn rename_over(part: &Path, dest: &Path) -> Result<()> {
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

        assert_eq!(written, PNG.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), PNG);
        assert!(!part_path(&dest).exists());
        assert_eq!(transport.calls(), 1);
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
