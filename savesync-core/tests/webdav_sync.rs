//! End-to-end sync through the WebDAV provider against an in-memory WebDAV
//! server. Mirrors `tests/google_drive_sync.rs`: the fake implements
//! [`HttpTransport`] and answers `PROPFIND`/`GET`/`HEAD`/`PUT`/`MKCOL`/`MOVE`
//! over a map, so the provider exercises real request sequences without a
//! network.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use savesync_core::blob::BlobStore;
use savesync_core::hash::{hash_bytes, hash_file};
use savesync_core::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, RetryPolicy};
use savesync_core::{
    AdapterType, Error, FileEntry, Manifest, RestoreMode, Revision, SaveSyncEngine, SyncProvider,
    WebDavProvider,
};
use url::Url;

/// Base collection shared by every test.
const BASE: &str = "/saves";

/// In-memory WebDAV server keyed by decoded absolute path.
struct FakeWebdav {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    collections: Mutex<HashSet<String>>,
    calls: Mutex<Vec<HttpRequest>>,
    /// Statuses injected for the next request of a given method.
    failures: Mutex<VecDeque<(HttpMethod, u16)>>,
    move_supported: AtomicBool,
    mkcol_status: Mutex<Option<u16>>,
}

impl FakeWebdav {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            objects: Mutex::new(HashMap::new()),
            collections: Mutex::new(HashSet::from([BASE.to_string()])),
            calls: Mutex::new(Vec::new()),
            failures: Mutex::new(VecDeque::new()),
            move_supported: AtomicBool::new(true),
            mkcol_status: Mutex::new(None),
        })
    }

    fn fail_next(&self, method: HttpMethod, status: u16) {
        self.failures.lock().unwrap().push_back((method, status));
    }

    fn set_move_supported(&self, supported: bool) {
        self.move_supported.store(supported, Ordering::SeqCst);
    }

    fn set_mkcol_status(&self, status: u16) {
        *self.mkcol_status.lock().unwrap() = Some(status);
    }

    fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, method: HttpMethod) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.method == method)
            .count()
    }

    /// Seed an object behind the provider's back (simulates another writer).
    fn seed(&self, path: &str, bytes: &[u8]) {
        self.objects
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes.to_vec());
    }

    fn object(&self, path: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(path).cloned()
    }

    fn object_keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    fn next_failure(&self, method: HttpMethod) -> Option<u16> {
        let mut failures = self.failures.lock().unwrap();
        match failures.front() {
            Some((expected, status)) if *expected == method => {
                let status = *status;
                failures.pop_front();
                Some(status)
            }
            _ => None,
        }
    }

    fn is_collection(&self, path: &str) -> bool {
        self.collections.lock().unwrap().contains(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.is_collection(path) || self.objects.lock().unwrap().contains_key(path)
    }

    fn propfind(&self, path: &str, depth: &str) -> HttpResponse {
        let objects = self.objects.lock().unwrap();
        let collections = self.collections.lock().unwrap();

        if depth == "0" {
            if !collections.contains(path) && !objects.contains_key(path) {
                return empty(404);
            }
            let entry = entry_xml(
                path,
                collections.contains(path),
                objects.get(path).map(Vec::len),
            );
            return multistatus(&[entry.as_str()]);
        }

        if !collections.contains(path) {
            return empty(404);
        }
        let mut entries = vec![entry_xml(path, true, None)];
        for (key, bytes) in objects.iter() {
            if parent(key).as_deref() == Some(path) {
                entries.push(entry_xml(key, false, Some(bytes.len())));
            }
        }
        for key in collections.iter() {
            if parent(key).as_deref() == Some(path) {
                entries.push(entry_xml(key, true, None));
            }
        }
        let entries: Vec<&str> = entries.iter().map(String::as_str).collect();
        multistatus(&entries)
    }

    fn get(&self, path: &str, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let objects = self.objects.lock().unwrap();
        let Some(bytes) = objects.get(path) else {
            return Ok(empty(404));
        };
        let length = bytes.len().to_string();
        if let Some(destination) = &request.download_to {
            fs::write(destination, bytes).map_err(|e| Error::io(destination.clone(), e))?;
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Length".into(), length)],
                body: Vec::new(),
            })
        } else {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Length".into(), length)],
                body: bytes.clone(),
            })
        }
    }

    fn head(&self, path: &str) -> HttpResponse {
        match self.object(path) {
            Some(bytes) => HttpResponse {
                status: 200,
                headers: vec![("Content-Length".into(), bytes.len().to_string())],
                body: Vec::new(),
            },
            None => empty(404),
        }
    }

    fn put(&self, path: &str, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let Some(parent) = parent(path) else {
            return Ok(empty(409));
        };
        if !self.is_collection(&parent) {
            return Ok(empty(409));
        }
        if request.header_value("If-None-Match") == Some("*") && self.exists(path) {
            return Ok(empty(412));
        }
        let body = match &request.body {
            Some(body) => body.read_all()?,
            None => Vec::new(),
        };
        self.objects.lock().unwrap().insert(path.to_string(), body);
        Ok(empty(201))
    }

    fn mkcol(&self, path: &str) -> HttpResponse {
        if let Some(status) = *self.mkcol_status.lock().unwrap() {
            // Servers report "already exists" as 405 or 409; either is success.
            if status == 405 && !self.is_collection(path) {
                self.collections.lock().unwrap().insert(path.to_string());
            }
            return empty(status);
        }
        let mut collections = self.collections.lock().unwrap();
        if collections.contains(path) {
            return empty(405);
        }
        collections.insert(path.to_string());
        empty(201)
    }

    fn move_object(&self, path: &str, request: &HttpRequest) -> Result<HttpResponse, Error> {
        if !self.move_supported.load(Ordering::SeqCst) {
            return Ok(empty(501));
        }
        let Some(destination) = request.header_value("Destination") else {
            return Ok(empty(400));
        };
        let url = Url::parse(destination)
            .map_err(|error| Error::Network(format!("invalid Destination: {error}")))?;
        let destination = percent_decode(url.path());
        if !self.exists(path) {
            return Ok(empty(404));
        }
        if request.header_value("Overwrite") == Some("F") && self.exists(&destination) {
            return Ok(empty(412));
        }
        let mut objects = self.objects.lock().unwrap();
        if let Some(bytes) = objects.remove(path) {
            objects.insert(destination, bytes);
        }
        Ok(empty(201))
    }
}

