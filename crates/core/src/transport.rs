use async_trait::async_trait;
use reqwest::{Client, Method, header};
use std::collections::BTreeMap;
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct UsageHttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageHttpResponse {
    pub status_code: u16,
    pub body: String,
    pub headers: BTreeMap<String, String>,
}

impl UsageHttpResponse {
    pub fn is_success(&self) -> bool {
        (200..=299).contains(&self.status_code)
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("invalid request URL: {0}")]
    InvalidUrl(String),
    #[error("request serialization failed: {0}")]
    Serialization(String),
    #[error("invalid request header {name}: {reason}")]
    InvalidHeader { name: String, reason: String },
    #[error("HTTP request timed out: {0}")]
    Timeout(String),
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
}

#[async_trait]
pub trait UsageHttpTransport: Send + Sync {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError>;
}

pub struct ReqwestUsageHttpTransport {
    client: Client,
}

impl ReqwestUsageHttpTransport {
    pub fn new(timeout: std::time::Duration) -> Result<Self, reqwest::Error> {
        Self::build(timeout, false)
    }

    /// Creates a transport for the provider's loopback HTTPS endpoint.
    ///
    /// The local language server uses a self-signed certificate. This client is
    /// kept separate from the normal internet client and is only used for
    /// requests pinned to 127.0.0.1 by the Antigravity local probe.
    pub fn new_loopback(timeout: std::time::Duration) -> Result<Self, reqwest::Error> {
        Self::build(timeout, true)
    }

    fn build(
        timeout: std::time::Duration,
        accept_invalid_certificates: bool,
    ) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(accept_invalid_certificates)
            .timeout(timeout)
            .build()?;
        Ok(Self { client })
    }
}

#[async_trait]
impl UsageHttpTransport for ReqwestUsageHttpTransport {
    async fn send(&self, request: UsageHttpRequest) -> Result<UsageHttpResponse, TransportError> {
        let content_type = request.headers.get("Content-Type").cloned();
        let mut builder = self.client.request(request.method, request.url);
        for (name, value) in request.headers {
            if name.eq_ignore_ascii_case("Content-Type") {
                continue;
            }
            let header_name =
                header::HeaderName::from_bytes(name.as_bytes()).map_err(|source| {
                    TransportError::InvalidHeader {
                        name: name.clone(),
                        reason: source.to_string(),
                    }
                })?;
            let header_value = header::HeaderValue::from_str(&value).map_err(|source| {
                TransportError::InvalidHeader {
                    name: name.clone(),
                    reason: source.to_string(),
                }
            })?;
            builder = builder.header(header_name, header_value);
        }
        if let Some(body) = request.body {
            // The caller may need form encoding for OAuth token exchange;
            // JSON remains the default for provider RPC requests.
            builder = builder.header(
                header::CONTENT_TYPE,
                content_type.as_deref().unwrap_or("application/json"),
            );
            builder = builder.body(body);
        }

        let response = builder.send().await?;
        let status_code = response.status().as_u16();
        let headers = collect_response_headers(response.headers());
        let body = response.text().await?;

        Ok(UsageHttpResponse {
            status_code,
            body,
            headers,
        })
    }
}

fn collect_response_headers(headers: &header::HeaderMap) -> BTreeMap<String, String> {
    let mut collected = BTreeMap::new();
    for (name, value) in headers {
        let Ok(value) = value.to_str() else {
            continue;
        };
        let name = name.as_str().to_owned();
        if name.eq_ignore_ascii_case("set-cookie") {
            let joined = collected.entry(name).or_insert_with(String::new);
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(value);
        } else {
            collected.insert(name, value.to_owned());
        }
    }
    collected
}

#[cfg(test)]
mod tests {
    use super::collect_response_headers;
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

    #[test]
    fn repeated_set_cookie_headers_remain_individually_parseable() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("__cf_bm=cloudflare; Path=/; HttpOnly"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("sessionKey=sk-ant-sid-renewed; Path=/; HttpOnly"),
        );

        assert_eq!(
            collect_response_headers(&headers)
                .get("set-cookie")
                .map(String::as_str),
            Some(
                "__cf_bm=cloudflare; Path=/; HttpOnly\nsessionKey=sk-ant-sid-renewed; Path=/; HttpOnly"
            )
        );
    }
}
