//! WebDAV provider (RFC 4918) over an injected [`HttpTransport`].
//!
//! The remote namespace mirrors [`crate::provider::LocalFolderProvider`]:
//!
//! ```text
//! <base>/blobs/<aa>/<sha256>.tar.zst
//! <base>/manifests/<sha256>.json
//! <base>/games/<game-id>/revisions/<revision-id>.json
//! ```
//!
//! # Safety properties
//!
//! - **HTTPS only.** Plain `http://` endpoints are rejected before any request
//!   is issued; server certificates are validated by the transport's default
//!   (rustls) policy, so self-signed certificates fail. Trusting a pinned
//!   certificate is a deferred, explicitly-reviewed config knob.
//! - **Immutable objects.** Nothing is ever overwritten or deleted except this
//!   device's own uncommitted `.part-<uuid>` staging objects. Before any
//!   create, the final key is `HEAD`ed; when it exists the provider downloads
//!   and verifies it (idempotent success on a digest match,
//!   [`Error::IntegrityMismatch`] otherwise).
//! - **Create-if-absent upload.** Preferred path: `PUT` to a uniquely named
//!   staging key, then `MOVE` with `Overwrite: F`. Servers that reject `MOVE`
//!   fall back to a direct `PUT` carrying `If-None-Match: *`, and the fallback
//!   is remembered for the rest of the session.
//! - **Idempotent retries.** `GET`/`HEAD`/`PROPFIND`/`MKCOL` and staging
//!   `PUT`s go through [`crate::provider_runtime::execute_idempotent`]; `MOVE`
//!   is retried at most once and its outcome is resolved by re-`HEAD`ing or
//!   verifying the final key, never by replaying the move blindly.
//! - **No credentials in errors.** Basic credentials are computed once and only
//!   placed in a request header; nothing in this module formats them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use url::Url;

use crate::error::{Error, Result};
use crate::hash;
use crate::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, RequestBody, RetryPolicy};
use crate::manifest::Manifest;
use crate::models::Revision;
use crate::provider::{SyncProvider, validate_hash, validate_segment};
use crate::provider_runtime::{
    MAX_DOWNLOAD_BYTES, ProviderFlavor, ensure_success, error_for, execute_idempotent,
};

const JSON_MIME: &str = "application/json";
const BLOB_MIME: &str = "application/zstd";
const USER_AGENT: &str = "SaveSync";

/// Body of a `PROPFIND`: exactly the properties the provider consumes.
const PROPFIND_BODY: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
    "<d:propfind xmlns:d=\"DAV:\"><d:prop>",
    "<d:resourcetype/><d:getcontentlength/><d:getetag/>",
    "</d:prop></d:propfind>"
);

/// Marker for this device's own uncommitted staging objects.
const PART_MARKER: &str = ".part-";

/// `MOVE` is retried at most once: an interrupted move can legitimately leave
/// the destination present or absent, so the next attempt resolves the real
/// state instead of replaying the move.
const MAX_MOVE_ATTEMPTS: u32 = 2;

/// WebDAV implementation of [`SyncProvider`].
pub struct WebDavProvider {
    transport: Arc<dyn HttpTransport>,
    base: Url,
    /// Precomputed `Authorization` header value. Never formatted anywhere.
    authorization: String,
    retry: RetryPolicy,
    session: Mutex<SessionState>,
}

#[derive(Debug)]
struct SessionState {
    /// Collection URLs already created (or reported as existing) this session.
    collections: HashSet<String>,
    /// Some servers implement no `MOVE`; stop trying once that is known.
    move_supported: bool,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            collections: HashSet::new(),
            move_supported: true,
        }
    }
}