impl HttpTransport for FakeWebdav {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        self.calls.lock().unwrap().push(request.clone());
        if let Some(status) = self.next_failure(request.method) {
            return Ok(empty(status));
        }
        let url = Url::parse(&request.url)
            .map_err(|error| Error::Network(format!("invalid fake URL: {error}")))?;
        let path = percent_decode(url.path());
        match request.method {
            HttpMethod::Propfind => {
                Ok(self.propfind(&path, request.header_value("Depth").unwrap_or("1")))
            }
            HttpMethod::Get => self.get(&path, request),
            HttpMethod::Head => Ok(self.head(&path)),
            HttpMethod::Put => self.put(&path, request),
            HttpMethod::Mkcol => Ok(self.mkcol(&path)),
            HttpMethod::Move => self.move_object(&path, request),
            HttpMethod::Delete => {
                self.objects.lock().unwrap().remove(&path);
                Ok(empty(204))
            }
            _ => Ok(empty(400)),
        }
    }
}

fn empty(status: u16) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn multistatus(entries: &[&str]) -> HttpResponse {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\">\n",
    );
    for entry in entries {
        body.push_str(entry);
    }
    body.push_str("</d:multistatus>");
    HttpResponse {
        status: 207,
        headers: vec![(
            "Content-Type".into(),
            "application/xml; charset=utf-8".into(),
        )],
        body: body.into_bytes(),
    }
}

/// One `d:response` with a percent-encoded `d:href`, as real servers send it.
fn entry_xml(path: &str, is_collection: bool, length: Option<usize>) -> String {
    let resource_type = if is_collection { "<d:collection/>" } else { "" };
    let length = length
        .map(|bytes| format!("<d:getcontentlength>{bytes}</d:getcontentlength>"))
        .unwrap_or_default();
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop>\
         <d:resourcetype>{resource_type}</d:resourcetype>\
         <d:getetag>\"weak-etag\"</d:getetag>{length}</d:prop>\
         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\n",
        encode_path(path)
    )
}

fn parent(path: &str) -> Option<String> {
    let (head, _) = path.rsplit_once('/')?;
    Some(if head.is_empty() {
        "/".to_string()
    } else {
        head.to_string()
    })
}

fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        let unreserved =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/');
        if unreserved {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap()
}

fn provider(server: Arc<FakeWebdav>) -> WebDavProvider {
    WebDavProvider::new(server, "https://dav.test/saves", "saveuser", "s3cret")
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        })
}

