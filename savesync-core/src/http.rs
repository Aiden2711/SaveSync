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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Request body, either in memory or a range of a file (for resumable uploads).
#[derive(Debug, Clone)]
pub enum RequestBody {
    Bytes(Arc<Vec<u8>>),
    FileRange {
        path: PathBuf,
        offset: u64,
        length: u64,
    },
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
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<RequestBody>,
    /// When set, the response body is streamed to this path instead of being
    /// buffered in [`HttpResponse::body`].
    pub download_to: Option<PathBuf>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            download_to: None,
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

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
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
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Delete => reqwest::Method::DELETE,
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
            .map_err(|error| Error::Network(error.to_string()))?;
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
            std::io::copy(&mut response, &mut file).map_err(|e| Error::io(path, e))?;
            file.flush().map_err(|e| Error::io(path, e))?;
            Vec::new()
        } else {
            let mut body = Vec::new();
            response
                .read_to_end(&mut body)
                .map_err(|error| Error::Network(error.to_string()))?;
            body
        };

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
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
    mut build: impl FnMut() -> HttpRequest,
) -> Result<HttpResponse> {
    let max_attempts = policy.max_attempts.max(1);
    for attempt in 1..=max_attempts {
        let request = build();
        match transport.execute(&request) {
            Ok(response) => {
                let retryable = is_retryable_status(response.status, &response.body);
                if retryable && attempt < max_attempts {
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
