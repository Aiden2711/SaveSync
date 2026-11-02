//! Dropbox provider over an injected [`HttpTransport`] and [`TokenProvider`].
//!
//! Objects live in the app's own Dropbox folder (app-folder access is a
//! property of the app, not a scope), so SaveSync only sees files it created
//! itself. The namespace mirrors [`crate::provider::LocalFolderProvider`],
//! [`crate::webdav::WebDavProvider`], and the Drive provider:
//!
//! ```text
//! /blobs/<aa>/<sha256>.tar.zst
//! /manifests/<sha256>.json
//! /games/<game-id>/revisions/<revision-id>.json
//! ```
//!
//! # Safety properties
//!
//! - **Immutable objects.** Every create uses `mode: "add"` with
//!   `autorename: false`, so Dropbox itself refuses to replace an existing
//!   object. Before any create the final path is checked with
//!   `files/get_metadata`; when it exists the provider downloads it and
//!   compares (idempotent success on a match, [`Error::Provider`] `409`
//!   otherwise — a differently signed object is never overwritten). A `409`
//!   answered by the create itself is resolved the same way, which is what a
//!   lost-race retry looks like.
//! - **Own digests only.** Dropbox's `content_hash` is advisory metadata; this
//!   module never trusts it. Downloaded blobs are re-hashed with SaveSync's own
//!   SHA-256 and a mismatch becomes [`Error::IntegrityMismatch`] with the
//!   partial file removed.
//! - **Resumable uploads.** Objects at or above one chunk use
//!   `upload_session/start` (`session_type: "concurrent"`), `append_v2` with
//!   the `cursor` wrapper, then `finish`. Every append except the final one is
//!   a multiple of [`UPLOAD_SESSION_ALIGNMENT`] bytes, as concurrent sessions
//!   require. A `409 incorrect_offset` is adopted once from the server's
//!   `correct_offset`; a second one fails instead of looping.
//! - **No credential handling.** The provider never refreshes a token: a `401`
//!   surfaces as [`Error::Auth`] so the caller decides whether to re-authorize.
//!   No token, refresh token, or authorization header is ever formatted into an
//!   error message, and server-provided text is truncated.
//! - **Bounded I/O.** Listing pages are capped by
//!   [`crate::provider_runtime::MAX_LIST_PAGES`] with a repeating-cursor guard;
//!   error bodies are read up to [`MAX_ERROR_BYTES`] and truncated; blobs stream
//!   through [`crate::http::RequestBody::FileRange`] and
//!   [`crate::http::HttpRequest::download_to`] instead of being buffered.
//!
//! `Dropbox-API-Arg` is an HTTP header, so the JSON it carries must be ASCII.
//! Values are serialized normally and then re-escaped (see [`ascii_escape`]):
//! `%`, `0x7F`, control characters, and non-ASCII text become `\uXXXX`
//! escapes, which is exactly what the API accepts in a header and what the
//! server decodes back to the original path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::hash;
use crate::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, RequestBody, RetryPolicy};
use crate::manifest::Manifest;
use crate::models::Revision;
use crate::oauth::{OAuthConfig, OAuthTokenManager, TokenProvider};
use crate::provider::{SyncProvider, validate_hash, validate_segment};
use crate::provider_runtime::{
    DROPBOX_CHUNK_SIZE, MAX_DOWNLOAD_BYTES, MAX_LIST_PAGES, ProviderFlavor, error_for,
    execute_idempotent,
};

pub const DROPBOX_API: &str = "https://api.dropboxapi.com/2";
pub const DROPBOX_CONTENT_API: &str = "https://content.dropboxapi.com/2";

/// Concurrent upload sessions accept only appends that are a multiple of this
/// many bytes, except for the final append that `finish` commits.
pub const UPLOAD_SESSION_ALIGNMENT: usize = 4 * 1024 * 1024;

/// Entries requested per listing page.
const LIST_LIMIT: u32 = 1000;

/// Upper bound on the bytes read from a failed download's streamed body before
/// it is discarded. The text is truncated again before it reaches an error.
const MAX_ERROR_BYTES: u64 = 4 * 1024;

const USER_AGENT: &str = "SaveSync";

/// Dropbox implementation of [`SyncProvider`].
pub struct DropboxProvider {
    transport: Arc<dyn HttpTransport>,
    tokens: Arc<dyn TokenProvider>,
    api_base: String,
    content_base: String,
    /// Chunk size for session uploads; always a multiple of
    /// [`UPLOAD_SESSION_ALIGNMENT`]. Objects smaller than one chunk are
    /// uploaded in a single request.
    chunk_size: usize,
    retry: RetryPolicy,
}

