//! IGDB metadata provider against a scripted transport.
//!
//! Everything runs offline. The provider is handed a [`HttpTransport`] that
//! replays the Twitch token exchange and IGDB `games` fixtures, so these tests
//! exercise the credential plumbing, the token cache, the field mapping, and the
//! error taxonomy without a network or a real Twitch application.
//!
//! The credential assertions are deliberately blunt: the token request must
//! carry `client_id`/`client_secret`/`grant_type=client_credentials`, every
//! `games` request must carry the `Client-ID` header plus a bearer token, and no
//! error string may contain either credential value or the access token.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use savesync_core::http::{
    HttpRequest, HttpResponse, HttpTransport, MAX_RESPONSE_BYTES, RetryPolicy,
};
use savesync_core::igdb_metadata::{
    FIELDS, GAMES_ENDPOINT, IgdbCredentials, IgdbProvider, TOKEN_ENDPOINT,
};
use savesync_core::metadata::{MetadataCreds, MetadataProvider, MetadataSource};
use savesync_core::models::AdapterType;
use savesync_core::{Error, SaveSyncEngine, provider_for};
use serde_json::{Value, json};

/// A real IGDB game id keeps the fixtures honest about the shape.
const HADES_IGDB_ID: &str = "113112";

const CLIENT_ID: &str = "savesync-twitch-client-id-8f31";
const CLIENT_SECRET: &str = "twitch-client-secret-4b91e7";
const ACCESS_TOKEN: &str = "app-access-token-7c2d";
const REFRESHED_TOKEN: &str = "app-access-token-9e40";

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

    fn token_requests(&self) -> Vec<HttpRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.url.starts_with(TOKEN_ENDPOINT))
            .collect()
    }

    fn games_requests(&self) -> Vec<HttpRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.url == GAMES_ENDPOINT)
            .collect()
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        self.requests.lock().unwrap().push(request.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Network("no scripted response".to_string()))
    }
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&value).unwrap(),
    }
}

fn text_response(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    }
}

fn token_response(access_token: &str, expires_in: u64) -> HttpResponse {
    json_response(
        200,
        json!({
            "access_token": access_token,
            "expires_in": expires_in,
            "token_type": "bearer"
        }),
    )
}

fn body_text(request: &HttpRequest) -> String {
    let bytes = request
        .body
        .as_ref()
        .expect("a games request carries an Apicalypse body")
        .read_all()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

fn credentials() -> IgdbCredentials {
    IgdbCredentials::new(CLIENT_ID, CLIENT_SECRET)
}

fn provider(transport: Arc<ScriptedTransport>) -> IgdbProvider {
    IgdbProvider::new(transport, credentials())
        .expect("credentials are complete")
        .with_retry_policy(fast_policy())
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    }
}

/// The relevance-ordered answer `search "Hades"` returns.
fn hades_search_results() -> Value {
    json!([
        {"id": 113113, "name": "Hades II"},
        {"id": 113112, "name": "Hades", "first_release_date": 1_600_300_800_i64}
    ])
}

/// The full detail payload `where id = …` returns.
fn hades_details() -> Value {
    json!([{
        "id": 113112,
        "name": "Hades",
        "first_release_date": 1_600_300_800_i64,
        "genres": [
            {"id": 12, "name": "Role-playing (RPG)"},
            {"id": 25, "name": "Hack and slash/Beat 'em up"}
        ],
        "involved_companies": [
            {"developer": true, "publisher": false, "company": {"id": 1, "name": "Supergiant Games"}},
            {"developer": false, "publisher": true, "company": {"id": 1, "name": "Supergiant Games"}},
            {"developer": false, "publisher": true, "company": {"id": 2, "name": "Private Division"}}
        ],
        "summary": "Defy the god of the dead as you hack and slash out of the Underworld.",
        "themes": [{"id": 1, "name": "Action"}, {"id": 17, "name": "Fantasy"}],
        "game_modes": [{"id": 1, "name": "Single player"}],
        "age_ratings": [
            {"category": 1, "rating": 5},
            {"category": 2, "rating": 3}
        ],
        "franchise": {"id": 3, "name": "Hades"}
    }])
}

