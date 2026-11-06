//! Minimal HTTP abstraction used by remote providers.
//!
//! The sync engine is synchronous; providers run on background worker threads
//! (for example `tauri::async_runtime::spawn_blocking`). Mocking happens at the
//! [`HttpTransport`] boundary so retry, OAuth, and Drive semantics can be
//! tested without a network.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};

/// HTTP methods used by the providers.
///
/// `Propfind`, `Mkcol`, and `Move` are the WebDAV extensions (RFC 4918);
/// `Head` is used for cheap existence checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Propfind,
    Mkcol,
    Move,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Propfind => "PROPFIND",
            Self::Mkcol => "MKCOL",
            Self::Move => "MOVE",
        }
    }
}

/// Ceiling for any response body that is buffered in memory.
///
/// Buffered responses are always small metadata: an OAuth token envelope, a
/// JSON manifest or revision object, a WebDAV `PROPFIND` listing, a Dropbox
/// metadata reply, or an error body. Blobs are streamed to disk instead. The
/// cap equals [`crate::provider_runtime::SMALL_OBJECT_LIMIT`], which is already
/// far above any legitimate metadata object, yet bounds what a hostile server
/// can make the client allocate for one request.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Request body, either in memory or a range of a file (for resumable uploads).
#[derive(Clone)]
pub enum RequestBody {
    Bytes(Arc<Vec<u8>>),
    FileRange {
        path: PathBuf,
        offset: u64,
        length: u64,
    },
}

/// Manual `Debug` that never prints body bytes.
///
/// OAuth token forms (`refresh_token=…`, `client_secret=…`) travel as
/// [`RequestBody::Bytes`]; a derived implementation would copy them into any
/// log or panic message that formats a request.
impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => write!(f, "Bytes(<{} bytes>)", bytes.len()),
            Self::FileRange {
                path,
                offset,
                length,
            } => f
                .debug_struct("FileRange")
                .field("path", path)
                .field("offset", offset)
                .field("length", length)
                .finish(),
        }
    }
}

impl RequestBody {
    pub fn bytes(bytes: Vec<u8>) -> Self {
        Self::Bytes(Arc::new(bytes))
    }

    pub fn len(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::FileRange { length, .. } => *length,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Materialize the body. Used by tests and by small JSON requests.
    pub fn read_all(&self) -> Result<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.as_ref().clone()),
            Self::FileRange {
                path,
                offset,
                length,
            } => read_file_range(path, *offset, *length),
        }
    }
}

/// One HTTP request.
#[derive(Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<RequestBody>,
    /// When set, the response body is streamed to this path instead of being
    /// buffered in [`HttpResponse::body`].
    pub download_to: Option<PathBuf>,
    /// Optional cap enforced *while* the body is streamed to
    /// [`Self::download_to`].
    ///
    /// Ignored when `download_to` is `None`. `None` (the default) keeps the
    /// historical behaviour of streaming without a limit.
    pub max_download_bytes: Option<u64>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            download_to: None,
            max_download_bytes: None,
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: RequestBody) -> Self {
        self.body = Some(body);
        self
    }

    pub fn download_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_to = Some(path.into());
        self
    }

    /// Refuse to stream more than `limit` bytes to [`Self::download_to`].
    ///
    /// Without a cap, a hostile or broken response can write an unbounded
    /// amount of data before the caller ever inspects it. With a cap, copying
    /// stops at the limit and the partial file is removed.
    pub fn max_download_bytes(mut self, limit: u64) -> Self {
        self.max_download_bytes = Some(limit);
        self
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Header names whose values are credentials or credential-bearing arguments.
///
/// `client-id` is the Twitch/IGDB application identifier: it is only useful
/// together with the client secret, so it is redacted alongside it.
const SENSITIVE_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "dropbox-api-arg",
    "client-id",
];

fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

