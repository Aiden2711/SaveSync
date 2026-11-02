//! End-to-end sync through the Dropbox provider against an in-memory Dropbox
//! server.
//!
//! Mirrors `tests/webdav_sync.rs`: the fake implements [`HttpTransport`] and
//! answers the JSON RPC endpoints and the content endpoints over a map. It
//! decodes `Dropbox-API-Arg` the way the real API does (JSON, including
//! `\uXXXX` escapes) and asserts that every header value is pure ASCII, so the
//! escaping in `src/dropbox.rs` is exercised rather than bypassed.
//!
//! The fake also enforces the two server-side rules the provider depends on:
//! `mode: "add"` writes fail with `409 path/conflict` when an object exists, and
//! a non-final `append_v2` payload must be a multiple of
//! [`UPLOAD_SESSION_ALIGNMENT`].

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use savesync_core::dropbox::UPLOAD_SESSION_ALIGNMENT;
use savesync_core::hash::hash_bytes;
use savesync_core::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, RetryPolicy};
use savesync_core::oauth::StaticToken;
use savesync_core::provider_runtime::MAX_LIST_PAGES;
use savesync_core::{
    AdapterType, DropboxProvider, Error, Manifest, RestoreMode, Revision, SaveSyncEngine,
    SyncProvider, TokenProvider,
};

const API: &str = "https://api.dropboxapi.com/2";
const CONTENT: &str = "https://content.dropboxapi.com/2";

/// Distinctive bearer value so a leak into an error message is unambiguous.
const BEARER: &str = "sl.super-secret-bearer-token-value";

// ---------------------------------------------------------------------------
// In-memory Dropbox
// ---------------------------------------------------------------------------

/// One injected failure, consumed when a request to `endpoint` arrives.
struct Failure {
    endpoint: &'static str,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct FakeDropbox {
    /// Decoded app-folder-relative path -> object bytes.
    objects: Mutex<HashMap<String, Vec<u8>>>,
    /// Upload session id -> bytes received so far.
    sessions: Mutex<HashMap<String, Vec<u8>>>,
    /// Listing cursor -> (directory, next index).
    cursors: Mutex<HashMap<String, (String, usize)>>,
    calls: Mutex<Vec<HttpRequest>>,
    failures: Mutex<VecDeque<Failure>>,
    page_limit: Mutex<Option<usize>>,
    repeat_cursor: AtomicBool,
    lose_first_append: AtomicBool,
    always_incorrect_offset: AtomicBool,
    empty_download: AtomicBool,
    next_id: AtomicU64,
}

