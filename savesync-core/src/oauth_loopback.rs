//! Loopback OAuth redirect listener.
//!
//! Google's desktop OAuth flow redirects the browser to a short-lived HTTP
//! server bound to `127.0.0.1`. The listener validates the `state` parameter
//! (CSRF protection), exchanges the authorization code with the PKCE verifier,
//! and returns the tokens to the caller. It never logs the code or tokens.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use url::Url;

use crate::error::{Error, Result};
use crate::http::HttpTransport;
use crate::oauth::{OAuthConfig, PkcePair, TokenSet, authorization_url, exchange_code};

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const ACCEPT_POLL: Duration = Duration::from_millis(25);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A prepared loopback authorization flow.
pub struct OAuthFlow {
    config: OAuthConfig,
    pkce: PkcePair,
    state: String,
    listener: TcpListener,
    redirect_uri: String,
}

impl OAuthFlow {
    /// Bind `127.0.0.1:<ephemeral>` and prepare a fresh PKCE/state pair.
    pub fn start(config: OAuthConfig) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| Error::Auth(format!("could not bind OAuth listener: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| Error::Auth(format!("could not read OAuth listener port: {error}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let state = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );

        Ok(Self {
            config,
            pkce: PkcePair::generate(),
            state,
            listener,
            redirect_uri,
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Consent URL to open in the system browser.
    pub fn authorization_url(&self) -> Result<String> {
        authorization_url(&self.config, &self.redirect_uri, &self.pkce, &self.state)
    }

    /// Wait for the redirect, validate state, exchange the code, and reply to
    /// the browser with a small status page.
    pub fn finish(self, transport: &dyn HttpTransport, timeout: Duration) -> Result<TokenSet> {
        let deadline = Instant::now() + timeout;
        let (code, mut stream) = self.wait_for_code(deadline)?;

        let outcome = exchange_code(
            transport,
            &self.config,
            &code,
            &self.pkce,
            &self.redirect_uri,
        );

        match &outcome {
            Ok(_) => respond(
                &mut stream,
                200,
                "SaveSync connected",
                "Google Drive is connected. You can close this window and return to SaveSync.",
            ),
            Err(_) => respond(
                &mut stream,
                500,
                "SaveSync could not finish sign-in",
                "The authorization code was received but the token exchange failed. Return to SaveSync and try again.",
            ),
        }
        outcome
    }

    fn wait_for_code(&self, deadline: Instant) -> Result<(String, TcpStream)> {
        self.listener
            .set_nonblocking(true)
            .map_err(|error| Error::Auth(format!("could not configure OAuth listener: {error}")))?;

        loop {
            if Instant::now() >= deadline {
                return Err(Error::Auth(
                    "timed out waiting for Google sign-in (5 minutes)".into(),
                ));
            }

            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    if let Some(result) = self.handle_connection(stream)? {
                        return Ok(result);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(Error::Auth(format!("OAuth listener failed: {error}")));
                }
            }
        }
    }

    fn handle_connection(&self, mut stream: TcpStream) -> Result<Option<(String, TcpStream)>> {
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|error| Error::Auth(format!("could not configure OAuth socket: {error}")))?;

        let request = match read_request(&mut stream) {
            Ok(Some(url)) => url,
            Ok(None) => {
                respond(&mut stream, 404, "Not found", "Unknown callback path.");
                return Ok(None);
            }
            Err(error) => {
                respond(&mut stream, 400, "Bad request", "Malformed OAuth callback.");
                return Err(error);
            }
        };

        if request.path() != "/callback" {
            respond(&mut stream, 404, "Not found", "Unknown callback path.");
            return Ok(None);
        }

        let mut code = None;
        let mut state = None;
        let mut oauth_error = None;
        let mut error_description = None;
        for (key, value) in request.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => oauth_error = Some(value.into_owned()),
                "error_description" => error_description = Some(value.into_owned()),
                _ => {}
            }
        }

        let state = state.unwrap_or_default();
        if !constant_time_eq(&state, &self.state) {
            respond(
                &mut stream,
                400,
                "Invalid sign-in request",
                "The OAuth state did not match. Return to SaveSync and start again.",
            );
            return Err(Error::Auth(
                "OAuth state mismatch; the callback was rejected".into(),
            ));
        }

        if let Some(error) = oauth_error {
            let detail = error_description.unwrap_or(error);
            respond(
                &mut stream,
                400,
                "Sign-in cancelled",
                &format!("Google reported: {detail}"),
            );
            return Err(Error::Auth(format!("Google sign-in failed: {detail}")));
        }