fn blob_path(hash: &str) -> String {
    format!("{BASE}/blobs/{}/{hash}.tar.zst", &hash[..2])
}

fn sample_source(dir: &std::path::Path, contents: &[u8]) -> std::path::PathBuf {
    let path = dir.join("blob.tar.zst");
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn two_devices_sync_through_the_webdav_provider() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let temp = tempfile::tempdir().unwrap();

    // Device A publishes a snapshot.
    let a_save = temp.path().join("a_save");
    fs::create_dir_all(&a_save).unwrap();
    fs::write(a_save.join("save.bin"), b"from-device-a").unwrap();
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game = a
        .enroll_game("WebDAV Game", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game.id, &a_save).unwrap();
    let first = a.snapshot_game(&game.id, Some("first")).unwrap();

    let report = a.sync(&provider, &game.id).unwrap();
    assert_eq!(report.pushed_revisions, 1);
    assert_eq!(report.uploaded_blobs, 1);
    assert_eq!(
        server.object_keys().len(),
        3,
        "expected blob, manifest, and revision objects"
    );
    assert!(
        !server
            .object_keys()
            .iter()
            .any(|key| key.contains(".part-")),
        "staging objects must not survive a successful upload"
    );

    // Device B pulls and explicitly restores.
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    b.enroll_game_with_id(&game.id, "WebDAV Game", AdapterType::CustomDirectory)
        .unwrap();
    b.add_directory_location(&game.id, &b_save).unwrap();

    let report = b.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 1);
    assert_eq!(report.downloaded_blobs, 1);
    assert!(fs::read_dir(&b_save).unwrap().next().is_none());

    b.restore(&first.revision_id).unwrap();
    assert_eq!(fs::read(b_save.join("save.bin")).unwrap(), b"from-device-a");

    // B advances, publishes; A pulls and restores.
    fs::write(b_save.join("save.bin"), b"from-device-b").unwrap();
    let second = b.snapshot_game(&game.id, Some("second")).unwrap();
    b.sync(&provider, &game.id).unwrap();

    let report = a.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 1);
    let heads = a.heads(&game.id).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, second.revision_id);

    a.restore_with_mode(&second.revision_id, RestoreMode::ExactMirror)
        .unwrap();
    assert_eq!(fs::read(a_save.join("save.bin")).unwrap(), b"from-device-b");
}

/// Build a real `tar.zst` blob the way the engine does and return
/// `(manifest_hash, archive_path)`.
///
/// The archive digest is independent of the manifest digest, which is exactly
/// the asymmetry the provider must not confuse: the object key is the manifest
/// hash, the content is the archive.
fn real_archive(
    root: &std::path::Path,
    name: &str,
    contents: &[u8],
) -> (String, std::path::PathBuf) {
    let save_dir = root.join(format!("save-{name}"));
    fs::create_dir_all(&save_dir).unwrap();
    let payload = save_dir.join("save.bin");
    fs::write(&payload, contents).unwrap();
    let entry = FileEntry {
        path: "save.bin".into(),
        size: contents.len() as u64,
        sha256: hash_bytes(contents),
        mtime_ms: None,
    };
    let manifest = Manifest::new(vec![entry]).unwrap();
    let store = BlobStore::open(root.join("blobs")).unwrap();
    let manifest_hash = store.put(&manifest, &save_dir).unwrap();
    let archive = store.path_for(&manifest_hash).unwrap();
    (manifest_hash, archive)
}

#[test]
fn blob_uploads_are_idempotent_and_never_replace_content() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let (hash, source) = real_archive(dir.path(), "one", b"blob-payload");

    // Guard against fixture coincidence: the stored object is the archive, so
    // its digest must differ from the manifest hash that names it.
    let archive_digest = hash_file(&source).unwrap();
    assert_ne!(
        archive_digest, hash,
        "fixture must exercise archive-vs-key digest divergence"
    );

    assert!(!provider.has_blob(&hash).unwrap());
    provider.put_blob(&hash, &source).unwrap();
    let stored = server
        .object(&blob_path(&hash))
        .expect("blob must be uploaded");
    assert_eq!(stored, fs::read(&source).unwrap());
    assert!(provider.has_blob(&hash).unwrap());

    let puts_after_first = server.count(HttpMethod::Put);
    provider.put_blob(&hash, &source).unwrap();

    assert_eq!(
        server.object(&blob_path(&hash)).unwrap(),
        stored,
        "an existing blob must never be rewritten"
    );
    assert_eq!(
        server.count(HttpMethod::Put),
        puts_after_first,
        "an already-present blob must not be re-uploaded"
    );
    assert!(
        !server
            .object_keys()
            .iter()
            .any(|key| key.contains(".part-")),
        "staging objects must not survive"
    );
}

