//! Account-scoped OpenCode Console OAuth token rotation.
//!
//! OpenCode's Console login uses the OAuth device authorization grant. The
//! resulting refresh token is kept in the host's secure auth store and rotated
//! per account before it expires.

use crate::{
    accounts::{AccountId, AccountRecord, OPENCODE_GO},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore, AuthError},
    transport::{UsageHttpRequest, UsageHttpTransport},
};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use url::Url;

pub const DEFAULT_OPENCODE_CONSOLE_CLIENT_ID: &str = "opencode-cli";
const DEFAULT_TOKEN_ENDPOINT: &str = "https://opencode.ai/console/auth/device/token";

/// Refreshes only OpenCode Go accounts, leaving all other provider material
/// untouched. Refresh state is persisted through the injected OS-secure store.
pub struct OpenCodeGoOAuthRefreshingAuthMaterialProvider {
    source: Arc<dyn AccountAuthMaterialProvider>,
    store: Arc<dyn AccountAuthMaterialStore>,
    transport: Arc<dyn UsageHttpTransport>,
    token_endpoint: Url,
    client_id: String,
    refresh_skew: Duration,
    refresh_locks: Mutex<HashMap<AccountId, Arc<Mutex<()>>>>,
}

impl OpenCodeGoOAuthRefreshingAuthMaterialProvider {
    pub fn new(
        source: Arc<dyn AccountAuthMaterialProvider>,
        store: Arc<dyn AccountAuthMaterialStore>,
        transport: Arc<dyn UsageHttpTransport>,
    ) -> Self {
        Self {
            source,
            store,
            transport,
            token_endpoint: Url::parse(DEFAULT_TOKEN_ENDPOINT)
                .expect("built-in OpenCode token endpoint is valid"),
            client_id: DEFAULT_OPENCODE_CONSOLE_CLIENT_ID.to_owned(),
            refresh_skew: Duration::minutes(5),
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_options(
        source: Arc<dyn AccountAuthMaterialProvider>,
        store: Arc<dyn AccountAuthMaterialStore>,
        transport: Arc<dyn UsageHttpTransport>,
        token_endpoint: Url,
        client_id: impl Into<String>,
        refresh_skew: Duration,
    ) -> Self {
        Self {
            source,
            store,
            transport,
            token_endpoint,
            client_id: client_id.into(),
            refresh_skew,
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }

    fn needs_refresh(&self, material: &AccountAuthMaterial) -> bool {
        material
            .oauth_refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
            && (!material.has_bearer_token()
                || material
                    .oauth_expires_at_utc
                    .is_none_or(|expiry| expiry <= Utc::now() + self.refresh_skew))
    }

    async fn account_lock(&self, account_id: AccountId) -> Arc<Mutex<()>> {
        self.refresh_locks
            .lock()
            .await
            .entry(account_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn refresh(
        &self,
        account_id: AccountId,
        mut material: AccountAuthMaterial,
    ) -> Result<AccountAuthMaterial, AuthError> {
        let refresh_token = material
            .oauth_refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                AuthError::ReauthenticationRequired(
                    "OpenCode Console refresh token is missing".to_owned(),
                )
            })?;
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": self.client_id,
        })
        .to_string();
        let response = self
            .transport
            .send(UsageHttpRequest {
                method: Method::POST,
                url: self.token_endpoint.clone(),
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                ]
                .into_iter()
                .collect(),
                body: Some(body),
            })
            .await
            .map_err(|error| AuthError::Transport(error.to_string()))?;
        if !response.is_success() {
            let parsed_error_code = serde_json::from_str::<Value>(&response.body)
                .ok()
                .and_then(|root| root.get("error")?.as_str().map(str::to_owned));
            // A 400/403 without an OAuth error body is typically an edge/WAF
            // block, which is temporary and must not sign the account out.
            let oauth_rejection = parsed_error_code.is_some();
            let error_code = parsed_error_code.unwrap_or_else(|| "unknown_error".to_owned());
            let message = format!(
                "OpenCode Console token refresh failed (HTTP {}; {error_code})",
                response.status_code
            );
            return if response.status_code == 401
                || (oauth_rejection && matches!(response.status_code, 400 | 403))
                || matches!(
                    error_code.as_str(),
                    "invalid_grant" | "invalid_client" | "access_denied"
                ) {
                Err(AuthError::ReauthenticationRequired(message))
            } else {
                Err(AuthError::TokenEndpoint(message))
            };
        }

        let root: Value = serde_json::from_str(&response.body).map_err(|error| {
            AuthError::TokenEndpoint(format!("invalid OpenCode token response: {error}"))
        })?;
        let access_token = non_empty_string(&root, "access_token").ok_or_else(|| {
            AuthError::TokenEndpoint("OpenCode token response omitted access_token".to_owned())
        })?;
        let expires_in = root
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| {
                AuthError::TokenEndpoint(
                    "OpenCode token response omitted a valid expires_in".to_owned(),
                )
            })?
            // Bound untrusted values; an enormous lifetime would overflow chrono.
            .min(366 * 24 * 60 * 60);