/// Manual `Debug` that prints header names but redacts credential values and
/// never renders body bytes.
///
/// A derived implementation would print `Authorization: Bearer …`,
/// `Dropbox-API-Arg: {...}`, and the raw OAuth form body.
impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let value = if is_sensitive_header(name) {
                    "[redacted]"
                } else {
                    value.as_str()
                };
                (name.as_str(), value)
            })
            .collect();
        // Providers that can only pass credentials as query parameters (for
        // example ScreenScraper's `devid`/`devpassword`) must not leak them
        // through a panic message or trace.
        let url = crate::provider_runtime::redact_secrets(&self.url);
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &url)
            .field("headers", &headers)
            .field("body", &self.body)
            .field("download_to", &self.download_to)
            .field("max_download_bytes", &self.max_download_bytes)
            .finish()
    }
}

/// One HTTP response with a buffered body (unless the request streamed to disk).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Pluggable HTTP executor.
pub trait HttpTransport: Send + Sync {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

/// Production transport backed by `reqwest` (rustls, no OpenSSL dependency).
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
    timeout: Duration,
}

impl ReqwestTransport {
    pub fn new(timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(timeout)
            .build()
            .map_err(|error| Error::Network(error.to_string()))?;
        Ok(Self { client, timeout })
    }

    /// Transport that never follows redirects.
    ///
    /// Used where the caller has already validated the exact destination host
    /// (for example artwork downloads), so a `3xx` must fail instead of
    /// silently moving the request to a host that was never checked.
    pub fn without_redirects(timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| Error::Network(error.to_string()))?;
        Ok(Self { client, timeout })
    }
}

/// Build a WebDAV extension method such as `PROPFIND`.
///
/// `reqwest::Method` accepts arbitrary tokens, so no per-method constants are
/// needed; the names come from [`HttpMethod::as_str`] and are always valid.
fn extension_method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("HTTP method names are valid tokens")
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(120)).expect("reqwest client configuration is valid")
    }
}

impl HttpTransport for ReqwestTransport {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Head => reqwest::Method::HEAD,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Delete => reqwest::Method::DELETE,
            HttpMethod::Propfind | HttpMethod::Mkcol | HttpMethod::Move => {
                extension_method(request.method.as_str())
            }
        };

        let mut builder = self
            .client
            .request(method, &request.url)
            .timeout(self.timeout);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = match body {
                RequestBody::Bytes(bytes) => builder.body(bytes.as_ref().clone()),
                RequestBody::FileRange {
                    path,
                    offset,
                    length,
                } => {
                    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
                    file.seek(SeekFrom::Start(*offset))
                        .map_err(|e| Error::io(path, e))?;
                    let reader = file.take(*length);
                    builder.body(reqwest::blocking::Body::sized(reader, *length))
                }
            };
        }

        let mut response = builder
            .send()
            // `reqwest::Error` renders the request URL, and a provider that can
            // only authenticate through query parameters (ScreenScraper) would
            // otherwise leak its credentials into a transport error.
            .map_err(|error| {
                Error::Network(crate::provider_runtime::redact_secrets(&error.to_string()))
            })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();

        let body = if let Some(path) = &request.download_to {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            let mut file = File::create(path).map_err(|e| Error::io(path, e))?;
            match request.max_download_bytes {
                Some(limit) => {
                    if let Err(error) = copy_bounded(&mut response, &mut file, limit) {
                        // Close the handle before unlinking so the partial
                        // file can be removed on every platform.
                        drop(file);
                        let _ = std::fs::remove_file(path);
                        return Err(error);
                    }
                }
                None => {
                    std::io::copy(&mut response, &mut file).map_err(|e| Error::io(path, e))?;
                }
            }
            file.flush().map_err(|e| Error::io(path, e))?;
            Vec::new()
        } else {
            read_body_bounded(&mut response, MAX_RESPONSE_BYTES)?
        };

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Message used when a download streams past its configured byte cap.
const DOWNLOAD_LIMIT_EXCEEDED: &str = "download exceeded the configured size limit";

