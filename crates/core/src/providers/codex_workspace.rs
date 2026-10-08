//! Account-scoped Codex workspace labels from the ChatGPT accounts endpoint.
//!
//! This is optional identity enrichment, not a usage source. A missing or
//! unavailable label must not prevent the account's usage probe from running.

use crate::transport::{TransportError, UsageHttpRequest, UsageHttpTransport};
use reqwest::Method;
use serde::Deserialize;
use std::collections::BTreeMap;
use thiserror::Error;
use url::Url;

const ACCOUNTS_URL: &str = "https://chatgpt.com/backend-api/accounts";
const USER_AGENT: &str = "AIUsageTray/0.1";
const PERSONAL_WORKSPACE_LABEL: &str = "Personal";

#[derive(Debug, Error)]
pub enum CodexWorkspaceLookupError {
    #[error("Codex workspace lookup transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("Codex workspace lookup returned HTTP {status_code}")]
    HttpStatus { status_code: u16 },
    #[error("Codex workspace lookup returned an invalid response")]
    InvalidResponse(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct AccountsResponse {
    items: Vec<AccountItem>,
}

#[derive(Debug, Deserialize)]
struct AccountItem {
    id: String,
    name: Option<String>,
}

/// Resolves the visible name for one already-selected Codex workspace.
///
/// The request carries both the account's OAuth bearer token and selected
/// workspace id. It never chooses a different workspace from the response.
/// `None` means the endpoint succeeded but did not list the requested id.
pub async fn resolve_workspace_name(
    transport: &dyn UsageHttpTransport,
    access_token: &str,
    workspace_id: &str,
) -> Result<Option<String>, CodexWorkspaceLookupError> {
    let workspace_id = workspace_id.trim();
    if workspace_id.is_empty() || access_token.trim().is_empty() {
        return Ok(None);
    }

    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_owned(),
        format!("Bearer {}", access_token.trim()),
    );
    headers.insert("ChatGPT-Account-Id".to_owned(), workspace_id.to_owned());
    headers.insert("User-Agent".to_owned(), USER_AGENT.to_owned());
    headers.insert("Accept".to_owned(), "application/json".to_owned());

    let response = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: Url::parse(ACCOUNTS_URL).expect("static Codex accounts URL"),
            headers,
            body: None,
        })
        .await?;
    if !response.is_success() {
        return Err(CodexWorkspaceLookupError::HttpStatus {
            status_code: response.status_code,
        });
    }

    let accounts: AccountsResponse = serde_json::from_str(&response.body)?;
    Ok(accounts
        .items
        .into_iter()
        .find(|account| account.id.trim().eq_ignore_ascii_case(workspace_id))
        .map(|account| {
            account
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(PERSONAL_WORKSPACE_LABEL)
                .to_owned()
        }))
}

#[cfg(test)]
mod tests {
    use super::{CodexWorkspaceLookupError, resolve_workspace_name};
    use crate::transport::{
        TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct StaticTransport {
        response: UsageHttpResponse,
        request: Mutex<Option<UsageHttpRequest>>,
    }

    impl StaticTransport {
        fn new(status_code: u16, body: &str) -> Self {
            Self {
                response: UsageHttpResponse {
                    status_code,
                    body: body.to_owned(),
                    headers: Default::default(),
                },
                request: Mutex::new(None),
            }
        }

        fn request(&self) -> UsageHttpRequest {
            self.request.lock().unwrap().clone().unwrap()
        }
    }

    #[async_trait]
    impl UsageHttpTransport for StaticTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            *self.request.lock().unwrap() = Some(request);
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn resolves_only_the_selected_workspace_and_uses_account_scoped_oauth() {
        let transport = StaticTransport::new(
            200,
            r#"{"items":[{"id":"workspace-other","name":"Other"},{"id":" Workspace-1 ","name":" Team North "}]}"#,
        );

        let name = resolve_workspace_name(&transport, "test-access-token", "workspace-1")
            .await
            .unwrap();

        assert_eq!(name.as_deref(), Some("Team North"));
        let request = transport.request();
        assert_eq!(request.method, reqwest::Method::GET);
        assert_eq!(
            request.url.as_str(),
            "https://chatgpt.com/backend-api/accounts"
        );
        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some("Bearer test-access-token")
        );
        assert_eq!(
            request
                .headers
                .get("ChatGPT-Account-Id")
                .map(String::as_str),
            Some("workspace-1")
        );
        assert_eq!(
            request.headers.get("User-Agent").map(String::as_str),
            Some("AIUsageTray/0.1")
        );
        assert!(request.body.is_none());
    }

    #[tokio::test]
    async fn missing_selected_workspace_name_falls_back_to_personal() {
        let transport = StaticTransport::new(200, r#"{"items":[{"id":"personal-id"}]}"#);

        let name = resolve_workspace_name(&transport, "test-access-token", "personal-id")
            .await
            .unwrap();

        assert_eq!(name.as_deref(), Some("Personal"));
    }

    #[tokio::test]
    async fn does_not_substitute_a_different_workspace_or_accept_bad_responses() {
        let transport = StaticTransport::new(200, r#"{"items":[{"id":"other","name":"Other"}]}"#);
        assert_eq!(
            resolve_workspace_name(&transport, "test-access-token", "selected")
                .await
                .unwrap(),
            None
        );

        let unauthorized = StaticTransport::new(401, "not logged in");
        assert!(matches!(
            resolve_workspace_name(&unauthorized, "test-access-token", "selected").await,
            Err(CodexWorkspaceLookupError::HttpStatus { status_code: 401 })
        ));

        let malformed = StaticTransport::new(200, "{}");
        assert!(matches!(
            resolve_workspace_name(&malformed, "test-access-token", "selected").await,
            Err(CodexWorkspaceLookupError::InvalidResponse(_))
        ));
    }

    #[tokio::test]
    async fn empty_workspace_or_token_skips_the_network_request() {
        let transport = StaticTransport::new(200, r#"{"items":[]}"#);
        assert_eq!(
            resolve_workspace_name(&transport, "test-access-token", " ")
                .await
                .unwrap(),
            None
        );
        assert!(transport.request.lock().unwrap().is_none());
        assert_eq!(
            resolve_workspace_name(&transport, " ", "workspace-1")
                .await
                .unwrap(),
            None
        );
        assert!(transport.request.lock().unwrap().is_none());
    }
}
