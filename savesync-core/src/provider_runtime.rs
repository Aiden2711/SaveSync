//! Shared runtime for remote storage providers.
//!
//! Every non-Drive provider (`webdav`, later S3 and Dropbox) needs the same four
//! things around [`crate::http`]: one classification of HTTP statuses into the
//! crate's [`Error`] taxonomy, one retry wrapper that is only ever applied to
//! requests that are safe to repeat, vendor-specific quota detection, and named
//! streaming limits instead of magic numbers in provider code.
//!
//! Nothing here decides anything about save data. A provider transports opaque
//! bytes; the caller's digest verification stays the authority.
//!
//! # Retry safety
//!
//! [`execute_idempotent`] must only wrap `GET`, `HEAD`, `PROPFIND`, metadata
//! listings, and upload steps that are safe by construction (a uniquely named
//! staging object, or the same byte range re-sent). No retry loop may re-issue a
//! replace-type write against an existing object. That rule is enforced by
//! review, not at runtime.

use crate::error::{Error, Result};
use crate::http::{
    HttpRequest, HttpResponse, HttpTransport, RetryPolicy, execute_with_retry_classified,
    is_retryable_status,
};

/// Objects at or below this size are built in memory; larger objects stream
/// from disk through [`crate::http::RequestBody::FileRange`].
pub const SMALL_OBJECT_LIMIT: u64 = 8 * 1024 * 1024;

/// Ceiling for one streamed download (8 GiB).
///
/// Save archives are not metadata: a large install can legitimately exceed one
/// gigabyte, so this is deliberately far above [`SMALL_OBJECT_LIMIT`] and is
/// *not* a size policy for saves. It exists so a hostile or broken server
/// cannot stream until the disk is full — the failure mode is a bounded
/// [`Error::Network`] with the partial file removed, not a full volume. Set per
/// deployment once real archive sizes are measured; a caller that needs another
/// bound passes its own limit to
/// [`crate::http::HttpRequest::max_download_bytes`].
pub const MAX_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Default chunk size for chunked or resumable uploads.
///
/// Matches Drive's resumable chunk size (a multiple of 256 KiB, as Drive
/// requires) so providers do not drift into per-vendor tuning.
pub const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Upload-session chunk size used by the Dropbox provider (stage 4).
pub const DROPBOX_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Hard cap on listing pages per operation, matching Drive's existing bound.
pub const MAX_LIST_PAGES: usize = 100;

/// Server-provided error text is truncated to this many characters before it
/// reaches an [`Error`] message or the UI.
pub const MAX_ERROR_CHARS: usize = 200;

/// Replacement text for any credential-looking run.
pub const REDACTED: &str = "[redacted]";

/// Query/body keys whose values are credentials.
///
/// Matched case-insensitively against the text itself, so a vendor error body
/// that echoes the offending form or query string is scrubbed before the error
/// reaches a log, a panic message, or the UI.
const SECRET_KEY_PREFIXES: [&str; 4] = [
    "refresh_token=",
    "access_token=",
    "client_secret=",
    "code_verifier=",
];

/// Authorization schemes whose parameter is a credential.
const SECRET_SCHEMES: [&str; 2] = ["bearer", "basic"];