impl WebDavProvider {
    /// Build a provider for `endpoint` with HTTP Basic credentials.
    ///
    /// `endpoint` must be an `https://` URL without embedded credentials, a
    /// query, or a fragment. The password is only used to compute the
    /// `Authorization` header.
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        endpoint: &str,
        username: &str,
        password: &str,
    ) -> Result<Self> {
        let base = validate_endpoint(endpoint)?;
        if username.is_empty() {
            return Err(Error::InvalidState(
                "WebDAV username must not be empty".into(),
            ));
        }
        let authorization = format!(
            "Basic {}",
            STANDARD.encode(format!("{username}:{password}"))
        );
        Ok(Self {
            transport,
            base,
            authorization,
            retry: RetryPolicy::default(),
            session: Mutex::new(SessionState::default()),
        })
    }

    /// Production provider using the shared `reqwest` transport.
    pub fn production(endpoint: &str, username: &str, password: &str) -> Result<Self> {
        Self::new(
            Arc::new(crate::http::ReqwestTransport::default()),
            endpoint,
            username,
            password,
        )
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Base collection URL, for diagnostics and settings display.
    pub fn endpoint(&self) -> &Url {
        &self.base
    }

    fn authorized(&self, request: HttpRequest) -> HttpRequest {
        request
            .header("Authorization", self.authorization.clone())
            .header("User-Agent", USER_AGENT)
    }

    fn send_idempotent(&self, build: impl FnMut() -> HttpRequest) -> Result<HttpResponse> {
        execute_idempotent(
            self.transport.as_ref(),
            &self.retry,
            ProviderFlavor::WebDav,
            build,
        )
    }

    /// Join validated segments onto the base collection.
    ///
    /// `Url` percent-encodes each segment for path use (RFC 3986), so the
    /// request URL is always well formed even for game ids with spaces or
    /// non-ASCII characters.
    fn build_url(&self, segments: &[String]) -> Result<String> {
        let mut url = self.base.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                Error::InvalidState("WebDAV endpoint cannot be used as a base URL".into())
            })?;
            // Tolerate a base endpoint that ends with `/`.
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url.into())
    }

    fn blob_segments(manifest_hash: &str) -> Result<Vec<String>> {
        validate_hash(manifest_hash)?;
        Ok(vec![
            "blobs".to_string(),
            manifest_hash[..2].to_string(),
            format!("{manifest_hash}.tar.zst"),
        ])
    }

    fn manifest_segments(manifest_hash: &str) -> Result<Vec<String>> {
        validate_hash(manifest_hash)?;
        Ok(vec![
            "manifests".to_string(),
            format!("{manifest_hash}.json"),
        ])
    }

    fn revision_dir_segments(game_id: &str) -> Result<Vec<String>> {
        validate_segment(game_id)?;
        Ok(vec![
            "games".to_string(),
            game_id.to_string(),
            "revisions".to_string(),
        ])
    }

    fn revision_segments(game_id: &str, revision_id: &str) -> Result<Vec<String>> {
        validate_segment(revision_id)?;
        let mut segments = Self::revision_dir_segments(game_id)?;
        segments.push(format!("{revision_id}.json"));
        Ok(segments)
    }

    /// `PROPFIND` a resource. `None` means the resource does not exist.
    fn propfind(&self, url: &str, depth: &str) -> Result<Option<Vec<DavEntry>>> {
        let body = RequestBody::bytes(PROPFIND_BODY.as_bytes().to_vec());
        let response = self.send_idempotent(|| {
            self.authorized(
                HttpRequest::new(HttpMethod::Propfind, url)
                    .header("Depth", depth)
                    .header("Content-Type", "application/xml; charset=utf-8")
                    .body(body.clone()),
            )
        })?;
        match response.status {
            404 => Ok(None),
            status if (200..300).contains(&status) => Ok(Some(parse_multistatus(&response.body)?)),
            status => Err(error_for(status, &response.body, ProviderFlavor::WebDav)),
        }
    }

    /// `HEAD` an object. `None` means it does not exist.
    ///
    /// Servers that refuse `HEAD` (`405`/`501`) are asked with a depth-0
    /// `PROPFIND` instead, so existence is never guessed.
    fn head(&self, url: &str) -> Result<Option<u64>> {
        let response =
            self.send_idempotent(|| self.authorized(HttpRequest::new(HttpMethod::Head, url)))?;
        match response.status {
            404 => Ok(None),
            405 | 501 => Ok(self
                .propfind(url, "0")?
                .and_then(|entries| entries.first().map(|entry| entry.content_length))),
            status if (200..300).contains(&status) => {
                Ok(Some(content_length(&response).unwrap_or(0)))
            }
            status => Err(error_for(status, &response.body, ProviderFlavor::WebDav)),
        }
    }

    /// Create every missing collection level, once per session.
    ///
    /// `405`/`409` mean the collection already exists (servers disagree on
    /// which), so both are success. The result is memoised: the tree is never
    /// re-created per object.
    fn ensure_collections(&self, segments: &[String]) -> Result<()> {
        for depth in 1..segments.len() {
            let url = self.build_url(&segments[..depth])?;
            if self
                .session
                .lock()
                .expect("webdav session mutex")
                .collections
                .contains(&url)
            {
                continue;
            }
            let response = self
                .send_idempotent(|| self.authorized(HttpRequest::new(HttpMethod::Mkcol, &url)))?;
            match response.status {
                200 | 201 | 204 | 405 | 409 => {
                    self.session
                        .lock()
                        .expect("webdav session mutex")
                        .collections
                        .insert(url);
                }
                status => {
                    return Err(error_for(status, &response.body, ProviderFlavor::WebDav));
                }
            }
        }
        Ok(())
    }

    fn move_supported(&self) -> bool {
        self.session
            .lock()
            .expect("webdav session mutex")
            .move_supported
    }

    fn disable_move(&self) {
        self.session
            .lock()
            .expect("webdav session mutex")
            .move_supported = false;
    }

    /// Create an immutable object, never replacing existing content.
    ///
    /// `make_body` is only called once the final key is known to be absent, so
    /// a missing local source is not reported when the remote already holds the
    /// object (mirroring [`crate::provider::LocalFolderProvider`]).
    fn put_immutable(
        &self,
        segments: &[String],
        content_type: &str,
        make_body: impl Fn() -> Result<RequestBody>,
        verify_existing: impl Fn(&str) -> Result<()>,
    ) -> Result<()> {
        let destination = self.build_url(segments)?;
        if self.head(&destination)?.is_some() {
            return verify_existing(&destination);
        }
        self.ensure_collections(segments)?;

        if self.move_supported() {
            let mut staging_segments = segments.to_vec();
            let last = staging_segments
                .pop()
                .ok_or_else(|| Error::InvalidState("empty WebDAV object path".into()))?;
            staging_segments.push(format!(
                "{last}{PART_MARKER}{}",
                uuid::Uuid::new_v4().simple()
            ));
            let staging = self.build_url(&staging_segments)?;

            match self.put_then_move(&staging, &destination, content_type, &make_body)? {
                MoveOutcome::Created => return Ok(()),
                MoveOutcome::DestinationExists => {
                    let _ = self.delete_staging(&staging);
                    return verify_existing(&destination);
                }
                MoveOutcome::Unsupported => {
                    // Staging `PUT` already happened; drop our own temp object
                    // (never a committed one) and use the direct path.
                    let _ = self.delete_staging(&staging);
                }
            }
        }

        let body = make_body()?;
        let response = self.send_idempotent(|| {
            self.authorized(
                HttpRequest::new(HttpMethod::Put, &destination)
                    .header("If-None-Match", "*")
                    .header("Content-Type", content_type)
                    .body(body.clone()),
            )
        })?;
        match response.status {
            412 => verify_existing(&destination),
            status if (200..300).contains(&status) => Ok(()),
            status => Err(error_for(status, &response.body, ProviderFlavor::WebDav)),
        }
    }

    /// `PUT` a staging object, then `MOVE` it onto the final key.
    fn put_then_move(
        &self,
        staging: &str,
        destination: &str,
        content_type: &str,
        make_body: &dyn Fn() -> Result<RequestBody>,
    ) -> Result<MoveOutcome> {
        let body = make_body()?;
        let put = self.send_idempotent(|| {
            self.authorized(
                HttpRequest::new(HttpMethod::Put, staging)
                    .header("If-None-Match", "*")
                    .header("Content-Type", content_type)
                    .body(body.clone()),
            )
        })?;
        match put.status {
            status if (200..300).contains(&status) => {}
            status => {
                return Err(error_for(status, &put.body, ProviderFlavor::WebDav));
            }
        }

        let move_policy = RetryPolicy {
            max_attempts: MAX_MOVE_ATTEMPTS.min(self.retry.max_attempts.max(1)),
            ..self.retry
        };
        let response = execute_idempotent(
            self.transport.as_ref(),
            &move_policy,
            ProviderFlavor::WebDav,
            || {
                self.authorized(
                    HttpRequest::new(HttpMethod::Move, staging)
                        .header("Destination", destination)
                        .header("Overwrite", "F"),
                )
            },
        )?;

        match response.status {
            status if (200..300).contains(&status) => Ok(MoveOutcome::Created),
            412 => Ok(MoveOutcome::DestinationExists),
            405 | 501 => {
                self.disable_move();
                Ok(MoveOutcome::Unsupported)
            }
            status => Err(error_for(status, &response.body, ProviderFlavor::WebDav)),
        }
    }

    /// Remove one of this device's own staging objects. Errors are ignored:
    /// an orphaned `.part-*` object is never referenced by a revision.
    fn delete_staging(&self, url: &str) -> Result<()> {
        let response = execute_idempotent(
            self.transport.as_ref(),
            &RetryPolicy {
                max_attempts: 1,
                ..self.retry
            },
            ProviderFlavor::WebDav,
            || self.authorized(HttpRequest::new(HttpMethod::Delete, url)),
        )?;
        ensure_success(response, ProviderFlavor::WebDav).map(|_| ())
    }

    fn get(&self, url: &str) -> Result<HttpResponse> {
        let response =
            self.send_idempotent(|| self.authorized(HttpRequest::new(HttpMethod::Get, url)))?;
        ensure_success(response, ProviderFlavor::WebDav)
    }

    /// Stream a remote object to `destination`.
    ///
    /// The caller's digest verification is the authority; this only aborts on a
    /// non-2xx status or an empty body, and removes the partial file so a failed
    /// download never looks like a staged blob. The stream is capped at
    /// [`MAX_DOWNLOAD_BYTES`] (a disk-exhaustion guard): on overflow the
    /// transport stops writing, the partial file is removed here, and the
    /// caller sees [`Error::Network`].
    fn download_to(&self, url: &str, destination: &Path) -> Result<()> {
        let response = self.send_idempotent(|| {
            self.authorized(
                HttpRequest::new(HttpMethod::Get, url)
                    .max_download_bytes(MAX_DOWNLOAD_BYTES)
                    .download_to(destination),
            )
        });
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
        };
        if !(200..300).contains(&response.status) {
            let _ = fs::remove_file(destination);
            return Err(error_for(
                response.status,
                &response.body,
                ProviderFlavor::WebDav,
            ));
        }
        let declared = content_length(&response);
        let written = fs::metadata(destination)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if written == 0 || declared == Some(0) {
            let _ = fs::remove_file(destination);
            return Err(Error::Provider {
                status: response.status,
                message: "remote object returned an empty body".into(),
            });
        }
        Ok(())
    }

    /// Download an already-present blob and compare it with the local archive.
    ///
    /// The object is keyed by the manifest digest, not by the archive digest, so
    /// the only sound comparison is against the archive the caller is trying to
    /// publish. Identical manifests produce byte-identical archives, so any
    /// difference is a genuine immutability conflict.
    fn verify_remote_blob(&self, url: &str, source: &Path) -> Result<()> {
        let expected = hash::hash_file(source)?;
        // The caller supplied `source` from a writable blob directory, so the
        // verification scratch file lives beside it instead of in a global
        // temp location (a multi-gigabyte blob may be streamed through here).
        let scratch = source.with_extension(format!("verify-{}", uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<()> {
            self.download_to(url, &scratch)?;
            let actual = hash::hash_file(&scratch)?;
            if !actual.eq_ignore_ascii_case(&expected) {
                return Err(Error::IntegrityMismatch {
                    path: url.to_string(),
                    expected,
                    actual,
                });
            }
            Ok(())
        })();
        let _ = fs::remove_file(&scratch);
        result
    }
}