impl FakeDropbox {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            objects: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            failures: Mutex::new(VecDeque::new()),
            page_limit: Mutex::new(None),
            repeat_cursor: AtomicBool::new(false),
            lose_first_append: AtomicBool::new(false),
            always_incorrect_offset: AtomicBool::new(false),
            empty_download: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        })
    }

    fn fail_next(&self, endpoint: &'static str, status: u16) {
        self.fail_next_with(endpoint, status, Vec::new(), Vec::new());
    }

    fn fail_next_with(
        &self,
        endpoint: &'static str,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) {
        self.failures.lock().unwrap().push_back(Failure {
            endpoint,
            status,
            headers,
            body,
        });
    }

    fn set_page_limit(&self, limit: usize) {
        *self.page_limit.lock().unwrap() = Some(limit);
    }

    fn repeat_cursor(&self) {
        self.repeat_cursor.store(true, Ordering::SeqCst);
    }

    fn lose_first_append(&self) {
        self.lose_first_append.store(true, Ordering::SeqCst);
    }

    fn always_incorrect_offset(&self) {
        self.always_incorrect_offset.store(true, Ordering::SeqCst);
    }

    fn empty_download(&self) {
        self.empty_download.store(true, Ordering::SeqCst);
    }

    /// Seed an object behind the provider's back (another writer or corruption).
    fn seed(&self, path: &str, bytes: &[u8]) {
        self.objects
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes.to_vec());
    }

    fn object(&self, path: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(path).cloned()
    }

    fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap().clone()
    }

    /// Number of requests whose URL ends with `endpoint`.
    fn count(&self, endpoint: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.url.ends_with(endpoint))
            .count()
    }

    /// Parsed `Dropbox-API-Arg` of every `append_v2` request.
    fn append_args(&self) -> Vec<Value> {
        self.calls()
            .iter()
            .filter(|call| call.url.ends_with("/files/upload_session/append_v2"))
            .map(api_arg)
            .collect()
    }

    /// Body length of every `append_v2` request.
    fn append_lengths(&self) -> Vec<u64> {
        self.calls()
            .iter()
            .filter(|call| call.url.ends_with("/files/upload_session/append_v2"))
            .map(|call| call.body.as_ref().map_or(0, |body| body.len()))
            .collect()
    }

    fn next_failure(&self, endpoint: &str) -> Option<Failure> {
        let mut failures = self.failures.lock().unwrap();
        match failures.front() {
            Some(failure) if failure.endpoint == endpoint => failures.pop_front(),
            _ => None,
        }
    }

    fn list_folder(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = rpc_arg(request);
        let directory = arg["path"].as_str().expect("list_folder path").to_string();
        assert_eq!(arg["recursive"], json!(false), "listing must not recurse");
        let limit = self.page_limit.lock().unwrap().unwrap_or(usize::MAX);
        self.page(&directory, 0, limit)
    }

    fn list_folder_continue(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = rpc_arg(request);
        let cursor = arg["cursor"].as_str().expect("continue cursor").to_string();
        let limit = self.page_limit.lock().unwrap().unwrap_or(usize::MAX);
        let Some((directory, start)) = self.cursors.lock().unwrap().get(&cursor).cloned() else {
            return Ok(envelope(400, "unknown cursor".into()));
        };
        self.page(&directory, start, limit)
    }

    fn page(&self, directory: &str, start: usize, limit: usize) -> Result<HttpResponse, Error> {
        let prefix = format!("{}/", directory.trim_end_matches('/'));
        let mut names: Vec<String> = self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|key| {
                key.strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .cloned()
            .collect();
        names.sort();
        if names.is_empty() {
            // Real Dropbox answers a missing folder with `path/not_found`.
            return Ok(not_found());
        }

        let end = names.len().min(start.saturating_add(limit));
        let has_more = end < names.len();
        let entries: Vec<Value> = names[start..end]
            .iter()
            .map(|path| {
                json!({
                    ".tag": "file",
                    "name": path.rsplit('/').next().unwrap_or_default(),
                    "path_display": path,
                    "size": 1,
                })
            })
            .collect();

        let mut response = json!({ "entries": entries, "has_more": has_more, "cursor": "" });
        if has_more {
            let cursor = if self.repeat_cursor.load(Ordering::SeqCst) {
                "cursor-repeat".to_string()
            } else {
                format!("cursor-{}", self.next_id.fetch_add(1, Ordering::SeqCst))
            };
            self.cursors
                .lock()
                .unwrap()
                .entry(cursor.clone())
                .or_insert((directory.to_string(), end));
            response["cursor"] = json!(cursor);
        }
        Ok(json_response(200, response))
    }

    fn get_metadata(&self, request: &HttpRequest) -> HttpResponse {
        let arg = rpc_arg(request);
        let path = arg["path"].as_str().expect("get_metadata path");
        if self.objects.lock().unwrap().contains_key(path) {
            metadata(path)
        } else {
            not_found()
        }
    }

    fn upload(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = api_arg(request);
        let path = arg["path"].as_str().expect("upload path").to_string();
        assert_create(&arg);
        let bytes = body_bytes(request)?;
        let mut objects = self.objects.lock().unwrap();
        if objects.contains_key(&path) {
            return Ok(conflict(&path));
        }
        objects.insert(path.clone(), bytes);
        Ok(metadata(&path))
    }

    fn session_start(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = api_arg(request);
        assert_eq!(arg["close"], json!(false));
        assert_eq!(arg["session_type"], json!("concurrent"));
        let id = format!("session-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.sessions.lock().unwrap().insert(id.clone(), Vec::new());
        Ok(json_response(200, json!({ "session_id": id })))
    }

    fn session_append(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = api_arg(request);
        let session_id = arg["cursor"]["session_id"]
            .as_str()
            .expect("cursor.session_id")
            .to_string();
        let offset = arg["cursor"]["offset"].as_u64().expect("cursor.offset") as usize;
        let close = arg["close"].as_bool().expect("close");
        let bytes = body_bytes(request)?;

        let mut sessions = self.sessions.lock().unwrap();
        let Some(buffer) = sessions.get_mut(&session_id) else {
            return Ok(envelope(409, "unknown upload session".into()));
        };
        if self.always_incorrect_offset.load(Ordering::SeqCst) {
            return Ok(incorrect_offset(buffer.len()));
        }
        if !close && !bytes.len().is_multiple_of(UPLOAD_SESSION_ALIGNMENT) {
            return Ok(envelope(
                400,
                format!("misaligned append of {} bytes", bytes.len()),
            ));
        }
        if offset != buffer.len() {
            return Ok(incorrect_offset(buffer.len()));
        }
        buffer.extend_from_slice(&bytes);

        if self.lose_first_append.swap(false, Ordering::SeqCst) {
            // The bytes arrived but the response was lost: the client must
            // adopt `correct_offset` instead of resending from zero.
            return Ok(incorrect_offset(buffer.len()));
        }
        Ok(empty(200))
    }

    fn session_finish(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = api_arg(request);
        let session_id = arg["cursor"]["session_id"]
            .as_str()
            .expect("cursor.session_id")
            .to_string();
        let offset = arg["cursor"]["offset"].as_u64().expect("cursor.offset") as usize;
        let commit = &arg["commit"];
        assert_create(commit);
        let path = commit["path"].as_str().expect("commit path").to_string();

        let mut sessions = self.sessions.lock().unwrap();
        let Some(buffer) = sessions.remove(&session_id) else {
            return Ok(envelope(409, "unknown upload session".into()));
        };
        let received = buffer.len();
        if offset != received {
            sessions.insert(session_id, buffer);
            return Ok(incorrect_offset(received));
        }
        drop(sessions);

        let mut objects = self.objects.lock().unwrap();
        if objects.contains_key(&path) {
            return Ok(conflict(&path));
        }
        objects.insert(path.clone(), buffer);
        Ok(metadata(&path))
    }

    fn download(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let arg = api_arg(request);
        let path = arg["path"].as_str().expect("download path").to_string();

        let payload = if self.empty_download.load(Ordering::SeqCst) {
            Some(Vec::new())
        } else {
            self.objects.lock().unwrap().get(&path).cloned()
        };

        let Some(payload) = payload else {
            // A streamed request receives the error envelope as its body.
            let body = not_found_body();
            if let Some(destination) = &request.download_to {
                fs::write(destination, &body)
                    .map_err(|error| Error::io(destination.clone(), error))?;
                return Ok(empty(409));
            }
            return Ok(HttpResponse {
                status: 409,
                headers: Vec::new(),
                body,
            });
        };

        if let Some(destination) = &request.download_to {
            fs::write(destination, &payload)
                .map_err(|error| Error::io(destination.clone(), error))?;
            Ok(empty(200))
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: payload,
            })
        }
    }
}

