//! OAuth 2.0 PKCE for Google Drive and Dropbox.
//!
//! SaveSync only requests the narrowest scope each vendor offers for app-owned
//! files: `drive.file` for Drive and the `files.content.*`/`files.metadata.read`
//! set for a Dropbox app folder. The refresh token is returned to the caller so
//! the desktop layer can persist it in the OS keyring; this module never writes
//! secrets to disk.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::{Error, Result};
use crate::http::{
    HttpMethod, HttpRequest, HttpTransport, RequestBody, RetryPolicy, execute_with_retry,
};
use crate::provider_runtime::redact_error_text;

/// Minimal, non-sensitive scope: only files created or opened by SaveSync.
pub const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
pub const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const DROPBOX_AUTH_ENDPOINT: &str = "https://www.dropbox.com/oauth2/authorize";
pub const DROPBOX_TOKEN_ENDPOINT: &str = "https://www.dropbox.com/oauth2/token";

/// Scopes SaveSync requests from a Dropbox app. App-folder access is a content
/// level on the app, not a scope, so these three cover read, write, and listing
/// of SaveSync's own objects and nothing outside the app folder.
pub const DROPBOX_SCOPES: [&str; 3] = [
    "files.content.write",
    "files.content.read",
    "files.metadata.read",
];

/// Refresh the access token this many seconds before it expires.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// OAuth client configuration (a Google "Desktop app" client or a Dropbox app).
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    /// Optional for public clients using PKCE; included when Google issues one.
    pub client_secret: Option<String>,
    pub auth_endpoint: String,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
    /// Short vendor name used in user-facing flow messages ("Google").
    pub service_name: String,
    /// Sentence the loopback completion page shows on success.
    pub connected_message: String,
    /// Vendor-specific authorization-endpoint query pairs, appended in order.
    ///
    /// Google needs `access_type`/`prompt`/`include_granted_scopes`; Dropbox
    /// needs `token_access_type=offline`. Keeping the list per provider leaves
    /// each vendor's consent URL exactly as that vendor documents it.
    pub auth_params: Vec<(String, String)>,
}

impl OAuthConfig {
    /// Google configuration requesting only `drive.file`.
    pub fn google(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            auth_endpoint: GOOGLE_AUTH_ENDPOINT.to_string(),
            token_endpoint: GOOGLE_TOKEN_ENDPOINT.to_string(),
            scopes: vec![DRIVE_FILE_SCOPE.to_string()],
            service_name: "Google".to_string(),
            connected_message:
                "Google Drive is connected. You can close this window and return to SaveSync."
                    .to_string(),
            auth_params: vec![
                ("access_type".to_string(), "offline".to_string()),
                // Ensure a refresh token is issued on first consent.
                ("prompt".to_string(), "consent".to_string()),
                ("include_granted_scopes".to_string(), "true".to_string()),
            ],
        }
    }

    /// Dropbox configuration for an app with app-folder access.
    ///
    /// `token_access_type=offline` is what makes the code exchange return a
    /// refresh token; Dropbox issues no client secret for PKCE apps, so the
    /// refresh request omits `client_secret` exactly as the documentation
    /// requires.
    pub fn dropbox(app_key: impl Into<String>) -> Self {
        Self {
            client_id: app_key.into(),
            client_secret: None,
            auth_endpoint: DROPBOX_AUTH_ENDPOINT.to_string(),
            token_endpoint: DROPBOX_TOKEN_ENDPOINT.to_string(),
            scopes: DROPBOX_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
            service_name: "Dropbox".to_string(),
            connected_message:
                "Dropbox is connected. You can close this window and return to SaveSync."
                    .to_string(),
            auth_params: vec![("token_access_type".to_string(), "offline".to_string())],
        }
    }

    pub fn with_client_secret(mut self, secret: impl Into<String>) -> Self {
        self.client_secret = Some(secret.into());
        self
    }
}

