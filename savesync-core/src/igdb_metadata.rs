//! IGDB metadata provider.
//!
//! IGDB (<https://api-docs.igdb.com/>) is a Twitch-owned game catalogue. Every
//! request needs a Twitch application identity: a `Client-ID` header plus a
//! bearer token minted from the app's client id and client secret through the
//! client-credentials grant. There is no end-user account, so the credentials
//! the caller supplies are the developer's own.
//!
//! # Endpoints
//!
//! - **token** — `POST https://id.twitch.tv/oauth2/token` with
//!   `client_id`, `client_secret`, and `grant_type=client_credentials`, which
//!   answers `{access_token, expires_in, token_type}`.
//! - **search** — `POST https://api.igdb.com/v4/games` with an Apicalypse body
//!   `search "<title>"; fields …; limit 10;`.
//! - **details** — the same endpoint with `where id = <id>; fields …;`.
//!
//! The query language is a body, not JSON, so the field list is a constant
//! ([`FIELDS`]) shared by both queries.
//!
//! # Credential flow
//!
//! Credentials are supplied by the caller ([`IgdbCredentials`] is built from the
//! desktop shell's OS-keyring entries) and owned by one provider instance. Core
//! never reads a keyring, never writes them to disk, and never renders them in
//! `Debug` output or an error message.
//!
//! The token request carries both values in the query string (the documented
//! form), so [`crate::provider_runtime::redact_secrets`] covers `client_id=` and
//! `client_secret=`, [`crate::http::HttpRequest`]'s `Debug` redacts the URL and
//! the `Authorization` header, and no error here ever embeds a request URL. The
//! access token is only ever held in memory, in this provider.
//!
//! Tokens are cached with their advertised lifetime and refreshed once, on
//! demand, when the API answers `401` before expiry. Refreshes are
//! single-flight: one mutex serializes them (mirroring
//! [`crate::oauth::OAuthTokenManager`]), so concurrent lookups collapse into one
//! network exchange instead of a stampede.
//!
//! # Field mapping
//!
//! | IGDB | [`GameMetadata`] |
//! | --- | --- |
//! | `name` | `name` |
//! | `id` | `source_id` |
//! | `first_release_date` (unix seconds) | `release_date` (ISO date) + `release_year` |
//! | `genres[].name` | `genres` |
//! | `involved_companies` where `developer` | `developers` (`company.name`) |
//! | `involved_companies` where `publisher` | `publishers` (`company.name`) |
//! | `summary` | `description_long` |
//! | `themes[].name` | `themes` |
//! | `game_modes[].name` | `players` (joined with `", "`) |
//! | `age_ratings` (`category` + `rating`) | `age_rating` (e.g. `"PEGI 12"`) |
//! | `franchise.name` | `franchise` |
//!
//! `description_short` stays `None` on purpose: IGDB publishes no short
//! description field, and `storyline` is another full-length prose blurb, so
//! filling the field with it would invent a distinction the catalogue does not
//! make. `platform_hint` stays `None` for the same reason — the search field
//! list carries no platform data.
//!
//! # Bounds
//!
//! A response body is buffered by the transport under
//! [`MAX_RESPONSE_BYTES`]; the provider repeats the check so an injected
//! transport in tests obeys the same rule. Search results are capped at
//! [`MAX_SEARCH_RESULTS`], the per-provider details cache at
//! [`MAX_DETAILS_CACHE_ENTRIES`], and the cached token lifetime at
//! [`MAX_TOKEN_LIFETIME`]. Nothing here touches save data, snapshots, or restore
//! state: metadata is optional enrichment.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::Datelike;
use serde::Deserialize;
use url::form_urlencoded;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, MAX_RESPONSE_BYTES, RequestBody,
    RetryPolicy, execute_with_retry_classified,
};
use crate::metadata::{GameMetadata, MetadataCandidate, MetadataProvider, MetadataSource};
use crate::provider_runtime::{
    ProviderFlavor, REDACTED, ensure_success, is_retryable_response, redact_error_text,
};

/// IGDB REST base URL.
pub const API_BASE: &str = "https://api.igdb.com/v4/";

/// The endpoint for both title search and game details.
pub const GAMES_ENDPOINT: &str = "https://api.igdb.com/v4/games";

/// Twitch client-credentials grant endpoint.
pub const TOKEN_ENDPOINT: &str = "https://id.twitch.tv/oauth2/token";