impl HttpTransport for FakeDropbox {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        self.calls.lock().unwrap().push(request.clone());

        let (kind, endpoint) = route(&request.url)?;
        let bearer = format!("Bearer {BEARER}");
        assert_eq!(request.method, HttpMethod::Post, "Dropbox calls are POST");
        assert_eq!(
            request.header_value("Authorization"),
            Some(bearer.as_str()),
            "every Dropbox request must carry the bearer token"
        );

        if let Some(failure) = self.next_failure(&endpoint) {
            return Ok(HttpResponse {
                status: failure.status,
                headers: failure.headers,
                body: failure.body,
            });
        }

        match (kind, endpoint.as_str()) {
            ("api", "/files/list_folder") => self.list_folder(request),
            ("api", "/files/list_folder/continue") => self.list_folder_continue(request),
            ("api", "/files/get_metadata") => Ok(self.get_metadata(request)),
            ("content", "/files/upload") => self.upload(request),
            ("content", "/files/upload_session/start") => self.session_start(request),
            ("content", "/files/upload_session/append_v2") => self.session_append(request),
            ("content", "/files/upload_session/finish") => self.session_finish(request),
            ("content", "/files/download") => self.download(request),
            _ => Ok(envelope(400, format!("unsupported endpoint {endpoint}"))),
        }
    }
}

/// Split a request URL into its host kind and Dropbox endpoint path.
fn route(url: &str) -> Result<(&'static str, String), Error> {
    if let Some(rest) = url.strip_prefix(API) {
        return Ok(("api", rest.to_string()));
    }
    if let Some(rest) = url.strip_prefix(CONTENT) {
        return Ok(("content", rest.to_string()));
    }
    Err(Error::Network(format!("unexpected Dropbox URL {url}")))
}