/// PKCE verifier/challenge pair (S256).
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl PkcePair {
    /// Generate a fresh verifier from 256 bits of UUID entropy.
    pub fn generate() -> Self {
        let verifier = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        Self::from_verifier(verifier)
    }

    /// Build a pair from an existing verifier (used for tests and resumed flows).
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// Tokens returned by Google's token endpoint.
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    /// Absolute expiry, computed locally; not part of the API response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Source of a valid bearer token.
pub trait TokenProvider: Send + Sync {
    /// Return a token, refreshing proactively when it is close to expiry.
    fn access_token(&self) -> Result<String>;
    /// Force a refresh; called after a 401 response.
    fn refresh(&self) -> Result<String>;
}

/// A fixed token provider for tests and CLI usage where refresh is handled
/// externally.
pub struct StaticToken {
    token: String,
}

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl TokenProvider for StaticToken {
    fn access_token(&self) -> Result<String> {
        Ok(self.token.clone())
    }

    fn refresh(&self) -> Result<String> {
        Ok(self.token.clone())
    }
}

/// Build the browser URL for a vendor's consent screen.
pub fn authorization_url(
    config: &OAuthConfig,
    redirect_uri: &str,
    pkce: &PkcePair,
    state: &str,
) -> Result<String> {
    let mut url = Url::parse(&config.auth_endpoint)
        .map_err(|error| Error::Auth(format!("invalid auth endpoint: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &config.scopes.join(" "))
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        for (name, value) in &config.auth_params {
            query.append_pair(name, value);
        }
    }
    Ok(url.into())
}

/// Exchange an authorization code for tokens.
pub fn exchange_code(
    transport: &dyn HttpTransport,
    config: &OAuthConfig,
    code: &str,
    pkce: &PkcePair,
    redirect_uri: &str,
) -> Result<TokenSet> {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", &config.client_id)
        .append_pair("code_verifier", &pkce.verifier);
    if let Some(secret) = &config.client_secret {
        form.append_pair("client_secret", secret);
    }

    request_tokens(transport, config, &form.finish(), true)
}

/// Refresh an access token using a stored refresh token.
pub fn refresh_tokens(
    transport: &dyn HttpTransport,
    config: &OAuthConfig,
    refresh_token: &str,
) -> Result<TokenSet> {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .append_pair("client_id", &config.client_id);
    if let Some(secret) = &config.client_secret {
        form.append_pair("client_secret", secret);
    }

    request_tokens(transport, config, &form.finish(), true)
}

fn request_tokens(
    transport: &dyn HttpTransport,
    config: &OAuthConfig,
    form: &str,
    include_scope: bool,
) -> Result<TokenSet> {
    let request = HttpRequest::new(HttpMethod::Post, &config.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(RequestBody::bytes(form.as_bytes().to_vec()));

    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: std::time::Duration::from_millis(500),
        max_delay: std::time::Duration::from_secs(5),
    };
    let response = execute_with_retry(transport, &policy, || request.clone())?;
    if !response.is_success() {
        return Err(Error::Auth(describe_token_error(&response.body)));
    }

    let mut tokens: TokenSet = serde_json::from_slice(&response.body)
        .map_err(|error| Error::Auth(format!("invalid token response: {error}")))?;
    if tokens.access_token.is_empty() {
        return Err(Error::Auth("token response is missing access_token".into()));
    }
    if !include_scope {
        tokens.scope = None;
    }
    if let Some(expires_in) = tokens.expires_in {
        tokens.expires_at = Some(chrono::Utc::now().timestamp() + expires_in as i64);
    }
    Ok(tokens)
}

/// Human-readable reason for a failed token request, scrubbed and truncated.
///
/// A vendor may echo the request back in `error_description` (for example
/// `bad refresh_token=<value>`), so the text passes through
/// [`redact_error_text`] before it can reach an [`Error::Auth`] message, a log,
/// or the UI. Truncation happens after redaction for the same reason.
fn describe_token_error(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<String>,
        error_description: Option<String>,
    }
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(parsed) => {
            let text = redact_error_text(
                &parsed
                    .error_description
                    .or(parsed.error)
                    .unwrap_or_else(|| "token request failed".into()),
            );
            if text.is_empty() {
                "token request failed".into()
            } else {
                text
            }
        }
        Err(_) => {
            let text = redact_error_text(&String::from_utf8_lossy(body));
            if text.is_empty() {
                "token request failed".into()
            } else {
                format!("token request failed: {text}")
            }
        }
    }
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// Manages refresh and caching for one OAuth client.
///
/// Refreshes are single-flight: `grant_type=refresh_token` is issued under a
/// mutex, so concurrent callers collapse into one network request instead of
/// racing each other. A [`TokenProvider::refresh`] call is always forced (the
/// caller has just seen a `401`); [`TokenProvider::access_token`] re-checks the
/// cache after taking the gate, which is what collapses the stampede.
pub struct OAuthTokenManager {
    config: OAuthConfig,
    transport: std::sync::Arc<dyn HttpTransport>,
    refresh_token: Mutex<Option<String>>,
    cached: Mutex<Option<CachedToken>>,
    refresh_lock: Mutex<()>,
}

impl OAuthTokenManager {
    pub fn new(
        config: OAuthConfig,
        transport: std::sync::Arc<dyn HttpTransport>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            config,
            transport,
            refresh_token: Mutex::new(Some(refresh_token.into())),
            cached: Mutex::new(None),
            refresh_lock: Mutex::new(()),
        }
    }

    /// Persist a token set after the authorization-code exchange.
    pub fn store_tokens(&self, tokens: &TokenSet) {
        if let Some(refresh) = &tokens.refresh_token {
            *self.refresh_token.lock().expect("token mutex") = Some(refresh.clone());
        }
        let lifetime = tokens
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOKEN_LIFETIME);
        *self.cached.lock().expect("token mutex") = Some(CachedToken {
            access_token: tokens.access_token.clone(),
            expires_at: Instant::now() + lifetime,
        });
    }

    pub fn refresh_token(&self) -> Option<String> {
        self.refresh_token.lock().expect("token mutex").clone()
    }

    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    pub fn transport(&self) -> &std::sync::Arc<dyn HttpTransport> {
        &self.transport
    }

    /// Cached access token, when it is valid beyond the expiry skew.
    fn cached_access_token(&self) -> Option<String> {
        let cached = self.cached.lock().expect("token mutex");
        cached
            .as_ref()
            .filter(|cached| cached.expires_at > Instant::now() + EXPIRY_SKEW)
            .map(|cached| cached.access_token.clone())
    }

    /// Issue the refresh request. The caller holds [`Self::refresh_lock`].
    fn refresh_locked(&self) -> Result<String> {
        let refresh_token = self
            .refresh_token
            .lock()
            .expect("token mutex")
            .clone()
            .ok_or_else(|| Error::Auth("no refresh token; complete the OAuth flow".into()))?;

        let tokens = refresh_tokens(self.transport.as_ref(), &self.config, &refresh_token)?;
        let access_token = tokens.access_token.clone();
        self.store_tokens(&tokens);
        Ok(access_token)
    }
}