#[test]
fn the_token_exchange_uses_the_client_credentials_grant() {
    let idle = Arc::new(ScriptedTransport::new(vec![token_response(
        ACCESS_TOKEN,
        5_184_000,
    )]));
    let idle_provider = provider(idle.clone());

    assert!(idle_provider.search("   ").unwrap().is_empty());
    assert_eq!(idle.calls(), 0, "an empty title costs no request");

    // Any real call mints a token first.
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, hades_search_results()),
    ]));
    let provider = provider(transport.clone());
    provider.search("Hades").unwrap();

    let token_requests = transport.token_requests();
    assert_eq!(token_requests.len(), 1);
    let token_request = &token_requests[0];
    assert_eq!(token_request.method.as_str(), "POST");
    for (key, value) in [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("grant_type", "client_credentials"),
    ] {
        assert!(
            token_request.url.contains(&format!("{key}={value}")),
            "{key} missing from {url}",
            url = token_request.url
        );
    }

    let games_requests = transport.games_requests();
    assert_eq!(games_requests.len(), 1);
    let search_request = &games_requests[0];
    assert_eq!(search_request.header_value("Client-ID"), Some(CLIENT_ID));
    assert_eq!(
        search_request.header_value("Authorization"),
        Some(format!("Bearer {ACCESS_TOKEN}").as_str())
    );
    assert_eq!(
        search_request.header_value("Content-Type"),
        Some("text/plain")
    );
    let body = body_text(search_request);
    assert!(body.starts_with("search \"Hades\";"), "{body}");
    assert!(body.contains(FIELDS), "{body}");
    assert!(body.ends_with("limit 10;"), "{body}");
}

#[test]
fn every_token_value_is_scrubbed_from_rendered_requests() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, hades_search_results()),
    ]));
    let provider = provider(transport.clone());
    provider.search("Hades").unwrap();

    let rendered = format!("{:?}", transport.token_requests()[0]);
    for secret in [CLIENT_ID, CLIENT_SECRET, ACCESS_TOKEN] {
        assert!(
            !rendered.contains(secret),
            "{secret} leaked into {rendered}"
        );
    }
    assert!(rendered.contains("[redacted]"), "{rendered}");

    let rendered = format!("{:?}", transport.games_requests()[0]);
    assert!(!rendered.contains(ACCESS_TOKEN), "{rendered}");
    assert!(!rendered.contains(CLIENT_ID), "{rendered}");
}

#[test]
fn the_token_exchange_happens_once_and_is_reused() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, hades_search_results()),
        json_response(200, hades_search_results()),
    ]));
    let provider = provider(transport.clone());

    // Two lookups: the token is minted once and reused for both.
    provider.search("Hades").unwrap();
    provider.search("Hades II").unwrap();

    assert_eq!(
        transport.token_requests().len(),
        1,
        "one token for 2 lookups"
    );
    assert_eq!(transport.games_requests().len(), 2);
    for request in transport.games_requests() {
        assert_eq!(
            request.header_value("Authorization"),
            Some(format!("Bearer {ACCESS_TOKEN}").as_str())
        );
    }
}

#[test]
fn a_401_forces_one_refresh_and_a_retry() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        text_response(401, "Unauthorized"),
        token_response(REFRESHED_TOKEN, 5_184_000),
        json_response(200, hades_search_results()),
    ]));
    let provider = provider(transport.clone());

    let candidates = provider.search("Hades").unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(transport.token_requests().len(), 2, "one forced refresh");
    let games = transport.games_requests();
    assert_eq!(games.len(), 2, "the query is replayed once");
    assert_eq!(
        games[1].header_value("Authorization"),
        Some(format!("Bearer {REFRESHED_TOKEN}").as_str())
    );
}

#[test]
fn a_second_401_is_surfaced_instead_of_looping() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        text_response(401, "Unauthorized"),
        token_response(REFRESHED_TOKEN, 5_184_000),
        text_response(401, "Unauthorized"),
    ]));
    let provider = provider(transport.clone());

    let error = provider.search("Hades").unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error:?}");
    assert_eq!(
        transport.token_requests().len(),
        2,
        "two exchanges, no more"
    );
    assert_eq!(transport.games_requests().len(), 2, "two attempts, no more");
    assert_no_secret_leak(&error);
}