/// Decode `Dropbox-API-Arg` the way Dropbox does: an ASCII JSON header.
fn api_arg(request: &HttpRequest) -> Value {
    let header = request
        .header_value("Dropbox-API-Arg")
        .expect("missing Dropbox-API-Arg header");
    assert!(
        header.is_ascii(),
        "Dropbox-API-Arg must be ASCII-safe, got {header:?}"
    );
    serde_json::from_str(header).expect("Dropbox-API-Arg must be JSON")
}

/// Decode a JSON RPC request body (`list_folder`, `get_metadata`, ...).
fn rpc_arg(request: &HttpRequest) -> Value {
    let body = body_bytes(request).expect("JSON RPC body must be readable");
    assert!(!body.is_empty(), "JSON RPC endpoints require a body");
    serde_json::from_slice(&body).expect("JSON RPC body must be JSON")
}

fn body_bytes(request: &HttpRequest) -> Result<Vec<u8>, Error> {
    match &request.body {
        Some(body) => body.read_all(),
        None => Ok(Vec::new()),
    }
}

/// Every create must ask for a new object and refuse to replace one.
fn assert_create(arg: &Value) {
    assert_eq!(
        arg["mode"],
        json!("add"),
        "objects are created, never replaced"
    );
    assert_eq!(arg["autorename"], json!(false));
    assert_eq!(arg["strict_conflict"], json!(true));
}

fn empty(status: u16) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(&value).unwrap(),
    }
}

fn envelope(status: u16, summary: String) -> HttpResponse {
    json_response(
        status,
        json!({ "error_summary": summary, "error": { ".tag": "other" } }),
    )
}

fn metadata(path: &str) -> HttpResponse {
    json_response(
        200,
        json!({
            ".tag": "file",
            "name": path.rsplit('/').next().unwrap_or_default(),
            "path_display": path,
        }),
    )
}

fn not_found_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "error_summary": "path/not_found/..",
        "error": { ".tag": "path", "path": { ".tag": "not_found" } },
    }))
    .unwrap()
}

fn not_found() -> HttpResponse {
    HttpResponse {
        status: 409,
        headers: Vec::new(),
        body: not_found_body(),
    }
}

fn conflict(path: &str) -> HttpResponse {
    json_response(
        409,
        json!({
            "error_summary": "path/conflict/file/..",
            "error": {
                ".tag": "path",
                "path": { ".tag": "conflict" },
                "path_display": path,
            },
        }),
    )
}

fn incorrect_offset(offset: usize) -> HttpResponse {
    json_response(
        409,
        json!({
            "error_summary": "incorrect_offset/..",
            "error": { ".tag": "incorrect_offset", "correct_offset": offset },
        }),
    )
}

// ---------------------------------------------------------------------------
// Provider helpers
// ---------------------------------------------------------------------------

fn tokens() -> Arc<dyn TokenProvider> {
    Arc::new(StaticToken::new(BEARER))
}

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    }
}

fn provider(server: Arc<FakeDropbox>) -> DropboxProvider {
    DropboxProvider::new(server, tokens()).with_retry_policy(fast_retry())
}

fn provider_with_chunk(server: Arc<FakeDropbox>, chunk: usize) -> DropboxProvider {
    provider(server).with_chunk_size(chunk)
}

fn blob_path(hash: &str) -> String {
    format!("/blobs/{}/{hash}.tar.zst", &hash[..2])
}

fn write_source(dir: &std::path::Path, contents: &[u8]) -> std::path::PathBuf {
    let path = dir.join("blob.tar.zst");
    fs::write(&path, contents).unwrap();
    path
}