/// Read a buffered response body, refusing anything over `limit` bytes.
///
/// The read stops one byte past the cap, so an oversized body is detected
/// without ever allocating more than `limit + 1` bytes. A response that
/// exceeds the cap is a protocol violation by the peer, reported as
/// [`Error::Network`] so it is treated like any other transport failure.
fn read_body_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| Error::Network(error.to_string()))?;
    if body.len() > limit {
        return Err(Error::Network(format!(
            "response body exceeded the {limit} byte limit"
        )));
    }
    Ok(body)
}

/// Copy at most `limit` bytes from `reader` into `file`.
///
/// Returns the number of bytes written. As soon as the source yields more than
/// `limit` bytes, copying stops *before* the surplus is written and
/// [`Error::Network`] is returned, so an untrusted response can never put more
/// than `limit` bytes on disk. The over-limit case is a peer protocol
/// violation, not an internal invariant break, so it maps to the transport
/// taxonomy; the caller removes the partial file.
fn copy_bounded(reader: &mut impl Read, file: &mut File, limit: u64) -> Result<u64> {
    const CHUNK_BYTES: usize = 64 * 1024;

    let mut buffer = [0u8; CHUNK_BYTES];
    let mut written = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(Error::from)?;
        if read == 0 {
            return Ok(written);
        }
        let chunk = read as u64;
        if written.saturating_add(chunk) > limit {
            return Err(Error::Network(DOWNLOAD_LIMIT_EXCEEDED.to_string()));
        }
        file.write_all(&buffer[..read]).map_err(Error::from)?;
        written += chunk;
    }
}

fn read_file_range(path: &std::path::Path, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Error::io(path, e))?;
    let mut buffer = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.take(length)
        .read_to_end(&mut buffer)
        .map_err(|e| Error::io(path, e))?;
    if buffer.len() as u64 != length {
        return Err(Error::InvalidState(format!(
            "short read from {}: expected {length} bytes, got {}",
            path.display(),
            buffer.len()
        )));
    }
    Ok(buffer)
}

/// Bounded exponential backoff with jitter.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Delay before `attempt` (1-based), using deterministic jitter in `[0, 1)`.
    pub fn delay_with_jitter(&self, attempt: u32, jitter: f64) -> Duration {
        let exponent = attempt.saturating_sub(1).min(30);
        let exponential = self
            .base_delay
            .saturating_mul(1u32 << exponent)
            .min(self.max_delay);
        // Equal jitter: half fixed, half randomized. Bounds are intentional so
        // a single attempt can never exceed `max_delay`.
        let half = exponential / 2;
        let jitter = jitter.clamp(0.0, 1.0);
        let randomized = half.mul_f64(jitter);
        (half + randomized).min(self.max_delay)
    }

    fn delay(&self, attempt: u32) -> Duration {
        let seed = uuid::Uuid::new_v4();
        let bytes = seed.as_bytes();
        let fraction = u16::from_be_bytes([bytes[0], bytes[1]]) as f64 / u16::MAX as f64;
        self.delay_with_jitter(attempt, fraction)
    }
}

/// Status codes that are safe to retry for idempotent requests and for
/// resumable upload chunks.
pub fn is_retryable_status(status: u16, body: &[u8]) -> bool {
    match status {
        408 | 425 | 429 | 500 | 502 | 503 | 504 => true,
        403 => {
            let text = String::from_utf8_lossy(body);
            text.contains("rateLimitExceeded")
                || text.contains("userRateLimitExceeded")
                || text.contains("backendError")
        }
        _ => false,
    }
}

/// Execute a request with bounded retries and jitter, honoring `Retry-After`.
///
/// `build` is called for each attempt so callers can refresh credentials. Only
/// idempotent operations may use this helper; resumable uploads are resume-safe
/// by construction (the same byte range is retried).
pub fn execute_with_retry(
    transport: &dyn HttpTransport,
    policy: &RetryPolicy,
    build: impl FnMut() -> HttpRequest,
) -> Result<HttpResponse> {
    execute_with_retry_classified(transport, policy, build, |response| {
        is_retryable_status(response.status, &response.body)
    })
}