        material.bearer_token = Some(access_token.clone());
        material.oauth_access_token = Some(access_token);
        material.oauth_refresh_token = non_empty_string(&root, "refresh_token")
            .or_else(|| material.oauth_refresh_token.clone());
        material.oauth_expires_at_utc = Some(Utc::now() + Duration::seconds(expires_in));
        self.store.save(account_id, &material).await?;
        Ok(material)
    }
}

#[async_trait]
impl AccountAuthMaterialProvider for OpenCodeGoOAuthRefreshingAuthMaterialProvider {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let Some(material) = self.source.get(account).await? else {
            return Ok(None);
        };
        if account.provider_id != OPENCODE_GO || !self.needs_refresh(&material) {
            return Ok(Some(material));
        }

        let lock = self.account_lock(account.id).await;
        let _guard = lock.lock().await;
        // Another request may have rotated this account while we waited.
        let latest = match self.store.get(account.id).await? {
            Some(stored) => {
                let mut merged = stored;
                merged.fill_missing_from(&material);
                merged
            }
            None => material,
        };
        if !self.needs_refresh(&latest) {
            return Ok(Some(latest));
        }
        let rejected_token = latest.oauth_refresh_token.clone().unwrap_or_default();
        let chain_material = latest.clone();
        match self.refresh(account.id, latest).await {
            Ok(material) => Ok(Some(material)),
            Err(AuthError::ReauthenticationRequired(message)) => {
                // Another process (CLI, tray, `usage watch`) may have rotated
                // the single-use refresh token after we read it.
                let rotated = match self.store.get(account.id).await? {
                    Some(mut stored) => {
                        stored.fill_missing_from(&chain_material);
                        stored
                            .oauth_refresh_token
                            .as_deref()
                            .map(str::trim)
                            .is_some_and(|token| {
                                !token.is_empty() && token != rejected_token.trim()
                            })
                            .then_some(stored)
                    }
                    None => None,
                };
                match rotated {
                    Some(stored) if !self.needs_refresh(&stored) => Ok(Some(stored)),
                    Some(stored) => self.refresh(account.id, stored).await.map(Some),
                    None => Err(AuthError::ReauthenticationRequired(message)),
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn non_empty_string(root: &Value, key: &str) -> Option<String> {
    root.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::AccountRecord,
        auth::{InMemoryAuthMaterialStore, StoredAuthMaterialProvider},
        transport::{TransportError, UsageHttpResponse},
    };
    use std::sync::Mutex as StdMutex;

    struct RefreshTransport {
        response_body: String,
        requests: StdMutex<Vec<UsageHttpRequest>>,
    }

    #[async_trait]
    impl UsageHttpTransport for RefreshTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            Ok(UsageHttpResponse {
                status_code: 200,
                body: self.response_body.clone(),
                headers: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn expired_open_code_access_token_is_rotated_and_saved_securely() {
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        let account = AccountRecord::create(
            "OpenCode",
            "go@example.com",
            Some("user-1".to_owned()),
            OPENCODE_GO,
            Some("wrk_123".to_owned()),
        )
        .unwrap();
        store
            .save(
                account.id,
                &AccountAuthMaterial {
                    bearer_token: Some("expired-access".to_owned()),
                    oauth_access_token: Some("expired-access".to_owned()),
                    oauth_refresh_token: Some("refresh-1".to_owned()),
                    oauth_expires_at_utc: Some(Utc::now() - Duration::seconds(1)),
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        let transport = Arc::new(RefreshTransport {
            response_body: json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 3600,
            })
            .to_string(),
            requests: StdMutex::new(Vec::new()),
        });
        let source = Arc::new(StoredAuthMaterialProvider::new(store.clone()));
        let provider = OpenCodeGoOAuthRefreshingAuthMaterialProvider::new(
            source,
            store.clone(),
            transport.clone(),
        );

        let refreshed = provider.get(&account).await.unwrap().unwrap();

        assert_eq!(refreshed.bearer_token.as_deref(), Some("access-2"));
        assert_eq!(refreshed.oauth_refresh_token.as_deref(), Some("refresh-2"));
        assert!(refreshed.oauth_expires_at_utc.unwrap() > Utc::now());
        assert_eq!(
            store
                .get(account.id)
                .await
                .unwrap()
                .unwrap()
                .bearer_token
                .as_deref(),
            Some("access-2")
        );
        let requests = transport.requests.lock().unwrap();
        let request = requests.first().unwrap();
        assert_eq!(request.url.as_str(), DEFAULT_TOKEN_ENDPOINT);
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["client_id"], DEFAULT_OPENCODE_CONSOLE_CLIENT_ID);
        assert_eq!(body["refresh_token"], "refresh-1");
    }

    #[tokio::test]
    async fn other_providers_do_not_trigger_open_code_refresh() {
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        let account =
            AccountRecord::create("Codex", "ch@example.com", None, "openai", None).unwrap();
        let material = AccountAuthMaterial {
            bearer_token: Some("expired".to_owned()),
            oauth_refresh_token: Some("refresh".to_owned()),
            oauth_expires_at_utc: Some(Utc::now() - Duration::seconds(1)),
            ..AccountAuthMaterial::default()
        };
        store.save(account.id, &material).await.unwrap();
        let transport = Arc::new(RefreshTransport {
            response_body: "{}".to_owned(),
            requests: StdMutex::new(Vec::new()),
        });
        let provider = OpenCodeGoOAuthRefreshingAuthMaterialProvider::new(
            Arc::new(StoredAuthMaterialProvider::new(store.clone())),
            store,
            transport.clone(),
        );

        let returned = provider.get(&account).await.unwrap().unwrap();

        assert_eq!(returned.bearer_token.as_deref(), Some("expired"));
        assert!(transport.requests.lock().unwrap().is_empty());
    }
    struct RotatedElsewhereTransport {
        account_id: AccountId,
        store: Arc<InMemoryAuthMaterialStore>,
    }

    #[async_trait]
    impl UsageHttpTransport for RotatedElsewhereTransport {
        async fn send(
            &self,
            _request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.store
                .save(
                    self.account_id,
                    &AccountAuthMaterial {
                        bearer_token: Some("access-from-other-process".to_owned()),
                        oauth_access_token: Some("access-from-other-process".to_owned()),
                        oauth_refresh_token: Some("refresh-rotated".to_owned()),
                        oauth_expires_at_utc: Some(Utc::now() + Duration::hours(1)),
                        ..AccountAuthMaterial::default()
                    },
                )
                .await
                .unwrap();
            Ok(UsageHttpResponse {
                status_code: 400,
                body: r#"{"error":"invalid_grant"}"#.to_owned(),
                headers: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn refresh_token_rotated_by_another_process_is_reused() {
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        let account =
            AccountRecord::create("OpenCode", "go@example.com", None, OPENCODE_GO, None).unwrap();
        store
            .save(
                account.id,
                &AccountAuthMaterial {
                    bearer_token: Some("expired-access".to_owned()),
                    oauth_refresh_token: Some("refresh-1".to_owned()),
                    oauth_expires_at_utc: Some(Utc::now() - Duration::seconds(1)),
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        let provider = OpenCodeGoOAuthRefreshingAuthMaterialProvider::with_options(
            Arc::new(StoredAuthMaterialProvider::new(store.clone())),
            store.clone(),
            Arc::new(RotatedElsewhereTransport {
                account_id: account.id,
                store: store.clone(),
            }),
            Url::parse("https://example.test/token").unwrap(),
            "client",
            Duration::minutes(5),
        );

        let material = provider.get(&account).await.unwrap().unwrap();
        assert_eq!(
            material.bearer_token.as_deref(),
            Some("access-from-other-process")
        );
    }
}