fn revision(id: &str, game_id: &str, manifest_hash: &str) -> Revision {
    Revision {
        id: id.to_string(),
        game_id: game_id.to_string(),
        parent_ids: Vec::new(),
        device_id: "device-1".to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        manifest_hash: manifest_hash.to_string(),
        blob_hash: None,
        message: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn two_devices_sync_through_the_dropbox_provider() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let temp = tempfile::tempdir().unwrap();

    // Device A publishes a snapshot.
    let a_save = temp.path().join("a_save");
    fs::create_dir_all(&a_save).unwrap();
    fs::write(a_save.join("save.bin"), b"from-device-a").unwrap();
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game = a
        .enroll_game("Dropbox Game", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game.id, &a_save).unwrap();
    let first = a.snapshot_game(&game.id, Some("first")).unwrap();

    let report = a.sync(&provider, &game.id).unwrap();
    assert_eq!(report.pushed_revisions, 1);
    assert_eq!(report.uploaded_blobs, 1);
    assert_eq!(
        server.keys().len(),
        3,
        "expected blob, manifest, and revision objects"
    );

    // Device B pulls and explicitly restores.
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    b.enroll_game_with_id(&game.id, "Dropbox Game", AdapterType::CustomDirectory)
        .unwrap();
    b.add_directory_location(&game.id, &b_save).unwrap();

    let report = b.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 1);
    assert_eq!(report.downloaded_blobs, 1);
    assert!(
        fs::read_dir(&b_save).unwrap().next().is_none(),
        "pulling must never touch live saves"
    );

    b.restore(&first.revision_id).unwrap();
    assert_eq!(fs::read(b_save.join("save.bin")).unwrap(), b"from-device-a");

    // B advances, publishes; A pulls and restores.
    fs::write(b_save.join("save.bin"), b"from-device-b").unwrap();
    let second = b.snapshot_game(&game.id, Some("second")).unwrap();
    let report = b.sync(&provider, &game.id).unwrap();
    assert_eq!(report.pushed_revisions, 1);

    let report = a.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 1);
    let heads = a.heads(&game.id).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, second.revision_id);

    a.restore_with_mode(&second.revision_id, RestoreMode::ExactMirror)
        .unwrap();
    assert_eq!(fs::read(a_save.join("save.bin")).unwrap(), b"from-device-b");
}

#[test]
fn conflicting_blob_content_is_rejected_and_left_untouched() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let source = write_source(dir.path(), b"expected-bytes");
    let hash = hash_bytes(b"expected-bytes");

    server.seed(&blob_path(&hash), b"a-different-object");

    let error = provider.put_blob(&hash, &source).unwrap_err();
    assert!(
        matches!(error, Error::Provider { status: 409, .. }),
        "expected an immutability conflict, got {error:?}"
    );
    assert_eq!(
        server.object(&blob_path(&hash)).unwrap(),
        b"a-different-object",
        "a conflicting object must never be overwritten"
    );
    assert_eq!(
        server.count("/files/upload"),
        0,
        "a conflict must never issue a write"
    );
}

#[test]
fn conflicting_revision_content_is_rejected_without_an_overwrite() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let manifest = Manifest::empty();
    let stored = revision("rev-1", "game-1", &manifest.hash());
    let path = "/games/game-1/revisions/rev-1.json";

    let mut other = stored.clone();
    other.message = Some("written by another device".into());
    server.seed(path, &serde_json::to_vec(&other).unwrap());

    let error = provider.put_revision(&stored, &manifest).unwrap_err();
    assert!(
        matches!(error, Error::Provider { status: 409, .. }),
        "expected an immutability conflict, got {error:?}"
    );
    assert_eq!(
        server.object(path).unwrap(),
        serde_json::to_vec(&other).unwrap()
    );
}

#[test]
fn repeated_blob_upload_is_idempotent_and_writes_nothing_again() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let contents = b"dropbox-payload";
    let source = write_source(dir.path(), contents);
    let hash = hash_bytes(contents);

    assert!(!provider.has_blob(&hash).unwrap());
    provider.put_blob(&hash, &source).unwrap();
    let uploads = server.count("/files/upload");
    assert_eq!(uploads, 1, "one small object, one request");

    provider.put_blob(&hash, &source).unwrap();
    assert_eq!(
        server.count("/files/upload"),
        uploads,
        "an already-present blob must not be re-uploaded"
    );
    assert_eq!(server.object(&blob_path(&hash)).unwrap(), contents);
    assert!(provider.has_blob(&hash).unwrap());
}