/// Fields requested for both the search and the details query.
///
/// `involved_companies.company.name` needs the explicit expansion because
/// `involved_companies.*` only expands the intermediate object's own columns.
pub const FIELDS: &str = "fields name,first_release_date,genres.name,involved_companies.*,\
involved_companies.company.name,summary,themes.name,game_modes.name,age_ratings.*,franchise.name";

/// Upper bound on the candidates one search returns, and the Apicalypse `limit`.
///
/// IGDB rate-limits at four requests per second, so a narrow result set keeps a
/// later details fetch (and the IPC payload) small.
pub const MAX_SEARCH_RESULTS: usize = 10;

/// Upper bound on the in-process details cache. When it is full it is cleared;
/// a metadata entry is cheap to re-fetch and nothing depends on it surviving.
const MAX_DETAILS_CACHE_ENTRIES: usize = 256;

/// Refresh the access token this many seconds before it expires.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// Lifetime assumed when the token response omits `expires_in`.
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// Ceiling for a cached token lifetime (60 days).
///
/// Twitch app tokens are long-lived, but an implausible `expires_in` must not
/// overflow the monotonic clock or keep a stale token cached indefinitely.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 24 * 60 * 60);

/// Credentials for the IGDB API: one Twitch application.
///
/// Supplied by the caller and cloned into one provider. `Debug` redacts both
/// values so a panic message or trace cannot leak them.
#[derive(Clone)]
pub struct IgdbCredentials {
    client_id: String,
    client_secret: String,
}

