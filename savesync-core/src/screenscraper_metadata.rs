//! ScreenScraper metadata provider.
//!
//! ScreenScraper (<https://api.screenscraper.fr/api2/>) is a credentialed
//! catalogue. Every request carries application-level developer credentials
//! (`devid` + `devpassword`, issued by the ScreenScraper staff) and our
//! `softname`; an optional end-user account (`ssid` + `sspassword`) raises the
//! request quota. The API only speaks query parameters, so those values travel
//! in the URL; [`crate::provider_runtime::redact_secrets`] covers the four
//! keys, [`crate::http::HttpRequest`]'s `Debug` redacts them, and no error here
//! ever embeds a request URL.
//!
//! # Credential flow
//!
//! Credentials are supplied by the caller ([`ScreenScraperCredentials`] is
//! built from the desktop shell's OS-keyring entries) and are owned by one
//! provider instance. Core never reads a keyring, never writes them to disk,
//! and never renders them in `Debug` output or an error message.
//!
//! # Endpoints
//!
//! - **search** — `jeuInfos.php?romnom=<title>` (documented and verified),
//!   which answers with the `jeu` object for a name match. A
//!   `jeuRecherche.php` endpoint can be added later: both paths map through
//!   [`ScreenScraperProvider::map_candidates`], and the parser already accepts
//!   a `response.jeux` array as well as a single `response.jeu`.
//! - **details** — `jeuInfos.php?jeuid=<id>`.
//!
//! Both send `output=json`.
//!
//! # Bounds
//!
//! A response body is buffered by the transport under
//! [`MAX_RESPONSE_BYTES`]; the provider repeats the check so an injected
//! transport in tests obeys the same rule. Search results are capped at
//! [`MAX_SEARCH_RESULTS`] and the per-provider details cache at
//! [`MAX_DETAILS_CACHE_ENTRIES`]. Nothing here touches save data, snapshots, or
//! restore state: metadata is optional enrichment.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Deserialize;
use url::form_urlencoded;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, HttpTransport, MAX_RESPONSE_BYTES, RetryPolicy,
    execute_with_retry_classified,
};
use crate::metadata::{GameMetadata, MetadataCandidate, MetadataProvider, MetadataSource};
use crate::provider_runtime::{
    ProviderFlavor, REDACTED, ensure_success, has_quota_signature, is_retryable_response,
    redact_error_text,
};

/// Verified ScreenScraper API v2 base URL.
pub const API_BASE: &str = "https://api.screenscraper.fr/api2/";

/// The documented endpoint for both name search and game details.
pub const GAME_INFO_ENDPOINT: &str = "jeuInfos.php";

/// Client name sent as `softname` unless the caller overrides it.
pub const DEFAULT_SOFTNAME: &str = "savesync";

/// Upper bound on the candidates one search returns.
///
/// Every candidate costs a details request when picked and the list crosses the
/// IPC boundary in a later milestone, so the tail is capped.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Upper bound on the in-process details cache. When it is full it is cleared;
/// a metadata entry is cheap to re-fetch and nothing depends on it surviving.
const MAX_DETAILS_CACHE_ENTRIES: usize = 256;

/// Region preference for titles and release dates.
const REGION_PREFERENCE: [&str; 5] = ["eu", "us", "wor", "jp", "fr"];

/// Language preference for localized text: English first, then Spanish, then
/// the catalogue's French originals.
const LANGUAGE_PREFERENCE: [&str; 3] = ["en", "es", "fr"];

/// Credentials for the ScreenScraper API.
///
/// Supplied by the caller and cloned into one provider. `Debug` redacts every
/// secret value so a panic message or trace cannot leak them.
#[derive(Clone)]
pub struct ScreenScraperCredentials {
    devid: String,
    devpassword: String,
    softname: String,
    ssid: Option<String>,
    sspassword: Option<String>,
}

impl ScreenScraperCredentials {
    /// Application credentials issued by the ScreenScraper staff.
    pub fn developer(devid: impl Into<String>, devpassword: impl Into<String>) -> Self {
        Self {
            devid: devid.into(),
            devpassword: devpassword.into(),
            softname: DEFAULT_SOFTNAME.to_string(),
            ssid: None,
            sspassword: None,
        }
    }

    /// Override the client name sent as `softname`.
    pub fn with_softname(mut self, softname: impl Into<String>) -> Self {
        self.softname = softname.into();
        self
    }

