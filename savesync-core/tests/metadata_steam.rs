//! Steam Store metadata provider and cache, against a scripted transport.
//!
//! Everything runs offline: the provider is handed a [`HttpTransport`] that
//! replays JSON fixtures, and the engine caches the normalized row only inside
//! its own data directory.

use std::collections::VecDeque;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use savesync_core::http::{
    HttpRequest, HttpResponse, HttpTransport, MAX_RESPONSE_BYTES, RetryPolicy,
};
use savesync_core::metadata::{MetadataProvider, MetadataSource};
use savesync_core::models::AdapterType;
use savesync_core::steam_metadata::{
    APP_LIST_CACHE_FILE, MAX_APP_LIST_BYTES, SteamMetadataProvider,
};
use savesync_core::{Error, SaveSyncEngine};
use serde_json::{Value, json};

/// Real Steam ids keep the fixture honest about the app-list shape.
const HADES_APPID: u32 = 1_145_360;

struct ScriptedTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        self.requests.lock().unwrap().push(request.clone());
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Network("no scripted response".to_string()))?;

        // Mirror `ReqwestTransport`: a download request streams the body to the
        // given path instead of buffering it in the response.
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

fn app_list_response(apps: &[(u32, &str)]) -> HttpResponse {
    let apps: Vec<Value> = apps
        .iter()
        .map(|(appid, name)| json!({"appid": appid, "name": name}))
        .collect();
    json_response(200, json!({"applist": {"apps": apps}}))
}

fn details_response(appid: u32, data: Value) -> HttpResponse {
    json_response(
        200,
        json!({ appid.to_string(): {"success": true, "data": data} }),
    )
}

fn hades_details() -> Value {
    json!({
        "type": "game",
        "name": "Hades",
        "short_description": "Defy the god of the dead.",
        "detailed_description": "<p>A rogue-like dungeon crawler.</p>",
        "developers": ["Supergiant Games"],
        "publishers": ["Supergiant Games"],
        "genres": [{"id": "1", "description": "Action"}, {"id": "23", "description": "Roguelike"}],
        "release_date": {"coming_soon": false, "date": "17 Sep, 2020"}
    })
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    }
}

#[test]
fn search_normalizes_titles_and_reports_platform_hints() {
    let transport = Arc::new(ScriptedTransport::new(vec![app_list_response(&[
        (HADES_APPID, "Hades"),
        (1_145_361, "Hades II"),
        (1_145_362, "Hades' Star"),
        (999, "Super Hades Deluxe"),
        (5, "Unrelated Game"),
        // Duplicate id and the sentinel id 0 must be dropped.
        (HADES_APPID, "Hades™"),
        (0, "Hades"),
    ])]));
    let provider = SteamMetadataProvider::new(transport.clone());

    let candidates = provider.search("HADES™").unwrap();
    let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Hades", "Hades II", "Hades' Star", "Super Hades Deluxe"]
    );
    assert_eq!(candidates[0].source_id, HADES_APPID.to_string());
    assert!(
        candidates
            .iter()
            .all(|c| c.platform_hint.as_deref() == Some("PC"))
    );
    assert!(candidates.iter().all(|c| c.release_year.is_none()));

    // One request: the full list is downloaded once and matched in memory.
    assert_eq!(transport.calls(), 1);
    let request = &transport.requests()[0];
    assert!(request.url.starts_with("https://api.steampowered.com/"));
    assert!(
        request.download_to.is_some(),
        "the list must stream to disk instead of being buffered"
    );
    assert_eq!(request.max_download_bytes, Some(MAX_APP_LIST_BYTES));

    // The second search reuses the in-memory list.
    provider.search("hades ii").unwrap();
    assert_eq!(transport.calls(), 1);
}

#[test]
fn empty_title_never_downloads_the_app_list() {
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let provider = SteamMetadataProvider::new(transport.clone());

    assert!(provider.search("   ").unwrap().is_empty());
    assert_eq!(transport.calls(), 0);
}

#[test]
fn unknown_title_is_an_empty_result() {
    let transport = Arc::new(ScriptedTransport::new(vec![app_list_response(&[(
        HADES_APPID,
        "Hades",
    )])]));
    let provider = SteamMetadataProvider::new(transport);

    assert!(provider.search("Celeste").unwrap().is_empty());
}

#[test]
fn a_fresh_app_list_cache_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first_transport = Arc::new(ScriptedTransport::new(vec![app_list_response(&[(
        HADES_APPID,
        "Hades",
    )])]));
    let first = SteamMetadataProvider::with_cache_dir(first_transport.clone(), dir.path());
    assert_eq!(first.search("Hades").unwrap().len(), 1);
    assert_eq!(first_transport.calls(), 1);

    let cache_file = dir.path().join(APP_LIST_CACHE_FILE);
    assert!(cache_file.is_file(), "the app list is persisted");

    // A second provider with an empty transport proves the file is reused: any
    // request would fail with "no scripted response".
    let second_transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let second = SteamMetadataProvider::with_cache_dir(second_transport.clone(), dir.path());
    assert_eq!(second.search("Hades").unwrap().len(), 1);
    assert_eq!(second_transport.calls(), 0);
}

