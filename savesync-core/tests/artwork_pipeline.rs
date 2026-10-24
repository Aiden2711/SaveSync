//! Cover-art caching pipeline against a scripted transport.
//!
//! Everything here runs offline: the engine caches artwork only inside its own
//! data directory, and a failed fetch must never disturb the cover that is
//! already recorded.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use savesync_core::artwork::SteamGridDbClient;
use savesync_core::db::Db;
use savesync_core::http::{HttpRequest, HttpResponse, HttpTransport};
use savesync_core::models::AdapterType;
use savesync_core::restore::DataPaths;
use savesync_core::store;
use savesync_core::{Error, Game, SaveSyncEngine};
use serde_json::{Value, json};

/// Stand-in credential: tests assert it is never echoed back to the caller.
const API_KEY: &str = "sgdb-test-key-never-echoed";

/// Real PNG signature; the client validates magic bytes, not pixel data.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const GRID_URL: &str = "https://cdn2.steamgriddb.com/grid/cover.png";

/// A payload the image sniffer accepts, distinguishable per fetch round.
fn png_payload(tag: u8) -> Vec<u8> {
    let mut bytes = PNG_SIGNATURE.to_vec();
    bytes.push(tag);
    bytes.push(0x00);
    bytes
}

/// JPEG-signature payload. Used to prove the cache name follows the bytes
/// rather than the MIME type declared in the grids response.
fn jpeg_payload(tag: u8) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    bytes.extend_from_slice(b"JFIF\0");
    bytes.push(tag);
    bytes
}

struct ScriptedTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Network("no scripted response".to_string()))?;

        // Mirror `ReqwestTransport`: when a download path is set the body is
        // streamed there instead of being buffered in the response.
        if let Some(path) = &request.download_to {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, &response.body).unwrap();
        }

        Ok(response)
    }
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&value).unwrap(),
    }
}

fn image_response(bytes: &[u8]) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "image/png".to_string())],
        body: bytes.to_vec(),
    }
}

fn autocomplete_hit() -> HttpResponse {
    json_response(
        200,
        json!({"success": true, "data": [{"id": 7, "name": "Hades"}]}),
    )
}

fn grids_hit() -> HttpResponse {
    json_response(
        200,
        json!({
            "success": true,
            "data": [{
                "url": GRID_URL,
                "width": 600,
                "height": 900,
                "mime": "image/png"
            }]
        }),
    )
}

/// The three requests a successful fetch makes, in order.
fn cover_round(image: &[u8]) -> Vec<HttpResponse> {
    vec![autocomplete_hit(), grids_hit(), image_response(image)]
}

struct Fixture {
    _temp: tempfile::TempDir,
    data_root: PathBuf,
    engine: SaveSyncEngine,
    game: Game,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("data");
        let mut engine = SaveSyncEngine::open(&data_root).unwrap();
        let game = engine
            .enroll_game("Hades", AdapterType::CustomDirectory)
            .unwrap();
        Self {
            _temp: temp,
            data_root,
            engine,
            game,
        }
    }

    fn client(responses: Vec<HttpResponse>) -> SteamGridDbClient {
        SteamGridDbClient::new(Arc::new(ScriptedTransport::new(responses)))
    }

    fn cover_name(&self) -> String {
        format!("{}.png", self.game.id)
    }

    fn cover_path_on_disk(&self) -> PathBuf {
        self.engine.covers_dir().join(self.cover_name())
    }

    /// Every entry in the cover cache, sorted, so "no orphans" is checkable.
    fn cached_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.engine.covers_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A second connection over the same database, used to plant a hostile
    /// `cover_path` the engine would never write itself.
    fn tamper_cover_path(&self, recorded: &str) {
        let db = Db::open(DataPaths::new(&self.data_root).database()).unwrap();
        store::set_cover_path(db.connection(), &self.game.id, Some(recorded)).unwrap();
    }
}