/// [`execute_with_retry`] with a caller-supplied retryability predicate.
///
/// Providers whose vendors signal throttling differently (a WebDAV server with
/// no quota body, an S3 `SlowDown`, a Dropbox `too_many_requests`) pass their
/// own predicate instead of duplicating the attempt cap, equal jitter, and
/// `Retry-After` handling. Everything else is identical, including counting a
/// transport failure as an attempt.
pub fn execute_with_retry_classified(
    transport: &dyn HttpTransport,
    policy: &RetryPolicy,
    mut build: impl FnMut() -> HttpRequest,
    is_retryable: impl Fn(&HttpResponse) -> bool,
) -> Result<HttpResponse> {
    let max_attempts = policy.max_attempts.max(1);
    for attempt in 1..=max_attempts {
        let request = build();
        match transport.execute(&request) {
            Ok(response) => {
                if is_retryable(&response) && attempt < max_attempts {
                    let delay = retry_after(&response)
                        .unwrap_or_else(|| policy.delay(attempt))
                        .min(policy.max_delay);
                    std::thread::sleep(delay);
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                if attempt < max_attempts {
                    std::thread::sleep(policy.delay(attempt));
                    continue;
                }
                return Err(error);
            }
        }
    }
    unreachable!("the loop always returns on the final attempt")
}

fn retry_after(response: &HttpResponse) -> Option<Duration> {
    let value = response.header("Retry-After")?;
    value.parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn new_request_has_no_download_cap() {
        // Existing callers must keep the unbounded streaming behaviour.
        let request = HttpRequest::new(HttpMethod::Get, "https://example.invalid/file");
        assert_eq!(request.max_download_bytes, None);

        let capped = HttpRequest::new(HttpMethod::Get, "https://example.invalid/file")
            .max_download_bytes(1234);
        assert_eq!(capped.max_download_bytes, Some(1234));
    }

    #[test]
    fn request_debug_scrubs_credentials_carried_in_the_url() {
        let request = HttpRequest::new(
            HttpMethod::Get,
            "https://api.screenscraper.fr/api2/jeuInfos.php?devid=dev-secret&devpassword=pw-secret&ssid=user-secret&sspassword=user-pw&output=json",
        )
        .header("Accept", "application/json");
        let rendered = format!("{request:?}");

        for secret in ["dev-secret", "pw-secret", "user-secret", "user-pw"] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert!(rendered.contains("output=json"), "{rendered}");
        assert!(rendered.contains("[redacted]"), "{rendered}");
    }

    #[test]
    fn copy_bounded_copies_input_under_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let mut file = File::create(&path).unwrap();
        let payload = vec![7u8; 4096];
        let mut reader = std::io::Cursor::new(payload.clone());

        let written = copy_bounded(&mut reader, &mut file, 8192).unwrap();

        assert_eq!(written, payload.len() as u64);
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn copy_bounded_allows_exactly_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let mut file = File::create(&path).unwrap();
        let mut reader = std::io::Cursor::new(vec![3u8; 1024]);

        let written = copy_bounded(&mut reader, &mut file, 1024).unwrap();

        assert_eq!(written, 1024);
    }

    #[test]
    fn copy_bounded_refuses_input_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let mut file = File::create(&path).unwrap();
        let mut reader = std::io::Cursor::new(vec![1u8; 10 * 1024]);

        let error = copy_bounded(&mut reader, &mut file, 1024).unwrap_err();

        assert!(matches!(error, Error::Network(_)));
        assert!(format!("{error}").contains("size limit"));

        drop(file);
        let written = std::fs::metadata(&path).unwrap().len();
        assert!(
            written <= 1024,
            "wrote {written} bytes to disk despite a 1024 byte cap"
        );
    }

    #[test]
    fn read_body_bounded_accepts_bodies_under_the_limit() {
        let body = read_body_bounded(&mut std::io::Cursor::new(b"payload".to_vec()), 4096).unwrap();
        assert_eq!(body, b"payload");

        // A body exactly at the cap is still a complete response.
        let exact = read_body_bounded(&mut std::io::Cursor::new(vec![1u8; 64]), 64).unwrap();
        assert_eq!(exact.len(), 64);
    }

    #[test]
    fn read_body_bounded_refuses_bodies_over_the_limit() {
        let error = read_body_bounded(&mut std::io::Cursor::new(vec![1u8; 65]), 64).unwrap_err();
        assert!(matches!(error, Error::Network(_)));

        // The reader is not drained past the cap: an oversized body costs at
        // most `limit + 1` bytes of allocation.
        let mut reader = std::io::Cursor::new(vec![1u8; 1024]);
        let _ = read_body_bounded(&mut reader, 16).unwrap_err();
        assert!(reader.position() <= 17);
    }

    #[test]
    fn request_debug_never_prints_credentials_or_body_bytes() {
        let request = HttpRequest::new(
            HttpMethod::Post,
            "https://api.dropboxapi.test/2/files/download",
        )
        .header("Authorization", "Bearer super-secret-token")
        .header("Dropbox-API-Arg", r#"{"path":"/secret/path"}"#)
        .header("Content-Type", "application/octet-stream")
        .body(RequestBody::bytes(b"refresh_token=body-secret".to_vec()));

        let rendered = format!("{request:?}");

        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(!rendered.contains("/secret/path"), "{rendered}");
        assert!(!rendered.contains("body-secret"), "{rendered}");
        assert!(rendered.contains("[redacted]"), "{rendered}");
        // Non-sensitive context stays visible, so the log is still useful.
        assert!(rendered.contains("application/octet-stream"), "{rendered}");
    }

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        let policy = RetryPolicy {
            max_attempts: 6,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(3),
        };
        let delays: Vec<u128> = (1..=6)
            .map(|attempt| policy.delay_with_jitter(attempt, 1.0).as_millis())
            .collect();
        assert_eq!(delays, vec![100, 200, 400, 800, 1600, 3000]);
        assert!(policy.delay_with_jitter(10, 1.0) <= Duration::from_secs(3));
        assert_eq!(policy.delay_with_jitter(1, 0.0).as_millis(), 50);
    }

    #[test]
    fn retryable_status_classification() {
        assert!(is_retryable_status(503, b""));
        assert!(is_retryable_status(429, b""));
        assert!(is_retryable_status(
            403,
            br#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#
        ));
        assert!(!is_retryable_status(
            403,
            br#"{"error":{"errors":[{"reason":"forbidden"}]}}"#
        ));
        assert!(!is_retryable_status(404, b""));
    }

    struct ScriptedTransport {
        responses: Mutex<Vec<HttpResponse>>,
        calls: Mutex<usize>,
    }

    impl HttpTransport for ScriptedTransport {
        fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
            *self.calls.lock().unwrap() += 1;
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(Error::Network("no scripted response".into()));
            }
            Ok(responses.remove(0))
        }
    }

    fn response(status: u16) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn retries_transient_failures_then_succeeds() {
        let transport = ScriptedTransport {
            responses: Mutex::new(vec![response(503), response(429), response(200)]),
            calls: Mutex::new(0),
        };
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        };
        let result = execute_with_retry(&transport, &policy, || {
            HttpRequest::new(HttpMethod::Get, "https://example.invalid/test")
        })
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(*transport.calls.lock().unwrap(), 3);
    }

    #[test]
    fn does_not_retry_permanent_failures() {
        let transport = ScriptedTransport {
            responses: Mutex::new(vec![response(400), response(200)]),
            calls: Mutex::new(0),
        };
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        };
        let result = execute_with_retry(&transport, &policy, || {
            HttpRequest::new(HttpMethod::Get, "https://example.invalid/test")
        })
        .unwrap();
        assert_eq!(result.status, 400);
        assert_eq!(*transport.calls.lock().unwrap(), 1);
    }
}
