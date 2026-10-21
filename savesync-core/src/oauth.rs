//! OAuth 2.0 PKCE for Google Drive.
//!
//! SaveSync only requests `drive.file`, which grants access to files the app
//! itself created. The refresh token is returned to the caller so the desktop
//! layer can persist it in the OS keyring; this module never writes secrets to
//! disk.

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

/// Minimal, non-sensitive scope: only files created or opened by SaveSync.
pub const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
pub const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Refresh the access token this many seconds before it expires.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// OAuth client configuration (a Google "Desktop app" client).
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    /// Optional for public clients using PKCE; included when Google issues one.
    pub client_secret: Option<String>,
    pub auth_endpoint: String,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
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

/// Build the browser URL for Google's consent screen.
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
            .append_pair("state", state)
            .append_pair("access_type", "offline")
            // Ensure a refresh token is issued on first consent.
            .append_pair("prompt", "consent")
            .append_pair("include_granted_scopes", "true");
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

fn describe_token_error(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<String>,
        error_description: Option<String>,
    }
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(parsed) => parsed
            .error_description
            .or(parsed.error)
            .unwrap_or_else(|| "token request failed".into()),
        Err(_) => format!(
            "token request failed: {}",
            String::from_utf8_lossy(body)
                .chars()
                .take(200)
                .collect::<String>()
        ),
    }
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// Manages refresh and caching for a Google OAuth client.
pub struct OAuthTokenManager {
    config: OAuthConfig,
    transport: std::sync::Arc<dyn HttpTransport>,
    refresh_token: Mutex<Option<String>>,
    cached: Mutex<Option<CachedToken>>,
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
}

impl TokenProvider for OAuthTokenManager {
    fn access_token(&self) -> Result<String> {
        if let Some(cached) = self.cached.lock().expect("token mutex").as_ref()
            && cached.expires_at > Instant::now() + EXPIRY_SKEW
        {
            return Ok(cached.access_token.clone());
        }
        self.refresh()
    }

    fn refresh(&self) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
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
}