enum MoveOutcome {
    Created,
    DestinationExists,
    Unsupported,
}

impl SyncProvider for WebDavProvider {
    fn list_revisions(&self, game_id: &str) -> Result<Vec<Revision>> {
        let collection = self.build_url(&Self::revision_dir_segments(game_id)?)?;
        let Some(entries) = self.propfind(&collection, "1")? else {
            // The game has never been pushed from any device.
            return Ok(Vec::new());
        };

        let mut revisions: HashMap<String, Revision> = HashMap::new();
        for entry in entries {
            if entry.is_collection {
                continue;
            }
            // Only the decoded basename is used; object URLs are rebuilt from
            // validated segments, so an href can never steer a request.
            let Some(name) = decoded_basename(&entry.href)? else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            validate_segment(stem)?;

            let object_url = self.build_url(&Self::revision_segments(game_id, stem)?)?;
            let response = self.get(&object_url)?;
            let revision: Revision = serde_json::from_slice(&response.body)?;
            if revision.id != stem {
                return Err(Error::InvalidState(format!(
                    "revision {} does not match its object name {stem}",
                    revision.id
                )));
            }
            if revision.game_id != game_id {
                return Err(Error::InvalidState(format!(
                    "revision {} claims game {}, expected {game_id}",
                    revision.id, revision.game_id
                )));
            }
            if let Some(existing) = revisions.get(&revision.id) {
                if existing != &revision {
                    return Err(Error::IntegrityMismatch {
                        path: format!("remote revision {}", revision.id),
                        expected: format!("{existing:?}"),
                        actual: format!("{revision:?}"),
                    });
                }
            } else {
                revisions.insert(revision.id.clone(), revision);
            }
        }

        Ok(revisions.into_values().collect())
    }