impl IgdbCredentials {
    /// Twitch application credentials from the Twitch developer console.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// Reject an incomplete credential set before any request is made.
    fn validate(&self) -> Result<()> {
        let missing = [&self.client_id, &self.client_secret]
            .iter()
            .any(|value| value.trim().is_empty());
        if missing {
            // Nothing was sent and nothing was rejected: a missing Twitch
            // application is a configuration gap, not a failed authentication.
            return Err(Error::Config(
                "IGDB needs both a Twitch client id and client secret".to_string(),
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for IgdbCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IgdbCredentials")
            .field("client_id", &REDACTED)
            .field("client_secret", &REDACTED)
            .finish()
    }
}

/// Recovery from mutex poisoning.
///
/// Every lock here guards a cache or serializes a refresh, never a state
/// machine: a panic in an unrelated thread leaves the cache merely incomplete,
/// so the next caller continues instead of turning the provider permanently
/// unusable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Named {
    #[serde(default)]
    name: Option<String>,
}

impl Named {
    fn value(&self) -> Option<String> {
        self.name.as_deref().and_then(non_empty)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InvolvedCompany {
    #[serde(default)]
    developer: bool,
    #[serde(default)]
    publisher: bool,
    #[serde(default)]
    company: Option<Named>,
}

/// One `{category, rating}` enum pair from `age_ratings.*`.
#[derive(Debug, Clone, Default, Deserialize)]
struct AgeRating {
    #[serde(default)]
    category: Option<i64>,
    #[serde(default)]
    rating: Option<i64>,
}

/// One `games` entry, narrowed to the fields [`FIELDS`] requests.
#[derive(Debug, Clone, Default, Deserialize)]
struct IgdbGame {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    first_release_date: Option<i64>,
    #[serde(default)]
    genres: Vec<Named>,
    #[serde(default)]
    involved_companies: Vec<InvolvedCompany>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    themes: Vec<Named>,
    #[serde(default)]
    game_modes: Vec<Named>,
    #[serde(default)]
    age_ratings: Vec<AgeRating>,
    #[serde(default)]
    franchise: Option<Named>,
}

impl IgdbGame {
    fn id_string(&self) -> Option<String> {
        self.id.filter(|id| *id > 0).map(|id| id.to_string())
    }

    /// Released date as an ISO date plus its year, when IGDB reports one.
    fn release(&self) -> Option<(String, i32)> {
        parse_release_seconds(self.first_release_date?)
    }

    /// Map one catalogue entry onto the normalized model.
    fn into_metadata(self, fallback_id: &str) -> Result<GameMetadata> {
        let name = self
            .name
            .as_deref()
            .and_then(non_empty)
            .ok_or_else(|| Error::NotFound(format!("IGDB game {fallback_id}")))?;
        let source_id = self.id_string().unwrap_or_else(|| fallback_id.to_string());
        let release = self.release();

        Ok(GameMetadata {
            source: MetadataSource::Igdb,
            source_id,
            name,
            release_year: release.as_ref().map(|(_, year)| *year),
            release_date: release.map(|(date, _)| date),
            genres: names(&self.genres),
            developers: companies(&self.involved_companies, |entry| entry.developer),
            publishers: companies(&self.involved_companies, |entry| entry.publisher),
            // IGDB has no short description; see the module docs.
            description_short: None,
            description_long: self.summary.as_deref().and_then(non_empty),
            players: joined_names(&self.game_modes),
            age_rating: age_rating(&self.age_ratings),
            franchise: self.franchise.as_ref().and_then(Named::value),
            themes: names(&self.themes),
            fetched_at: crate::store::now_iso(),
        })
    }
}

/// IGDB metadata over a mockable [`crate::http::HttpTransport`].
///
/// Holds caller-supplied credentials, the cached access token, and a small
/// details cache; does not derive `Debug` because all three would render
/// uselessly (and the first is a secret).
pub struct IgdbProvider {
    transport: Arc<dyn HttpTransport>,
    credentials: IgdbCredentials,
    policy: RetryPolicy,
    cached: Mutex<Option<CachedToken>>,
    /// Serializes token exchanges (single-flight per provider).
    token_gate: Mutex<()>,
    details: Mutex<HashMap<String, GameMetadata>>,
    /// Serializes details requests (single-flight per provider).
    details_gate: Mutex<()>,
}

impl IgdbProvider {
    /// Build a provider, rejecting an incomplete credential set before any
    /// request is made.
    pub fn new(transport: Arc<dyn HttpTransport>, credentials: IgdbCredentials) -> Result<Self> {
        credentials.validate()?;
        Ok(Self {
            transport,
            credentials,
            policy: RetryPolicy::default(),
            cached: Mutex::new(None),
            token_gate: Mutex::new(()),
            details: Mutex::new(HashMap::new()),
            details_gate: Mutex::new(()),
        })
    }

    /// Override the retry policy (tests use a short one so retries do not
    /// sleep for the production backoff).
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Cached access token, when it is valid beyond the expiry skew.
    fn cached_access_token(&self) -> Option<String> {
        let cached = lock(&self.cached);
        cached
            .as_ref()
            .filter(|cached| cached.expires_at > Instant::now() + EXPIRY_SKEW)
            .map(|cached| cached.access_token.clone())
    }

    /// Exchange the application credentials for an access token.
    ///
    /// The caller holds [`Self::token_gate`], which is what makes concurrent
    /// lookups collapse into one exchange.
    fn request_token_locked(&self) -> Result<String> {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("client_id", &self.credentials.client_id)
            .append_pair("client_secret", &self.credentials.client_secret)
            .append_pair("grant_type", "client_credentials");
        let url = format!("{TOKEN_ENDPOINT}?{}", query.finish());
        let request = HttpRequest::new(HttpMethod::Post, url).header("Accept", "application/json");

        let response = execute_with_retry_classified(
            self.transport.as_ref(),
            &self.policy,
            || request.clone(),
            // Throttling and `5xx` are retried; a rejected grant is not.
            |response| is_retryable_response(response, ProviderFlavor::Igdb),
        )?;

        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(oversize("IGDB token response"));
        }
        if !response.is_success() {
            // Twitch answers a bad client id/secret with `400`; every refusal
            // of the grant is a credential or configuration problem.
            return Err(Error::Auth(format!(
                "IGDB token request failed: {}",
                error_text(&response.body)
            )));
        }

        let token: TokenResponse = serde_json::from_slice(&response.body)
            .map_err(|_| Error::Auth("IGDB returned an unreadable token response".to_string()))?;
        let access_token = token.access_token.trim().to_string();
        if access_token.is_empty() {
            return Err(Error::Auth(
                "IGDB token response is missing access_token".to_string(),
            ));
        }
        if let Some(kind) = token.token_type.as_deref()
            && !kind.trim().eq_ignore_ascii_case("bearer")
        {
            return Err(Error::Auth("IGDB issued a non-bearer token".to_string()));
        }

        let lifetime = token
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOKEN_LIFETIME)
            .min(MAX_TOKEN_LIFETIME);
        *lock(&self.cached) = Some(CachedToken {
            access_token: access_token.clone(),
            expires_at: Instant::now() + lifetime,
        });
        Ok(access_token)
    }

    /// A usable token, exchanging credentials only when the cache cannot answer.
    fn access_token(&self) -> Result<String> {
        if let Some(token) = self.cached_access_token() {
            return Ok(token);
        }
        let _guard = lock(&self.token_gate);
        // Another caller may have refreshed while this one waited.
        if let Some(token) = self.cached_access_token() {
            return Ok(token);
        }
        self.request_token_locked()
    }

    /// Force a fresh token; called once after an API request answered `401`.
    fn refresh_token(&self) -> Result<String> {
        let _guard = lock(&self.token_gate);
        self.request_token_locked()
    }

    /// POST one Apicalypse query with an authenticated request.
    fn post_games(&self, query: &str, token: &str) -> Result<HttpResponse> {
        let request = HttpRequest::new(HttpMethod::Post, GAMES_ENDPOINT)
            .header("Client-ID", &self.credentials.client_id)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .header("Content-Type", "text/plain")
            .body(RequestBody::bytes(query.as_bytes().to_vec()));
        execute_with_retry_classified(
            self.transport.as_ref(),
            &self.policy,
            || request.clone(),
            |response| is_retryable_response(response, ProviderFlavor::Igdb),
        )
    }

    /// Run one `games` query and return the entries it carries.
    fn request_games(&self, query: &str) -> Result<Vec<IgdbGame>> {
        let mut token = self.access_token()?;
        let mut response = self.post_games(query, &token)?;
        if response.status == 401 {
            // A token can be revoked before its advertised expiry. Refresh once
            // and replay the query; a second `401` is a real credential failure.
            token = self.refresh_token()?;
            response = self.post_games(query, &token)?;
        }

        if response.status == 404 {
            return Err(Error::NotFound("IGDB game".to_string()));
        }
        let response = ensure_success(response, ProviderFlavor::Igdb)?;
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(oversize("IGDB response"));
        }
        parse_games(&response.body)
    }

    /// Map upstream entries onto candidates, best match first.
    ///
    /// IGDB orders a `search` result by relevance; ranking through
    /// [`crate::metadata::best_candidate`] still keeps the caller's stored title
    /// the authority for which entry is chosen.
    fn map_candidates(&self, title: &str, games: &[IgdbGame]) -> Vec<MetadataCandidate> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for game in games {
            let Some(source_id) = game.id_string() else {
                continue;
            };
            let Some(name) = game.name.as_deref().and_then(non_empty) else {
                continue;
            };
            if !seen.insert(source_id.clone()) {
                continue;
            }
            candidates.push(MetadataCandidate {
                source_id,
                name,
                release_year: game.release().map(|(_, year)| year),
                // The search field list carries no platform data; a hint would
                // be invented, so it stays `None`.
                platform_hint: None,
            });
            if candidates.len() >= MAX_SEARCH_RESULTS {
                break;
            }
        }

        if let Some(best) = crate::metadata::best_candidate(title, &candidates) {
            let best_id = best.source_id.clone();
            candidates.sort_by_key(|candidate| candidate.source_id != best_id);
        }
        candidates
    }
}

