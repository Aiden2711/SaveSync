//! ScreenScraper metadata provider against a scripted transport.
//!
//! Everything runs offline. The provider is handed a [`HttpTransport`] that
//! replays `jeuInfos.php` JSON fixtures, so these tests exercise the credential
//! plumbing, the field mapping, and the error taxonomy without a network or a
//! real ScreenScraper account.
//!
//! The credential assertions are deliberately blunt: every request must carry
//! `devid`/`devpassword`/`softname` (plus `ssid`/`sspassword` when a user
//! account is attached), and no error string may contain those values.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use savesync_core::http::{
    HttpRequest, HttpResponse, HttpTransport, MAX_RESPONSE_BYTES, RetryPolicy,
};
use savesync_core::metadata::{MetadataProvider, MetadataSource};
use savesync_core::models::AdapterType;
use savesync_core::screenscraper_metadata::{ScreenScraperCredentials, ScreenScraperProvider};
use savesync_core::{Error, SaveSyncEngine};
use serde_json::{Value, json};

/// Real-looking ids keep the fixtures honest about the `jeu` shape.
const HADES_JEU_ID: &str = "3";

const DEV_ID: &str = "savesync-dev-id";
const DEV_PASSWORD: &str = "dev-pw-9f2c1a";
const SS_ID: &str = "player-ssid";
const SS_PASSWORD: &str = "ss-pw-77bd30";

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

fn envelope(jeu: Value) -> HttpResponse {
    json_response(200, json!({"response": {"jeu": jeu}}))
}

fn error_envelope(message: &str) -> HttpResponse {
    json_response(200, json!({"response": {"erreur": message}}))
}

fn credentials() -> ScreenScraperCredentials {
    ScreenScraperCredentials::developer(DEV_ID, DEV_PASSWORD).with_user(SS_ID, SS_PASSWORD)
}

fn provider(transport: Arc<ScriptedTransport>) -> ScreenScraperProvider {
    ScreenScraperProvider::new(transport, credentials())
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

/// The search answer `jeuInfos.php?romnom=…` returns for a name match.
fn hades_search_result() -> Value {
    json!({
        "id": HADES_JEU_ID,
        "noms": [
            {"region": "us", "text": "Hades (US)"},
            {"region": "eu", "text": "Hades"}
        ],
        "systeme": {"id": "75", "text": "Windows"},
        "dates": [
            {"region": "fr", "text": "2020-09-17"},
            {"region": "eu", "text": "2020-09-17"}
        ]
    })
}

/// The full detail payload `jeuInfos.php?jeuid=…` returns.
fn hades_details() -> Value {
    json!({
        "id": HADES_JEU_ID,
        "noms": [
            {"region": "us", "text": "Hades (US)"},
            {"region": "eu", "text": "Hades"}
        ],
        "systeme": {"id": "75", "text": "Windows"},
        "dates": [
            {"region": "fr", "text": "2020-09-17"},
            {"region": "eu", "text": "2020-09-17"}
        ],
        // `genres` is absent: the localized `genres_en` array has to win.
        "genres_en": [
            {"id": "10", "noms": [{"langue": "fr", "text": "Action"}, {"langue": "en", "text": "Action"}]},
            {"id": "11", "noms": [{"langue": "en", "text": "Roguelike"}]}
        ],
        "editeur": {"id": "2", "text": "Supergiant Games"},
        "developpeur": {"id": "1", "text": "Supergiant Games"},
        "joueurs": {"text": "1"},
        "themes": [
            {"id": "20", "noms": [{"langue": "en", "text": "Action-RPG"}]}
        ],
        "familles": [
            {"id": "30", "noms": [{"langue": "en", "text": "Hades"}]}
        ],
        "classifications": [
            {"type": "ESRB", "text": "T"},
            {"type": "PEGI", "text": "12"}
        ],
        "resume": {"text": "Defy the god of the dead."},
        "synopsis_en": {"text": "A rogue-like dungeon crawler."},
        "synopsis_fr": {"text": "Un dungeon crawler rogue-like."}
    })
}

#[test]
fn every_request_carries_credentials_and_search_matches_the_rom_name() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        envelope(hades_search_result()),
    ]));
    let provider = provider(transport.clone());

    let candidates = provider.search("Hades").unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].source_id, HADES_JEU_ID);
    assert_eq!(candidates[0].name, "Hades", "the EU name wins");
    assert_eq!(candidates[0].release_year, Some(2020));
    assert_eq!(candidates[0].platform_hint.as_deref(), Some("Windows"));

    assert_eq!(transport.calls(), 1);
    let request = &transport.requests()[0];
    assert!(
        request
            .url
            .starts_with("https://api.screenscraper.fr/api2/jeuInfos.php?")
    );
    for (key, value) in [
        ("devid", DEV_ID),
        ("devpassword", DEV_PASSWORD),
        ("softname", "savesync"),
        ("ssid", SS_ID),
        ("sspassword", SS_PASSWORD),
        ("output", "json"),
        ("romnom", "Hades"),
    ] {
        assert!(
            request.url.contains(&format!("{key}={value}")),
            "{key} missing from {url}",
            url = request.url
        );
    }
    assert_eq!(request.header_value("Accept"), Some("application/json"));
}