    fn get_manifest(&self, manifest_hash: &str) -> Result<Manifest> {
        let url = self.build_url(&Self::manifest_segments(manifest_hash)?)?;
        let response = self.get(&url)?;
        let manifest = Manifest::from_json(std::str::from_utf8(&response.body).map_err(|_| {
            Error::InvalidState(format!("remote manifest {manifest_hash} is not UTF-8"))
        })?)?;
        if manifest.hash() != manifest_hash {
            return Err(Error::IntegrityMismatch {
                path: format!("remote manifest {manifest_hash}"),
                expected: manifest_hash.to_string(),
                actual: manifest.hash(),
            });
        }
        Ok(manifest)
    }

    fn put_revision(&self, revision: &Revision, manifest: &Manifest) -> Result<()> {
        validate_segment(&revision.id)?;
        validate_segment(&revision.game_id)?;
        let manifest_hash = manifest.hash();
        if revision.manifest_hash != manifest_hash {
            return Err(Error::InvalidState(format!(
                "revision {} does not match manifest {}",
                revision.id, manifest_hash
            )));
        }

        let manifest_bytes = manifest.to_json()?.into_bytes();
        let expected = manifest_hash.clone();
        self.put_immutable(
            &Self::manifest_segments(&manifest_hash)?,
            JSON_MIME,
            || Ok(RequestBody::bytes(manifest_bytes.clone())),
            |url| {
                let response = self.get(url)?;
                let remote =
                    Manifest::from_json(std::str::from_utf8(&response.body).map_err(|_| {
                        Error::InvalidState("remote manifest is not UTF-8".into())
                    })?)?;
                if remote.hash() != expected {
                    return Err(Error::IntegrityMismatch {
                        path: url.to_string(),
                        expected: expected.clone(),
                        actual: remote.hash(),
                    });
                }
                Ok(())
            },
        )?;

        let revision_bytes = serde_json::to_vec(revision)?;
        self.put_immutable(
            &Self::revision_segments(&revision.game_id, &revision.id)?,
            JSON_MIME,
            || Ok(RequestBody::bytes(revision_bytes.clone())),
            |url| {
                let response = self.get(url)?;
                let remote: Revision = serde_json::from_slice(&response.body)?;
                if &remote != revision {
                    return Err(Error::IntegrityMismatch {
                        path: url.to_string(),
                        expected: format!("{revision:?}"),
                        actual: format!("{remote:?}"),
                    });
                }
                Ok(())
            },
        )
    }

