//! Normalized game metadata and the provider contract.
//!
//! Metadata (a release year, genre tags, a description) is optional
//! enrichment. Nothing in this module reads or writes save data, and no engine
//! path may block snapshot, restore, or sync on a metadata provider: a fetch
//! only happens when a caller explicitly asks for one.
//!
//! The seam mirrors [`crate::provider::SyncProvider`] and
//! [`crate::artwork::SteamGridDbClient`]: one provider trait over an injected
//! [`crate::http::HttpTransport`], a normalized model that every source maps
//! into, and a registry that builds the production implementation for a
//! [`MetadataSource`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::http::ReqwestTransport;
use crate::igdb_metadata::{IgdbCredentials, IgdbProvider};
use crate::screenscraper_metadata::{ScreenScraperCredentials, ScreenScraperProvider};
use crate::steam_metadata::SteamMetadataProvider;

/// Where a piece of game metadata came from.
///
/// Stored in `game_metadata.source` as the snake_case identifier, exactly like
/// [`crate::models::AdapterType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSource {
    /// The keyless Steam Store endpoints.
    Steam,
    /// ScreenScraper (requires user credentials; provider lands in a later
    /// milestone). Single-word brand name: the stored id matches [`Self::as_str`].
    #[serde(rename = "screenscraper")]
    ScreenScraper,
    /// IGDB (requires a Twitch client id/secret).
    Igdb,
}

impl MetadataSource {
    /// Stable identifier stored in the database and used in error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steam => "steam",
            Self::ScreenScraper => "screenscraper",
            Self::Igdb => "igdb",
        }
    }

    /// Parse a stored source identifier.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "steam" => Self::Steam,
            "screenscraper" => Self::ScreenScraper,
            "igdb" => Self::Igdb,
            _ => return None,
        })
    }
}

impl std::fmt::Display for MetadataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One search result a provider proposes before details are fetched.
///
/// A candidate only carries what a search endpoint actually knows; everything
/// else is filled in by [`MetadataProvider::details`]. `source_id` is opaque to
/// SaveSync: it is handed back to the same provider verbatim and never used as
/// a path or URL component by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataCandidate {
    pub source_id: String,
    pub name: String,
    /// Known before details when the search endpoint reports it.
    pub release_year: Option<i32>,
    /// Best-effort platform label ("PC", "PlayStation 2", ...). Advisory only.
    pub platform_hint: Option<String>,
}

/// Normalized metadata for one game, independent of the provider that
/// produced it.
///
/// Vector fields are stored as JSON text in `game_metadata`; optional fields
/// are `None` and list fields empty when the provider has no answer, never
/// invented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameMetadata {
    pub source: MetadataSource,
    /// Provider-local identity (Steam app id, ScreenScraper game id, ...).
    pub source_id: String,
    pub name: String,
    pub release_year: Option<i32>,
    /// Free-form release date as published, e.g. `"3 Oct, 2018"`.
    pub release_date: Option<String>,
    pub genres: Vec<String>,
    pub developers: Vec<String>,
    pub publishers: Vec<String>,
    pub description_short: Option<String>,
    pub description_long: Option<String>,
    pub players: Option<String>,
    pub age_rating: Option<String>,
    pub franchise: Option<String>,
    pub themes: Vec<String>,
    /// RFC 3339 timestamp of the fetch that produced this row.
    pub fetched_at: String,
}

/// One metadata source.
///
/// Implementations are `Send + Sync` (the engine and command layer share them
/// across threads), perform only blocking network I/O through an injected
/// [`crate::http::HttpTransport`], and never touch the filesystem outside their
/// own cache.
pub trait MetadataProvider: Send + Sync {
    /// The source this provider answers for.
    fn source(&self) -> MetadataSource;

    /// Search for `title` and return candidates, best first.
    ///
    /// An empty title or no match is `Ok(Vec::new())`; a transport or protocol
    /// failure is an [`Error`].
    fn search(&self, title: &str) -> Result<Vec<MetadataCandidate>>;