#[test]
fn a_name_miss_is_an_empty_result_not_an_error() {
    let transport = Arc::new(ScriptedTransport::new(vec![error_envelope(
        "Rom non trouvée",
    )]));
    let provider = provider(transport.clone());

    assert!(provider.search("Celeste").unwrap().is_empty());
    assert_eq!(transport.calls(), 1);
}

#[test]
fn an_empty_title_never_sends_a_request() {
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let provider = provider(transport.clone());

    assert!(provider.search("   ").unwrap().is_empty());
    assert_eq!(transport.calls(), 0);
}

#[test]
fn details_map_the_jeu_object_onto_the_normalized_model() {
    let transport = Arc::new(ScriptedTransport::new(vec![envelope(hades_details())]));
    let provider = provider(transport.clone());

    let metadata = provider.details(HADES_JEU_ID).unwrap();
    assert_eq!(metadata.source, MetadataSource::ScreenScraper);
    assert_eq!(metadata.source_id, HADES_JEU_ID);
    assert_eq!(metadata.name, "Hades");
    assert_eq!(metadata.release_date.as_deref(), Some("2020-09-17"));
    assert_eq!(metadata.release_year, Some(2020));
    assert_eq!(metadata.genres, vec!["Action", "Roguelike"]);
    assert_eq!(metadata.developers, vec!["Supergiant Games"]);
    assert_eq!(metadata.publishers, vec!["Supergiant Games"]);
    assert_eq!(
        metadata.description_short.as_deref(),
        Some("Defy the god of the dead.")
    );
    assert_eq!(
        metadata.description_long.as_deref(),
        Some("A rogue-like dungeon crawler."),
        "synopsis_en must beat the French original"
    );
    assert_eq!(metadata.players.as_deref(), Some("1"));
    assert_eq!(metadata.age_rating.as_deref(), Some("PEGI 12"));
    assert_eq!(metadata.franchise.as_deref(), Some("Hades"));
    assert_eq!(metadata.themes, vec!["Action-RPG"]);
    assert!(!metadata.fetched_at.is_empty());

    let request = &transport.requests()[0];
    assert!(
        request.url.contains(&format!("jeuid={HADES_JEU_ID}")),
        "{}",
        request.url
    );
    assert!(request.url.contains(&format!("devid={DEV_ID}")));

    // Details are cached per jeu id: a second lookup is served locally.
    assert_eq!(provider.details(HADES_JEU_ID).unwrap(), metadata);
    assert_eq!(transport.calls(), 1);
}

#[test]
fn rejected_credentials_are_auth_errors_that_never_echo_the_secrets() {
    // A `403` whose body echoes the request query string is the worst case for
    // a leak: the scrubber has to remove every credential value.
    let transport = Arc::new(ScriptedTransport::new(vec![HttpResponse {
        status: 403,
        headers: Vec::new(),
        body: format!(
            "Erreur de login : identifiants incorrects \
             (devid={DEV_ID}&devpassword={DEV_PASSWORD}&softname=savesync&ssid={SS_ID}&sspassword={SS_PASSWORD})"
        )
        .into_bytes(),
    }]));
    let provider = provider(transport.clone());

    let error = provider.details(HADES_JEU_ID).unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error:?}");
    assert_eq!(
        transport.calls(),
        1,
        "a credential rejection is never retried"
    );
    assert_no_secret_leak(&error);
}

#[test]
fn a_200_credential_payload_is_an_auth_error() {
    let transport = Arc::new(ScriptedTransport::new(vec![error_envelope(
        "Erreur de login : identifiants incorrects devpassword=dev-pw-9f2c1a",
    )]));
    let provider = provider(transport.clone());

    let error = provider.search("Hades").unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error:?}");
    assert_no_secret_leak(&error);
    assert_eq!(
        transport.calls(),
        1,
        "an API-level rejection is never retried"
    );
}