impl TokenProvider for OAuthTokenManager {
    fn access_token(&self) -> Result<String> {
        if let Some(token) = self.cached_access_token() {
            return Ok(token);
        }
        let _guard = self.refresh_lock.lock().expect("token mutex");
        // Another caller may have refreshed while this one waited.
        if let Some(token) = self.cached_access_token() {
            return Ok(token);
        }
        self.refresh_locked()
    }

    fn refresh(&self) -> Result<String> {
        let _guard = self.refresh_lock.lock().expect("token mutex");
        self.refresh_locked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use crate::provider_runtime::{MAX_ERROR_CHARS, REDACTED};
    use std::sync::{Arc, Mutex};

    #[test]
    fn pkce_matches_rfc7636_vector() {
        let pair = PkcePair::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pair.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_is_long_enough() {
        let pair = PkcePair::generate();
        assert!((43..=128).contains(&pair.verifier.len()));
        assert!(!pair.challenge.contains('='));
        assert_ne!(PkcePair::generate().verifier, pair.verifier);
    }

    #[test]
    fn authorization_url_contains_scopes_and_pkce() {
        let config = OAuthConfig::google("client-id");
        let pkce = PkcePair::from_verifier("verifier-verifier-verifier-verifier-verifier");
        let url =
            authorization_url(&config, "http://127.0.0.1:45812/callback", &pkce, "state").unwrap();
        let parsed = Url::parse(&url).unwrap();
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(params.get("client_id").unwrap(), "client-id");
        assert_eq!(params.get("response_type").unwrap(), "code");
        assert_eq!(params.get("scope").unwrap(), DRIVE_FILE_SCOPE);
        assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(params.get("code_challenge").unwrap(), &pkce.challenge);
        assert_eq!(params.get("state").unwrap(), "state");
        assert_eq!(params.get("access_type").unwrap(), "offline");
    }

    struct TokenTransport {
        response: Mutex<Option<HttpResponse>>,
        request: Mutex<Option<HttpRequest>>,
    }

    impl HttpTransport for TokenTransport {
        fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
            *self.request.lock().unwrap() = Some(request.clone());
            Ok(self.response.lock().unwrap().take().unwrap())
        }
    }

    #[test]
    fn exchanges_code_with_verifier_and_parses_tokens() {
        let transport = TokenTransport {
            response: Mutex::new(Some(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"access_token":"at","refresh_token":"rt","expires_in":3600,"scope":"drive.file","token_type":"Bearer"}"#.to_vec(),
            })),
            request: Mutex::new(None),
        };
        let config = OAuthConfig::google("client-id");
        let pkce = PkcePair::from_verifier("verifier");
        let tokens = exchange_code(
            &transport,
            &config,
            "code-123",
            &pkce,
            "http://127.0.0.1/cb",
        )
        .unwrap();

        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        assert!(tokens.expires_at.is_some());
        assert!(format!("{tokens:?}").contains("<redacted>"));