#[test]
fn fetch_writes_the_cover_and_records_a_relative_path() {
    let mut fixture = Fixture::new();
    let payload = png_payload(1);
    let client = Fixture::client(cover_round(&payload));

    let updated = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();

    let expected = format!("covers/{}", fixture.cover_name());
    assert_eq!(updated.cover_path.as_deref(), Some(expected.as_str()));

    let on_disk = fixture.cover_path_on_disk();
    assert!(on_disk.starts_with(fixture.engine.covers_dir()));
    assert_eq!(fs::read(&on_disk).unwrap(), payload);
    assert_eq!(fixture.cached_entries(), vec![fixture.cover_name()]);
}

#[test]
fn cover_file_resolves_the_cached_image() {
    let mut fixture = Fixture::new();
    let payload = png_payload(2);
    let client = Fixture::client(cover_round(&payload));

    fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();

    let resolved = fixture
        .engine
        .cover_file(&fixture.game.id)
        .unwrap()
        .expect("a fetched cover resolves");

    assert!(
        resolved.is_absolute(),
        "{} is not absolute",
        resolved.display()
    );
    assert!(resolved.starts_with(fixture.engine.covers_dir()));
    assert_eq!(fs::read(&resolved).unwrap(), payload);
}

#[test]
fn refetch_replaces_the_cover() {
    let mut fixture = Fixture::new();
    let first = png_payload(3);
    let second = png_payload(4);
    let client = Fixture::client([cover_round(&first), cover_round(&second)].concat());

    fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();
    let updated = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();

    let expected = format!("covers/{}", fixture.cover_name());
    assert_eq!(updated.cover_path.as_deref(), Some(expected.as_str()));
    assert_eq!(fs::read(fixture.cover_path_on_disk()).unwrap(), second);
    assert_eq!(
        fixture.cached_entries(),
        vec![fixture.cover_name()],
        "a re-fetch must leave exactly one file per game"
    );
}

#[test]
fn failed_refetch_keeps_the_previous_cover() {
    let mut fixture = Fixture::new();
    let first = png_payload(5);
    let mut responses = cover_round(&first);
    responses.push(json_response(
        401,
        json!({"success": false, "errors": ["unauthorized"]}),
    ));
    let client = Fixture::client(responses);

    let fetched = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();
    let recorded = fetched.cover_path.clone();
    assert!(recorded.is_some());

    let error = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap_err();
    assert!(matches!(error, Error::Auth(_)));
    assert!(
        !format!("{error}").contains(API_KEY),
        "the API key must never surface in an error"
    );

    let unchanged = fixture.engine.get_game(&fixture.game.id).unwrap();
    assert_eq!(unchanged.cover_path, recorded);
    assert_eq!(fs::read(fixture.cover_path_on_disk()).unwrap(), first);
}

#[test]
fn unknown_title_creates_no_file() {
    let mut fixture = Fixture::new();
    let client = Fixture::client(vec![json_response(
        200,
        json!({"success": true, "data": []}),
    )]);

    let error = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap_err();

    assert!(matches!(error, Error::NotFound(_)));
    assert!(
        fixture.cached_entries().is_empty(),
        "no cover file may be written without a match"
    );
    assert_eq!(
        fixture
            .engine
            .get_game(&fixture.game.id)
            .unwrap()
            .cover_path,
        None
    );
}

#[test]
fn non_image_payload_leaves_no_cover_and_no_path() {
    let mut fixture = Fixture::new();
    let client = Fixture::client(vec![
        autocomplete_hit(),
        grids_hit(),
        HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/html".to_string())],
            body: b"<html>definitely not a cover</html>".to_vec(),
        },
    ]);

    let error = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap_err();

    assert!(matches!(error, Error::InvalidState(_)));
    assert_eq!(
        fixture
            .engine
            .get_game(&fixture.game.id)
            .unwrap()
            .cover_path,
        None
    );
    let leftovers = fixture.cached_entries();
    assert!(
        leftovers.is_empty(),
        "nothing may remain in the cover cache: {leftovers:?}"
    );
    assert!(
        !leftovers.iter().any(|name| name.ends_with(".part")),
        "a failed download must remove its staging file: {leftovers:?}"
    );
}

