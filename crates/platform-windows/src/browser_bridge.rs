//! Account-add-only browser bridge for Chromium app-bound cookie stores.
//!
//! Chromium's app-bound (`v20`) cookie values cannot be read by a separate
//! desktop process. This module provides a one-shot loopback HTTP listener
//! protected by a random bearer code. The extension can submit only the
//! pending provider's cookies, after its provider session becomes available.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::random;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io, net::SocketAddr, time::Duration};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use usage_monitor_core::{
    accounts::{OPENAI, OPENCODE_GO, normalize_provider_id},
    auth::{AuthError, CookieValue},
};

const BRIDGE_PATH: &str = "/v1/browser-bridge";
const BOOTSTRAP_PATH: &str = "/v1/browser-bridge/start";
const OPENAI_LOGIN_URL: &str = "https://chatgpt.com/";
const OPENCODE_LOGIN_URL: &str = "https://opencode.ai/auth";
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 512 * 1024;
const MAX_COOKIE_COUNT: usize = 128;
const MAX_COOKIE_NAME_BYTES: usize = 256;
const MAX_COOKIE_VALUE_BYTES: usize = 128 * 1024;
const MAX_USER_AGENT_BYTES: usize = 1024;
const PAIRING_CODE_BYTES: usize = 32;

/// JSON shape sent by the browser extension.  Domain/path metadata is kept in
/// the transport contract so the desktop side can reject cookies outside the
/// provider's allow-list before reducing them to its internal header model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowserBridgeCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct BrowserBridgeRequest {
    provider_id: String,
    cookies: Vec<BrowserBridgeCookie>,
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    browser: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
}

/// Validated, account-ready browser material returned by the one-shot bridge.
#[derive(Debug, Clone)]
pub struct BrowserBridgePayload {
    pub provider_id: String,
    pub cookies: Vec<CookieValue>,
    pub user_agent: Option<String>,
    pub browser: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BrowserBridgeInfo {
    pub endpoint: String,
    pub pairing_code: String,
}

/// A pending one-shot bridge session.  It owns the loopback listener and is
/// consumed by [`BrowserBridgeSession::wait`].
#[derive(Debug)]
pub struct BrowserBridgeSession {
    listener: TcpListener,
    provider_id: String,
    pairing_code: String,
    local_addr: SocketAddr,
}

#[derive(Debug, Error)]
pub enum BrowserBridgeError {
    #[error("invalid browser bridge configuration: {0}")]
    InvalidConfiguration(String),
    #[error("could not bind the local browser bridge: {0}")]
    Bind(#[source] io::Error),
    #[error("browser bridge I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("browser bridge payload is invalid: {0}")]
    InvalidPayload(String),
    #[error("browser bridge request is unauthorized")]
    Unauthorized,
    #[error("browser bridge request was not found")]
    NotFound,
    #[error("browser bridge response handled")]
    ResponseHandled,
    #[error("browser bridge timed out after {timeout_seconds}s")]
    TimedOut { timeout_seconds: u64 },
    #[error("browser bridge JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<BrowserBridgeError> for AuthError {
    fn from(error: BrowserBridgeError) -> Self {
        AuthError::Callback(error.to_string())
    }
}

impl BrowserBridgeSession {
    /// Bind a fresh loopback listener and generate a one-time pairing code.
    pub async fn bind(provider_id: &str) -> Result<Self, BrowserBridgeError> {
        let provider_id = normalize_provider_id(provider_id)
            .map_err(|error| BrowserBridgeError::InvalidConfiguration(error.to_string()))?;
        if !matches!(provider_id.as_str(), OPENAI | OPENCODE_GO) {
            return Err(BrowserBridgeError::InvalidConfiguration(
                "the browser bridge supports Codex and OpenCode Go only".to_owned(),
            ));
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(BrowserBridgeError::Bind)?;
        let local_addr = listener.local_addr().map_err(BrowserBridgeError::Bind)?;
        let bytes: [u8; PAIRING_CODE_BYTES] = random();
        let pairing_code = URL_SAFE_NO_PAD.encode(bytes);
        Ok(Self {
            listener,
            provider_id,
            pairing_code,
            local_addr,
        })
    }

    pub fn info(&self) -> BrowserBridgeInfo {
        BrowserBridgeInfo {
            endpoint: format!(
                "http://{}/{}",
                self.local_addr,
                BRIDGE_PATH.trim_start_matches('/')
            ),
            pairing_code: self.pairing_code.clone(),
        }
    }

    pub fn endpoint(&self) -> String {
        self.info().endpoint
    }

    /// A local bootstrap URL. The pairing code stays in the URL fragment, so
    /// it is not sent in the HTTP request or recorded as a server query.
    /// The extension stores it, then the page redirects to provider login.
    pub fn bootstrap_url(&self) -> String {
        format!(
            "http://{}/{}#provider_id={}&pairing={}",
            self.local_addr,
            BOOTSTRAP_PATH.trim_start_matches('/'),
            self.provider_id,
            self.pairing_code
        )
    }

    pub fn pairing_code(&self) -> &str {
        &self.pairing_code
    }

    /// Wait for one explicit extension submission and then close the listener.
    pub async fn wait(
        self,
        wait_timeout: Duration,
    ) -> Result<BrowserBridgePayload, BrowserBridgeError> {
        let timeout_duration = if wait_timeout.is_zero() {
            Duration::from_secs(300)
        } else {
            wait_timeout
        };
        timeout(timeout_duration, self.wait_inner())
            .await
            .map_err(|_| BrowserBridgeError::TimedOut {
                timeout_seconds: timeout_duration.as_secs().max(1),
            })?
    }

    async fn wait_inner(self) -> Result<BrowserBridgePayload, BrowserBridgeError> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(BrowserBridgeError::Io)?;
            match self.handle_connection(&mut stream).await {
                Ok(payload) => return Ok(payload),
                Err(error @ BrowserBridgeError::Unauthorized)
                | Err(error @ BrowserBridgeError::InvalidPayload(_))
                | Err(error @ BrowserBridgeError::Json(_)) => {
                    let status = if matches!(&error, BrowserBridgeError::Unauthorized) {
                        401
                    } else {
                        400
                    };
                    write_json_response(&mut stream, status, &error.to_string()).await?;
                }
                Err(error @ BrowserBridgeError::NotFound) => {
                    write_json_response(&mut stream, 404, &error.to_string()).await?;
                }
                Err(BrowserBridgeError::ResponseHandled) => {}
                Err(error) => {
                    write_json_response(&mut stream, 500, &error.to_string()).await?;
                }
            }
        }
    }