    /// Fetch normalized metadata for one provider-local `source_id`.
    ///
    /// A missing or unavailable entry is [`Error::NotFound`].
    fn details(&self, source_id: &str) -> Result<GameMetadata>;
}

/// Optional credentials for the sources that need them.
///
/// Credentialed sources keep growing, and a positional argument list would make
/// every new provider a breaking change for every caller. This bundle names what
/// it carries instead: a caller passes only the source's own entry and leaves
/// the rest `None`.
///
/// The bundle borrows; nothing here is stored, and each provider clones what it
/// needs for the lifetime of one fetch. `Debug` is derived only because the
/// credential types redact themselves — it renders `Some`/`None`, never a
/// secret.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetadataCreds<'a> {
    /// ScreenScraper developer credentials.
    pub screenscraper: Option<&'a ScreenScraperCredentials>,
    /// Twitch application credentials for IGDB.
    pub igdb: Option<&'a IgdbCredentials>,
}

impl<'a> MetadataCreds<'a> {
    /// A bundle holding ScreenScraper credentials only.
    pub fn screenscraper(credentials: Option<&'a ScreenScraperCredentials>) -> Self {
        Self {
            screenscraper: credentials,
            igdb: None,
        }
    }

    /// A bundle holding IGDB credentials only.
    pub fn igdb(credentials: Option<&'a IgdbCredentials>) -> Self {
        Self {
            screenscraper: None,
            igdb: credentials,
        }
    }

    /// Attach IGDB credentials to an existing bundle.
    pub fn with_igdb(mut self, credentials: Option<&'a IgdbCredentials>) -> Self {
        self.igdb = credentials;
        self
    }
}

/// Build the production provider for a source that needs no credentials.
///
/// This is [`provider_for_with_creds`] with an empty [`MetadataCreds`], kept so
/// every existing caller is unchanged: Steam behaves exactly as before, and a
/// credentialed source fails with [`Error::Config`] instead of silently
/// searching without the credentials it needs.
///
/// `cache_dir` may be `None` for a provider that keeps its search index in
/// memory only (tests).
///
/// Sources that have no implementation fail with [`Error::NotFound`] rather
/// than silently falling back to another provider.
pub fn provider_for(
    source: MetadataSource,
    cache_dir: Option<&Path>,
) -> Result<Box<dyn MetadataProvider>> {
    provider_for_with_creds(source, cache_dir, MetadataCreds::default())
}

/// Build the production provider for `source`, passing caller-supplied
/// ScreenScraper credentials when the source needs them.
///
/// The historical three-argument form, kept so existing callers (the desktop
/// command layer, the engine) compile unchanged. Any other credentialed source
/// goes through [`provider_for_with_creds`].
pub fn provider_for_with(
    source: MetadataSource,
    cache_dir: Option<&Path>,
    credentials: Option<&ScreenScraperCredentials>,
) -> Result<Box<dyn MetadataProvider>> {
    provider_for_with_creds(source, cache_dir, MetadataCreds::screenscraper(credentials))
}