impl MetadataProvider for IgdbProvider {
    fn source(&self) -> MetadataSource {
        MetadataSource::Igdb
    }

    /// Title search through the Apicalypse `search` operator.
    ///
    /// A no-match answer (an empty array or a `404`) is an empty candidate list,
    /// exactly like the other providers; rejected credentials stay an
    /// [`Error::Auth`].
    fn search(&self, title: &str) -> Result<Vec<MetadataCandidate>> {
        let trimmed = title.trim();
        if crate::metadata::normalize_title(trimmed).is_empty() {
            return Ok(Vec::new());
        }

        match self.request_games(&search_query(trimmed)) {
            Ok(games) => Ok(self.map_candidates(trimmed, &games)),
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    fn details(&self, source_id: &str) -> Result<GameMetadata> {
        let id = parse_game_id(source_id)?;
        let key = id.to_string();

        if let Some(hit) = lock(&self.details).get(&key).cloned() {
            return Ok(hit);
        }
        let _gate = lock(&self.details_gate);
        if let Some(hit) = lock(&self.details).get(&key).cloned() {
            return Ok(hit);
        }

        let game = self
            .request_games(&details_query(id))?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound(format!("IGDB game {key}")))?;
        let metadata = game.into_metadata(&key)?;

        let mut cache = lock(&self.details);
        if cache.len() >= MAX_DETAILS_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, metadata.clone());
        Ok(metadata)
    }
}