#[test]
fn chunked_upload_keeps_four_mib_alignment_and_finishes_once() {
    let server = FakeDropbox::new();
    let provider = provider_with_chunk(server.clone(), UPLOAD_SESSION_ALIGNMENT);
    let dir = tempfile::tempdir().unwrap();

    let mut payload = vec![7u8; UPLOAD_SESSION_ALIGNMENT];
    payload.extend(vec![9u8; 1024]);
    let source = write_source(dir.path(), &payload);
    let hash = hash_bytes(&payload);

    provider.put_blob(&hash, &source).unwrap();

    let appends = server.append_args();
    assert_eq!(appends.len(), 2, "one aligned chunk plus the final 1 KiB");
    assert_eq!(appends[0]["cursor"]["offset"], json!(0));
    assert_eq!(appends[0]["close"], json!(false));
    assert_eq!(
        appends[1]["cursor"]["offset"],
        json!(UPLOAD_SESSION_ALIGNMENT)
    );
    assert_eq!(appends[1]["close"], json!(true));

    let lengths = server.append_lengths();
    assert_eq!(lengths[0], UPLOAD_SESSION_ALIGNMENT as u64);
    assert!(
        lengths[0].is_multiple_of(UPLOAD_SESSION_ALIGNMENT as u64),
        "a non-final append must be aligned"
    );
    assert_eq!(lengths[1], 1024);

    assert_eq!(server.count("/files/upload_session/start"), 1);
    assert_eq!(server.count("/files/upload_session/finish"), 1);
    assert_eq!(
        server.object(&blob_path(&hash)).unwrap(),
        payload,
        "the finished object must be byte-identical to the source"
    );
}

#[test]
fn incorrect_offset_is_adopted_once_and_the_upload_resumes() {
    let server = FakeDropbox::new();
    let provider = provider_with_chunk(server.clone(), UPLOAD_SESSION_ALIGNMENT);
    let dir = tempfile::tempdir().unwrap();

    let mut payload = vec![1u8; UPLOAD_SESSION_ALIGNMENT];
    payload.extend(vec![2u8; 2048]);
    let source = write_source(dir.path(), &payload);
    let hash = hash_bytes(&payload);
    server.lose_first_append();

    provider.put_blob(&hash, &source).unwrap();

    let offsets: Vec<u64> = server
        .append_args()
        .iter()
        .map(|arg| arg["cursor"]["offset"].as_u64().unwrap())
        .collect();
    assert_eq!(
        offsets,
        vec![0, UPLOAD_SESSION_ALIGNMENT as u64],
        "the retry resumes at the server's offset instead of restarting"
    );
    assert_eq!(
        server.object(&blob_path(&hash)).unwrap(),
        payload,
        "resuming must not duplicate the already-received bytes"
    );
}

#[test]
fn repeated_incorrect_offset_fails_instead_of_looping() {
    let server = FakeDropbox::new();
    let provider = provider_with_chunk(server.clone(), UPLOAD_SESSION_ALIGNMENT);
    let dir = tempfile::tempdir().unwrap();

    let mut payload = vec![3u8; UPLOAD_SESSION_ALIGNMENT];
    payload.extend(vec![4u8; 512]);
    let source = write_source(dir.path(), &payload);
    let hash = hash_bytes(&payload);
    server.always_incorrect_offset();

    let error = provider.put_blob(&hash, &source).unwrap_err();
    assert!(
        matches!(error, Error::Provider { status: 409, .. }),
        "expected a provider error, got {error:?}"
    );
    assert_eq!(
        server.count("/files/upload_session/append_v2"),
        2,
        "one adopted offset, then a hard failure"
    );
    assert!(
        server.object(&blob_path(&hash)).is_none(),
        "a failed session must not commit an object"
    );
}

#[test]
fn throttling_honors_retry_after_and_stays_within_the_attempt_cap() {
    let server = FakeDropbox::new();
    for _ in 0..2 {
        server.fail_next_with(
            "/files/get_metadata",
            429,
            vec![("Retry-After".into(), "0".into())],
            br#"{"error_summary":"too_many_requests/.."}"#.to_vec(),
        );
    }
    let provider = DropboxProvider::new(server.clone(), tokens()).with_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
    });

    let started = Instant::now();
    assert!(!provider.has_blob(&"0".repeat(64)).unwrap());
    let elapsed = started.elapsed();

    assert_eq!(
        server.count("/files/get_metadata"),
        3,
        "two throttled attempts then the real answer"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "Retry-After must replace the 200ms base delay, took {elapsed:?}"
    );
}