/// Build the production provider for `source`, passing whichever credentials it
/// needs.
///
/// The registry counterpart of [`crate::adapters::adapter_for`], with one
/// deliberate difference: a provider needs a transport (and, for Steam, a cache
/// location), so construction is fallible and takes the engine's data directory
/// instead of returning a `'static` value.
///
/// Credentials are never stored by core: they are cloned into the returned
/// provider and dropped with it. A credentialed source without its credentials
/// is [`Error::Config`] — nothing was sent, and nothing was rejected — while
/// Steam ignores them, so it stays keyless.
pub fn provider_for_with_creds(
    source: MetadataSource,
    cache_dir: Option<&Path>,
    credentials: MetadataCreds<'_>,
) -> Result<Box<dyn MetadataProvider>> {
    match source {
        MetadataSource::Steam => {
            // The storefront search needs no cache, but the published app list
            // it falls back to is worth keeping between runs when the engine
            // supplies a metadata directory.
            let transport = std::sync::Arc::new(ReqwestTransport::default());
            Ok(Box::new(match cache_dir {
                Some(directory) => SteamMetadataProvider::with_cache_dir(transport, directory),
                None => SteamMetadataProvider::new(transport),
            }))
        }
        MetadataSource::ScreenScraper => {
            let credentials = credentials.screenscraper.ok_or_else(|| {
                Error::Config(format!(
                    "{source} requires application developer credentials (dev id and dev password)"
                ))
            })?;
            let transport = ReqwestTransport::default();
            Ok(Box::new(ScreenScraperProvider::new(
                std::sync::Arc::new(transport),
                credentials.clone(),
            )?))
        }
        MetadataSource::Igdb => {
            let credentials = credentials.igdb.ok_or_else(|| {
                Error::Config(format!(
                    "{source} requires a Twitch client id and client secret"
                ))
            })?;
            let transport = ReqwestTransport::default();
            Ok(Box::new(IgdbProvider::new(
                std::sync::Arc::new(transport),
                credentials.clone(),
            )?))
        }
    }
}

/// Lowercase a title and reduce it to single-space-separated alphanumeric
/// words.
///
/// Providers name the same game differently ("Hades™", "Hades: Ultimate
/// Edition", "HADES"), and the Steam app list contains every trademark spelling
/// it has ever shipped. Normalizing both sides before matching keeps a lookup
/// from depending on punctuation. Deliberately dependency-free.
pub fn normalize_title(title: &str) -> String {
    let mut normalized = String::with_capacity(title.len());
    let mut pending_space = false;
    for ch in title.chars() {
        // Trademark/copyright glyphs are decoration, not title words. Note that
        // apostrophes are dropped so "Hades' Star" and "Hades Star" match.
        if matches!(ch, '\u{2122}' | '\u{00AE}' | '\u{00A9}') {
            continue;
        }
        if ch.is_alphanumeric() {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_space = false;
            normalized.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    normalized
}

/// Pick the candidate that best matches `title`.
///
/// Ranking is deliberately simple and deterministic: a normalized exact match
/// wins, then a prefix or whole-word match, then a substring match, and
/// finally the provider's first result. The caller's stored title is what the
/// user enrolled, so an exact match is always preferred over an unrelated
/// prefix.
pub fn best_candidate<'a>(
    title: &str,
    candidates: &'a [MetadataCandidate],
) -> Option<&'a MetadataCandidate> {
    let target = normalize_title(title);
    if target.is_empty() {
        return candidates.first();
    }
    candidates
        .iter()
        .find(|candidate| normalize_title(&candidate.name) == target)
        .or_else(|| {
            candidates.iter().find(|candidate| {
                let name = normalize_title(&candidate.name);
                name.starts_with(&target) || name.split(' ').any(|word| word == target)
            })
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| normalize_title(&candidate.name).contains(&target))
        })
        .or_else(|| candidates.first())
}