        let request = transport.request.lock().unwrap().clone().unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        let form = String::from_utf8(request.body.unwrap().read_all().unwrap()).unwrap();
        assert!(form.contains("grant_type=authorization_code"));
        assert!(form.contains("code=code-123"));
        assert!(form.contains("code_verifier=verifier"));
    }

    #[test]
    fn token_errors_are_surfaced_without_secrets() {
        let transport = TokenTransport {
            response: Mutex::new(Some(HttpResponse {
                status: 400,
                headers: vec![],
                body: br#"{"error":"invalid_grant","error_description":"Bad Request"}"#.to_vec(),
            })),
            request: Mutex::new(None),
        };
        let error = refresh_tokens(&transport, &OAuthConfig::google("id"), "rt").unwrap_err();
        assert!(matches!(error, Error::Auth(_)));
        assert!(error.to_string().contains("Bad Request"));
    }

    #[test]
    fn token_error_description_is_redacted_then_truncated() {
        // The vendor echoes the offending credential inside `error_description`;
        // that is exactly the leak `describe_token_error` must close. The
        // credential sits inside the retained prefix, so redaction and the
        // truncation bound are both observable in the same message.
        let secret = "SEKRET-REFRESH-VALUE";
        let description = format!("refresh_token={secret} expired {}", "detail ".repeat(80));
        let body = serde_json::json!({
            "error": "invalid_grant",
            "error_description": description,
        });
        let transport = TokenTransport {
            response: Mutex::new(Some(HttpResponse {
                status: 400,
                headers: vec![],
                body: serde_json::to_vec(&body).unwrap(),
            })),
            request: Mutex::new(None),
        };
        let error = refresh_tokens(&transport, &OAuthConfig::google("id"), "rt").unwrap_err();
        let message = format!("{error}");

        assert!(!message.contains(secret), "credential leaked: {message}");
        assert!(!message.contains("refresh_token="), "key leaked: {message}");
        assert!(message.contains(REDACTED), "not redacted: {message}");
        // Truncation bounds what a hostile vendor can push into the UI.
        assert!(
            message.chars().count() < description.chars().count(),
            "description was not truncated ({} chars): {message}",
            message.chars().count()
        );
        assert!(
            message.chars().count() <= MAX_ERROR_CHARS + "authentication error: ".len() + 8,
            "unbounded message ({} chars): {message}",
            message.chars().count()
        );
    }

    #[test]
    fn token_manager_refreshes_and_caches() {
        let transport = Arc::new(TokenTransport {
            response: Mutex::new(Some(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"access_token":"fresh","expires_in":3600}"#.to_vec(),
            })),
            request: Mutex::new(None),
        });
        let manager = OAuthTokenManager::new(
            OAuthConfig::google("id"),
            transport.clone(),
            "refresh-token",
        );
        assert_eq!(manager.access_token().unwrap(), "fresh");
        assert_eq!(manager.access_token().unwrap(), "fresh");
        assert!(transport.response.lock().unwrap().is_none());
    }

    #[test]
    fn google_authorization_url_pairs_are_unchanged() {
        // Byte-identical consent URL for Drive: same pairs, same order.
        let config = OAuthConfig::google("client-id");
        let pkce = PkcePair::from_verifier("verifier-verifier-verifier-verifier-verifier");
        let url =
            authorization_url(&config, "http://127.0.0.1:45812/callback", &pkce, "state").unwrap();
        let parsed = Url::parse(&url).unwrap();
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();

        assert_eq!(
            pairs,
            vec![
                ("client_id".to_string(), "client-id".to_string()),
                (
                    "redirect_uri".to_string(),
                    "http://127.0.0.1:45812/callback".to_string()
                ),
                ("response_type".to_string(), "code".to_string()),
                ("scope".to_string(), DRIVE_FILE_SCOPE.to_string()),
                ("code_challenge".to_string(), pkce.challenge.clone()),
                ("code_challenge_method".to_string(), "S256".to_string()),
                ("state".to_string(), "state".to_string()),
                ("access_type".to_string(), "offline".to_string()),
                ("prompt".to_string(), "consent".to_string()),
                ("include_granted_scopes".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn dropbox_authorization_url_requests_offline_access() {
        let config = OAuthConfig::dropbox("app-key");
        let pkce = PkcePair::from_verifier("verifier-verifier-verifier-verifier-verifier");
        let url =
            authorization_url(&config, "http://127.0.0.1:45812/callback", &pkce, "state").unwrap();
        let parsed = Url::parse(&url).unwrap();
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.host_str(), Some("www.dropbox.com"));
        assert_eq!(parsed.path(), "/oauth2/authorize");
        assert_eq!(params.get("client_id").unwrap(), "app-key");
        assert_eq!(params.get("token_access_type").unwrap(), "offline");
        assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(params.get("code_challenge").unwrap(), &pkce.challenge);
        assert_eq!(params.get("state").unwrap(), "state");
        assert_eq!(
            params.get("scope").unwrap(),
            "files.content.write files.content.read files.metadata.read"
        );
        // Google-only parameters must not leak into Dropbox's consent URL.
        assert!(!params.contains_key("access_type"));
        assert!(!params.contains_key("prompt"));
        assert!(!params.contains_key("include_granted_scopes"));
    }

    #[test]
    fn dropbox_code_exchange_and_refresh_omit_client_secret() {
        let transport = TokenTransport {
            response: Mutex::new(Some(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"access_token":"at","refresh_token":"rt","expires_in":14400,"token_type":"bearer","scope":"files.content.write files.content.read files.metadata.read"}"#.to_vec(),
            })),
            request: Mutex::new(None),
        };
        let config = OAuthConfig::dropbox("app-key");
        let pkce = PkcePair::from_verifier("verifier-verifier-verifier-verifier-verifier");
        let tokens = exchange_code(
            &transport,
            &config,
            "code-123",
            &pkce,
            "http://127.0.0.1:45812/callback",
        )
        .unwrap();

        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        assert_eq!(tokens.expires_in, Some(14400));
        assert!(tokens.expires_at.is_some());

        let request = transport.request.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.url, "https://www.dropbox.com/oauth2/token",
            "the token exchange must target Dropbox's token endpoint"
        );
        let form = String::from_utf8(request.body.unwrap().read_all().unwrap()).unwrap();
        assert!(form.contains("grant_type=authorization_code"));
        assert!(form.contains("client_id=app-key"));
        assert!(
            form.contains("code_verifier=verifier-verifier-verifier-verifier-verifier"),
            "PKCE verifier must be sent: {form}"
        );
        assert!(
            !form.contains("client_secret"),
            "PKCE clients have no secret: {form}"
        );

        // The refresh grant uses the same verifier-free form and no secret.
        let transport = TokenTransport {
            response: Mutex::new(Some(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"access_token":"at2","expires_in":14400}"#.to_vec(),
            })),
            request: Mutex::new(None),
        };
        let tokens = refresh_tokens(&transport, &config, "rt").unwrap();
        assert_eq!(tokens.access_token, "at2");
        let request = transport.request.lock().unwrap().clone().unwrap();
        let form = String::from_utf8(request.body.unwrap().read_all().unwrap()).unwrap();
        assert!(form.contains("grant_type=refresh_token"));
        assert!(form.contains("refresh_token=rt"));
        assert!(!form.contains("client_secret"));
        assert!(!form.contains("code_verifier"));
    }

    struct CountingTransport {
        requests: Mutex<usize>,
    }

    impl HttpTransport for CountingTransport {
        fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
            *self.requests.lock().unwrap() += 1;
            // Hold the gate long enough for the other callers to queue up.
            std::thread::sleep(Duration::from_millis(30));
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: br#"{"access_token":"shared","expires_in":3600}"#.to_vec(),
            })
        }
    }

    #[test]
    fn concurrent_token_requests_refresh_once() {
        let transport = Arc::new(CountingTransport {
            requests: Mutex::new(0),
        });
        let manager: Arc<OAuthTokenManager> = Arc::new(OAuthTokenManager::new(
            OAuthConfig::dropbox("app-key"),
            transport.clone(),
            "refresh-token",
        ));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let manager = manager.clone();
                std::thread::spawn(move || manager.access_token())
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().unwrap(), "shared");
        }

        assert_eq!(
            *transport.requests.lock().unwrap(),
            1,
            "concurrent refreshes must collapse into a single token request"
        );

        // A forced refresh (after a 401) is still a real network request.
        assert_eq!(manager.refresh().unwrap(), "shared");
        assert_eq!(*transport.requests.lock().unwrap(), 2);
    }
}