#[test]
fn conflicting_archive_under_the_same_key_is_rejected_and_left_untouched() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let (hash, source) = real_archive(dir.path(), "expected", b"expected-bytes");
    let (_, other_archive) = real_archive(dir.path(), "other", b"a-different-object");
    let other_bytes = fs::read(&other_archive).unwrap();
    assert_ne!(
        hash_file(&other_archive).unwrap(),
        hash_file(&source).unwrap(),
        "conflict fixture must not be byte-identical"
    );

    server.seed(&blob_path(&hash), &other_bytes);

    let error = provider.put_blob(&hash, &source).unwrap_err();
    assert!(
        matches!(error, Error::IntegrityMismatch { .. }),
        "expected an integrity error, got {error:?}"
    );
    assert_eq!(
        server.object(&blob_path(&hash)).unwrap(),
        other_bytes,
        "a conflicting object must never be overwritten"
    );
}

#[test]
fn existing_blob_with_different_archive_bytes_is_rejected() {
    // Same shape as above but the conflict is discovered only after a first
    // successful upload, so the re-push path (not the pre-check) is exercised
    // with a real archive whose digest differs from the key.
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let (hash, source) = real_archive(dir.path(), "expected", b"expected-bytes");
    let (_, other_archive) = real_archive(dir.path(), "other", b"a-different-object");

    provider.put_blob(&hash, &source).unwrap();
    let puts_after_first = server.count(HttpMethod::Put);
    // Another writer replaced the object with a different but valid archive.
    server.seed(&blob_path(&hash), &fs::read(&other_archive).unwrap());

    let error = provider.put_blob(&hash, &source).unwrap_err();
    assert!(
        matches!(error, Error::IntegrityMismatch { .. }),
        "expected an integrity error, got {error:?}"
    );
    assert_eq!(
        server.count(HttpMethod::Put),
        puts_after_first,
        "a conflicting blob must never be re-uploaded"
    );
}

#[test]
fn transient_upload_failure_is_retried_once_and_succeeds() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let contents = b"retry-payload";
    let source = sample_source(dir.path(), contents);
    let hash = hash_bytes(contents);

    server.fail_next(HttpMethod::Put, 500);

    provider.put_blob(&hash, &source).unwrap();

    assert_eq!(server.count(HttpMethod::Put), 2, "one retry expected");
    assert_eq!(server.object(&blob_path(&hash)).unwrap(), contents);
    assert!(
        !server
            .object_keys()
            .iter()
            .any(|key| key.contains(".part-")),
        "the retried staging upload must not linger"
    );
}

#[test]
fn mkcol_is_idempotent_and_issued_once_per_collection() {
    let server = FakeWebdav::new();
    server.set_mkcol_status(405); // every collection already exists
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let source = sample_source(dir.path(), b"mkcol-payload");

    let first_hash = format!("ab{}", "0".repeat(62));
    let second_hash = format!("ab{}", "1".repeat(62));

    provider.put_blob(&first_hash, &source).unwrap();
    let mkcol_calls = server.count(HttpMethod::Mkcol);
    assert_eq!(mkcol_calls, 2, "blobs/ and blobs/<aa>/ only");

    provider.put_blob(&second_hash, &source).unwrap();
    assert_eq!(
        server.count(HttpMethod::Mkcol),
        mkcol_calls,
        "collections are created once per session, not per object"
    );
    assert!(server.object(&blob_path(&first_hash)).is_some());
    assert!(server.object(&blob_path(&second_hash)).is_some());
}

#[test]
fn move_is_preferred_with_overwrite_disabled() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let contents = b"move-payload";
    let source = sample_source(dir.path(), contents);
    let hash = hash_bytes(contents);

    provider.put_blob(&hash, &source).unwrap();

    let calls = server.calls();
    let moves: Vec<&HttpRequest> = calls
        .iter()
        .filter(|call| call.method == HttpMethod::Move)
        .collect();
    assert_eq!(moves.len(), 1, "one MOVE for one new object");
    assert_eq!(moves[0].header_value("Overwrite"), Some("F"));
    assert_eq!(
        moves[0].header_value("Destination"),
        Some(format!("https://dav.test/saves/blobs/{}/{hash}.tar.zst", &hash[..2]).as_str())
    );
    assert!(
        calls
            .iter()
            .any(|call| call.method == HttpMethod::Put && call.url.contains(".part-")),
        "the upload must go to a staging object first"
    );
    assert_eq!(server.object(&blob_path(&hash)).unwrap(), contents);
    assert!(
        !server
            .object_keys()
            .iter()
            .any(|key| key.contains(".part-")),
        "the staging object must be moved into place"
    );
}