#[test]
fn throttling_that_never_clears_returns_after_the_attempt_cap() {
    let server = FakeDropbox::new();
    for _ in 0..10 {
        server.fail_next_with(
            "/files/get_metadata",
            429,
            vec![("Retry-After".into(), "0".into())],
            br#"{"error_summary":"too_many_requests/.."}"#.to_vec(),
        );
    }
    let provider = DropboxProvider::new(server.clone(), tokens()).with_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    });

    let error = provider.has_blob(&"0".repeat(64)).unwrap_err();
    assert!(
        matches!(error, Error::Provider { status: 429, .. }),
        "expected the throttled response, got {error:?}"
    );
    assert_eq!(
        server.count("/files/get_metadata"),
        3,
        "retries are bounded by the policy"
    );
}

#[test]
fn unauthorized_surfaces_auth_and_never_leaks_the_bearer_token() {
    let server = FakeDropbox::new();
    server.fail_next("/files/get_metadata", 401);
    let provider = provider(server.clone());

    let error = provider.has_blob(&"0".repeat(64)).unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "got {error:?}");
    assert!(
        !format!("{error}").contains(BEARER),
        "the bearer token must never reach an error message: {error}"
    );
    assert!(
        !format!("{error:?}").contains(BEARER),
        "the bearer token must never reach a debug rendering"
    );
    assert_eq!(
        server.count("/files/get_metadata"),
        1,
        "a 401 is surfaced, never retried by the provider"
    );
}

#[test]
fn conflicting_paths_never_echo_the_bearer_token() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let source = write_source(dir.path(), b"conflict-payload");
    let hash = hash_bytes(b"conflict-payload");
    server.seed(&blob_path(&hash), b"someone-elses-bytes");

    let error = provider.put_blob(&hash, &source).unwrap_err();
    assert!(!format!("{error}").contains(BEARER));
    assert!(!format!("{error:?}").contains(BEARER));
}

#[test]
fn missing_paths_map_to_not_found_and_unknown_games_list_empty() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("download.tar.zst");
    let hash = hash_bytes(b"never-uploaded");

    assert!(matches!(
        provider.get_manifest(&hash).unwrap_err(),
        Error::NotFound(_)
    ));
    assert!(matches!(
        provider.get_blob(&hash, &destination).unwrap_err(),
        Error::NotFound(_)
    ));
    assert!(!provider.has_blob(&hash).unwrap());
    assert!(
        provider.list_revisions("no-such-game").unwrap().is_empty(),
        "an unknown game has no revisions, exactly like WebDAV"
    );
    assert!(
        !destination.exists(),
        "a failed download must not leave a partial staged blob"
    );
}

#[test]
fn an_empty_download_body_is_an_error_and_leaves_no_partial_file() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("download.tar.zst");
    let hash = hash_bytes(b"declared-but-empty");

    server.seed(&blob_path(&hash), b"");
    server.empty_download();

    let error = provider.get_blob(&hash, &destination).unwrap_err();
    assert!(
        matches!(error, Error::Provider { .. }),
        "an empty body is not a successful download, got {error:?}"
    );
    assert!(
        !destination.exists(),
        "an empty download must be removed before it looks staged"
    );
}

#[test]
fn a_corrupted_remote_blob_is_rejected_before_it_becomes_a_pulled_snapshot() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let temp = tempfile::tempdir().unwrap();

    // Device A publishes a valid snapshot.
    let a_save = temp.path().join("a_save");
    fs::create_dir_all(&a_save).unwrap();
    fs::write(a_save.join("save.bin"), b"expected-content").unwrap();
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game = a
        .enroll_game("Corrupt Game", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game.id, &a_save).unwrap();
    a.snapshot_game(&game.id, None).unwrap();
    a.sync(&provider, &game.id).unwrap();

    // Corrupt the archive behind the provider's back. The object key still
    // names the manifest, so only SaveSync's per-entry digests can reject it.
    let manifest_hash = a.heads(&game.id).unwrap()[0].manifest_hash.clone();
    server.seed(&blob_path(&manifest_hash), b"corrupted-content");

    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    b.enroll_game_with_id(&game.id, "Corrupt Game", AdapterType::CustomDirectory)
        .unwrap();
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    b.add_directory_location(&game.id, &b_save).unwrap();

    let error = b.pull(&provider, &game.id).unwrap_err();
    assert!(
        matches!(
            error,
            Error::IntegrityMismatch { .. } | Error::InvalidArchive(_)
        ),
        "a corrupted blob must never become a pulled snapshot, got {error:?}"
    );
    assert!(
        fs::read_dir(&b_save).unwrap().next().is_none(),
        "a rejected pull must never touch live saves"
    );
}

