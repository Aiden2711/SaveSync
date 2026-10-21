//! Google Drive REST v3 provider.
//!
//! Objects live in the app-only `appDataFolder`, so SaveSync only sees files it
//! created itself. The provider implements the same immutable, idempotent
//! contract as [`crate::provider::LocalFolderProvider`]:
//!
//! - revisions, manifests, and blobs are content-addressed and never mutated;
//! - uploads are verified against their digest before being recorded;
//! - small objects use multipart upload, blobs above 5 MiB use resumable
//!   uploads with bounded exponential backoff and jitter;
//! - `401` responses refresh the access token once and retry;
//! - transient `5xx`, `429`, and rate-limit `403` responses are retried.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, RequestBody, RetryPolicy,
    is_retryable_status,
};
use crate::manifest::Manifest;
use crate::models::Revision;
use crate::oauth::{OAuthConfig, OAuthTokenManager, TokenProvider};
use crate::provider::SyncProvider;

pub const GOOGLE_DRIVE_API: &str = "https://www.googleapis.com/drive/v3";
pub const GOOGLE_DRIVE_UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";

/// Blobs at or below this size use multipart upload.
const MULTIPART_LIMIT: u64 = 5 * 1024 * 1024;
/// Default resumable chunk size (multiple of 256 KiB, as Drive requires).
const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

const KIND: &str = "savesyncKind";
const GAME_ID: &str = "savesyncGameId";
const REVISION_ID: &str = "savesyncRevisionId";
const MANIFEST_HASH: &str = "savesyncManifestHash";
const VALUE: &str = "savesyncValue";

const KIND_REVISION: &str = "revision";
const KIND_MANIFEST: &str = "manifest";
const KIND_BLOB: &str = "blob";

const JSON_MIME: &str = "application/json";
const BLOB_MIME: &str = "application/zstd";

/// Google Drive implementation of [`SyncProvider`].
pub struct GoogleDriveProvider {
    transport: Arc<dyn HttpTransport>,
    tokens: Arc<dyn TokenProvider>,
    api_base: String,
    upload_base: String,
    chunk_size: usize,
    multipart_limit: u64,
    retry: RetryPolicy,
    /// (kind, value) -> Drive file id. Objects are immutable, so entries never
    /// go stale.
    cache: Mutex<HashMap<(String, String), String>>,
    upload_lock: Mutex<()>,
}

impl GoogleDriveProvider {
    pub fn new(transport: Arc<dyn HttpTransport>, tokens: Arc<dyn TokenProvider>) -> Self {
        Self {
            transport,
            tokens,
            api_base: GOOGLE_DRIVE_API.to_string(),
            upload_base: GOOGLE_DRIVE_UPLOAD_API.to_string(),
            chunk_size: DEFAULT_CHUNK_SIZE,
            multipart_limit: MULTIPART_LIMIT,
            retry: RetryPolicy::default(),
            cache: Mutex::new(HashMap::new()),
            upload_lock: Mutex::new(()),
        }
    }

    /// Build a production provider from a Google OAuth client and a refresh
    /// token previously stored in the OS keyring.
    pub fn production(client_id: &str, refresh_token: &str) -> Result<Self> {
        let transport = Arc::new(crate::http::ReqwestTransport::default());
        let tokens = Arc::new(OAuthTokenManager::new(
            OAuthConfig::google(client_id),
            transport.clone(),
            refresh_token,
        ));
        Ok(Self::new(transport, tokens))
    }