/// Scrub credential material out of free-form text.
///
/// Handles `Authorization: Bearer <token>` / `Basic <base64>` values and
/// `refresh_token=`, `access_token=`, `client_secret=`, `code_verifier=`
/// assignments. A token run ends at whitespace or at a delimiter that cannot
/// appear unescaped in a form/query value (`&`, quotes, JSON/XML brackets…).
///
/// Over-matching is intentional: `"basic usage"` loses a word, a leaked token
/// loses the whole response. Deliberately dependency-free (no regex crate).
pub fn redact_secrets(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let rest = &text[index..];
        match secret_run_len(rest) {
            Some(consumed) => {
                output.push_str(REDACTED);
                index += consumed;
            }
            None => {
                let ch = rest.chars().next().expect("rest is non-empty");
                output.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    output
}

/// Length of the credential-bearing run starting at `text`, if any.
fn secret_run_len(text: &str) -> Option<usize> {
    for prefix in SECRET_KEY_PREFIXES {
        if !starts_with_ignore_ascii_case(text, prefix) {
            continue;
        }
        let value = &text[prefix.len()..];
        return Some(prefix.len() + token_run_len(value));
    }
    for scheme in SECRET_SCHEMES {
        if !starts_with_ignore_ascii_case(text, scheme) {
            continue;
        }
        let after = &text[scheme.len()..];
        let separator = after
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .map(char::len_utf8)
            .sum::<usize>();
        // `"basic"` alone, or `"basicValue"`, is not a scheme plus credentials.
        if separator == 0 {
            continue;
        }
        let value = &text[scheme.len() + separator..];
        let run = token_run_len(value);
        if run == 0 {
            continue;
        }
        return Some(scheme.len() + separator + run);
    }
    None
}

fn token_run_len(text: &str) -> usize {
    text.char_indices()
        .find(|(_, ch)| is_token_delimiter(*ch))
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn is_token_delimiter(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | '&' | ',' | ';' | '<' | '>' | ')' | ']' | '}' | ':' | '#' | '?' | '\\'
        )
}

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Redact credentials, then truncate: the order matters.
///
/// Truncating first could cut a token in half, leaving a fragment that no
/// longer matches a [`SECRET_KEY_PREFIXES`] entry and would survive into the
/// error message. Redacting first means truncation can only cut
/// [`REDACTED`] itself.
pub fn redact_error_text(text: &str) -> String {
    redact_secrets(text)
        .trim()
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

/// Which remote a request belongs to.
///
/// Quota signals are vendor-specific: Google Drive answers a throttled `403`
/// with `rateLimitExceeded`-style bodies, S3 answers `503 SlowDown`, and
/// Dropbox uses `429`/`too_many_requests`. WebDAV has no quota body, so a `403`
/// there is always permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFlavor {
    GoogleDrive,
    WebDav,
    S3,
    Dropbox,
}

/// How one HTTP response is treated at the provider boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    /// 2xx, including WebDAV's `207 Multi-Status`.
    Success,
    /// Missing or rejected credentials (`401`, and `403` without a quota body).
    Auth,
    /// `404`: the object does not exist.
    NotFound,
    /// `409`/`412`: an immutable object is already there, or the target is
    /// unusable. Never a rewrite trigger; callers verify or fail.
    Conflict,
    /// Safe to retry within the attempt cap.
    Transient,
    /// Any other non-2xx status: a permanent usage or server error.
    Permanent,
}

impl StatusClass {
    pub fn is_success(self) -> bool {
        self == Self::Success
    }

    pub fn is_retryable(self) -> bool {
        self == Self::Transient
    }
}

/// Classify one status code once, at the boundary.
pub fn classify_status(status: u16, body: &[u8], flavor: ProviderFlavor) -> StatusClass {
    if (200..300).contains(&status) {
        return StatusClass::Success;
    }
    match status {
        401 => StatusClass::Auth,
        403 => {
            if has_quota_signature(body, flavor) {
                StatusClass::Transient
            } else {
                StatusClass::Auth
            }
        }
        404 => StatusClass::NotFound,
        408 | 425 | 429 | 500 | 502 | 503 | 504 => StatusClass::Transient,
        409 | 412 => StatusClass::Conflict,
        _ => StatusClass::Permanent,
    }
}

/// Whether a response may be retried for this provider.
pub fn is_retryable_response(response: &HttpResponse, flavor: ProviderFlavor) -> bool {
    classify_status(response.status, &response.body, flavor).is_retryable()
}

/// Map a non-success response onto the crate's error taxonomy.
///
/// The server's text is truncated to [`MAX_ERROR_CHARS`] and never includes
/// request headers, so credentials cannot leak through an error message.
pub fn error_for(status: u16, body: &[u8], flavor: ProviderFlavor) -> Error {
    let message = error_text(body);
    match classify_status(status, body, flavor) {
        StatusClass::Success => Error::Provider { status, message },
        StatusClass::Auth => Error::Auth(format!("HTTP {status}: {message}")),
        StatusClass::NotFound => Error::NotFound(format!("HTTP {status}: {message}")),
        StatusClass::Conflict | StatusClass::Transient | StatusClass::Permanent => {
            Error::Provider { status, message }
        }
    }
}

/// Return the response when it is a success, otherwise map the failure.
pub fn ensure_success(response: HttpResponse, flavor: ProviderFlavor) -> Result<HttpResponse> {
    if (200..300).contains(&response.status) {
        Ok(response)
    } else {
        Err(error_for(response.status, &response.body, flavor))
    }
}

/// Execute an idempotent request with bounded retries for this provider.
///
/// Only requests safe to repeat may use this wrapper; see the module docs.
pub fn execute_idempotent(
    transport: &dyn HttpTransport,
    policy: &RetryPolicy,
    flavor: ProviderFlavor,
    build: impl FnMut() -> HttpRequest,
) -> Result<HttpResponse> {
    execute_with_retry_classified(transport, policy, build, |response| {
        is_retryable_response(response, flavor)
    })
}

/// Whether a `403` body carries the provider's throttling signature.
fn has_quota_signature(body: &[u8], flavor: ProviderFlavor) -> bool {
    match flavor {
        // Keep Drive's existing behaviour exactly (see `http`).
        ProviderFlavor::GoogleDrive => is_retryable_status(403, body),
        // Generic WebDAV has no quota protocol: 403 is a permission failure.
        ProviderFlavor::WebDav => false,
        ProviderFlavor::S3 => {
            let text = String::from_utf8_lossy(body);
            [
                "SlowDown",
                "RequestLimitExceeded",
                "Throttling",
                "TooManyRequests",
            ]
            .iter()
            .any(|needle| text.contains(needle))
        }
        ProviderFlavor::Dropbox => {
            let text = String::from_utf8_lossy(body);
            [
                "rate_limit",
                "too_many_requests",
                "too_many_write_operations",
            ]
            .iter()
            .any(|needle| text.contains(needle))
        }
    }
}

/// Scrubber-routed, truncated, lossy rendering of a server body.
///
/// A body may echo a request URL or form value; [`redact_error_text`] removes
/// credentials before anything is truncated into an error message.
fn error_text(body: &[u8]) -> String {
    let text = redact_error_text(&String::from_utf8_lossy(body));
    if text.is_empty() {
        "request failed".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::http::HttpMethod;

    struct ScriptedTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        calls: Mutex<usize>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
            *self.calls.lock().unwrap() += 1;
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Network("no scripted response".into()))?;
            Ok(response)
        }
    }

    fn response(status: u16, body: impl Into<Vec<u8>>) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    #[test]
    fn classifies_every_status_into_one_outcome() {
        for flavor in [
            ProviderFlavor::GoogleDrive,
            ProviderFlavor::WebDav,
            ProviderFlavor::S3,
            ProviderFlavor::Dropbox,
        ] {
            assert_eq!(classify_status(200, b"", flavor), StatusClass::Success);
            assert_eq!(classify_status(204, b"", flavor), StatusClass::Success);
            assert_eq!(classify_status(207, b"", flavor), StatusClass::Success);
            assert_eq!(classify_status(401, b"", flavor), StatusClass::Auth);
            assert_eq!(classify_status(404, b"", flavor), StatusClass::NotFound);
            assert_eq!(classify_status(409, b"", flavor), StatusClass::Conflict);
            assert_eq!(classify_status(412, b"", flavor), StatusClass::Conflict);
            assert_eq!(classify_status(429, b"", flavor), StatusClass::Transient);
            assert_eq!(classify_status(503, b"", flavor), StatusClass::Transient);
            assert_eq!(classify_status(400, b"", flavor), StatusClass::Permanent);
            // A bare 403 is a permission failure everywhere.
            assert_eq!(
                classify_status(403, b"forbidden", flavor),
                StatusClass::Auth
            );
        }
    }

    #[test]
    fn quota_403_signatures_are_per_flavor() {
        let drive = br#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#;
        assert_eq!(
            classify_status(403, drive, ProviderFlavor::GoogleDrive),
            StatusClass::Transient
        );
        // Drive's legacy body strings never make a WebDAV 403 retryable.
        assert_eq!(
            classify_status(403, drive, ProviderFlavor::WebDav),
            StatusClass::Auth
        );

        let s3 = b"<Error><Code>SlowDown</Code></Error>";
        assert_eq!(
            classify_status(503, s3, ProviderFlavor::S3),
            StatusClass::Transient
        );
        assert_eq!(
            classify_status(403, s3, ProviderFlavor::S3),
            StatusClass::Transient
        );

        let dropbox = br#"{"error_summary":"too_many_requests/.."}"#;
        assert_eq!(
            classify_status(429, dropbox, ProviderFlavor::Dropbox),
            StatusClass::Transient
        );
        assert_eq!(
            classify_status(403, dropbox, ProviderFlavor::Dropbox),
            StatusClass::Transient
        );

        // An unrelated S3/Dropbox body stays a permanent auth failure.
        assert_eq!(
            classify_status(403, b"AccessDenied", ProviderFlavor::S3),
            StatusClass::Auth
        );
    }

    #[test]
    fn maps_statuses_onto_errors() {
        assert!(matches!(
            error_for(401, b"", ProviderFlavor::WebDav),
            Error::Auth(_)
        ));
        assert!(matches!(
            error_for(404, b"missing", ProviderFlavor::WebDav),
            Error::NotFound(_)
        ));
        assert!(matches!(
            error_for(409, b"", ProviderFlavor::WebDav),
            Error::Provider { status: 409, .. }
        ));
        assert!(matches!(
            error_for(412, b"", ProviderFlavor::WebDav),
            Error::Provider { status: 412, .. }
        ));
        assert!(matches!(
            error_for(429, b"slow down", ProviderFlavor::WebDav),
            Error::Provider { status: 429, .. }
        ));
    }

    #[test]
    fn redact_secrets_scrubs_every_credential_pattern() {
        let cases = [
            ("Authorization: Bearer abc.def-123", "abc.def-123"),
            ("authorization: basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("refresh_token=rt-secret", "rt-secret"),
            ("ACCESS_TOKEN=at-secret", "at-secret"),
            ("client_secret=cs-secret", "cs-secret"),
            ("code_verifier=cv-secret", "cv-secret"),
        ];
        for (text, secret) in cases {
            let scrubbed = redact_secrets(text);
            assert!(!scrubbed.contains(secret), "{text} -> {scrubbed}");
            assert!(scrubbed.contains(REDACTED), "{text} -> {scrubbed}");
        }

        // A form/query value ends at its delimiter, so the rest of the line
        // survives and stays debuggable.
        assert_eq!(
            redact_secrets("refresh_token=abc&error=invalid_grant"),
            format!("{REDACTED}&error=invalid_grant")
        );

        // Benign text is returned byte-identical: no over-redaction of ordinary
        // vendor errors, states, or file names.
        for benign in [
            "invalid_grant",
            "forbidden",
            "no credentials here",
            "file.tar.zst",
            "state=abc123",
            "Bearer",
        ] {
            assert_eq!(redact_secrets(benign), benign);
        }
    }

    #[test]
    fn redact_error_text_bounds_output_after_redacting() {
        let text = format!(
            "access_token={} {}",
            "s".repeat(64),
            "x".repeat(MAX_ERROR_CHARS * 2)
        );
        let scrubbed = redact_error_text(&text);

        assert!(
            !scrubbed.contains("access_token="),
            "key leaked: {scrubbed}"
        );
        assert!(
            !scrubbed.contains(&"s".repeat(64)),
            "value leaked: {scrubbed}"
        );
        assert!(scrubbed.chars().count() <= MAX_ERROR_CHARS);
        assert!(scrubbed.contains(REDACTED));
    }

    #[test]
    fn error_text_is_truncated_and_never_empty() {
        let long = "x".repeat(MAX_ERROR_CHARS * 2);
        let error = error_for(400, long.as_bytes(), ProviderFlavor::WebDav);
        let message = format!("{error}");
        assert!(message.len() < MAX_ERROR_CHARS * 2);
        assert!(!message.contains(&"x".repeat(MAX_ERROR_CHARS + 1)));

        assert!(
            format!("{}", error_for(400, b"   ", ProviderFlavor::WebDav))
                .contains("request failed")
        );
    }

    #[test]
    fn permanent_failure_is_not_retried() {
        let transport =
            ScriptedTransport::new(vec![response(403, b"forbidden"), response(200, b"")]);
        let result = execute_idempotent(&transport, &policy(5), ProviderFlavor::WebDav, || {
            HttpRequest::new(HttpMethod::Head, "https://dav.test/saves")
        })
        .unwrap();
        assert_eq!(result.status, 403);
        assert_eq!(transport.calls(), 1);
    }

    #[test]
    fn throttled_request_is_retried_within_the_cap() {
        let transport = ScriptedTransport::new(vec![
            response(429, b""),
            response(503, b""),
            response(200, b"ok"),
        ]);
        let result = execute_idempotent(&transport, &policy(5), ProviderFlavor::WebDav, || {
            HttpRequest::new(HttpMethod::Propfind, "https://dav.test/saves")
        })
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(transport.calls(), 3);
    }

    #[test]
    fn attempts_are_capped_for_transient_failures() {
        let transport = ScriptedTransport::new(vec![
            response(500, b""),
            response(500, b""),
            response(500, b""),
            response(200, b""),
        ]);
        let result = execute_idempotent(&transport, &policy(3), ProviderFlavor::WebDav, || {
            HttpRequest::new(HttpMethod::Head, "https://dav.test/saves")
        })
        .unwrap();
        assert_eq!(result.status, 500);
        assert_eq!(transport.calls(), 3);
    }
}