impl DropboxProvider {
    pub fn new(transport: Arc<dyn HttpTransport>, tokens: Arc<dyn TokenProvider>) -> Self {
        Self {
            transport,
            tokens,
            api_base: DROPBOX_API.to_string(),
            content_base: DROPBOX_CONTENT_API.to_string(),
            chunk_size: DROPBOX_CHUNK_SIZE,
            retry: RetryPolicy::default(),
        }
    }

    /// Production provider from a Dropbox app key and a refresh token already
    /// stored in the OS keyring.
    pub fn production(app_key: &str, refresh_token: &str) -> Result<Self> {
        let transport = Arc::new(crate::http::ReqwestTransport::default());
        let tokens = Arc::new(OAuthTokenManager::new(
            OAuthConfig::dropbox(app_key),
            transport.clone(),
            refresh_token,
        ));
        Ok(Self::new(transport, tokens))
    }

    /// Override endpoints (tests and regional mirrors).
    pub fn with_endpoints(
        mut self,
        api_base: impl Into<String>,
        content_base: impl Into<String>,
    ) -> Self {
        self.api_base = api_base.into();
        self.content_base = content_base.into();
        self
    }

    /// Override the session chunk size.
    ///
    /// Values are clamped to at least one alignment unit and rounded down to a
    /// multiple of it, because Dropbox rejects misaligned concurrent appends.
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        let aligned = chunk_size / UPLOAD_SESSION_ALIGNMENT * UPLOAD_SESSION_ALIGNMENT;
        self.chunk_size = aligned.max(UPLOAD_SESSION_ALIGNMENT);
        self
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Path of the blob object for `manifest_hash`.
    fn blob_path(manifest_hash: &str) -> Result<String> {
        validate_hash(manifest_hash)?;
        Ok(format!(
            "/blobs/{}/{manifest_hash}.tar.zst",
            &manifest_hash[..2]
        ))
    }

    /// Path of the manifest object for `manifest_hash`.
    fn manifest_path(manifest_hash: &str) -> Result<String> {
        validate_hash(manifest_hash)?;
        Ok(format!("/manifests/{manifest_hash}.json"))
    }

    /// Path of the revision collection for `game_id`.
    fn revision_dir_path(game_id: &str) -> Result<String> {
        validate_segment(game_id)?;
        Ok(format!("/games/{game_id}/revisions"))
    }

    /// Path of one revision object.
    fn revision_path(game_id: &str, revision_id: &str) -> Result<String> {
        validate_segment(revision_id)?;
        Ok(format!(
            "{}/{revision_id}.json",
            Self::revision_dir_path(game_id)?
        ))
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("{}{endpoint}", self.api_base.trim_end_matches('/'))
    }

    fn content_url(&self, endpoint: &str) -> String {
        format!("{}{endpoint}", self.content_base.trim_end_matches('/'))
    }

    /// Execute an idempotent request with the current access token.
    ///
    /// The token is resolved once per call: a retry re-sends the same request
    /// with the same credentials, and a `401` is never retried by the transport
    /// (it is not a retryable status), so it reaches the caller as
    /// [`Error::Auth`].
    fn send_idempotent(&self, mut build: impl FnMut(&str) -> HttpRequest) -> Result<HttpResponse> {
        let token = self.tokens.access_token()?;
        execute_idempotent(
            self.transport.as_ref(),
            &self.retry,
            ProviderFlavor::Dropbox,
            || build(&token),
        )
    }

    /// `POST` a JSON RPC endpoint (metadata, listing).
    fn post_json(&self, endpoint: &str, body: &Value) -> Result<HttpResponse> {
        let url = self.api_url(endpoint);
        let payload = serde_json::to_vec(body)?;
        self.send_idempotent(|token| {
            authorized(HttpRequest::new(HttpMethod::Post, &url), token)
                .header("Content-Type", "application/json")
                .body(RequestBody::bytes(payload.clone()))
        })
    }

    /// `POST` a content endpoint, optionally with a body.
    fn content_request(
        &self,
        endpoint: &str,
        arg: &Value,
        body: Option<RequestBody>,
    ) -> Result<HttpResponse> {
        let url = self.content_url(endpoint);
        let header = api_arg(arg)?;
        self.send_idempotent(|token| {
            let mut request = authorized(HttpRequest::new(HttpMethod::Post, &url), token)
                .header("Dropbox-API-Arg", &header);
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            request
        })
    }