#[test]
fn clear_removes_the_file_and_the_recorded_path() {
    let mut fixture = Fixture::new();
    let payload = png_payload(6);
    let client = Fixture::client(cover_round(&payload));

    fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();
    assert!(fixture.cover_path_on_disk().exists());

    let cleared = fixture.engine.clear_cover_art(&fixture.game.id).unwrap();

    assert_eq!(cleared.cover_path, None);
    assert!(!fixture.cover_path_on_disk().exists());
    assert!(
        fixture
            .engine
            .cover_file(&fixture.game.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn cover_file_rejects_a_tampered_relative_path() {
    let fixture = Fixture::new();
    // Reachable only through traversal, so the test can only pass if the path
    // is rejected instead of resolved and read.
    let outside = fixture.data_root.join("escaped.png");
    fs::write(&outside, png_payload(7)).unwrap();

    fixture.tamper_cover_path("../../escaped.png");
    let error = fixture.engine.cover_file(&fixture.game.id).unwrap_err();
    assert!(
        matches!(error, Error::InvalidState(_)),
        "expected an out-of-cache rejection, got {error:?}"
    );

    // A value that carries the cache prefix must still be re-validated rather
    // than trusted.
    fixture.tamper_cover_path("covers/../../escaped.png");
    let error = fixture.engine.cover_file(&fixture.game.id).unwrap_err();
    assert!(
        matches!(error, Error::UnsafePath(_)),
        "expected a traversal rejection, got {error:?}"
    );
}

#[test]
fn cover_file_rejects_an_absolute_path() {
    let fixture = Fixture::new();
    let absolute = if cfg!(windows) {
        r"C:\Users\Public\escaped.png"
    } else {
        "/tmp/escaped.png"
    };
    fixture.tamper_cover_path(absolute);

    let error = fixture.engine.cover_file(&fixture.game.id).unwrap_err();

    assert!(
        matches!(error, Error::InvalidState(_)),
        "expected an out-of-cache rejection, got {error:?}"
    );
}

/// `grids_hit` declares `"mime": "image/png"`; the bytes are a JPEG. The cache
/// name, the recorded path, and the served media type must follow the bytes.
#[test]
fn cached_extension_follows_the_bytes_not_the_declared_mime() {
    let mut fixture = Fixture::new();
    let payload = jpeg_payload(8);
    let client = Fixture::client(cover_round(&payload));

    let updated = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();

    let expected_name = format!("{}.jpg", fixture.game.id);
    let expected_relative = format!("covers/{expected_name}");
    assert_eq!(
        updated.cover_path.as_deref(),
        Some(expected_relative.as_str())
    );

    let on_disk = fixture.engine.covers_dir().join(&expected_name);
    assert_eq!(fs::read(&on_disk).unwrap(), payload);
    assert_eq!(
        fixture.cached_entries(),
        vec![expected_name],
        "no wrongly-named, staging, or partial file may remain in the cache"
    );

    let resolved = fixture
        .engine
        .cover_file(&fixture.game.id)
        .unwrap()
        .expect("the recorded cover resolves");
    assert_eq!(resolved, on_disk);
}

/// A re-fetch in a different format moves the record to the new extension and
/// prunes the old file, so the cache never holds two copies.
#[test]
fn refetch_in_another_format_prunes_the_old_extension() {
    let mut fixture = Fixture::new();
    let first = png_payload(9);
    let second = jpeg_payload(10);
    let client = Fixture::client([cover_round(&first), cover_round(&second)].concat());

    let initial = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();
    assert!(
        initial
            .cover_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".png")),
        "expected a PNG cover first, got {:?}",
        initial.cover_path
    );

    let updated = fixture
        .engine
        .fetch_cover_art(&client, API_KEY, &fixture.game.id)
        .unwrap();

    let expected_name = format!("{}.jpg", fixture.game.id);
    assert!(
        updated
            .cover_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".jpg")),
        "expected a JPEG cover after the re-fetch, got {:?}",
        updated.cover_path
    );
    assert_eq!(
        fixture.cached_entries(),
        vec![expected_name],
        "the stale PNG must be pruned once the record points at the JPEG"
    );
    assert_eq!(
        fs::read(fixture.cover_path_on_disk()).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        fs::read(
            fixture
                .engine
                .covers_dir()
                .join(format!("{}.jpg", fixture.game.id))
        )
        .unwrap(),
        second
    );
}
