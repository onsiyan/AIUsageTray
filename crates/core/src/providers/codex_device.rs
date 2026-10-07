//! Codex sign-in with a one-time code, for computers where the usual
//! sign-in cannot receive its reply on localhost: Windows sometimes reserves
//! the port OpenAI sends it to (Hyper-V, WSL, and Docker do this). It is the
//! flow behind `codex login --device-auth`: OpenAI shows a code, the user
//! enters it on OpenAI's page, and the app then exchanges the authorization
//! it is handed like any other sign-in.

use std::{collections::BTreeMap, time::Duration};

use reqwest::Method;
use serde_json::{Value, json};
use url::Url;

use crate::transport::{UsageHttpRequest, UsageHttpTransport};

const USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// Where the user enters the code.
pub const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
/// The redirect the authorization from this flow is bound to.
pub const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// OpenAI keeps a code valid for 15 minutes.
const CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);
const DEFAULT_INTERVAL: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
}

/// What OpenAI hands over once the code was entered: an authorization code
/// and the PKCE verifier it was made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub authorization_code: String,
    pub code_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexDeviceError {
    /// The code expired before the user entered it.
    Expired,
    Rejected(String),
    Transport(String),
}

impl std::fmt::Display for CodexDeviceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => formatter.write_str("The OpenAI sign-in code expired"),
            Self::Rejected(reason) => write!(formatter, "OpenAI rejected the sign-in: {reason}"),
            Self::Transport(reason) => write!(formatter, "Could not reach OpenAI: {reason}"),
        }
    }
}

impl std::error::Error for CodexDeviceError {}

fn json_request(url: &str, body: Value) -> UsageHttpRequest {
    UsageHttpRequest {
        method: Method::POST,
        url: Url::parse(url).expect("static OpenAI device sign-in URL"),
        headers: BTreeMap::from([
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ]),
        body: Some(body.to_string()),
    }
}

/// Asks OpenAI for a code to show the user.
pub async fn request_device_code(
    transport: &dyn UsageHttpTransport,
    client_id: &str,
) -> Result<DeviceCode, CodexDeviceError> {
    let response = transport
        .send(json_request(
            USER_CODE_URL,
            json!({ "client_id": client_id }),
        ))
        .await
        .map_err(|error| CodexDeviceError::Transport(error.to_string()))?;
    if !response.is_success() {
        return Err(CodexDeviceError::Rejected(format!(
            "HTTP {}",
            response.status_code
        )));
    }
    parse_device_code(&response.body)
}

fn parse_device_code(body: &str) -> Result<DeviceCode, CodexDeviceError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|_| CodexDeviceError::Rejected("unreadable device code".to_owned()))?;
    let text = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| root.get(*name).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let device_auth_id = text(&["device_auth_id"])
        .ok_or_else(|| CodexDeviceError::Rejected("device code is missing its id".to_owned()))?;
    let user_code = text(&["user_code", "usercode"])
        .ok_or_else(|| CodexDeviceError::Rejected("device code is missing the code".to_owned()))?;
    // OpenAI sends the interval as a string; accept a number too.
    let interval = root
        .get("interval")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .unwrap_or(DEFAULT_INTERVAL)
        .clamp(1, 30);
    Ok(DeviceCode {
        device_auth_id,
        user_code,
        interval,
    })
}

/// Waits until the user enters the code on OpenAI's page.
pub async fn poll_for_authorization(
    transport: &dyn UsageHttpTransport,
    code: &DeviceCode,
) -> Result<DeviceAuthorization, CodexDeviceError> {
    let request = json_request(
        TOKEN_URL,
        json!({ "device_auth_id": code.device_auth_id, "user_code": code.user_code }),
    );
    let deadline = tokio::time::Instant::now() + CODE_LIFETIME;
    loop {
        tokio::time::sleep(Duration::from_secs(code.interval)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(CodexDeviceError::Expired);
        }
        let response = transport
            .send(request.clone())
            .await
            .map_err(|error| CodexDeviceError::Transport(error.to_string()))?;
        match poll_outcome(response.status_code, &response.body) {
            Poll::Pending => {}
            Poll::Authorized(authorization) => return Ok(authorization),
            Poll::Failed(error) => return Err(error),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Poll {
    Pending,
    Authorized(DeviceAuthorization),
    Failed(CodexDeviceError),
}

fn poll_outcome(status_code: u16, body: &str) -> Poll {
    // Until the code is entered, OpenAI answers 403 or 404.
    if matches!(status_code, 403 | 404) {
        return Poll::Pending;
    }
    if !(200..=299).contains(&status_code) {
        return Poll::Failed(CodexDeviceError::Rejected(format!("HTTP {status_code}")));
    }
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        return Poll::Failed(CodexDeviceError::Rejected(
            "unreadable authorization".to_owned(),
        ));
    };
    let field = |name: &str| {
        root.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    match (field("authorization_code"), field("code_verifier")) {
        (Some(authorization_code), Some(code_verifier)) => Poll::Authorized(DeviceAuthorization {
            authorization_code,
            code_verifier,
        }),
        _ => Poll::Failed(CodexDeviceError::Rejected(
            "the authorization was incomplete".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{TransportError, UsageHttpResponse};
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[test]
    fn device_codes_accept_both_spellings_and_a_string_interval() {
        let code = parse_device_code(
            r#"{"device_auth_id":"dev-1","user_code":"ABCD-1234","interval":"7"}"#,
        )
        .unwrap();
        assert_eq!(
            code,
            DeviceCode {
                device_auth_id: "dev-1".to_owned(),
                user_code: "ABCD-1234".to_owned(),
                interval: 7,
            }
        );
        let code = parse_device_code(r#"{"device_auth_id":"dev-1","usercode":"WXYZ"}"#).unwrap();
        assert_eq!((code.user_code.as_str(), code.interval), ("WXYZ", 5));
        assert!(parse_device_code(r#"{"user_code":"WXYZ"}"#).is_err());
        assert!(parse_device_code("<html>").is_err());
    }

    #[test]
    fn polling_waits_on_403_and_404_and_needs_both_fields() {
        assert_eq!(poll_outcome(403, ""), Poll::Pending);
        assert_eq!(poll_outcome(404, "not yet"), Poll::Pending);
        assert_eq!(
            poll_outcome(
                200,
                r#"{"authorization_code":"code-1","code_challenge":"c","code_verifier":"verifier-1"}"#
            ),
            Poll::Authorized(DeviceAuthorization {
                authorization_code: "code-1".to_owned(),
                code_verifier: "verifier-1".to_owned(),
            })
        );
        assert!(matches!(
            poll_outcome(200, r#"{"authorization_code":"code-1"}"#),
            Poll::Failed(_)
        ));
        assert!(matches!(poll_outcome(500, ""), Poll::Failed(_)));
    }

    struct CannedTransport {
        body: &'static str,
        requests: Mutex<Vec<UsageHttpRequest>>,
    }

    #[async_trait]
    impl UsageHttpTransport for CannedTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            Ok(UsageHttpResponse {
                status_code: 200,
                body: self.body.to_owned(),
                headers: BTreeMap::new(),
            })
        }
    }

    #[tokio::test]
    async fn the_code_request_sends_the_client_id_as_json() {
        let transport = CannedTransport {
            body: r#"{"device_auth_id":"dev-1","user_code":"ABCD","interval":"5"}"#,
            requests: Mutex::new(Vec::new()),
        };
        request_device_code(&transport, "app_client").await.unwrap();
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[0].url.as_str(), USER_CODE_URL);
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].body.as_deref().unwrap()).unwrap(),
            json!({ "client_id": "app_client" })
        );
    }
}