    /// Attach an end-user account, which raises the request quota.
    ///
    /// Both values are required together: ScreenScraper rejects an `ssid`
    /// without its password.
    pub fn with_user(mut self, ssid: impl Into<String>, sspassword: impl Into<String>) -> Self {
        self.ssid = Some(ssid.into());
        self.sspassword = Some(sspassword.into());
        self
    }

    /// Reject an incomplete credential set before any request is made.
    fn validate(&self) -> Result<()> {
        let missing = [&self.devid, &self.devpassword, &self.softname]
            .iter()
            .any(|value| value.trim().is_empty());
        if missing {
            // Nothing was sent and nothing was rejected: this is a missing
            // application configuration, not an authentication failure, and it
            // must not be reported as "no metadata found" either.
            return Err(Error::Config(
                "ScreenScraper requires a complete application developer pair (dev id and dev password); \
                 add both before searching"
                    .to_string(),
            ));
        }
        // A user account is optional, but a half-specified one is a
        // configuration error: ScreenScraper rejects an `ssid` without its
        // password, and a blank half would silently drop the quota boost.
        let blank =
            |value: &Option<String>| value.as_deref().is_some_and(|text| text.trim().is_empty());
        if self.ssid.is_some() != self.sspassword.is_some()
            || blank(&self.ssid)
            || blank(&self.sspassword)
        {
            return Err(Error::Config(
                "ScreenScraper user credentials need both the username (ssid) and its password"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for ScreenScraperCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScreenScraperCredentials")
            .field("devid", &REDACTED)
            .field("devpassword", &REDACTED)
            .field("softname", &self.softname)
            .field("ssid", &self.ssid.as_ref().map(|_| REDACTED))
            .field("sspassword", &self.sspassword.as_ref().map(|_| REDACTED))
            .finish()
    }
}

/// Recovery from mutex poisoning.
///
/// Every lock here guards a cache, never a state machine: a panic in an
/// unrelated thread leaves the cache merely incomplete, so the next caller
/// continues instead of turning the whole provider permanently unusable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum IdValue {
    Integer(i64),
    Text(String),
}

impl IdValue {
    fn as_string(&self) -> String {
        match self {
            Self::Integer(value) => value.to_string(),
            Self::Text(value) => value.trim().to_string(),
        }
    }
}

/// One `{langue|region, text}` entry.
#[derive(Debug, Clone, Default, Deserialize)]
struct LocalizedText {
    #[serde(default)]
    langue: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

impl LocalizedText {
    fn key(&self) -> Option<&str> {
        self.langue.as_deref().or(self.region.as_deref())
    }

    fn matches(&self, target: &str) -> bool {
        self.key()
            .is_some_and(|key| key.eq_ignore_ascii_case(target))
    }

    fn value(&self) -> Option<String> {
        self.text.as_deref().and_then(non_empty)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct NameRef {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    noms: Vec<LocalizedText>,
}

/// A field ScreenScraper publishes either as a plain string, as a `{text}`
/// object, or as an array of localized entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FlexibleText {
    Plain(String),
    Entries(Vec<LocalizedText>),
    Named(NameRef),
}

/// A genre/theme/family entry: an id plus localized names.
#[derive(Debug, Clone, Default, Deserialize)]
struct NestedNames {
    #[serde(default)]
    noms: Vec<LocalizedText>,
    #[serde(default)]
    nom: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

impl NestedNames {
    fn value(&self) -> Option<String> {
        pick_localized(&self.noms, &LANGUAGE_PREFERENCE)
            .or_else(|| self.nom.as_deref().and_then(non_empty))
            .or_else(|| self.text.as_deref().and_then(non_empty))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Classification {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// The `jeu` object (details) or one `jeux` entry (search result).
#[derive(Debug, Clone, Default, Deserialize)]
struct Jeu {
    #[serde(default)]
    id: Option<IdValue>,
    #[serde(default)]
    nom: Option<String>,
    #[serde(default)]
    noms: Vec<LocalizedText>,
    #[serde(default)]
    systeme: Option<FlexibleText>,
    #[serde(default)]
    editeur: Option<FlexibleText>,
    #[serde(default)]
    developpeur: Option<FlexibleText>,
    #[serde(default)]
    joueurs: Option<FlexibleText>,
    #[serde(default)]
    dates: Vec<LocalizedText>,
    #[serde(default)]
    genres: Vec<NestedNames>,
    #[serde(default)]
    genres_en: Vec<NestedNames>,
    #[serde(default)]
    genres_es: Vec<NestedNames>,
    #[serde(default)]
    genres_fr: Vec<NestedNames>,
    #[serde(default)]
    themes: Vec<NestedNames>,
    #[serde(default)]
    familles: Vec<NestedNames>,
    #[serde(default)]
    classifications: Vec<Classification>,
    #[serde(default)]
    synopsis: Vec<LocalizedText>,
    #[serde(default)]
    synopsis_en: Option<FlexibleText>,
    #[serde(default)]
    synopsis_es: Option<FlexibleText>,
    #[serde(default)]
    synopsis_fr: Option<FlexibleText>,
    /// Best-effort short summary; absent on most catalogue entries.
    #[serde(default)]
    resume: Option<FlexibleText>,
}

impl Jeu {
    fn id_string(&self) -> Option<String> {
        self.id
            .as_ref()
            .map(IdValue::as_string)
            .filter(|id| !id.is_empty())
    }

    /// Preferred display title: `noms` by region, then the flat `nom`.
    fn preferred_name(&self) -> Option<String> {
        pick_localized(&self.noms, &REGION_PREFERENCE)
            .or_else(|| self.nom.as_deref().and_then(non_empty))
    }

    /// Preferred release date text (`date_fr`/`date_eu`/… by region).
    fn preferred_date(&self) -> Option<String> {
        pick_localized(&self.dates, &REGION_PREFERENCE)
    }

    fn release_year(&self) -> Option<i32> {
        self.preferred_date().as_deref().and_then(parse_year)
    }

    fn platform_hint(&self) -> Option<String> {
        text_of(&self.systeme)
    }

    /// Map one catalogue entry onto the normalized model.
    fn into_metadata(self, fallback_id: &str) -> Result<GameMetadata> {
        let name = self
            .preferred_name()
            .ok_or_else(|| Error::NotFound(format!("ScreenScraper game {fallback_id}")))?;
        let source_id = self.id_string().unwrap_or_else(|| fallback_id.to_string());
        let release_date = self.preferred_date();
        let release_year = release_date.as_deref().and_then(parse_year);

        Ok(GameMetadata {
            source: MetadataSource::ScreenScraper,
            source_id,
            name,
            release_year,
            release_date,
            genres: localized_genres(
                &self.genres,
                &self.genres_en,
                &self.genres_es,
                &self.genres_fr,
            ),
            developers: text_of(&self.developpeur).into_iter().collect(),
            publishers: text_of(&self.editeur).into_iter().collect(),
            description_short: text_of(&self.resume),
            description_long: text_of(&self.synopsis_en)
                .or_else(|| text_of(&self.synopsis_es))
                .or_else(|| text_of(&self.synopsis_fr))
                .or_else(|| pick_localized(&self.synopsis, &LANGUAGE_PREFERENCE)),
            players: text_of(&self.joueurs),
            age_rating: age_rating(&self.classifications),
            franchise: if self.familles.len() == 1 {
                self.familles[0].value()
            } else {
                None
            },
            themes: collect_names(&self.themes),
            fetched_at: crate::store::now_iso(),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    header: Header,
    #[serde(default)]
    response: Option<ResponseBody>,
    #[serde(default)]
    erreur: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Header {
    #[serde(default)]
    erreur: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponseBody {
    #[serde(default)]
    jeu: Option<Jeu>,
    /// Present when a search endpoint answers with several entries.
    #[serde(default)]
    jeux: Vec<Jeu>,
    #[serde(default)]
    erreur: Option<String>,
}

/// ScreenScraper metadata over a mockable [`crate::http::HttpTransport`].
///
/// Holds caller-supplied credentials plus a small details cache; does not derive
/// `Debug` because credentials and cache would both render uselessly.
pub struct ScreenScraperProvider {
    transport: Arc<dyn HttpTransport>,
    credentials: ScreenScraperCredentials,
    policy: RetryPolicy,
    details: Mutex<HashMap<String, GameMetadata>>,
    /// Serializes details requests (single-flight per provider).
    details_gate: Mutex<()>,
}

impl ScreenScraperProvider {
    /// Build a provider, rejecting an incomplete credential set before any
    /// request is made.
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        credentials: ScreenScraperCredentials,
    ) -> Result<Self> {
        credentials.validate()?;
        Ok(Self {
            transport,
            credentials,
            policy: RetryPolicy::default(),
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

    /// Build one authenticated request URL.
    ///
    /// `output=json` is appended last so a fixture or a log excerpt still ends
    /// with the response format.
    fn endpoint_url(&self, endpoint: &str, params: &[(&str, &str)]) -> String {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("devid", &self.credentials.devid);
        query.append_pair("devpassword", &self.credentials.devpassword);
        query.append_pair("softname", &self.credentials.softname);
        if let (Some(ssid), Some(sspassword)) = (
            self.credentials.ssid.as_deref(),
            self.credentials.sspassword.as_deref(),
        ) {
            query.append_pair("ssid", ssid);
            query.append_pair("sspassword", sspassword);
        }
        query.append_pair("output", "json");
        for (key, value) in params {
            query.append_pair(key, value);
        }
        format!("{API_BASE}{endpoint}?{}", query.finish())
    }

    /// Run one `jeuInfos` request and return the game payloads it carries.
    fn request_games(&self, url: &str) -> Result<Vec<Jeu>> {
        let response = execute_with_retry_classified(
            self.transport.as_ref(),
            &self.policy,
            || {
                HttpRequest::new(HttpMethod::Get, url)
                    .header("Accept", "application/json")
                    .header("Accept-Language", "en, es;q=0.8, fr;q=0.5")
            },
            |response| {
                is_retryable_response(response, ProviderFlavor::ScreenScraper)
                    // ScreenScraper also reports quota exhaustion in a `200`
                    // JSON payload, so the retry decision uses the same
                    // vocabulary as the status classifier.
                    || (response.status == 200
                        && has_quota_signature(&response.body, ProviderFlavor::ScreenScraper))
            },
        )?;

        if response.status == 404 {
            return Err(Error::NotFound("ScreenScraper game".to_string()));
        }
        let response = ensure_success(response, ProviderFlavor::ScreenScraper)?;
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Network(format!(
                "ScreenScraper response exceeded the {MAX_RESPONSE_BYTES} byte limit"
            )));
        }
        parse_games(&response.body)
    }

    /// Map upstream entries onto candidates, best match first.
    ///
    /// `jeuInfos` answers with a single entry, but a future
    /// `jeuRecherche.php` path returns several; ranking through
    /// [`crate::metadata::best_candidate`] keeps the caller's stored title the
    /// authority either way.
    fn map_candidates(&self, title: &str, games: &[Jeu]) -> Vec<MetadataCandidate> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for game in games {
            let Some(source_id) = game.id_string() else {
                continue;
            };
            let Some(name) = game.preferred_name() else {
                continue;
            };
            if !seen.insert(source_id.clone()) {
                continue;
            }
            candidates.push(MetadataCandidate {
                source_id,
                name,
                release_year: game.release_year(),
                platform_hint: game.platform_hint(),
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

impl MetadataProvider for ScreenScraperProvider {
    fn source(&self) -> MetadataSource {
        MetadataSource::ScreenScraper
    }

    /// Name search through `jeuInfos.php?romnom=<title>`.
    ///
    /// A no-match answer is an empty candidate list, exactly like the other
    /// providers; a rejected credential set stays an [`Error::Auth`].
    fn search(&self, title: &str) -> Result<Vec<MetadataCandidate>> {
        let trimmed = title.trim();
        if crate::metadata::normalize_title(trimmed).is_empty() {
            return Ok(Vec::new());
        }

        let url = self.endpoint_url(GAME_INFO_ENDPOINT, &[("romnom", trimmed)]);
        match self.request_games(&url) {
            Ok(games) => Ok(self.map_candidates(trimmed, &games)),
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    fn details(&self, source_id: &str) -> Result<GameMetadata> {
        let id = parse_jeu_id(source_id)?;
        let key = id.to_string();

        if let Some(hit) = lock(&self.details).get(&key).cloned() {
            return Ok(hit);
        }
        let _gate = lock(&self.details_gate);
        if let Some(hit) = lock(&self.details).get(&key).cloned() {
            return Ok(hit);
        }

        let url = self.endpoint_url(GAME_INFO_ENDPOINT, &[("jeuid", &key)]);
        let game = self
            .request_games(&url)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound(format!("ScreenScraper game {key}")))?;
        let metadata = game.into_metadata(&key)?;

        let mut cache = lock(&self.details);
        if cache.len() >= MAX_DETAILS_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, metadata.clone());
        Ok(metadata)
    }
}

/// Parse provider-supplied game id.
///
/// The id comes from a candidate or a caller, never from a path, but it still
/// has to be a positive integer before it is interpolated into a URL.
fn parse_jeu_id(source_id: &str) -> Result<u64> {
    match source_id.trim().parse::<u64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(Error::InvalidState(
            "ScreenScraper game id must be a positive integer".to_string(),
        )),
    }
}

/// Parse a `jeuInfos` envelope into game payloads or a classified error.
///
/// The body is never echoed: an API-level `erreur` string is scrubbed and
/// truncated by [`redact_error_text`] before it reaches an [`Error`].
fn parse_games(body: &[u8]) -> Result<Vec<Jeu>> {
    if body.is_empty() {
        return Err(Error::NotFound("ScreenScraper game".to_string()));
    }

    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| Error::Provider {
        status: 200,
        message: "ScreenScraper returned an unreadable response".to_string(),
    })?;

    let mut games = Vec::new();
    if let Some(response) = envelope.response.as_ref() {
        if let Some(jeu) = response.jeu.as_ref() {
            games.push(jeu.clone());
        }
        games.extend(response.jeux.iter().cloned());
    }
    if !games.is_empty() {
        return Ok(games);
    }

    let message = envelope
        .response
        .and_then(|response| response.erreur)
        .or(envelope.erreur)
        .or(envelope.header.erreur);
    match message
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    {
        Some(text) => Err(classify_api_error(&text)),
        None => Err(Error::NotFound("ScreenScraper game".to_string())),
    }
}

/// Classify an API-level error payload (HTTP 200 with an `erreur` string).
fn classify_api_error(text: &str) -> Error {
    let message = redact_error_text(text);
    let lower = message.to_lowercase();

    const AUTH_MARKERS: [&str; 7] = [
        "identifiant",
        "mot de passe",
        "login",
        "password",
        "credential",
        "clé",
        "non autorisé",
    ];
    if AUTH_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return Error::Auth(format!("ScreenScraper rejected the credentials: {message}"));
    }
    if lower.contains("quota") || lower.contains("limite") || lower.contains("trop de requ") {
        return Error::Provider {
            status: 200,
            message,
        };
    }
    if lower.contains("non trouv")
        || lower.contains("not found")
        || lower.contains("aucun")
        || lower.contains("inconnu")
    {
        return Error::NotFound(message);
    }
    Error::Provider {
        status: 200,
        message,
    }
}

fn pick_localized(entries: &[LocalizedText], preference: &[&str]) -> Option<String> {
    for preferred in preference {
        if let Some(value) = entries
            .iter()
            .find(|entry| entry.matches(preferred))
            .and_then(LocalizedText::value)
        {
            return Some(value);
        }
    }
    entries.iter().find_map(LocalizedText::value)
}

fn text_of(field: &Option<FlexibleText>) -> Option<String> {
    match field.as_ref()? {
        FlexibleText::Plain(text) => non_empty(text),
        FlexibleText::Entries(entries) => pick_localized(entries, &LANGUAGE_PREFERENCE),
        FlexibleText::Named(named) => named
            .text
            .as_deref()
            .and_then(non_empty)
            .or_else(|| pick_localized(&named.noms, &LANGUAGE_PREFERENCE)),
    }
}

fn collect_names(entries: &[NestedNames]) -> Vec<String> {
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

fn localized_genres(
    primary: &[NestedNames],
    en: &[NestedNames],
    es: &[NestedNames],
    fr: &[NestedNames],
) -> Vec<String> {
    for entries in [primary, en, es, fr] {
        let values = collect_names(entries);
        if !values.is_empty() {
            return values;
        }
    }
    Vec::new()
}

/// Join or pick the age-rating classifications.
///
/// PEGI is the primary European label, ESRB the North American one; anything
/// else is joined so no rating is silently dropped.
fn age_rating(classifications: &[Classification]) -> Option<String> {
    let text_of_classification = |wanted: &str| -> Option<String> {
        classifications
            .iter()
            .find(|entry| entry.kind.eq_ignore_ascii_case(wanted))
            .and_then(|entry| entry.text.as_deref().and_then(non_empty))
            .map(|value| format!("{wanted} {value}"))
    };

    if let Some(rating) = text_of_classification("PEGI") {
        return Some(rating);
    }
    if let Some(rating) = text_of_classification("ESRB") {
        return Some(rating);
    }

    let mut values = Vec::new();
    for entry in classifications {
        if let Some(value) = entry.text.as_deref().and_then(non_empty)
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
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

/// Extract a plausible release year from ScreenScraper's date text.
///
/// The catalogue publishes `"2020-09-17"`, `"2020"`, and non-commitments such
/// as `"2020 (TBA)"`; the first standalone four-digit run in a plausible range
/// wins.
fn parse_year(text: &str) -> Option<i32> {
    text.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| part.len() == 4)
        .filter_map(|part| part.parse::<i32>().ok())
        .find(|year| (1900..=2200).contains(year))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_release_years_from_screenscraper_dates() {
        assert_eq!(parse_year("2020-09-17"), Some(2020));
        assert_eq!(parse_year("1998"), Some(1998));
        assert_eq!(parse_year("2020 (TBA)"), Some(2020));
        assert_eq!(parse_year("TBA"), None);
        assert_eq!(parse_year("12/03"), None);
        assert_eq!(parse_year("12345"), None);
    }

    #[test]
    fn rejects_non_numeric_game_ids() {
        for invalid in ["", "  ", "abc", "0", "-1", "12.5"] {
            assert!(parse_jeu_id(invalid).is_err(), "{invalid}");
        }
        assert_eq!(parse_jeu_id(" 3 ").unwrap(), 3);
    }

    #[test]
    fn classifies_api_level_errors_without_echoing_credentials() {
        let error = classify_api_error("Erreur de login : identifiants incorrects");
        assert!(matches!(error, Error::Auth(_)), "{error:?}");

        let error = classify_api_error("Rom non trouvée");
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");

        let leaked = classify_api_error("bad request devpassword=pw-secret&devid=dev");
        assert!(!leaked.to_string().contains("pw-secret"), "{leaked}");
        assert!(!leaked.to_string().contains("devid=dev"), "{leaked}");
    }

    /// Locally incomplete credentials are a configuration gap: nothing was sent
    /// and nothing was rejected. A credential set the service actually refuses
    /// stays [`Error::Auth`], and a missing catalogue entry stays
    /// [`Error::NotFound`].
    #[test]
    fn credentials_reject_an_incomplete_set() {
        let empty = ScreenScraperCredentials::developer("", "pw-sentinel-secret");
        let error = empty.validate().unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        assert!(!matches!(error, Error::Auth(_)), "{error:?}");
        assert!(!matches!(error, Error::NotFound(_)), "{error:?}");
        // Actionable: it names both halves the application must supply and
        // never echoes the supplied value.
        assert!(error.to_string().contains("dev id"), "{error}");
        assert!(error.to_string().contains("dev password"), "{error}");
        assert!(!error.to_string().contains("pw-sentinel-secret"), "{error}");

        for partial in [
            ScreenScraperCredentials::developer("dev", "pw").with_user("ssid-sentinel", " "),
            ScreenScraperCredentials::developer("dev", "pw").with_user(" ", "sspassword-sentinel"),
        ] {
            let error = partial
                .validate()
                .expect_err("half a user pair is a configuration gap");
            assert!(matches!(error, Error::Config(_)), "{error:?}");
            assert!(!error.to_string().contains("ssid-sentinel"), "{error}");
            assert!(
                !error.to_string().contains("sspassword-sentinel"),
                "{error}"
            );
        }

        let valid = ScreenScraperCredentials::developer("dev", "pw");
        assert!(valid.validate().is_ok());

        // Complete credentials that the service refuses are an auth failure.
        let rejected = classify_api_error("Erreur de login : identifiants incorrects");
        assert!(matches!(rejected, Error::Auth(_)), "{rejected:?}");
        // A provider answer with no matching game is not a credential problem.
        let no_match = classify_api_error("Rom non trouvée");
        assert!(matches!(no_match, Error::NotFound(_)), "{no_match:?}");
    }

    #[test]
    fn preferred_language_beats_catalogue_order() {
        let entries = vec![
            LocalizedText {
                langue: Some("fr".into()),
                region: None,
                text: Some("Défiez le dieu des morts.".into()),
            },
            LocalizedText {
                langue: Some("en".into()),
                region: None,
                text: Some("Defy the god of the dead.".into()),
            },
        ];
        assert_eq!(
            pick_localized(&entries, &LANGUAGE_PREFERENCE).as_deref(),
            Some("Defy the god of the dead.")
        );
    }
}