#[test]
fn revisions_are_listed_across_pages() {
    let server = FakeDropbox::new();
    server.set_page_limit(2);
    let provider = provider(server.clone());
    let manifest = Manifest::empty();

    let mut expected = Vec::new();
    for index in 0..5 {
        let revision = revision(&format!("rev-{index}"), "game-1", &manifest.hash());
        server.seed(
            &format!("/games/game-1/revisions/{}.json", revision.id),
            &serde_json::to_vec(&revision).unwrap(),
        );
        expected.push(revision);
    }

    let mut revisions = provider.list_revisions("game-1").unwrap();
    revisions.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(revisions, expected);
    assert_eq!(
        server.count("/files/list_folder/continue"),
        2,
        "five entries in pages of two need two continuations"
    );
}

#[test]
fn a_repeating_listing_cursor_is_rejected() {
    let server = FakeDropbox::new();
    server.set_page_limit(1);
    server.repeat_cursor();
    let provider = provider(server.clone());

    for index in 0..3 {
        server.seed(&format!("/games/game-1/revisions/stub-{index}.json"), b"{}");
    }

    let error = provider.list_revisions("game-1").unwrap_err();
    assert!(
        format!("{error}").contains("repeating cursor"),
        "a stalled cursor must fail loudly, got {error}"
    );
}

#[test]
fn listing_stops_at_the_page_cap_instead_of_looping_forever() {
    let server = FakeDropbox::new();
    server.set_page_limit(1);
    let provider = provider(server.clone());

    for index in 0..(MAX_LIST_PAGES + 50) {
        server.seed(&format!("/games/game-1/revisions/stub-{index}.json"), b"{}");
    }

    let error = provider.list_revisions("game-1").unwrap_err();
    assert!(
        format!("{error}").contains("exceeded"),
        "the page cap must terminate the listing, got {error}"
    );
    let total = server.count("/files/list_folder") + server.count("/files/list_folder/continue");
    assert_eq!(
        total, MAX_LIST_PAGES,
        "exactly the cap's worth of requests, never one more"
    );
}

#[test]
fn non_ascii_and_percent_paths_round_trip_through_the_api_arg_header() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let manifest = Manifest::empty();

    let plain = revision("rëv 1", "gäme 1", &manifest.hash());
    let percent = revision("50% done", "gäme 1", &manifest.hash());
    provider.put_revision(&plain, &manifest).unwrap();
    provider.put_revision(&percent, &manifest).unwrap();

    let headers: Vec<String> = server
        .calls()
        .iter()
        .filter_map(|call| call.header_value("Dropbox-API-Arg").map(str::to_string))
        .collect();
    assert!(!headers.is_empty());
    assert!(
        headers.iter().all(|header| header.is_ascii()),
        "every header value must be ASCII-safe"
    );
    assert!(
        headers.iter().any(|header| header.contains("\\u00eb")),
        "non-ASCII text must be \\uXXXX escaped: {headers:?}"
    );
    assert!(
        headers.iter().any(|header| header.contains("\\u0025")),
        "percent signs must be escaped so the value survives intact: {headers:?}"
    );

    // The fake decoded the header as JSON, so these are the real object names.
    assert_eq!(
        server.object("/games/gäme 1/revisions/rëv 1.json"),
        Some(serde_json::to_vec(&plain).unwrap())
    );
    assert_eq!(
        server.object("/games/gäme 1/revisions/50% done.json"),
        Some(serde_json::to_vec(&percent).unwrap())
    );

    let mut revisions = provider.list_revisions("gäme 1").unwrap();
    revisions.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(revisions, vec![percent, plain]);
}

#[test]
fn revision_objects_stay_within_the_app_folder_namespace() {
    let server = FakeDropbox::new();
    let provider = provider(server.clone());
    let manifest = Manifest::empty();
    let revision = revision("rev-1", "game-1", &manifest.hash());

    provider.put_revision(&revision, &manifest).unwrap();

    for key in server.keys() {
        assert!(
            key.starts_with("/blobs/")
                || key.starts_with("/manifests/")
                || key.starts_with("/games/"),
            "unexpected object outside the documented layout: {key}"
        );
    }

    // Escaping out of the game directory is refused before any request.
    let mut escaping = revision;
    escaping.game_id = "../escape".into();
    let error = provider.put_revision(&escaping, &manifest).unwrap_err();
    assert!(matches!(error, Error::UnsafePath(_)), "got {error:?}");
}