    async fn handle_connection(
        &self,
        stream: &mut TcpStream,
    ) -> Result<BrowserBridgePayload, BrowserBridgeError> {
        let request = read_http_request(stream).await?;
        if request.method == "OPTIONS" {
            write_options_response(stream).await?;
            return Err(BrowserBridgeError::ResponseHandled);
        }
        if request.method == "GET" && request.path == BOOTSTRAP_PATH {
            write_bootstrap_response(stream, &self.provider_id).await?;
            return Err(BrowserBridgeError::ResponseHandled);
        }
        if request.method != "POST" || request.path != BRIDGE_PATH {
            return Err(BrowserBridgeError::NotFound);
        }
        if !constant_time_equal(
            request
                .headers
                .get("authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default(),
            &self.pairing_code,
        ) {
            return Err(BrowserBridgeError::Unauthorized);
        }
        if let Some(origin) = request.headers.get("origin") {
            if !origin.starts_with("chrome-extension://") && !origin.starts_with("moz-extension://")
            {
                return Err(BrowserBridgeError::Unauthorized);
            }
        }
        let incoming: BrowserBridgeRequest = serde_json::from_slice(&request.body)?;
        let payload = validate_payload(incoming, &self.provider_id)?;
        write_json_response(stream, 200, r#"{"accepted":true}"#).await?;
        Ok(payload)
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, BrowserBridgeError> {
    let mut buffer = Vec::with_capacity(4096);
    let header_end = loop {
        if buffer.len() > MAX_HTTP_HEADER_BYTES {
            return Err(BrowserBridgeError::InvalidPayload(
                "HTTP headers are too large".to_owned(),
            ));
        }
        let mut chunk = [0_u8; 2048];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(BrowserBridgeError::Io)?;
        if read == 0 {
            return Err(BrowserBridgeError::InvalidPayload(
                "connection closed before HTTP headers".to_owned(),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let body_start = header_end + 4;
    let header_text = std::str::from_utf8(&buffer[..header_end])
        .map_err(|_| BrowserBridgeError::InvalidPayload("HTTP headers are not UTF-8".to_owned()))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| {
        BrowserBridgeError::InvalidPayload("HTTP request line is missing".to_owned())
    })?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or_default().to_owned();
    let version = request_parts.next().unwrap_or_default();
    if method.is_empty()
        || path.is_empty()
        || version != "HTTP/1.1"
        || request_parts.next().is_some()
    {
        return Err(BrowserBridgeError::InvalidPayload(
            "HTTP request line is invalid".to_owned(),
        ));
    }
    let mut headers = std::collections::BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(BrowserBridgeError::InvalidPayload(
                "HTTP header is invalid".to_owned(),
            ));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let content_length = match headers.get("content-length") {
        Some(value) => value.parse::<usize>().map_err(|_| {
            BrowserBridgeError::InvalidPayload("content-length is invalid".to_owned())
        })?,
        None if matches!(method.as_str(), "OPTIONS" | "GET") => 0,
        None => {
            return Err(BrowserBridgeError::InvalidPayload(
                "content-length is required".to_owned(),
            ));
        }
    };
    if content_length > MAX_HTTP_BODY_BYTES {
        return Err(BrowserBridgeError::InvalidPayload(
            "HTTP body is too large".to_owned(),
        ));
    }
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let mut chunk = vec![0_u8; remaining.min(8192)];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(BrowserBridgeError::Io)?;
        if read == 0 {
            return Err(BrowserBridgeError::InvalidPayload(
                "connection closed before HTTP body".to_owned(),
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

async fn write_options_response(stream: &mut TcpStream) -> Result<(), BrowserBridgeError> {
    let response = concat!(
        "HTTP/1.1 204 No Content\r\n",
        "Access-Control-Allow-Origin: *\r\n",
        "Access-Control-Allow-Methods: POST, OPTIONS\r\n",
        "Access-Control-Allow-Headers: authorization, content-type\r\n",
        "Access-Control-Max-Age: 60\r\n",
        "Connection: close\r\n\r\n"
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(BrowserBridgeError::Io)
}

async fn write_bootstrap_response(
    stream: &mut TcpStream,
    provider_id: &str,
) -> Result<(), BrowserBridgeError> {
    let login_url = provider_login_url(provider_id)?;
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Account bridge</title><p>يتم فتح صفحة تسجيل الدخول...</p><script>setTimeout(() => location.replace('{}'), 1200)</script>",
        login_url
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(BrowserBridgeError::Io)
}

fn provider_login_url(provider_id: &str) -> Result<&'static str, BrowserBridgeError> {
    match provider_id {
        OPENAI => Ok(OPENAI_LOGIN_URL),
        OPENCODE_GO => Ok(OPENCODE_LOGIN_URL),
        _ => Err(BrowserBridgeError::InvalidConfiguration(
            "the browser bridge provider is not supported".to_owned(),
        )),
    }
}

async fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
) -> Result<(), BrowserBridgeError> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(BrowserBridgeError::Io)
}

fn validate_payload(
    incoming: BrowserBridgeRequest,
    expected_provider: &str,
) -> Result<BrowserBridgePayload, BrowserBridgeError> {
    let provider_id = normalize_provider_id(&incoming.provider_id)
        .map_err(|error| BrowserBridgeError::InvalidPayload(error.to_string()))?;
    if provider_id != expected_provider {
        return Err(BrowserBridgeError::InvalidPayload(
            "payload provider does not match the pending account".to_owned(),
        ));
    }
    let (allowed_domains, required_names, provider_name): (&[&str], &[&str], &str) =
        match expected_provider {
            OPENAI => (&["chatgpt.com", "openai.com"], &[], "ChatGPT"),
            OPENCODE_GO => (
                &["opencode.ai", "app.opencode.ai"],
                &["auth", "__host-auth", "__host-console_session"],
                "OpenCode",
            ),
            _ => {
                return Err(BrowserBridgeError::InvalidPayload(
                    "provider is not supported by the browser bridge".to_owned(),
                ));
            }
        };
    if incoming.cookies.is_empty() || incoming.cookies.len() > MAX_COOKIE_COUNT {
        return Err(BrowserBridgeError::InvalidPayload(
            "cookie count is outside the allowed range".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut cookies = Vec::with_capacity(incoming.cookies.len());
    let mut total_bytes = 0usize;
    for cookie in incoming.cookies {
        let domain = normalize_cookie_domain(&cookie.domain).ok_or_else(|| {
            BrowserBridgeError::InvalidPayload("cookie domain is invalid".to_owned())
        })?;
        if !allowed_domains
            .iter()
            .any(|allowed| host_matches(&domain, allowed))
        {
            return Err(BrowserBridgeError::InvalidPayload(format!(
                "payload contains a cookie outside {provider_name} domains"
            )));
        }
        let name = cookie.name.trim();
        let value = cookie.value.trim();
        if name.is_empty()
            || name.len() > MAX_COOKIE_NAME_BYTES
            || value.is_empty()
            || value.len() > MAX_COOKIE_VALUE_BYTES
        {
            return Err(BrowserBridgeError::InvalidPayload(
                "cookie name or value is invalid".to_owned(),
            ));
        }
        total_bytes = total_bytes
            .saturating_add(name.len())
            .saturating_add(value.len())
            .saturating_add(2);
        if total_bytes > MAX_HTTP_BODY_BYTES {
            return Err(BrowserBridgeError::InvalidPayload(
                "selected cookies are too large".to_owned(),
            ));
        }
        if names.insert(name.to_ascii_lowercase()) {
            cookies.push(CookieValue {
                name: name.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if !required_names.is_empty()
        && !required_names
            .iter()
            .any(|required| names.contains(*required))
    {
        return Err(BrowserBridgeError::InvalidPayload(
            "no OpenCode authentication cookie was supplied".to_owned(),
        ));
    }
    let user_agent = incoming.user_agent.and_then(|value| {
        let value = value.trim();
        (!value.is_empty() && value.len() <= MAX_USER_AGENT_BYTES).then_some(value.to_owned())
    });
    let browser = incoming.browser.and_then(|value| {
        let value = value.trim();
        (!value.is_empty() && value.len() <= 64).then_some(value.to_owned())
    });
    let profile_id = incoming.profile_id.and_then(|value| {
        let value = value.trim();
        (!value.is_empty() && value.len() <= 128 && !value.contains(['/', '\\', ':']))
            .then_some(value.to_owned())
    });
    Ok(BrowserBridgePayload {
        provider_id,
        cookies,
        user_agent,
        browser,
        profile_id,
    })
}

fn normalize_cookie_domain(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('.').to_ascii_lowercase();
    (!value.is_empty() && !value.contains(['/', '\\', ':'])).then_some(value)
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = (left.len() ^ right.len()) as u8;
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        difference |= left.get(index).copied().unwrap_or_default()
            ^ right.get(index).copied().unwrap_or_default();
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(cookies: Vec<BrowserBridgeCookie>) -> BrowserBridgeRequest {
        request_for(OPENCODE_GO, cookies)
    }

    fn request_for(provider_id: &str, cookies: Vec<BrowserBridgeCookie>) -> BrowserBridgeRequest {
        BrowserBridgeRequest {
            provider_id: provider_id.to_owned(),
            cookies,
            user_agent: None,
            browser: None,
            profile_id: None,
        }
    }

    #[test]
    fn payload_requires_an_opencode_auth_cookie() {
        let error = validate_payload(
            request(vec![BrowserBridgeCookie {
                name: "unrelated".to_owned(),
                value: "value".to_owned(),
                domain: "opencode.ai".to_owned(),
                path: "/".to_owned(),
            }]),
            "opencodego",
        )
        .unwrap_err();
        assert!(error.to_string().contains("authentication cookie"));
    }

    #[test]
    fn payload_rejects_cookies_outside_provider_domains() {
        let error = validate_payload(
            request(vec![BrowserBridgeCookie {
                name: "auth".to_owned(),
                value: "secret".to_owned(),
                domain: "evil.example".to_owned(),
                path: "/".to_owned(),
            }]),
            "opencodego",
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside OpenCode"));
    }

    #[test]
    fn payload_deduplicates_cookie_names_without_logging_values() {
        let payload = validate_payload(
            request(vec![
                BrowserBridgeCookie {
                    name: "auth".to_owned(),
                    value: "first".to_owned(),
                    domain: ".opencode.ai".to_owned(),
                    path: "/".to_owned(),
                },
                BrowserBridgeCookie {
                    name: "AUTH".to_owned(),
                    value: "second".to_owned(),
                    domain: "app.opencode.ai".to_owned(),
                    path: "/".to_owned(),
                },
            ]),
            "opencodego",
        )
        .unwrap();
        assert_eq!(payload.cookies.len(), 1);
        assert_eq!(payload.cookies[0].value, "first");
    }

    #[test]
    fn pairing_code_comparison_is_exact() {
        assert!(constant_time_equal("abc", "abc"));
        assert!(!constant_time_equal("abc", "abd"));
        assert!(!constant_time_equal("abc", "abc-longer"));
    }

    #[tokio::test]
    async fn bootstrap_then_explicit_post_completes_one_shot_session() {
        let session = BrowserBridgeSession::bind("opencodego").await.unwrap();
        let address = session.local_addr;
        let pairing_code = session.pairing_code.clone();
        let waiter = tokio::spawn(async move { session.wait(Duration::from_secs(3)).await });

        let mut bootstrap = TcpStream::connect(address).await.unwrap();
        bootstrap
            .write_all(
                format!(
                    "GET {BOOTSTRAP_PATH} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut bootstrap_response = Vec::new();
        bootstrap
            .read_to_end(&mut bootstrap_response)
            .await
            .unwrap();
        assert!(bootstrap_response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(String::from_utf8_lossy(&bootstrap_response).contains(OPENCODE_LOGIN_URL));

        let payload = serde_json::to_vec(&request(vec![BrowserBridgeCookie {
            name: "auth".to_owned(),
            value: "session".to_owned(),
            domain: "opencode.ai".to_owned(),
            path: "/".to_owned(),
        }]))
        .unwrap();
        let mut post = TcpStream::connect(address).await.unwrap();
        post.write_all(
            format!(
                "POST {BRIDGE_PATH} HTTP/1.1\r\nHost: {address}\r\nOrigin: chrome-extension://test\r\nAuthorization: Bearer {pairing_code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        post.write_all(&payload).await.unwrap();
        let mut post_response = Vec::new();
        post.read_to_end(&mut post_response).await.unwrap();
        assert!(post_response.starts_with(b"HTTP/1.1 200 OK"));

        let result = waiter.await.unwrap().unwrap();
        assert_eq!(result.provider_id, "opencodego");
        assert_eq!(result.cookies[0].name, "auth");
    }

    #[test]
    fn codex_payload_accepts_only_chatgpt_and_openai_domains() {
        let payload = validate_payload(
            request_for(
                OPENAI,
                vec![
                    BrowserBridgeCookie {
                        name: "session-cookie".to_owned(),
                        value: "session".to_owned(),
                        domain: ".chatgpt.com".to_owned(),
                        path: "/".to_owned(),
                    },
                    BrowserBridgeCookie {
                        name: "device-cookie".to_owned(),
                        value: "device".to_owned(),
                        domain: ".openai.com".to_owned(),
                        path: "/".to_owned(),
                    },
                ],
            ),
            OPENAI,
        )
        .unwrap();

        assert_eq!(payload.provider_id, OPENAI);
        assert_eq!(payload.cookies.len(), 2);
    }

    #[test]
    fn codex_payload_rejects_unrelated_cookie_domains() {
        let error = validate_payload(
            request_for(
                OPENAI,
                vec![BrowserBridgeCookie {
                    name: "session-cookie".to_owned(),
                    value: "session".to_owned(),
                    domain: "evil.example".to_owned(),
                    path: "/".to_owned(),
                }],
            ),
            OPENAI,
        )
        .unwrap_err();

        assert!(error.to_string().contains("outside ChatGPT"));
    }

    #[tokio::test]
    async fn codex_bootstrap_redirects_to_chatgpt_and_keeps_pairing_in_fragment() {
        let session = BrowserBridgeSession::bind(OPENAI).await.unwrap();
        let address = session.local_addr;
        let bootstrap_url = session.bootstrap_url();
        assert!(bootstrap_url.contains("#provider_id=openai&pairing="));
        let waiter = tokio::spawn(async move { session.wait(Duration::from_secs(1)).await });

        let mut bootstrap = TcpStream::connect(address).await.unwrap();
        bootstrap
            .write_all(
                format!(
                    "GET {BOOTSTRAP_PATH} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        bootstrap.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains(OPENAI_LOGIN_URL));
        waiter.abort();
    }
}