#[test]
fn search_maps_candidates_with_the_best_match_first() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, hades_search_results()),
    ]));
    let provider = provider(transport);

    let candidates = provider.search("Hades").unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].source_id, HADES_IGDB_ID, "exact match first");
    assert_eq!(candidates[0].name, "Hades");
    assert_eq!(candidates[0].release_year, Some(2020));
    assert_eq!(
        candidates[0].platform_hint, None,
        "the search field list carries no platform data"
    );
    assert_eq!(candidates[1].name, "Hades II");
    assert_eq!(candidates[1].release_year, None);
}

#[test]
fn details_map_the_game_object_onto_the_normalized_model() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, hades_details()),
    ]));
    let provider = provider(transport.clone());

    let metadata = provider.details(HADES_IGDB_ID).unwrap();
    assert_eq!(metadata.source, MetadataSource::Igdb);
    assert_eq!(metadata.source_id, HADES_IGDB_ID);
    assert_eq!(metadata.name, "Hades");
    assert_eq!(metadata.release_date.as_deref(), Some("2020-09-17"));
    assert_eq!(metadata.release_year, Some(2020));
    assert_eq!(
        metadata.genres,
        vec!["Role-playing (RPG)", "Hack and slash/Beat 'em up"]
    );
    assert_eq!(
        metadata.developers,
        vec!["Supergiant Games"],
        "only developer companies"
    );
    assert_eq!(
        metadata.publishers,
        vec!["Supergiant Games", "Private Division"],
        "only publisher companies, deduplicated"
    );
    assert_eq!(
        metadata.description_long.as_deref(),
        Some("Defy the god of the dead as you hack and slash out of the Underworld.")
    );
    assert_eq!(
        metadata.description_short, None,
        "IGDB has no short description field"
    );
    assert_eq!(metadata.players.as_deref(), Some("Single player"));
    assert_eq!(
        metadata.age_rating.as_deref(),
        Some("PEGI 12"),
        "PEGI wins over ESRB"
    );
    assert_eq!(metadata.franchise.as_deref(), Some("Hades"));
    assert_eq!(metadata.themes, vec!["Action", "Fantasy"]);
    assert!(!metadata.fetched_at.is_empty());

    let body = body_text(&transport.games_requests()[0]);
    assert!(
        body.starts_with(&format!("where id = {HADES_IGDB_ID};")),
        "{body}"
    );
    assert!(body.ends_with("limit 1;"), "{body}");

    // Details are cached per IGDB id: a second lookup is served locally.
    assert_eq!(provider.details(HADES_IGDB_ID).unwrap(), metadata);
    assert_eq!(transport.games_requests().len(), 1);
}

#[test]
fn a_no_match_answer_is_an_empty_result_not_an_error() {
    let empty = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, json!([])),
    ]));
    let empty_provider = provider(empty);
    assert!(empty_provider.search("Celeste").unwrap().is_empty());

    let missing = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        text_response(404, "Not Found"),
    ]));
    let missing_provider = provider(missing);
    assert!(missing_provider.search("Celeste").unwrap().is_empty());
}

#[test]
fn rejected_credentials_are_auth_errors_that_never_echo_the_secrets() {
    // Twitch answers a bad client id/secret with `400` and a body that may echo
    // the grant; the scrubber has to remove both values.
    let transport = Arc::new(ScriptedTransport::new(vec![json_response(
        400,
        json!({
            "status": 400,
            "message": format!(
                "invalid client (client_id={CLIENT_ID}&client_secret={CLIENT_SECRET})"
            )
        }),
    )]));
    let provider = provider(transport.clone());

    let error = provider.search("Hades").unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error:?}");
    assert_eq!(
        transport.token_requests().len(),
        1,
        "a rejected grant is never retried"
    );
    assert_eq!(
        transport.games_requests().len(),
        0,
        "no API request without a token"
    );
    assert_no_secret_leak(&error);
}

#[test]
fn throttling_is_retried_to_the_attempt_cap_then_surfaced() {
    let throttled = || {
        let mut response = text_response(429, r#"{"status":429,"message":"Too many requests"}"#);
        // `Retry-After` is honored and capped by the policy, so the test does
        // not sleep for the production backoff.
        response.headers = vec![("Retry-After".to_string(), "0".to_string())];
        response
    };
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        throttled(),
        throttled(),
        throttled(),
    ]));
    let provider = provider(transport.clone());

    let error = provider.search("Hades").unwrap_err();
    match error {
        Error::Provider { status, .. } => assert_eq!(status, 429),
        other => panic!("expected a provider error, got {other:?}"),
    }
    assert_eq!(
        transport.games_requests().len(),
        3,
        "retries stop at the attempt cap"
    );
    assert_eq!(transport.token_requests().len(), 1);
}