#[test]
fn details_map_steam_fields_onto_the_normalized_model() {
    let transport = Arc::new(ScriptedTransport::new(vec![details_response(
        HADES_APPID,
        hades_details(),
    )]));
    let provider = SteamMetadataProvider::new(transport.clone());

    let metadata = provider.details(&HADES_APPID.to_string()).unwrap();
    assert_eq!(metadata.source, MetadataSource::Steam);
    assert_eq!(metadata.source_id, HADES_APPID.to_string());
    assert_eq!(metadata.name, "Hades");
    assert_eq!(metadata.release_year, Some(2020));
    assert_eq!(metadata.release_date.as_deref(), Some("17 Sep, 2020"));
    assert_eq!(metadata.genres, vec!["Action", "Roguelike"]);
    assert_eq!(metadata.developers, vec!["Supergiant Games"]);
    assert_eq!(metadata.publishers, vec!["Supergiant Games"]);
    assert_eq!(
        metadata.description_short.as_deref(),
        Some("Defy the god of the dead.")
    );
    assert_eq!(
        metadata.description_long.as_deref(),
        Some("<p>A rogue-like dungeon crawler.</p>")
    );
    assert_eq!(metadata.players, None);
    assert_eq!(metadata.age_rating, None);
    assert_eq!(metadata.franchise, None);
    assert!(metadata.themes.is_empty());
    assert!(!metadata.fetched_at.is_empty());

    let request = &transport.requests()[0];
    assert!(request.url.contains(&format!("appids={HADES_APPID}")));
    assert!(request.url.contains("l=english"));

    // Details are cached per app id (single request per lookup).
    provider.details(&HADES_APPID.to_string()).unwrap();
    assert_eq!(transport.calls(), 1);
}

#[test]
fn missing_or_empty_details_are_not_found() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        HttpResponse {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        },
        json_response(200, json!({"2": {"success": false}})),
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        },
    ]));
    let provider = SteamMetadataProvider::new(transport);

    for source_id in ["1", "2", "3"] {
        let error = provider.details(source_id).unwrap_err();
        assert!(
            matches!(error, Error::NotFound(_)),
            "{source_id} -> {error:?}"
        );
    }
}

#[test]
fn throttling_is_retried_and_a_forbidden_response_is_not() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        HttpResponse {
            status: 429,
            headers: Vec::new(),
            body: Vec::new(),
        },
        HttpResponse {
            status: 503,
            headers: Vec::new(),
            body: Vec::new(),
        },
        details_response(HADES_APPID, hades_details()),
    ]));
    let provider = SteamMetadataProvider::new(transport.clone()).with_retry_policy(fast_policy());
    assert!(provider.details(&HADES_APPID.to_string()).is_ok());
    assert_eq!(transport.calls(), 3);

    let forbidden = Arc::new(ScriptedTransport::new(vec![HttpResponse {
        status: 403,
        headers: Vec::new(),
        body: b"Access Denied".to_vec(),
    }]));
    let provider = SteamMetadataProvider::new(forbidden.clone()).with_retry_policy(fast_policy());
    let error = provider.details(&HADES_APPID.to_string()).unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error:?}");
    assert_eq!(forbidden.calls(), 1, "403 must never be retried");
}

#[test]
fn oversized_details_are_rejected_before_parsing() {
    let transport = Arc::new(ScriptedTransport::new(vec![HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: vec![b'{'; MAX_RESPONSE_BYTES + 1],
    }]));
    let provider = SteamMetadataProvider::new(transport);

    let error = provider.details(&HADES_APPID.to_string()).unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert!(error.to_string().contains("byte limit"));
}

#[test]
fn invalid_app_ids_are_refused_without_a_request() {
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let provider = SteamMetadataProvider::new(transport.clone());

    for invalid in ["", "  ", "abc", "0", "-3"] {
        assert!(provider.details(invalid).is_err(), "{invalid}");
    }
    assert_eq!(transport.calls(), 0);
}

#[test]
fn engine_caches_fetched_metadata_for_the_game() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(dir.path()).unwrap();
    let game = engine
        .enroll_game("Hades", AdapterType::CustomDirectory)
        .unwrap();

    let transport = Arc::new(ScriptedTransport::new(vec![
        app_list_response(&[(HADES_APPID, "Hades")]),
        details_response(HADES_APPID, hades_details()),
    ]));
    let provider = SteamMetadataProvider::new(transport.clone());

    let fetched = engine.fetch_metadata_with(&provider, &game.id).unwrap();
    assert_eq!(fetched.source_id, HADES_APPID.to_string());

    let cached = engine.cached_metadata(&game.id).unwrap().unwrap();
    assert_eq!(cached, fetched);
    assert_eq!(transport.calls(), 2, "one list + one details request");

    // A second fetch reuses both provider caches and rewrites the same row.
    let again = engine.fetch_metadata_with(&provider, &game.id).unwrap();
    assert_eq!(again, fetched);
    assert_eq!(transport.calls(), 2);
    assert_eq!(engine.cached_metadata(&game.id).unwrap().unwrap(), fetched);
}

#[test]
fn cached_metadata_is_absent_for_unknown_games() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SaveSyncEngine::open(dir.path()).unwrap();
    assert!(engine.cached_metadata("missing").unwrap().is_none());
}

#[test]
fn a_failed_fetch_never_overwrites_a_cached_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(dir.path()).unwrap();
    let game = engine
        .enroll_game("Hades", AdapterType::CustomDirectory)
        .unwrap();

    let good = Arc::new(ScriptedTransport::new(vec![
        app_list_response(&[(HADES_APPID, "Hades")]),
        details_response(HADES_APPID, hades_details()),
    ]));
    let good_provider = SteamMetadataProvider::new(good);
    let cached = engine
        .fetch_metadata_with(&good_provider, &game.id)
        .unwrap();

    let failing = Arc::new(ScriptedTransport::new(Vec::new()));
    let failing_provider = SteamMetadataProvider::new(failing);
    assert!(
        engine
            .fetch_metadata_with(&failing_provider, &game.id)
            .is_err()
    );

    assert_eq!(
        engine.cached_metadata(&game.id).unwrap().unwrap(),
        cached,
        "the previous good row must survive a failed fetch"
    );
}