    fn has_blob(&self, manifest_hash: &str) -> Result<bool> {
        let url = self.build_url(&Self::blob_segments(manifest_hash)?)?;
        Ok(self.head(&url)?.is_some())
    }

    fn put_blob(&self, manifest_hash: &str, source: &Path) -> Result<()> {
        let segments = Self::blob_segments(manifest_hash)?;
        self.put_immutable(
            &segments,
            BLOB_MIME,
            || {
                if !source.is_file() {
                    return Err(Error::NotFound(format!("blob {}", source.display())));
                }
                let length = fs::metadata(source)
                    .map_err(|e| Error::io(source, e))?
                    .len();
                Ok(RequestBody::FileRange {
                    path: source.to_path_buf(),
                    offset: 0,
                    length,
                })
            },
            |url| self.verify_remote_blob(url, source),
        )
    }

    fn get_blob(&self, manifest_hash: &str, destination: &Path) -> Result<()> {
        let url = self.build_url(&Self::blob_segments(manifest_hash)?)?;
        self.download_to(&url, destination)
    }
}

fn validate_endpoint(endpoint: &str) -> Result<Url> {
    let url = Url::parse(endpoint)
        .map_err(|error| Error::InvalidState(format!("invalid WebDAV endpoint: {error}")))?;
    if url.scheme() != "https" {
        return Err(Error::InvalidState(
            "WebDAV endpoints must use https://; plain HTTP would send credentials unencrypted"
                .into(),
        ));
    }
    if url.host_str().is_none() {
        return Err(Error::InvalidState("WebDAV endpoint has no host".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::InvalidState(
            "WebDAV credentials must not be embedded in the endpoint URL".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidState(
            "WebDAV endpoint must not carry a query or fragment".into(),
        ));
    }
    Ok(url)
}

fn content_length(response: &HttpResponse) -> Option<u64> {
    response
        .header("Content-Length")
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// One `d:response` entry of a `207 Multi-Status` body.
#[derive(Debug, Default, PartialEq, Eq)]
struct DavEntry {
    href: String,
    is_collection: bool,
    content_length: u64,
}

/// Percent-decoded final path component of an `href`, if it has one.
fn decoded_basename(href: &str) -> Result<Option<String>> {
    let decoded = percent_decode(href)?;
    let trimmed = decoded.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or_default();
    Ok(if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    })
}

/// Percent-decode an `href` value (RFC 3986). `+` stays literal: WebDAV hrefs
/// are not form-encoded.
fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let escape = bytes
                .get(index + 1..index + 3)
                .and_then(|slice| std::str::from_utf8(slice).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| {
                    Error::InvalidState(format!("invalid percent-encoding in WebDAV href: {value}"))
                })?;
            out.push(escape);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::InvalidState("WebDAV href is not valid UTF-8".into()))
}