/// Resolve a title to metadata: search, pick the best candidate, then fetch its
/// details.
///
/// Pure network composition: no engine state, no database, no cache write, so
/// a caller may run it without holding a database lock (mirroring
/// [`crate::engine::cover_candidates_for_title`]).
///
/// The returned metadata's identity is pinned to the provider and the candidate
/// that was actually chosen, so a provider that reports a different id cannot
/// make the cache row disagree with the lookup.
pub fn fetch_for_title(provider: &dyn MetadataProvider, title: &str) -> Result<GameMetadata> {
    let candidates = provider.search(title)?;
    let candidate = best_candidate(title, &candidates)
        .ok_or_else(|| Error::NotFound(format!("no {} match for this title", provider.source())))?;

    let mut metadata = provider.details(&candidate.source_id)?;
    metadata.source = provider.source();
    metadata.source_id = candidate.source_id.clone();
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str) -> MetadataCandidate {
        MetadataCandidate {
            source_id: name.len().to_string(),
            name: name.to_string(),
            release_year: None,
            platform_hint: Some("PC".into()),
        }
    }

    #[test]
    fn normalizes_case_punctuation_and_trademark_glyphs() {
        assert_eq!(normalize_title("Hades"), "hades");
        assert_eq!(normalize_title("  HADES™  "), "hades");
        assert_eq!(
            normalize_title("Hades: Ultimate Edition"),
            "hades ultimate edition"
        );
        assert_eq!(normalize_title("Hades' Star"), "hades star");
        assert_eq!(
            normalize_title("Ori and the Blind Forest"),
            "ori and the blind forest"
        );
        assert_eq!(normalize_title("---"), "");
        assert_eq!(normalize_title(""), "");
    }

    #[test]
    fn best_candidate_prefers_an_exact_normalized_title() {
        let candidates = vec![
            candidate("Hades II"),
            candidate("Hades' Star"),
            candidate("Hades™"),
        ];
        assert_eq!(
            best_candidate("Hades", &candidates).map(|c| c.name.as_str()),
            Some("Hades™")
        );
    }

    #[test]
    fn best_candidate_falls_back_from_prefix_to_contains_to_first() {
        let prefix = vec![candidate("Hades II"), candidate("Hades' Star")];
        assert_eq!(
            best_candidate("Hades", &prefix).map(|c| c.name.as_str()),
            Some("Hades II")
        );

        let contains = vec![candidate("Super Hades Deluxe"), candidate("Unrelated")];
        assert_eq!(
            best_candidate("hades", &contains).map(|c| c.name.as_str()),
            Some("Super Hades Deluxe")
        );

        let unrelated = vec![candidate("Unrelated"), candidate("Other")];
        assert_eq!(
            best_candidate("hades", &unrelated).map(|c| c.name.as_str()),
            Some("Unrelated")
        );
        assert!(best_candidate("hades", &[]).is_none());
    }

    #[test]
    fn metadata_source_round_trips_through_its_stored_string() {
        for source in [
            MetadataSource::Steam,
            MetadataSource::ScreenScraper,
            MetadataSource::Igdb,
        ] {
            assert_eq!(MetadataSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(MetadataSource::Steam.as_str(), "steam");
        assert_eq!(
            serde_json::to_string(&MetadataSource::ScreenScraper).unwrap(),
            "\"screenscraper\""
        );
        assert_eq!(MetadataSource::parse("unknown"), None);
    }

    #[test]
    fn credentialled_sources_do_not_silently_fall_back() {
        for source in [MetadataSource::ScreenScraper, MetadataSource::Igdb] {
            let error = provider_for(source, None)
                .err()
                .expect("credentials are required");
            assert!(matches!(error, Error::Config(_)), "{error:?}");
            assert!(
                error.to_string().contains(source.as_str()),
                "the error names the source: {error}"
            );
        }
    }

    #[test]
    fn a_credentialed_source_requires_credentials() {
        let error = provider_for(MetadataSource::ScreenScraper, None)
            .err()
            .expect("credentials are required");
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        // A missing configuration is not a rejected credential set and not a
        // "the provider has no match" answer.
        assert!(!matches!(error, Error::Auth(_)), "{error:?}");
        assert!(!matches!(error, Error::NotFound(_)), "{error:?}");

        let credentials = ScreenScraperCredentials::developer("dev", "dev-password")
            .with_softname("savesync-test");
        let provider =
            provider_for_with(MetadataSource::ScreenScraper, None, Some(&credentials)).unwrap();
        assert_eq!(provider.source(), MetadataSource::ScreenScraper);

        // IGDB goes through the credential bundle; the historical
        // three-argument form deliberately cannot supply its credentials.
        let igdb = IgdbCredentials::new("client-id", "client-secret");
        let provider =
            provider_for_with_creds(MetadataSource::Igdb, None, MetadataCreds::igdb(Some(&igdb)))
                .unwrap();
        assert_eq!(provider.source(), MetadataSource::Igdb);
        assert!(matches!(
            provider_for_with(MetadataSource::Igdb, None, Some(&credentials)),
            Err(Error::Config(_))
        ));
    }
}