#[test]
fn servers_without_move_fall_back_to_direct_put() {
    let server = FakeWebdav::new();
    server.set_move_supported(false);
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let source = sample_source(dir.path(), b"fallback-payload");

    let first_hash = format!("cd{}", "0".repeat(62));
    let second_hash = format!("cd{}", "1".repeat(62));

    provider.put_blob(&first_hash, &source).unwrap();
    provider.put_blob(&second_hash, &source).unwrap();

    assert_eq!(
        server.count(HttpMethod::Move),
        1,
        "MOVE is attempted once, then remembered as unsupported"
    );
    assert_eq!(
        server.count(HttpMethod::Put),
        3,
        "one abandoned staging PUT, then two direct PUTs"
    );

    let calls = server.calls();
    let direct = calls
        .iter()
        .find(|call| {
            call.method == HttpMethod::Put
                && call.url
                    == format!(
                        "https://dav.test/saves/blobs/{}/{second_hash}.tar.zst",
                        &second_hash[..2]
                    )
        })
        .expect("the final object must be written by a direct PUT");
    assert_eq!(direct.header_value("If-None-Match"), Some("*"));

    assert!(server.object(&blob_path(&first_hash)).is_some());
    assert!(server.object(&blob_path(&second_hash)).is_some());
    assert!(
        !server
            .object_keys()
            .iter()
            .any(|key| key.contains(".part-")),
        "abandoned staging objects are cleaned up"
    );
}

#[test]
fn plain_http_endpoints_are_rejected_before_any_request() {
    let server = FakeWebdav::new();
    let error = WebDavProvider::new(server.clone(), "http://dav.test/saves", "user", "pass")
        .err()
        .expect("plain HTTP must be rejected");
    assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    assert!(
        server.calls().is_empty(),
        "a rejected endpoint must not reach the transport"
    );
}

#[test]
fn revision_paths_are_percent_encoded_and_decoded_again() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let manifest = Manifest::empty();
    let revision = Revision {
        id: "rëv 1".into(),
        game_id: "gäme 1".into(),
        parent_ids: Vec::new(),
        device_id: "device-1".into(),
        created_at: "2026-01-01T00:00:00.000Z".into(),
        manifest_hash: manifest.hash(),
        blob_hash: None,
        message: None,
    };

    provider.put_revision(&revision, &manifest).unwrap();
    assert_eq!(
        server.object(&format!("{BASE}/games/gäme 1/revisions/rëv 1.json")),
        Some(serde_json::to_vec(&revision).unwrap())
    );

    let revisions = provider.list_revisions("gäme 1").unwrap();
    assert_eq!(revisions, vec![revision]);

    assert!(
        server
            .calls()
            .iter()
            .any(|call| call.url.contains("g%C3%A4me%201") && call.url.contains("r%C3%ABv%201")),
        "URLs must percent-encode non-ASCII path segments"
    );
}

#[test]
fn listing_an_unknown_game_returns_no_revisions() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    assert!(provider.list_revisions("no-such-game").unwrap().is_empty());
}

#[test]
fn missing_blobs_are_reported_without_leaving_partial_files() {
    let server = FakeWebdav::new();
    let provider = provider(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("download.tar.zst");

    let error = provider
        .get_blob(&"0".repeat(64), &destination)
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    assert!(
        !destination.exists(),
        "a failed download must not leave a partial staged blob"
    );
}

#[test]
fn transport_failures_never_become_silent_successes() {
    struct FailingTransport;

    impl HttpTransport for FailingTransport {
        fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse, Error> {
            // Stands in for a TLS handshake failure: no response at all.
            Err(Error::Network("tls certificate verification failed".into()))
        }
    }

    let provider = WebDavProvider::new(
        Arc::new(FailingTransport),
        "https://dav.test/saves",
        "user",
        "pass",
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    });

    let error = provider.has_blob(&"0".repeat(64)).unwrap_err();
    assert!(matches!(error, Error::Network(_)), "got {error:?}");
}