/// Build the Apicalypse search body for `title`.
///
/// Apicalypse strings are double-quoted and the language documents no escape
/// for a quote, a backslash, or a semicolon inside one, so the search term is
/// scrubbed before interpolation:
///
/// * `"` and control characters become a space, so a quote can never close the
///   string early and adjacent words stay separated.
/// * `;` and `\` are dropped, so a crafted title cannot append an operator
///   (`"; limit 500; search "x`) or introduce an undocumented escape. Neither
///   character carries a word boundary, so removing it loses nothing.
///
/// The remaining title text — including marks such as `™` and punctuation such
/// as `:` — is kept verbatim; only whitespace at the ends is trimmed. A title
/// can therefore never terminate the query, and the body always holds exactly
/// three structural semicolons: `search …;`, `fields …;` and `limit …;`.
fn search_query(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .filter_map(|ch| match ch {
            ';' | '\\' => None,
            '"' => Some(' '),
            ch if ch.is_control() => Some(' '),
            ch => Some(ch),
        })
        .collect();
    format!(
        "search \"{}\"; {FIELDS}; limit {MAX_SEARCH_RESULTS};",
        sanitized.trim()
    )
}

/// Build the Apicalypse details body for a provider-supplied id.
fn details_query(id: u64) -> String {
    format!("where id = {id}; {FIELDS}; limit 1;")
}

/// Parse a provider-supplied game id.
///
/// The id comes from a candidate or a caller, never from a path, but it still
/// has to be a positive integer before it is interpolated into a query.
fn parse_game_id(source_id: &str) -> Result<u64> {
    match source_id.trim().parse::<u64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(Error::InvalidState(
            "IGDB game id must be a positive integer".to_string(),
        )),
    }
}

/// Parse a `games` response body into entries.
///
/// IGDB answers a JSON array; an empty body is a miss, and an unreadable body is
/// a protocol failure rather than a "no match".
fn parse_games(body: &[u8]) -> Result<Vec<IgdbGame>> {
    if body.is_empty() {
        return Err(Error::NotFound("IGDB game".to_string()));
    }
    serde_json::from_slice(body).map_err(|_| Error::Provider {
        status: 200,
        message: "IGDB returned an unreadable response".to_string(),
    })
}

fn oversize(what: &str) -> Error {
    Error::Network(format!(
        "{what} exceeded the {MAX_RESPONSE_BYTES} byte limit"
    ))
}