    /// Whether an object exists. `409 path/not_found` means "no".
    fn exists(&self, path: &str) -> Result<bool> {
        let response = self.post_json("/files/get_metadata", &json!({ "path": path }))?;
        match response.status {
            status if (200..300).contains(&status) => Ok(true),
            409 if is_path_not_found(&response.body) => Ok(false),
            status => Err(error_for(status, &response.body, ProviderFlavor::Dropbox)),
        }
    }

    /// Download a small object into memory (manifests and revision JSON).
    fn download_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let url = self.content_url("/files/download");
        let header = api_arg(&json!({ "path": path }))?;
        let response = self.send_idempotent(|token| {
            authorized(HttpRequest::new(HttpMethod::Post, &url), token)
                .header("Dropbox-API-Arg", &header)
        })?;
        match response.status {
            status if (200..300).contains(&status) => {
                if response.body.is_empty() {
                    return Err(Error::Provider {
                        status,
                        message: format!("Dropbox object {path} returned an empty body"),
                    });
                }
                Ok(response.body)
            }
            409 if is_path_not_found(&response.body) => {
                Err(Error::NotFound(format!("Dropbox object {path}")))
            }
            status => Err(error_for(status, &response.body, ProviderFlavor::Dropbox)),
        }
    }

    /// Stream a remote object to `destination`.
    ///
    /// A failure removes the destination, so a partial download can never look
    /// like a staged blob. The digest check lives in the caller
    /// ([`SyncProvider::get_blob`]); this only enforces a non-empty body. The
    /// stream is capped at [`MAX_DOWNLOAD_BYTES`] (a disk-exhaustion guard): on
    /// overflow the transport stops writing, the partial file is removed here,
    /// and the caller sees [`Error::Network`].
    fn download_to(&self, path: &str, destination: &Path) -> Result<()> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let url = self.content_url("/files/download");
        let header = api_arg(&json!({ "path": path }))?;
        let response = self.send_idempotent(|token| {
            authorized(HttpRequest::new(HttpMethod::Post, &url), token)
                .header("Dropbox-API-Arg", &header)
                .max_download_bytes(MAX_DOWNLOAD_BYTES)
                .download_to(destination)
        });

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
        };

        if !(200..300).contains(&response.status) {
            // The error envelope was streamed to the destination file by the
            // transport; read a bounded prefix before discarding it, because
            // the response body itself is empty for streamed requests.
            let detail = read_bounded(destination, MAX_ERROR_BYTES);
            let _ = fs::remove_file(destination);
            if is_path_not_found(&detail) {
                // Same taxonomy as `download_bytes`: a missing object is a
                // `NotFound`, not a generic provider failure.
                return Err(Error::NotFound(format!("Dropbox object {path}")));
            }
            return Err(error_for(response.status, &detail, ProviderFlavor::Dropbox));
        }

        let written = fs::metadata(destination)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if written == 0 {
            let _ = fs::remove_file(destination);
            return Err(Error::Provider {
                status: response.status,
                message: format!("Dropbox object {path} returned an empty body"),
            });
        }
        Ok(())
    }

    /// The `Dropbox-API-Arg` of a create: never replace, never rename.
    fn commit_arg(path: &str) -> Value {
        json!({
            "path": path,
            "mode": "add",
            "autorename": false,
            "mute": true,
            "strict_conflict": true,
        })
    }

    /// Create an immutable object, or confirm an identical one already exists.
    ///
    /// `verify` inspects the existing bytes and returns an error when they do
    /// not match, so a differently signed object is never overwritten.
    fn put_bytes(
        &self,
        path: &str,
        bytes: Vec<u8>,
        verify: impl Fn(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if self.exists(path)? {
            let existing = self.download_bytes(path)?;
            return verify(&existing);
        }

        let response = self.content_request(
            "/files/upload",
            &Self::commit_arg(path),
            Some(RequestBody::bytes(bytes)),
        )?;
        match response.status {
            status if (200..300).contains(&status) => Ok(()),
            // Another writer won the race between the metadata check and the
            // create. Resolve by verifying what is actually there.
            409 if is_conflict(&response.body) => {
                let existing = self.download_bytes(path)?;
                verify(&existing)
            }
            status => Err(error_for(status, &response.body, ProviderFlavor::Dropbox)),
        }
    }

    /// Upload a blob, resuming through a concurrent upload session when it is
    /// at least one chunk long.
    fn upload_blob(&self, path: &str, source: &Path, length: u64) -> Result<()> {
        let response = if length < self.chunk_size as u64 {
            let bytes = fs::read(source).map_err(|e| Error::io(source, e))?;
            self.content_request(
                "/files/upload",
                &Self::commit_arg(path),
                Some(RequestBody::bytes(bytes)),
            )?
        } else {
            self.upload_session(path, source, length)?
        };

        match response.status {
            status if (200..300).contains(&status) => Ok(()),
            409 if is_conflict(&response.body) => Err(conflict_error(path)),
            status => Err(error_for(status, &response.body, ProviderFlavor::Dropbox)),
        }
    }

    /// Chunked upload: `upload_session/start` -> `append_v2` -> `finish`.
    ///
    /// Every append but the last sends exactly `chunk_size` bytes, which is a
    /// multiple of [`UPLOAD_SESSION_ALIGNMENT`]; the final append carries
    /// `close: true` and `finish` commits the session.
    fn upload_session(&self, path: &str, source: &Path, length: u64) -> Result<HttpResponse> {
        let start = self.content_request(
            "/files/upload_session/start",
            &json!({ "close": false, "session_type": "concurrent" }),
            Some(RequestBody::bytes(Vec::new())),
        )?;
        let start = success_or_error(start)?;
        let session: SessionStart = serde_json::from_slice(&start.body)?;
        if session.session_id.is_empty() {
            return Err(Error::Provider {
                status: start.status,
                message: "Dropbox upload session did not return a session id".into(),
            });
        }

        let mut offset = 0u64;
        let mut resynced = false;
        loop {
            let Some(remaining) = length.checked_sub(offset) else {
                return Err(Error::Provider {
                    status: 0,
                    message: "Dropbox reported an upload offset beyond the object length".into(),
                });
            };
            let take = remaining.min(self.chunk_size as u64);
            let close = take == remaining;
            let arg = json!({
                "cursor": { "session_id": session.session_id, "offset": offset },
                "close": close,
            });
            let response = self.content_request(
                "/files/upload_session/append_v2",
                &arg,
                Some(RequestBody::FileRange {
                    path: source.to_path_buf(),
                    offset,
                    length: take,
                }),
            )?;

            match response.status {
                status if (200..300).contains(&status) => {}
                status if status == 409 => {
                    // Adopt the server's offset exactly once. Repeating it means
                    // the session is not converging, so fail instead of looping.
                    let Some(correct) = incorrect_offset(&response.body) else {
                        return Err(error_for(status, &response.body, ProviderFlavor::Dropbox));
                    };
                    if resynced || correct > length {
                        return Err(Error::Provider {
                            status,
                            message: format!(
                                "Dropbox upload session rejected offset {offset} twice"
                            ),
                        });
                    }
                    resynced = true;
                    offset = correct;
                    continue;
                }
                status => {
                    return Err(error_for(status, &response.body, ProviderFlavor::Dropbox));
                }
            }

            offset += take;
            if close {
                break;
            }
        }

        let finish = self.content_request(
            "/files/upload_session/finish",
            &json!({
                "cursor": { "session_id": session.session_id, "offset": offset },
                "commit": Self::commit_arg(path),
            }),
            Some(RequestBody::bytes(Vec::new())),
        )?;
        success_or_error(finish)
    }

    /// Download an existing blob and compare it with the local `source` bytes.
    ///
    /// The object is keyed by the manifest digest, not by the archive digest, so
    /// the only sound comparison is against the archive the caller is trying to
    /// publish. Identical manifests produce byte-identical archives, so any
    /// difference is a genuine immutability conflict.
    fn verify_remote_blob(&self, path: &str, source: &Path) -> Result<()> {
        let expected = hash::hash_file(source)?;
        // The caller's `source` lives in the writable blob directory, so the
        // scratch file goes beside it rather than in a global temp location.
        let scratch = source.with_extension(format!("verify-{}", uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<()> {
            self.download_to(path, &scratch)?;
            let actual = hash::hash_file(&scratch)?;
            if !actual.eq_ignore_ascii_case(&expected) {
                return Err(conflict_error(path));
            }
            Ok(())
        })();
        let _ = fs::remove_file(&scratch);
        result
    }

    /// Revision object names in one collection, paged and bounded.
    fn list_revision_names(&self, directory: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen: HashSet<String> = HashSet::new();
        let mut pages = 0usize;

        loop {
            if pages >= MAX_LIST_PAGES {
                return Err(Error::Provider {
                    status: 0,
                    message: format!(
                        "Dropbox listing of {directory} exceeded {MAX_LIST_PAGES} pages; refusing to loop"
                    ),
                });
            }
            pages += 1;

            let (endpoint, body) = match &cursor {
                None => (
                    "/files/list_folder",
                    json!({
                        "path": directory,
                        "recursive": false,
                        "limit": LIST_LIMIT,
                    }),
                ),
                Some(cursor) => ("/files/list_folder/continue", json!({ "cursor": cursor })),
            };
            let response = self.post_json(endpoint, &body)?;
            if response.status == 409 && is_path_not_found(&response.body) {
                // The game has never been pushed from any device.
                return Ok(Vec::new());
            }
            let response = success_or_error(response)?;

            let page: ListFolder = serde_json::from_slice(&response.body)?;
            for entry in page.entries {
                if entry.tag.as_deref() != Some("file") {
                    continue;
                }
                let Some(name) = entry.name else { continue };
                if let Some(stem) = name.strip_suffix(".json") {
                    names.push(stem.to_string());
                }
            }

            if !page.has_more {
                return Ok(names);
            }
            match page.cursor {
                Some(next) if !next.is_empty() && seen.insert(next.clone()) => {
                    cursor = Some(next);
                }
                // A cursor that repeats forever would either loop or silently
                // truncate the listing; neither is acceptable for metadata.
                Some(_) => {
                    return Err(Error::Provider {
                        status: 0,
                        message: format!(
                            "Dropbox listing of {directory} returned a repeating cursor"
                        ),
                    });
                }
                None => {
                    return Err(Error::Provider {
                        status: 0,
                        message: format!(
                            "Dropbox listing of {directory} reported more pages without a cursor"
                        ),
                    });
                }
            }
        }
    }
}

impl SyncProvider for DropboxProvider {
    fn list_revisions(&self, game_id: &str) -> Result<Vec<Revision>> {
        let directory = Self::revision_dir_path(game_id)?;
        let mut revisions: HashMap<String, Revision> = HashMap::new();

        for stem in self.list_revision_names(&directory)? {
            // The listing's `path_display` is never used to steer a request:
            // the object path is rebuilt from a validated segment.
            validate_segment(&stem)?;
            let path = Self::revision_path(game_id, &stem)?;
            let bytes = self.download_bytes(&path)?;
            let revision: Revision = serde_json::from_slice(&bytes)?;

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
        let path = Self::manifest_path(manifest_hash)?;
        let bytes = self.download_bytes(&path)?;
        let manifest = Manifest::from_json(std::str::from_utf8(&bytes).map_err(|_| {
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

        self.put_bytes(
            &Self::manifest_path(&manifest_hash)?,
            manifest.to_json()?.into_bytes(),
            |existing| {
                let remote =
                    Manifest::from_json(std::str::from_utf8(existing).map_err(|_| {
                        Error::InvalidState("remote manifest is not UTF-8".into())
                    })?)?;
                if remote.hash() != manifest_hash {
                    return Err(conflict_error(&format!("/manifests/{manifest_hash}.json")));
                }
                Ok(())
            },
        )?;

        self.put_bytes(
            &Self::revision_path(&revision.game_id, &revision.id)?,
            serde_json::to_vec(revision)?,
            |existing| {
                let remote: Revision = serde_json::from_slice(existing)?;
                if &remote != revision {
                    return Err(conflict_error(&format!(
                        "/games/{}/revisions/{}.json",
                        revision.game_id, revision.id
                    )));
                }
                Ok(())
            },
        )
    }

    fn has_blob(&self, manifest_hash: &str) -> Result<bool> {
        self.exists(&Self::blob_path(manifest_hash)?)
    }

    fn put_blob(&self, manifest_hash: &str, source: &Path) -> Result<()> {
        let path = Self::blob_path(manifest_hash)?;
        if self.exists(&path)? {
            return self.verify_remote_blob(&path, source);
        }
        if !source.is_file() {
            return Err(Error::NotFound(format!("blob {}", source.display())));
        }
        let length = fs::metadata(source)
            .map_err(|e| Error::io(source, e))?
            .len();
        self.upload_blob(&path, source, length)
    }

    fn get_blob(&self, manifest_hash: &str, destination: &Path) -> Result<()> {
        // `manifest_hash` names the revision object; the stored archive is its
        // tar.zst payload and therefore has a different digest. Content is
        // verified on every download by `BlobStore::ingest`, which checks each
        // archive entry against the manifest, exactly as the WebDAV and Drive
        // providers rely on. Verifying the payload against the object key here
        // would reject every legitimate blob. The transport still rejects
        // empty bodies and removes every partial file.
        self.download_to(&Self::blob_path(manifest_hash)?, destination)
    }
}

/// Dropbox's `upload_session/start` response.
#[derive(Debug, Deserialize)]
struct SessionStart {
    session_id: String,
}

/// One `files/list_folder` page.
#[derive(Debug, Deserialize)]
struct ListFolder {
    #[serde(default)]
    entries: Vec<ListEntry>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Debug, Deserialize)]
struct ListEntry {
    #[serde(rename = ".tag")]
    tag: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

fn authorized(request: HttpRequest, token: &str) -> HttpRequest {
    request
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
}

/// Serialize a content-endpoint argument into an ASCII-safe header value.
fn api_arg(value: &Value) -> Result<String> {
    Ok(ascii_escape(&serde_json::to_string(value)?))
}

/// Escape everything an HTTP header cannot carry as an ASCII `\uXXXX` sequence.
///
/// `serde_json` has already escaped quotes, backslashes, and control characters;
/// this additionally escapes `%`, `0x7F`, and non-ASCII text (as UTF-16 code
/// units, so characters outside the BMP become surrogate pairs). Both are
/// lossless under JSON decoding, which is what Dropbox applies to the header.
fn ascii_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        let needs_escape =
            character == '%' || character == '\u{7f}' || !character.is_ascii() || character < ' ';
        if !needs_escape {
            escaped.push(character);
            continue;
        }
        let mut units = [0u16; 2];
        for unit in character.encode_utf16(&mut units) {
            escaped.push_str(&format!("\\u{unit:04x}"));
        }
    }
    escaped
}

/// Turn a successful response into `Ok`, any other status into an error.
fn success_or_error(response: HttpResponse) -> Result<HttpResponse> {
    if (200..300).contains(&response.status) {
        Ok(response)
    } else {
        Err(error_for(
            response.status,
            &response.body,
            ProviderFlavor::Dropbox,
        ))
    }
}

/// Error returned when an immutable object already holds different bytes.
fn conflict_error(path: &str) -> Error {
    Error::Provider {
        status: 409,
        message: format!("refusing to overwrite immutable Dropbox object {path}"),
    }
}

/// Read at most `limit` bytes from a streamed response body.
fn read_bounded(path: &Path, limit: u64) -> Vec<u8> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut buffer = Vec::new();
    let _ = file.take(limit).read_to_end(&mut buffer);
    buffer
}

/// `error_summary` of a Dropbox error envelope, if the body is one.
fn error_summary(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Envelope {
        error_summary: Option<String>,
    }
    serde_json::from_slice::<Envelope>(body)
        .ok()
        .and_then(|envelope| envelope.error_summary)
        .unwrap_or_default()
}

/// Whether an error body says the requested path does not exist.
///
/// Dropbox answers a missing path with `409` and
/// `"error_summary": "path/not_found/..."`; the `.tag` form is checked too
/// because a body may carry only one of the two.
fn is_path_not_found(body: &[u8]) -> bool {
    if error_summary(body).contains("path/not_found") {
        return true;
    }
    matches!(
        serde_json::from_slice::<Value>(body)
            .ok()
            .as_ref()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("path"))
                    .and_then(|path| path.get(".tag"))
                    .and_then(Value::as_str)
            }),
        Some("not_found")
    )
}

/// Whether an error body is a `conflict` (an immutable object already exists).
fn is_conflict(body: &[u8]) -> bool {
    error_summary(body).contains("conflict") || String::from_utf8_lossy(body).contains("conflict")
}

/// `correct_offset` of an `incorrect_offset` session error.
fn incorrect_offset(body: &[u8]) -> Option<u64> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error")?;
    let tag = error.get(".tag").and_then(Value::as_str);
    let summary = value
        .get("error_summary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if tag != Some("incorrect_offset") && !summary.contains("incorrect_offset") {
        return None;
    }
    error
        .get("correct_offset")
        .and_then(Value::as_u64)
        .or_else(|| {
            error
                .get("incorrect_offset")?
                .get("correct_offset")?
                .as_u64()
        })
}