#[test]
fn oversized_bodies_are_rejected_before_parsing() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: vec![b'['; MAX_RESPONSE_BYTES + 1],
        },
    ]));
    let provider = provider(transport);

    let error = provider.details(HADES_IGDB_ID).unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert!(error.to_string().contains("byte limit"), "{error}");
}

#[test]
fn non_positive_ids_are_refused_without_a_request() {
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let provider = provider(transport.clone());

    for invalid in ["", "  ", "abc", "0", "-3", "12.5"] {
        assert!(provider.details(invalid).is_err(), "{invalid}");
    }
    assert_eq!(transport.calls(), 0);
}

#[test]
fn engine_round_trip_caches_and_reuses_the_details_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(dir.path()).unwrap();
    let game = engine
        .enroll_game("Hades", AdapterType::CustomDirectory)
        .unwrap();

    let transport = Arc::new(ScriptedTransport::new(vec![
        token_response(ACCESS_TOKEN, 5_184_000),
        json_response(200, hades_search_results()),
        json_response(200, hades_details()),
        // A second engine round trip searches again; only `details` is cached.
        json_response(200, hades_search_results()),
    ]));
    let provider = provider(transport.clone());

    let fetched = engine.fetch_metadata_with(&provider, &game.id).unwrap();
    assert_eq!(fetched.source, MetadataSource::Igdb);
    assert_eq!(fetched.source_id, HADES_IGDB_ID);
    assert_eq!(engine.cached_metadata(&game.id).unwrap().unwrap(), fetched);
    assert_eq!(
        transport.calls(),
        3,
        "one token exchange, one search, one details request"
    );

    let again = engine.fetch_metadata_with(&provider, &game.id).unwrap();
    assert_eq!(again, fetched);
    assert_eq!(transport.calls(), 4, "the repeated search is not cached");
    assert_eq!(transport.token_requests().len(), 1, "the token is reused");
    let details_requests = transport
        .games_requests()
        .iter()
        .filter(|request| body_text(request).starts_with("where id ="))
        .count();
    assert_eq!(details_requests, 1, "details are fetched once per IGDB id");
    assert_eq!(engine.cached_metadata(&game.id).unwrap().unwrap(), fetched);
}

#[test]
fn engine_without_credentials_reports_an_auth_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(dir.path()).unwrap();
    let game = engine
        .enroll_game("Hades", AdapterType::CustomDirectory)
        .unwrap();

    // No credentials at all: the registry refuses before any request is made.
    for error in [
        engine
            .fetch_metadata_with_all_creds(&game.id, MetadataSource::Igdb, MetadataCreds::default())
            .unwrap_err(),
        engine
            .fetch_metadata(&game.id, MetadataSource::Igdb)
            .unwrap_err(),
        provider_for(MetadataSource::Igdb, None).err().unwrap(),
    ] {
        assert!(matches!(error, Error::Auth(_)), "{error:?}");
        assert!(error.to_string().contains("igdb"), "{error}");
    }
    assert!(engine.cached_metadata(&game.id).unwrap().is_none());
}

#[test]
fn credentials_debug_output_redacts_both_secrets() {
    let rendered = format!("{:?}", credentials());
    for secret in [CLIENT_ID, CLIENT_SECRET] {
        assert!(
            !rendered.contains(secret),
            "{secret} leaked into {rendered}"
        );
    }
    assert!(rendered.contains("IgdbCredentials"), "{rendered}");
    assert!(rendered.contains("[redacted]"), "{rendered}");
}

/// Assert that no credential value survives into a user-visible error string.
fn assert_no_secret_leak(error: &Error) {
    let rendered = error.to_string();
    for secret in [CLIENT_ID, CLIENT_SECRET, ACCESS_TOKEN, REFRESHED_TOKEN] {
        assert!(
            !rendered.contains(secret),
            "{secret} leaked into {rendered}"
        );
    }
}