/// Parse the `207 Multi-Status` XML produced by `PROPFIND`.
///
/// Hand-rolled over a token stream: the element set is tiny
/// (`response`/`href`/`collection`/`getcontentlength`), namespace prefixes vary
/// between servers, and a full XML dependency is not justified for it.
fn parse_multistatus(body: &[u8]) -> Result<Vec<DavEntry>> {
    let xml = std::str::from_utf8(body)
        .map_err(|_| Error::InvalidState("WebDAV PROPFIND response is not valid UTF-8".into()))?;

    let tokens = tokenize(xml);
    // A body with no element at all is not a Multi-Status document: reporting it
    // as an empty listing would hide a broken (or non-XML) response from the sync
    // engine and make a populated collection look empty.
    if !tokens
        .iter()
        .any(|token| matches!(token, DavToken::Tag { .. }))
    {
        return Err(Error::InvalidState(
            "WebDAV PROPFIND response contains no XML elements".into(),
        ));
    }

    let mut entries = Vec::new();
    let mut current: Option<DavEntry> = None;
    let mut capturing_href = false;
    let mut capturing_length = false;

    for token in tokens {
        match token {
            DavToken::Tag {
                name,
                closing,
                self_closing,
            } => match name {
                "response" => {
                    if closing {
                        if let Some(entry) = current.take() {
                            entries.push(entry);
                        }
                    } else if !self_closing {
                        current = Some(DavEntry::default());
                    }
                }
                "href" => {
                    if current.is_some() && !self_closing {
                        capturing_href = !closing;
                    }
                }
                "collection" => {
                    if let Some(entry) = current.as_mut() {
                        entry.is_collection = true;
                    }
                }
                "getcontentlength" if current.is_some() && !self_closing => {
                    capturing_length = !closing;
                }
                _ => {}
            },
            DavToken::Text(text) => {
                let Some(entry) = current.as_mut() else {
                    continue;
                };
                if capturing_href {
                    entry.href.push_str(text.trim());
                } else if capturing_length && let Ok(value) = text.trim().parse::<u64>() {
                    entry.content_length = value;
                }
            }
        }
    }

    Ok(entries)
}

enum DavToken<'a> {
    Tag {
        name: &'a str,
        closing: bool,
        self_closing: bool,
    },
    Text(&'a str),
}