#[test]
fn throttling_is_retried_to_the_attempt_cap_then_surfaced() {
    let throttled = || HttpResponse {
        status: 429,
        // `Retry-After` is honored and capped by the policy, so the test does
        // not sleep for the production backoff.
        headers: vec![("Retry-After".to_string(), "0".to_string())],
        body: Vec::new(),
    };
    let transport = Arc::new(ScriptedTransport::new(vec![
        throttled(),
        throttled(),
        throttled(),
    ]));
    let provider = provider(transport.clone());

    let error = provider.details(HADES_JEU_ID).unwrap_err();
    match error {
        Error::Provider { status, .. } => assert_eq!(status, 429),
        other => panic!("expected a provider error, got {other:?}"),
    }
    assert_eq!(transport.calls(), 3, "retries stop at the attempt cap");
}

#[test]
fn quota_exhaustion_inside_a_200_payload_is_retried_then_surfaced() {
    let quota = || error_envelope("Quota dépassé pour votre compte");
    let transport = Arc::new(ScriptedTransport::new(vec![quota(), quota(), quota()]));
    let provider = provider(transport.clone());

    let error = provider.details(HADES_JEU_ID).unwrap_err();
    match error {
        Error::Provider { status, message } => {
            assert_eq!(status, 200, "the quota signal arrived in the payload");
            assert!(message.to_lowercase().contains("quota"), "{message}");
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
    assert_eq!(
        transport.calls(),
        3,
        "the 200 quota body was retried to the cap"
    );
}

#[test]
fn oversized_bodies_are_rejected_before_parsing() {
    let transport = Arc::new(ScriptedTransport::new(vec![HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: vec![b'{'; MAX_RESPONSE_BYTES + 1],
    }]));
    let provider = provider(transport);

    let error = provider.details(HADES_JEU_ID).unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert!(error.to_string().contains("byte limit"), "{error}");
}

#[test]
fn non_numeric_ids_are_refused_without_a_request() {
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let provider = provider(transport.clone());

    for invalid in ["", "  ", "abc", "0", "-3"] {
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
        envelope(hades_search_result()),
        envelope(hades_details()),
        // A second engine round trip searches again; only `details` is cached.
        envelope(hades_search_result()),
    ]));
    let provider = provider(transport.clone());

    let fetched = engine.fetch_metadata_with(&provider, &game.id).unwrap();
    assert_eq!(fetched.source, MetadataSource::ScreenScraper);
    assert_eq!(fetched.source_id, HADES_JEU_ID);
    assert_eq!(engine.cached_metadata(&game.id).unwrap().unwrap(), fetched);
    assert_eq!(transport.calls(), 2, "one search + one details request");

    // The provider's details cache is keyed by jeu id, so repeating the whole
    // fetch costs one search and no second details request.
    let again = engine.fetch_metadata_with(&provider, &game.id).unwrap();
    assert_eq!(again, fetched);
    assert_eq!(
        transport.calls(),
        3,
        "the repeated search is not cached, the details are"
    );
    assert!(
        transport.requests()[2].url.contains("romnom=Hades"),
        "the third request is the search, not a details lookup"
    );
    let details_requests = transport
        .requests()
        .iter()
        .filter(|request| request.url.contains("jeuid="))
        .count();
    assert_eq!(details_requests, 1, "details are fetched once per jeu id");
    assert_eq!(engine.cached_metadata(&game.id).unwrap().unwrap(), fetched);
}

#[test]
fn engine_without_credentials_reports_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SaveSyncEngine::open(dir.path()).unwrap();
    let game = engine
        .enroll_game("Hades", AdapterType::CustomDirectory)
        .unwrap();

    // `fetch_metadata_with_creds(.., None)` builds the production provider, so
    // this is the offline path: validation fails before any request, which is a
    // missing configuration rather than a rejected credential set.
    let error = engine
        .fetch_metadata_with_creds(&game.id, MetadataSource::ScreenScraper, None)
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(!matches!(error, Error::Auth(_)), "{error:?}");
    assert!(engine.cached_metadata(&game.id).unwrap().is_none());
}

#[test]
fn credentials_debug_output_redacts_every_secret() {
    let rendered = format!("{:?}", credentials());
    for secret in [DEV_ID, DEV_PASSWORD, SS_ID, SS_PASSWORD] {
        assert!(
            !rendered.contains(secret),
            "{secret} leaked into {rendered}"
        );
    }
    assert!(rendered.contains("savesync"), "{rendered}");
}

/// Assert that no credential value survives into a user-visible error string.
fn assert_no_secret_leak(error: &Error) {
    let rendered = error.to_string();
    for secret in [DEV_ID, DEV_PASSWORD, SS_ID, SS_PASSWORD] {
        assert!(
            !rendered.contains(secret),
            "{secret} leaked into {rendered}"
        );
    }
}