        match code {
            Some(code) if !code.is_empty() => Ok(Some((code, stream))),
            _ => {
                respond(
                    &mut stream,
                    400,
                    "Missing authorization code",
                    "Return to SaveSync and start sign-in again.",
                );
                Ok(None)
            }
        }
    }
}

/// Read the request line and headers. Returns the callback target when the
/// request is a GET for this listener.
fn read_request(stream: &mut TcpStream) -> Result<Option<Url>> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|error| Error::Auth(format!("could not read OAuth callback: {error}")))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(Error::Auth("OAuth callback headers are too large".into()));
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method != "GET" || !target.starts_with('/') {
        return Ok(None);
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| Error::Auth("malformed OAuth callback URL".into()))?;
    Ok(Some(url))
}

fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#12141a;color:#e6e8ee;\
         display:grid;place-items:center;height:100vh;margin:0}}main{{max-width:32rem;\
         padding:2rem;border:1px solid #2a2f3a;border-radius:1rem;background:#181b22}}\
         h1{{font-size:1.25rem;margin:0 0 .75rem}}p{{color:#a9b0c0;line-height:1.5;\
         margin:0}}</style></head><body><main><h1>{}</h1><p>{}</p></main></body></html>",
        html_escape(title),
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpRequest, HttpResponse, HttpTransport};
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    struct TokenTransport {
        response: Mutex<Option<HttpResponse>>,
    }

    impl HttpTransport for TokenTransport {
        fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse> {
            Ok(self.response.lock().unwrap().take().unwrap())
        }
    }

    fn token_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![],
            body: br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#
                .to_vec(),
        }
    }

    fn state_from(url: &str) -> String {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .unwrap()
    }

    fn send(redirect: &str, query: &str) -> String {
        let url = Url::parse(redirect).unwrap();
        let mut stream =
            TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
        stream
            .write_all(
                format!(
                    "GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn completes_flow_and_validates_state() {
        let flow = OAuthFlow::start(OAuthConfig::google("client-id")).unwrap();
        let redirect = flow.redirect_uri().to_string();
        let url = flow.authorization_url().unwrap();
        let state = state_from(&url);
        assert!(url.contains("code_challenge_method=S256"));

        let transport = Arc::new(TokenTransport {
            response: Mutex::new(Some(token_response())),
        });
        let handle =
            std::thread::spawn(move || flow.finish(transport.as_ref(), Duration::from_secs(5)));

        let response = send(&redirect, &format!("code=abc123&state={state}"));
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Google Drive is connected"));

        let tokens = handle.join().unwrap().unwrap();
        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
    }

    #[test]
    fn rejects_state_mismatch() {
        let flow = OAuthFlow::start(OAuthConfig::google("client-id")).unwrap();
        let redirect = flow.redirect_uri().to_string();
        let transport = Arc::new(TokenTransport {
            response: Mutex::new(Some(token_response())),
        });
        let handle =
            std::thread::spawn(move || flow.finish(transport.as_ref(), Duration::from_secs(5)));

        let response = send(&redirect, "code=abc&state=wrong");
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(matches!(handle.join().unwrap(), Err(Error::Auth(_))));
    }

    #[test]
    fn surfaces_google_cancellation() {
        let flow = OAuthFlow::start(OAuthConfig::google("client-id")).unwrap();
        let redirect = flow.redirect_uri().to_string();
        let url = flow.authorization_url().unwrap();
        let state = state_from(&url);
        let transport = Arc::new(TokenTransport {
            response: Mutex::new(Some(token_response())),
        });
        let handle =
            std::thread::spawn(move || flow.finish(transport.as_ref(), Duration::from_secs(5)));

        let response = send(
            &redirect,
            &format!("error=access_denied&error_description=User+cancelled&state={state}"),
        );
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(matches!(
            handle.join().unwrap(),
            Err(Error::Auth(message)) if message.contains("cancelled")
        ));
    }

    #[test]
    fn times_out_without_callback() {
        let flow = OAuthFlow::start(OAuthConfig::google("client-id")).unwrap();
        let transport = Arc::new(TokenTransport {
            response: Mutex::new(Some(token_response())),
        });
        let started = Instant::now();
        let error = flow
            .finish(transport.as_ref(), Duration::from_millis(150))
            .unwrap_err();
        assert!(matches!(error, Error::Auth(_)));
        assert!(started.elapsed() >= Duration::from_millis(140));
    }

    #[test]
    fn binds_only_to_loopback() {
        let flow = OAuthFlow::start(OAuthConfig::google("client-id")).unwrap();
        assert!(flow.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(!flow.redirect_uri().contains("0.0.0.0"));
    }
}