/// Scrubber-routed, truncated rendering of a token-endpoint error body.
///
/// Twitch may echo the offending request back, so the text passes through
/// [`redact_error_text`] before it can reach an [`Error`].
fn error_text(body: &[u8]) -> String {
    let text = redact_error_text(&String::from_utf8_lossy(body));
    if text.is_empty() {
        "request failed".to_string()
    } else {
        text
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn names(entries: &[Named]) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for entry in entries {
        if let Some(value) = entry.value()
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    values
}

fn joined_names(entries: &[Named]) -> Option<String> {
    let values = names(entries);
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

fn companies(
    entries: &[InvolvedCompany],
    select: impl Fn(&InvolvedCompany) -> bool,
) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for entry in entries.iter().filter(|entry| select(entry)) {
        if let Some(value) = entry.company.as_ref().and_then(Named::value)
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    values
}

/// Render the age rating from IGDB's enum pair.
///
/// IGDB publishes `category`/`rating` numbers, not text. PEGI (category 2) is
/// the primary European label and ESRB (category 1) the North American one, so
/// PEGI wins when both exist. An unknown category is ignored instead of being
/// surfaced as a bare number pair the user cannot interpret.
fn age_rating(ratings: &[AgeRating]) -> Option<String> {
    fn label_for(entry: &AgeRating) -> Option<String> {
        let (category, rating) = (entry.category?, entry.rating?);
        let (label, value) = match (category, rating) {
            (2, 1) => ("PEGI", "3"),
            (2, 2) => ("PEGI", "7"),
            (2, 3) => ("PEGI", "12"),
            (2, 4) => ("PEGI", "16"),
            (2, 5) => ("PEGI", "18"),
            (1, 1) => ("ESRB", "RP"),
            (1, 2) => ("ESRB", "EC"),
            (1, 3) => ("ESRB", "E"),
            (1, 4) => ("ESRB", "E10+"),
            (1, 5) => ("ESRB", "T"),
            (1, 6) => ("ESRB", "M"),
            (1, 7) => ("ESRB", "AO"),
            _ => return None,
        };
        Some(format!("{label} {value}"))
    }

    let by_category = |wanted: i64| {
        ratings
            .iter()
            .find(|entry| entry.category == Some(wanted))
            .and_then(label_for)
    };
    by_category(2).or_else(|| by_category(1))
}

/// Convert IGDB's unix-seconds release timestamp into an ISO date and year.
///
/// A missing, zero, or implausible timestamp yields `None`: the normalized model
/// carries no invented date, and `1970-01-01` is a data error, not a release.
fn parse_release_seconds(seconds: i64) -> Option<(String, i32)> {
    if seconds <= 0 {
        return None;
    }
    let date = chrono::DateTime::from_timestamp(seconds, 0)?;
    let year = date.year();
    if !(1900..=2200).contains(&year) {
        return None;
    }
    Some((date.format("%Y-%m-%d").to_string(), year))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_unix_release_timestamps_to_iso_dates() {
        assert_eq!(
            parse_release_seconds(1_600_300_800),
            Some(("2020-09-17".to_string(), 2020))
        );
        // Leap day, and an early timestamp: `div_euclid`-free chrono handles it.
        assert_eq!(
            parse_release_seconds(1_582_934_400),
            Some(("2020-02-29".to_string(), 2020))
        );
        assert_eq!(parse_release_seconds(0), None);
        assert_eq!(parse_release_seconds(-86400), None);
        assert_eq!(parse_release_seconds(i64::MAX), None);
    }

    #[test]
    fn rejects_non_numeric_game_ids() {
        for invalid in ["", "  ", "abc", "0", "-1", "12.5"] {
            assert!(parse_game_id(invalid).is_err(), "{invalid}");
        }
        assert_eq!(parse_game_id(" 113112 ").unwrap(), 113_112);
    }

    #[test]
    fn search_bodies_drop_query_metacharacters() {
        let body = search_query("Hades™: \"Ultimate\"; Edition\\");
        assert!(
            body.starts_with("search \"Hades™:  Ultimate  Edition\";"),
            "{body}"
        );
        assert_eq!(
            body.matches('"').count(),
            2,
            "a quote in the title must not terminate the query: {body}"
        );
        // The body always carries three structural semicolons — `search …;`,
        // `fields …;`, `limit …;` — so a title semicolon would raise the count.
        assert_eq!(
            body.matches(';').count(),
            3,
            "a semicolon in the title must not append operators: {body}"
        );
        assert!(
            body.ends_with(&format!("limit {MAX_SEARCH_RESULTS};")),
            "{body}"
        );
        assert!(body.contains(FIELDS), "{body}");

        assert_eq!(
            details_query(113_112),
            format!("where id = 113112; {FIELDS}; limit 1;")
        );
    }

    #[test]
    fn age_ratings_prefer_pegi_and_ignore_unknown_categories() {
        let pegi = AgeRating {
            category: Some(2),
            rating: Some(3),
        };
        let esrb = AgeRating {
            category: Some(1),
            rating: Some(5),
        };
        let unknown = AgeRating {
            category: Some(7),
            rating: Some(2),
        };

        assert_eq!(
            age_rating(&[esrb.clone(), pegi.clone()]).as_deref(),
            Some("PEGI 12")
        );
        assert_eq!(age_rating(&[esrb]).as_deref(), Some("ESRB T"));
        assert_eq!(age_rating(&[unknown]), None);
        assert_eq!(age_rating(&[]), None);
    }

    #[test]
    fn credentials_reject_an_incomplete_set() {
        assert!(matches!(
            IgdbCredentials::new("", "secret").validate(),
            Err(Error::Config(_))
        ));
        assert!(matches!(
            IgdbCredentials::new("id", "  ").validate(),
            Err(Error::Config(_))
        ));
        assert!(IgdbCredentials::new("id", "secret").validate().is_ok());
    }
}