/// Minimal XML tokenizer: element tags (with their local name) and text runs.
///
/// Comments, declarations, and processing instructions are skipped. Attribute
/// values are irrelevant to the element set this provider consumes, so tags are
/// reduced to their name and shape.
fn tokenize(xml: &str) -> Vec<DavToken<'_>> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while let Some(relative) = xml[index..].find('<') {
        let open = index + relative;
        if open > index {
            tokens.push(DavToken::Text(&xml[index..open]));
        }
        if xml[open..].starts_with("<!--") {
            index = xml[open..]
                .find("-->")
                .map_or(xml.len(), |end| open + end + 3);
            continue;
        }
        if xml[open..].starts_with("<?") || xml[open..].starts_with("<!") {
            index = xml[open..]
                .find('>')
                .map_or(xml.len(), |end| open + end + 1);
            continue;
        }
        let Some(close) = xml[open..].find('>') else {
            break;
        };
        let inner = &xml[open + 1..open + close];
        let closing = inner.starts_with('/');
        let trimmed = inner.trim_start_matches('/');
        let self_closing = trimmed.ends_with('/');
        let raw_name = trimmed
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default();
        tokens.push(DavToken::Tag {
            name: local_name(raw_name),
            closing,
            self_closing,
        });
        index = open + close + 1;
    }
    if index < xml.len() {
        tokens.push(DavToken::Text(&xml[index..]));
    }
    tokens
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plain_http_endpoints_before_any_request() {
        struct PanicTransport;
        impl HttpTransport for PanicTransport {
            fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
                panic!("no request may be issued for a rejected endpoint");
            }
        }

        let error = WebDavProvider::new(
            Arc::new(PanicTransport),
            "http://dav.test/saves",
            "user",
            "secret",
        )
        .err()
        .expect("plain HTTP must be rejected");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    }

    #[test]
    fn rejects_endpoints_carrying_secrets_or_queries() {
        struct PanicTransport;
        impl HttpTransport for PanicTransport {
            fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
                panic!("no request may be issued for a rejected endpoint");
            }
        }

        for endpoint in [
            "https://user:secret@dav.test/saves",
            "https://dav.test/saves?token=abc",
            "https://dav.test/saves#frag",
            "not a url",
        ] {
            assert!(
                WebDavProvider::new(Arc::new(PanicTransport), endpoint, "user", "secret").is_err(),
                "expected {endpoint} to be rejected"
            );
        }
    }

    #[test]
    fn builds_percent_encoded_object_urls() {
        struct PanicTransport;
        impl HttpTransport for PanicTransport {
            fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
                panic!("no request may be issued while building URLs");
            }
        }

        let provider = WebDavProvider::new(
            Arc::new(PanicTransport),
            "https://dav.test/saves/",
            "user",
            "secret",
        )
        .unwrap();

        let url = provider
            .build_url(&WebDavProvider::revision_segments("gäme 1", "rëv 1").unwrap())
            .unwrap();
        assert!(
            url.ends_with("/saves/games/g%C3%A4me%201/revisions/r%C3%ABv%201.json"),
            "unexpected URL: {url}"
        );

        let blob = provider
            .build_url(&WebDavProvider::blob_segments(&"ab".repeat(32)).unwrap())
            .unwrap();
        assert!(blob.contains("/saves/blobs/ab/"));
        assert!(blob.ends_with(".tar.zst"));
    }

    #[test]
    fn parses_multistatus_with_varied_namespace_prefixes() {
        let xml = br#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/saves/games/g%C3%A4me%201/revisions/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat>
  </D:response>
  <D:response>
    <d:href xmlns:d="DAV:">https://dav.test/saves/games/g%C3%A4me%201/revisions/r%C3%ABv%201.json</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>42</d:getcontentlength></d:prop></d:propstat>
  </D:response>
  <response>
    <href>no-namespace.json</href>
    <prop><resourcetype/><getcontentlength>7</getcontentlength></prop>
  </response>
</D:multistatus>"#;

        let entries = parse_multistatus(xml).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_collection);
        assert!(entries[0].href.ends_with('/'));
        assert!(!entries[1].is_collection);
        assert_eq!(entries[1].content_length, 42);
        assert_eq!(
            decoded_basename(&entries[1].href).unwrap().as_deref(),
            Some("rëv 1.json")
        );
        assert_eq!(entries[2].content_length, 7);
    }

    #[test]
    fn rejects_malformed_multistatus() {
        let error = parse_multistatus(b"not xml at all").unwrap_err();
        assert!(matches!(error, Error::InvalidState(_)));
        assert!(parse_multistatus(b"<d:multistatus/>").unwrap().is_empty());
    }

    #[test]
    fn percent_decoding_keeps_plus_literal() {
        assert_eq!(percent_decode("a%2Fb+c").unwrap(), "a/b+c");
        assert!(percent_decode("bad%zz").is_err());
        assert!(percent_decode("bad%f").is_err());
    }
}