    /// Override endpoints (used by tests and by regional mirrors).
    pub fn with_endpoints(
        mut self,
        api_base: impl Into<String>,
        upload_base: impl Into<String>,
    ) -> Self {
        self.api_base = api_base.into();
        self.upload_base = upload_base.into();
        self
    }

    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size.max(1);
        self
    }

    /// Override the multipart threshold (tests and constrained transports).
    pub fn with_multipart_limit(mut self, limit: u64) -> Self {
        self.multipart_limit = limit;
        self
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    fn execute(&self, build: impl Fn(&str) -> HttpRequest) -> Result<HttpResponse> {
        let max_attempts = self.retry.max_attempts.max(1);
        let mut attempt = 1u32;
        let mut refreshed = false;

        loop {
            let token = self.tokens.access_token()?;
            match self.transport.execute(&build(&token)) {
                Ok(response) => {
                    if response.status == 401 && !refreshed {
                        refreshed = true;
                        self.tokens.refresh()?;
                        continue;
                    }
                    if is_retryable_status(response.status, &response.body)
                        && attempt < max_attempts
                    {
                        std::thread::sleep(self.retry.delay_with_jitter(attempt, jitter()));
                        attempt += 1;
                        continue;
                    }
                    return Ok(response);
                }
                Err(error) => {
                    if attempt < max_attempts {
                        std::thread::sleep(self.retry.delay_with_jitter(attempt, jitter()));
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    fn list_files(&self, query: &str) -> Result<Vec<DriveFile>> {
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        let mut pages = 0usize;

        loop {
            if pages > 100 {
                return Err(Error::Provider {
                    status: 0,
                    message: "Drive file listing exceeded 100 pages; refusing to loop".into(),
                });
            }
            pages += 1;

            let mut params: Vec<(&str, String)> = vec![
                ("spaces", "appDataFolder".into()),
                ("q", query.into()),
                (
                    "fields",
                    "files(id,name,appProperties),nextPageToken".into(),
                ),
                ("pageSize", "1000".into()),
            ];
            if let Some(token) = &page_token {
                params.push(("pageToken", token.clone()));
            }
            let url = build_url(&self.api_base, "/files", &params)?;

            let response =
                self.execute(|token| authorized(HttpRequest::new(HttpMethod::Get, &url), token))?;
            let response = ensure_success(response)?;
            let page: FileList = serde_json::from_slice(&response.body)?;
            files.extend(page.files);
            match page.next_page_token {
                Some(token) if !token.is_empty() && seen_tokens.insert(token.clone()) => {
                    page_token = Some(token);
                }
                _ => break,
            }
        }
        Ok(files)
    }

    fn find_file(&self, kind: &str, value: &str) -> Result<Option<String>> {
        let cache_key = (kind.to_string(), value.to_string());
        if let Some(id) = self.cache.lock().expect("cache mutex").get(&cache_key) {
            return Ok(Some(id.clone()));
        }

        let query = format!(
            "appProperties has {{ key='{KIND}' and value='{}' }} \
             and appProperties has {{ key='{VALUE}' and value='{}' }} and trashed=false",
            escape_query_value(kind)?,
            escape_query_value(value)?
        );
        let files = self.list_files(&query)?;
        if let Some(file) = files.first() {
            validate_file_id(&file.id)?;
            self.cache
                .lock()
                .expect("cache mutex")
                .insert(cache_key, file.id.clone());
            return Ok(Some(file.id.clone()));
        }
        Ok(None)
    }

    fn download(&self, file_id: &str) -> Result<Vec<u8>> {
        validate_file_id(file_id)?;
        let url = build_url(
            &self.api_base,
            &format!("/files/{file_id}"),
            &[("alt", "media".into())],
        )?;
        let response =
            self.execute(|token| authorized(HttpRequest::new(HttpMethod::Get, &url), token))?;
        Ok(ensure_success(response)?.body)
    }

    fn download_to(&self, file_id: &str, destination: &Path) -> Result<()> {
        validate_file_id(file_id)?;
        let url = build_url(
            &self.api_base,
            &format!("/files/{file_id}"),
            &[("alt", "media".into())],
        )?;
        let request = HttpRequest::new(HttpMethod::Get, &url).download_to(destination);
        let response = self.execute(|token| authorized(request.clone(), token))?;
        ensure_success(response)?;
        Ok(())
    }

    fn metadata(&self, kind: &str, value: &str, name: String, mime: &str) -> Value {
        let mut app_properties = serde_json::Map::new();
        app_properties.insert(KIND.into(), Value::String(kind.into()));
        app_properties.insert(VALUE.into(), Value::String(value.into()));
        json!({
            "name": name,
            "parents": ["appDataFolder"],
            "mimeType": mime,
            "appProperties": app_properties,
        })
    }

    fn put_immutable_json(
        &self,
        kind: &str,
        value: &str,
        name: String,
        bytes: Vec<u8>,
        verify: impl Fn(&[u8]) -> Result<()>,
    ) -> Result<String> {
        let metadata = self.metadata(kind, value, name, JSON_MIME);
        self.put_immutable_json_with_metadata(kind, value, metadata, bytes, verify)
    }

    fn put_immutable_json_with_metadata(
        &self,
        kind: &str,
        value: &str,
        metadata: Value,
        bytes: Vec<u8>,
        verify: impl Fn(&[u8]) -> Result<()>,
    ) -> Result<String> {
        let _guard = self.upload_lock.lock().expect("upload mutex");
        if let Some(file_id) = self.find_file(kind, value)? {
            let existing = self.download(&file_id)?;
            verify(&existing)?;
            return Ok(file_id);
        }

        let file = self.upload_multipart(&metadata, JSON_MIME, &bytes)?;
        self.cache
            .lock()
            .expect("cache mutex")
            .insert((kind.to_string(), value.to_string()), file.id.clone());
        Ok(file.id)
    }

    fn upload_multipart(&self, metadata: &Value, mime: &str, data: &[u8]) -> Result<DriveFile> {
        let boundary = format!("savesync-{}", uuid::Uuid::new_v4().simple());
        let body = multipart_body(&boundary, metadata, mime, data)?;
        let url = build_url(
            &self.upload_base,
            "/files",
            &[
                ("uploadType", "multipart".into()),
                ("fields", "id,name,appProperties".into()),
            ],
        )?;
        let content_type = format!("multipart/related; boundary={boundary}");

        let response = self.execute(|token| {
            authorized(HttpRequest::new(HttpMethod::Post, &url), token)
                .header("Content-Type", &content_type)
                .body(RequestBody::bytes(body.clone()))
        })?;
        let response = ensure_success(response)?;
        let file: DriveFile = serde_json::from_slice(&response.body)?;
        validate_file_id(&file.id)?;
        Ok(file)
    }

    fn upload_resumable(
        &self,
        metadata: &Value,
        mime: &str,
        path: &Path,
        length: u64,
    ) -> Result<DriveFile> {
        let url = build_url(
            &self.upload_base,
            "/files",
            &[
                ("uploadType", "resumable".into()),
                ("fields", "id,name,appProperties".into()),
            ],
        )?;
        let metadata_bytes = serde_json::to_vec(metadata)?;

        let response = self.execute(|token| {
            authorized(HttpRequest::new(HttpMethod::Post, &url), token)
                .header("Content-Type", JSON_MIME)
                .header("X-Upload-Content-Type", mime)
                .header("X-Upload-Content-Length", length.to_string())
                .body(RequestBody::bytes(metadata_bytes.clone()))
        })?;
        let response = ensure_success(response)?;
        let session_url = response
            .header("Location")
            .ok_or_else(|| Error::Provider {
                status: response.status,
                message: "resumable upload did not return a session URL".into(),
            })?
            .to_string();

        let mut offset = 0u64;
        while offset < length {
            let chunk = (length - offset).min(self.chunk_size as u64);
            let end = offset + chunk - 1;
            let content_range = format!("bytes {offset}-{end}/{length}");
            let chunk_response = self.execute(|token| {
                authorized(HttpRequest::new(HttpMethod::Put, &session_url), token)
                    .header("Content-Range", &content_range)
                    .body(RequestBody::FileRange {
                        path: path.to_path_buf(),
                        offset,
                        length: chunk,
                    })
            })?;

            match chunk_response.status {
                308 => {
                    offset = resume_offset(&chunk_response, offset, chunk, length);
                }
                200 | 201 => {
                    let file: DriveFile = serde_json::from_slice(&chunk_response.body)?;
                    validate_file_id(&file.id)?;
                    return Ok(file);
                }
                status => {
                    return Err(provider_error(HttpResponse {
                        status,
                        headers: chunk_response.headers,
                        body: chunk_response.body,
                    }));
                }
            }
        }

        Err(Error::Provider {
            status: 0,
            message: "resumable upload ended without a final response".into(),
        })
    }

    fn ensure_blob_metadata(&self, manifest_hash: &str) -> Result<Value> {
        Ok(json!({
            "name": format!("blob-{manifest_hash}.tar.zst"),
            "parents": ["appDataFolder"],
            "mimeType": BLOB_MIME,
            "appProperties": {
                KIND: KIND_BLOB,
                VALUE: manifest_hash,
            },
        }))
    }
}

impl SyncProvider for GoogleDriveProvider {
    fn list_revisions(&self, game_id: &str) -> Result<Vec<Revision>> {
        let query = format!(
            "appProperties has {{ key='{KIND}' and value='{KIND_REVISION}' }} \
             and appProperties has {{ key='{GAME_ID}' and value='{}' }} and trashed=false",
            escape_query_value(game_id)?
        );
        let files = self.list_files(&query)?;
        let mut revisions: HashMap<String, Revision> = HashMap::new();

        for file in files {
            if let Some(properties) = &file.app_properties
                && properties
                    .get(GAME_ID)
                    .is_some_and(|value| value != game_id)
            {
                return Err(Error::InvalidState(format!(
                    "Drive file {} belongs to a different game",
                    file.id
                )));
            }

            let bytes = self.download(&file.id)?;
            let revision: Revision = serde_json::from_slice(&bytes)?;
            if revision.game_id != game_id {
                return Err(Error::InvalidState(format!(
                    "revision {} claims game {}, expected {game_id}",
                    revision.id, revision.game_id
                )));
            }
            if revision.id.is_empty() {
                return Err(Error::InvalidState(format!(
                    "Drive file {} contains a revision without an id",
                    file.id
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
                self.cache.lock().expect("cache mutex").insert(
                    (KIND_REVISION.to_string(), revision.id.clone()),
                    file.id.clone(),
                );
                revisions.insert(revision.id.clone(), revision);
            }
        }

        Ok(revisions.into_values().collect())
    }

    fn get_manifest(&self, manifest_hash: &str) -> Result<Manifest> {
        validate_hash(manifest_hash)?;
        let file_id = self
            .find_file(KIND_MANIFEST, manifest_hash)?
            .ok_or_else(|| Error::NotFound(format!("remote manifest {manifest_hash}")))?;
        let bytes = self.download(&file_id)?;
        let manifest = Manifest::from_json(
            std::str::from_utf8(&bytes)
                .map_err(|_| Error::InvalidState("remote manifest is not UTF-8".into()))?,
        )?;
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
        if revision.manifest_hash != manifest.hash() {
            return Err(Error::InvalidState(format!(
                "revision {} does not match manifest {}",
                revision.id, revision.manifest_hash
            )));
        }

        let manifest_value = manifest.hash();
        let manifest_bytes = manifest.to_json()?.into_bytes();
        self.put_immutable_json(
            KIND_MANIFEST,
            &manifest_value,
            format!("manifest-{manifest_value}.json"),
            manifest_bytes,
            |existing| {
                let remote =
                    Manifest::from_json(std::str::from_utf8(existing).map_err(|_| {
                        Error::InvalidState("remote manifest is not UTF-8".into())
                    })?)?;
                if remote.hash() != manifest_value {
                    return Err(Error::IntegrityMismatch {
                        path: format!("remote manifest {manifest_value}"),
                        expected: manifest_value.clone(),
                        actual: remote.hash(),
                    });
                }
                Ok(())
            },
        )?;

        let revision_bytes = serde_json::to_vec(revision)?;
        let mut app_properties = serde_json::Map::new();
        app_properties.insert(KIND.into(), Value::String(KIND_REVISION.into()));
        app_properties.insert(VALUE.into(), Value::String(revision.id.clone()));
        app_properties.insert(GAME_ID.into(), Value::String(revision.game_id.clone()));
        app_properties.insert(REVISION_ID.into(), Value::String(revision.id.clone()));
        app_properties.insert(
            MANIFEST_HASH.into(),
            Value::String(revision.manifest_hash.clone()),
        );
        let metadata = json!({
            "name": format!("revision-{}.json", revision.id),
            "parents": ["appDataFolder"],
            "mimeType": JSON_MIME,
            "appProperties": app_properties,
        });

        self.put_immutable_json_with_metadata(
            KIND_REVISION,
            &revision.id,
            metadata,
            revision_bytes,
            |existing| {
                let remote: Revision = serde_json::from_slice(existing)?;
                if &remote != revision {
                    return Err(Error::IntegrityMismatch {
                        path: format!("remote revision {}", revision.id),
                        expected: format!("{revision:?}"),
                        actual: format!("{remote:?}"),
                    });
                }
                Ok(())
            },
        )?;

        Ok(())
    }

    fn has_blob(&self, manifest_hash: &str) -> Result<bool> {
        validate_hash(manifest_hash)?;
        Ok(self.find_file(KIND_BLOB, manifest_hash)?.is_some())
    }

    fn put_blob(&self, manifest_hash: &str, source: &Path) -> Result<()> {
        validate_hash(manifest_hash)?;
        let _guard = self.upload_lock.lock().expect("upload mutex");
        if self.find_file(KIND_BLOB, manifest_hash)?.is_some() {
            return Ok(());
        }

        let metadata = self.ensure_blob_metadata(manifest_hash)?;
        let length = fs::metadata(source)
            .map_err(|e| Error::io(source, e))?
            .len();

        let file = if length == 0 || length <= self.multipart_limit {
            let bytes = fs::read(source).map_err(|e| Error::io(source, e))?;
            self.upload_multipart(&metadata, BLOB_MIME, &bytes)?
        } else {
            self.upload_resumable(&metadata, BLOB_MIME, source, length)?
        };

        self.cache
            .lock()
            .expect("cache mutex")
            .insert((KIND_BLOB.to_string(), manifest_hash.to_string()), file.id);
        Ok(())
    }

    fn get_blob(&self, manifest_hash: &str, destination: &Path) -> Result<()> {
        validate_hash(manifest_hash)?;
        let file_id = self
            .find_file(KIND_BLOB, manifest_hash)?
            .ok_or_else(|| Error::NotFound(format!("remote blob {manifest_hash}")))?;
        self.download_to(&file_id, destination)
    }
}

#[derive(Debug, Deserialize)]
struct DriveFile {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default, rename = "appProperties")]
    app_properties: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

fn authorized(request: HttpRequest, token: &str) -> HttpRequest {
    request.header("Authorization", format!("Bearer {token}"))
}

fn ensure_success(response: HttpResponse) -> Result<HttpResponse> {
    if response.is_success() {
        Ok(response)
    } else {
        Err(provider_error(response))
    }
}

fn provider_error(response: HttpResponse) -> Error {
    #[derive(Deserialize)]
    struct ErrorEnvelope {
        error: Option<ErrorBody>,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        message: Option<String>,
        #[serde(default)]
        errors: Vec<ErrorDetail>,
    }
    #[derive(Deserialize)]
    struct ErrorDetail {
        reason: Option<String>,
        message: Option<String>,
    }

    let message = serde_json::from_slice::<ErrorEnvelope>(&response.body)
        .ok()
        .and_then(|envelope| envelope.error)
        .map(|body| {
            let detail = body
                .errors
                .first()
                .and_then(|detail| detail.message.clone().or_else(|| detail.reason.clone()));
            match (body.message, detail) {
                (Some(message), Some(detail)) => format!("{message} ({detail})"),
                (Some(message), None) => message,
                (None, Some(detail)) => detail,
                (None, None) => "request failed".into(),
            }
        })
        .unwrap_or_else(|| {
            format!(
                "request failed: {}",
                String::from_utf8_lossy(&response.body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        });

    if response.status == 401 || response.status == 403 {
        Error::Auth(format!("HTTP {}: {message}", response.status))
    } else {
        Error::Provider {
            status: response.status,
            message,
        }
    }
}

fn build_url(base: &str, path: &str, params: &[(&str, String)]) -> Result<String> {
    let mut url = Url::parse(&format!("{}{}", base.trim_end_matches('/'), path))
        .map_err(|error| Error::InvalidState(format!("invalid Drive URL: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Ok(url.into())
}

fn escape_query_value(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(Error::InvalidState(
            "Drive query value contains control characters".into(),
        ));
    }
    Ok(value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn validate_file_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(Error::InvalidState(format!(
            "invalid Drive file id: {value:?}"
        )));
    }
    Ok(())
}

fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidState(format!("invalid digest: {value:?}")));
    }
    Ok(())
}

fn multipart_body(boundary: &str, metadata: &Value, mime: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(data.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(&serde_json::to_vec(metadata)?);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(body)
}

/// Continue an interrupted resumable upload, honoring Drive's `Range` header.
fn resume_offset(response: &HttpResponse, offset: u64, chunk: u64, total: u64) -> u64 {
    if let Some(range) = response.header("Range")
        && let Some(last) = range
            .rsplit('-')
            .next()
            .and_then(|value| value.parse::<u64>().ok())
    {
        return (last + 1).min(total);
    }
    (offset + chunk).min(total)
}

fn jitter() -> f64 {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    u16::from_be_bytes([bytes[0], bytes[1]]) as f64 / u16::MAX as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    use crate::http::HttpTransport;
    use crate::manifest::{FileEntry, Manifest};
    use crate::oauth::OAuthTokenManager;
    use crate::oauth::StaticToken;

    struct ScriptedTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        calls: Mutex<Vec<HttpRequest>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<HttpRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
            self.calls.lock().unwrap().push(request.clone());
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Network("no scripted response left".into()))?;
            if let Some(path) = &request.download_to
                && response.is_success()
            {
                fs::write(path, &response.body).map_err(|e| Error::io(path, e))?;
            }
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

    fn provider(transport: Arc<ScriptedTransport>, chunk_size: usize) -> GoogleDriveProvider {
        GoogleDriveProvider::new(transport, Arc::new(StaticToken::new("test-token")))
            .with_endpoints(
                "https://drive.test/drive/v3",
                "https://drive.test/upload/drive/v3",
            )
            .with_chunk_size(chunk_size)
            .with_retry_policy(RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(2),
            })
    }

    fn revision(id: &str, game_id: &str) -> Revision {
        Revision {
            id: id.into(),
            game_id: game_id.into(),
            parent_ids: vec![],
            device_id: "device-1".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            manifest_hash: Manifest::empty().hash(),
            blob_hash: Some(Manifest::empty().hash()),
            message: None,
        }
    }

    fn file_list(files: &[(&str, &str, &str)]) -> HttpResponse {
        let files: Vec<Value> = files
            .iter()
            .map(|(id, kind, value)| {
                json!({
                    "id": id,
                    "name": format!("{kind}-{value}"),
                    "appProperties": {
                        "savesyncKind": kind,
                        "savesyncValue": value,
                    },
                })
            })
            .collect();
        response(200, serde_json::to_vec(&json!({ "files": files })).unwrap())
    }

    fn empty_list() -> HttpResponse {
        response(200, br#"{"files":[]}"#.to_vec())
    }

    #[test]
    fn lists_and_downloads_revisions_with_pagination() {
        let first = revision("rev-1", "game-1");
        let second = revision("rev-2", "game-1");
        let transport = ScriptedTransport::new(vec![
            response(
                200,
                serde_json::to_vec(&json!({
                    "files": [{
                        "id": "drive-rev-1",
                        "name": "revision-rev-1.json",
                        "appProperties": {
                            "savesyncKind": "revision",
                            "savesyncGameId": "game-1"
                        }
                    }],
                    "nextPageToken": "page-2"
                }))
                .unwrap(),
            ),
            response(
                200,
                serde_json::to_vec(&json!({
                    "files": [{
                        "id": "drive-rev-2",
                        "name": "revision-rev-2.json",
                        "appProperties": {
                            "savesyncKind": "revision",
                            "savesyncGameId": "game-1"
                        }
                    }]
                }))
                .unwrap(),
            ),
            response(200, serde_json::to_vec(&first).unwrap()),
            response(200, serde_json::to_vec(&second).unwrap()),
        ]);
        let provider = provider(transport.clone(), 8 * 1024 * 1024);

        let revisions = provider.list_revisions("game-1").unwrap();
        assert_eq!(revisions.len(), 2);

        let calls = transport.calls();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].url.contains("spaces=appDataFolder"));
        assert!(calls[0].url.contains("pageSize=1000"));
        assert!(calls[1].url.contains("pageToken=page-2"));
        assert!(calls[2].url.contains("alt=media"));
        assert_eq!(
            calls[0].header_value("Authorization"),
            Some("Bearer test-token")
        );
    }

    #[test]
    fn escapes_game_ids_in_drive_queries() {
        let transport = ScriptedTransport::new(vec![empty_list()]);
        let provider = provider(transport.clone(), 8);
        provider.list_revisions("game'with\\oddities").unwrap();
        let url = &transport.calls()[0].url;
        assert!(url.contains("game%5C%27with%5C%5Coddities"));
        // Reject control characters instead of building an unsafe query.
        assert!(provider.list_revisions("bad\nid").is_err());
    }

    #[test]
    fn downloads_blob_to_destination() {
        let hash = Manifest::empty().hash();
        let transport = ScriptedTransport::new(vec![
            file_list(&[("drive-blob-1", KIND_BLOB, &hash)]),
            response(200, b"archive-bytes".to_vec()),
        ]);
        let provider = provider(transport.clone(), 8);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("blob.tar.zst");

        provider.get_blob(&hash, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"archive-bytes");
        assert!(transport.calls()[1].url.contains("alt=media"));
        assert_eq!(
            transport.calls()[1].download_to.as_deref(),
            Some(destination.as_path())
        );
    }

    #[test]
    fn small_blobs_use_multipart_upload() {
        let hash = Manifest::empty().hash();
        let transport = ScriptedTransport::new(vec![
            empty_list(),
            response(200, br#"{"id":"drive-blob-2"}"#.to_vec()),
        ]);
        let provider = provider(transport.clone(), 8);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("blob.tar.zst");
        fs::write(&source, b"small-archive").unwrap();

        provider.put_blob(&hash, &source).unwrap();
        let calls = transport.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].method, HttpMethod::Post);
        assert!(calls[1].url.contains("uploadType=multipart"));
        let body = calls[1].body.as_ref().unwrap().read_all().unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(KIND_BLOB));
        assert!(text.contains("small-archive"));
        assert!(text.contains(&hash));

        // Cached after upload: no further list call.
        assert!(provider.has_blob(&hash).unwrap());
        assert_eq!(transport.calls().len(), 2);
    }

    #[test]
    fn large_blobs_use_resumable_upload_and_resume_from_range() {
        let hash = Manifest::empty().hash();
        let transport = ScriptedTransport::new(vec![
            empty_list(),
            HttpResponse {
                status: 200,
                headers: vec![("Location".into(), "https://upload.test/session/abc".into())],
                body: b"{}".to_vec(),
            },
            HttpResponse {
                status: 308,
                headers: vec![("Range".into(), "bytes=0-3".into())],
                body: vec![],
            },
            HttpResponse {
                status: 308,
                headers: vec![],
                body: vec![],
            },
            response(200, br#"{"id":"drive-blob-3"}"#.to_vec()),
        ]);
        let provider = provider(transport.clone(), 4).with_multipart_limit(0);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("blob.tar.zst");
        fs::write(&source, b"0123456789").unwrap();

        provider.put_blob(&hash, &source).unwrap();
        let calls = transport.calls();
        assert_eq!(calls.len(), 5);
        assert!(calls[1].url.contains("uploadType=resumable"));
        assert_eq!(calls[1].header_value("X-Upload-Content-Length"), Some("10"));
        assert_eq!(calls[2].header_value("Content-Range"), Some("bytes 0-3/10"));
        assert_eq!(calls[3].header_value("Content-Range"), Some("bytes 4-7/10"));
        assert_eq!(calls[4].header_value("Content-Range"), Some("bytes 8-9/10"));
        assert_eq!(calls[2].body.as_ref().unwrap().len(), 4);
        assert_eq!(calls[4].body.as_ref().unwrap().len(), 2);
        assert_eq!(calls[2].url, "https://upload.test/session/abc");
    }

    #[test]
    fn put_revision_uploads_manifest_and_revision_metadata() {
        let manifest = Manifest::empty();
        let hash = manifest.hash();
        let rev = revision("rev-9", "game-9");
        let transport = ScriptedTransport::new(vec![
            empty_list(),
            response(200, br#"{"id":"drive-manifest-1"}"#.to_vec()),
            empty_list(),
            response(200, br#"{"id":"drive-revision-1"}"#.to_vec()),
        ]);
        let provider = provider(transport.clone(), 8);

        provider.put_revision(&rev, &manifest).unwrap();
        let calls = transport.calls();
        assert_eq!(calls.len(), 4);
        assert!(calls[1].url.contains("uploadType=multipart"));
        assert!(calls[3].url.contains("uploadType=multipart"));

        let revision_body =
            String::from_utf8_lossy(&calls[3].body.as_ref().unwrap().read_all().unwrap())
                .to_string();
        assert!(revision_body.contains(GAME_ID));
        assert!(revision_body.contains("game-9"));
        assert!(revision_body.contains(KIND_REVISION));
        assert!(revision_body.contains("rev-9"));
        assert!(revision_body.contains(&hash));
    }

    #[test]
    fn manifest_download_detects_hash_mismatch() {
        let requested = Manifest::empty().hash();
        let other = Manifest::new(vec![FileEntry {
            path: "a.bin".into(),
            size: 1,
            sha256: crate::hash::hash_bytes(b"a"),
            mtime_ms: None,
        }])
        .unwrap();
        let transport = ScriptedTransport::new(vec![
            file_list(&[("drive-manifest-1", KIND_MANIFEST, &requested)]),
            response(200, other.to_json().unwrap().into_bytes()),
        ]);
        let provider = provider(transport, 8);
        assert!(matches!(
            provider.get_manifest(&requested),
            Err(Error::IntegrityMismatch { .. })
        ));
    }

    #[test]
    fn retries_transient_list_failures() {
        let transport =
            ScriptedTransport::new(vec![response(503, b"unavailable".to_vec()), empty_list()]);
        let provider = provider(transport.clone(), 8);
        assert!(!provider.has_blob(&Manifest::empty().hash()).unwrap());
        assert_eq!(transport.calls().len(), 2);
    }

    #[test]
    fn refreshes_token_once_after_401() {
        let transport = ScriptedTransport::new(vec![
            response(
                200,
                br#"{"access_token":"initial-token","expires_in":3600}"#.to_vec(),
            ),
            response(401, br#"{"error":{"message":"expired"}}"#.to_vec()),
            response(
                200,
                br#"{"access_token":"new-token","expires_in":3600}"#.to_vec(),
            ),
            empty_list(),
        ]);
        let tokens = Arc::new(OAuthTokenManager::new(
            OAuthConfig::google("client-id"),
            transport.clone(),
            "refresh-token",
        ));
        let provider = GoogleDriveProvider::new(transport.clone(), tokens)
            .with_endpoints(
                "https://drive.test/drive/v3",
                "https://drive.test/upload/drive/v3",
            )
            .with_chunk_size(8)
            .with_retry_policy(RetryPolicy {
                max_attempts: 2,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(2),
            });

        assert!(!provider.has_blob(&Manifest::empty().hash()).unwrap());
        let calls = transport.calls();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].url.contains("token"));
        assert_eq!(
            calls[1].header_value("Authorization"),
            Some("Bearer initial-token")
        );
        assert!(calls[1].url.contains("drive.test/drive/v3/files"));
        assert!(calls[2].url.contains("oauth2") || calls[2].url.contains("token"));
        let form = String::from_utf8_lossy(&calls[2].body.as_ref().unwrap().read_all().unwrap())
            .to_string();
        assert!(form.contains("grant_type=refresh_token"));
        assert_eq!(
            calls[3].header_value("Authorization"),
            Some("Bearer new-token")
        );
    }

    #[test]
    fn rejects_invalid_digests_and_file_ids() {
        let transport = ScriptedTransport::new(vec![response(
            200,
            br#"{"files":[{"id":"../escape","name":"x"}]}"#.to_vec(),
        )]);
        let provider = provider(transport, 8);
        assert!(provider.get_manifest("not-a-hash").is_err());
        let error = provider.has_blob(&Manifest::empty().hash()).unwrap_err();
        assert!(matches!(error, Error::InvalidState(_)));
    }

    #[test]
    fn oauth_pkce_and_authorization_url_remain_wired() {
        let config = OAuthConfig::google("id");
        let pair = crate::oauth::PkcePair::generate();
        let url =
            crate::oauth::authorization_url(&config, "http://127.0.0.1:0/callback", &pair, "state")
                .unwrap();
        assert!(url.contains("drive.file"));
    }
}
