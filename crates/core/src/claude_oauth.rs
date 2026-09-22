//! Account-scoped Claude OAuth access-token rotation.
//!
//! Claude's local credential file carries an access token, refresh token, and
//! expiry timestamp. This wrapper refreshes only the Claude account that is
//! being probed, serializes concurrent refreshes per account, and writes the
//! rotated material to the injected secure store. It never needs a browser or
//! a WebView after the initial login/import.

use crate::{
    accounts::{AccountId, AccountRecord, CLAUDE},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore, AuthError},
    transport::{UsageHttpRequest, UsageHttpTransport},
};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use url::{Url, form_urlencoded::Serializer};

pub const DEFAULT_CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const DEFAULT_CLAUDE_OAUTH_TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";

/// Adds automatic Claude OAuth refresh to an existing account-material chain.
/// The source may be a composite of the secure store, provider-owned files,
/// and explicit environment overrides; refresh state is persisted only to the
/// supplied account-auth store.
pub struct ClaudeOAuthRefreshingAuthMaterialProvider {
    source: Arc<dyn AccountAuthMaterialProvider>,
    store: Arc<dyn AccountAuthMaterialStore>,
    transport: Arc<dyn UsageHttpTransport>,
    token_endpoint: Url,
    client_id: String,
    refresh_skew: Duration,
    refresh_locks: Mutex<HashMap<AccountId, Arc<Mutex<()>>>>,
    refresh_failures: Mutex<HashMap<AccountId, RefreshFailureState>>,
}

#[derive(Debug, Clone)]
struct RefreshFailureState {
    refresh_token_key: String,
    terminal: bool,
    transient_failures: u32,
    blocked_until: Option<chrono::DateTime<Utc>>,
}

impl ClaudeOAuthRefreshingAuthMaterialProvider {
    pub fn new(
        source: Arc<dyn AccountAuthMaterialProvider>,
        store: Arc<dyn AccountAuthMaterialStore>,
        transport: Arc<dyn UsageHttpTransport>,
    ) -> Self {
        let client_id = std::env::var("CLAUDE_OAUTH_CLIENT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CLAUDE_OAUTH_CLIENT_ID.to_owned());
        Self {
            source,
            store,
            transport,
            token_endpoint: Url::parse(DEFAULT_CLAUDE_OAUTH_TOKEN_ENDPOINT)
                .expect("built-in Claude OAuth token endpoint is valid"),
            client_id,
            refresh_skew: Duration::seconds(60),
            refresh_locks: Mutex::new(HashMap::new()),
            refresh_failures: Mutex::new(HashMap::new()),
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
            refresh_failures: Mutex::new(HashMap::new()),
        }
    }

    async fn account_lock(&self, account_id: AccountId) -> Arc<Mutex<()>> {
        let mut locks = self.refresh_locks.lock().await;
        locks
            .entry(account_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn needs_refresh(&self, material: &AccountAuthMaterial) -> bool {
        let has_refresh_token = material
            .oauth_refresh_token
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        if !has_refresh_token {
            return false;
        }
        if !material.has_bearer_token() {
            return true;
        }
        material
            .oauth_expires_at_utc
            .is_none_or(|expires_at| expires_at <= Utc::now() + self.refresh_skew)
    }

    async fn refresh(
        &self,
        account_id: AccountId,
        mut material: AccountAuthMaterial,
    ) -> Result<AccountAuthMaterial, AuthError> {
        let refresh_token = material
            .oauth_refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AuthError::ReauthenticationRequired(
                    "Claude OAuth refresh token is missing".to_owned(),
                )
            })?;
        let body = {
            let mut form = Serializer::new(String::new());
            form.append_pair("grant_type", "refresh_token");
            form.append_pair("refresh_token", refresh_token);
            form.append_pair("client_id", &self.client_id);
            form.finish()
        };
        let response = self
            .transport
            .send(UsageHttpRequest {
                method: Method::POST,
                url: self.token_endpoint.clone(),
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    (
                        "Content-Type".to_owned(),
                        "application/x-www-form-urlencoded".to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                body: Some(body),
            })
            .await
            .map_err(|error| AuthError::Transport(error.to_string()))?;
        if !response.is_success() {
            return Err(refresh_error(&response));
        }
        let root: Value = serde_json::from_str(&response.body).map_err(|error| {
            AuthError::TokenEndpoint(format!("invalid refresh response: {error}"))
        })?;
        let access_token = root
            .get("access_token")
            .and_then(Value::as_str)
            .and_then(normalize_oauth_token)
            .ok_or_else(|| {
                AuthError::TokenEndpoint(
                    "refresh response omitted a Claude OAuth access token".to_owned(),
                )
            })?;
        let expires_in = root
            .get("expires_in")
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or(3600)
            .max(1);
        material.bearer_token = Some(access_token);
        material.oauth_refresh_token = root
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| Some(refresh_token.to_owned()));
        material.oauth_expires_at_utc = Some(Utc::now() + Duration::seconds(expires_in));
        self.store.save(account_id, &material).await?;
        Ok(material)
    }

    async fn refresh_gate_error(
        &self,
        account_id: AccountId,
        refresh_token: &str,
    ) -> Option<AuthError> {
        let token_key = refresh_token_key(refresh_token);
        let now = Utc::now();
        let mut failures = self.refresh_failures.lock().await;
        if failures
            .get(&account_id)
            .is_some_and(|state| state.refresh_token_key != token_key)
        {
            failures.remove(&account_id);
            return None;
        }
        let state = failures.get_mut(&account_id)?;
        if state.terminal {
            return Some(AuthError::ReauthenticationRequired(
                "Claude OAuth refresh is blocked until the authentication changes".to_owned(),
            ));
        }
        if let Some(blocked_until) = state.blocked_until {
            if blocked_until > now {
                return Some(AuthError::TokenEndpoint(
                    "Claude OAuth refresh is temporarily backed off after a prior failure"
                        .to_owned(),
                ));
            }
            state.blocked_until = None;
        }
        None
    }

    async fn record_refresh_failure(
        &self,
        account_id: AccountId,
        refresh_token: &str,
        error: &AuthError,
    ) {
        let token_key = refresh_token_key(refresh_token);
        let mut failures = self.refresh_failures.lock().await;
        let state = failures
            .entry(account_id)
            .or_insert_with(|| RefreshFailureState {
                refresh_token_key: token_key.clone(),
                terminal: false,
                transient_failures: 0,
                blocked_until: None,
            });
        if state.refresh_token_key != token_key {
            *state = RefreshFailureState {
                refresh_token_key: token_key,
                terminal: false,
                transient_failures: 0,
                blocked_until: None,
            };
        }
        if matches!(error, AuthError::ReauthenticationRequired(_)) {
            state.terminal = true;
            state.blocked_until = None;
            return;
        }
        state.transient_failures = state.transient_failures.saturating_add(1);
        let exponent = state.transient_failures.saturating_sub(1).min(9);
        let delay_seconds = 30_i64.saturating_mul(1_i64 << exponent).min(1_800);
        state.blocked_until = Some(Utc::now() + Duration::seconds(delay_seconds));
    }

    async fn clear_refresh_failure(&self, account_id: AccountId) {
        self.refresh_failures.lock().await.remove(&account_id);
    }
}

#[async_trait]
impl AccountAuthMaterialProvider for ClaudeOAuthRefreshingAuthMaterialProvider {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let Some(material) = self.source.get(account).await? else {
            return Ok(None);
        };
        if account.provider_id != CLAUDE || !self.needs_refresh(&material) {
            return Ok(Some(material));
        }

        let lock = self.account_lock(account.id).await;
        let _guard = lock.lock().await;

        // The secure store may have been updated by another source or a
        // previous refresh while this caller was waiting for the lock. Prefer
        // its OAuth fields, while retaining cookies/user-agent from the chain.
        let material = if let Some(stored) = self.store.get(account.id).await? {
            let mut merged = stored;
            merged.fill_missing_from(&material);
            merged
        } else {
            material
        };
        if !self.needs_refresh(&material) {
            return Ok(Some(material));
        }
        let refresh_token = material
            .oauth_refresh_token
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        if let Some(error) = self.refresh_gate_error(account.id, &refresh_token).await {
            return Err(error);
        }
        match self.refresh(account.id, material).await {
            Ok(material) => {
                self.clear_refresh_failure(account.id).await;
                Ok(Some(material))
            }
            Err(error) => {
                self.record_refresh_failure(account.id, &refresh_token, &error)
                    .await;
                Err(error)
            }
        }
    }
}

fn refresh_token_key(refresh_token: &str) -> String {
    let digest = Sha256::digest(refresh_token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize_oauth_token(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .unwrap_or(value)
        .trim();
    (value.to_ascii_lowercase().starts_with("sk-ant-oat") && !value.is_empty())
        .then_some(value.to_owned())
}

fn refresh_error(response: &crate::transport::UsageHttpResponse) -> AuthError {
    let error_code = serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown_error".to_owned());
    let message = format!("HTTP {} ({error_code})", response.status_code);
    if matches!(response.status_code, 400 | 401 | 403)
        || matches!(
            error_code.as_str(),
            "invalid_grant" | "invalid_client" | "unauthorized_client"
        )
    {
        AuthError::ReauthenticationRequired(format!("Claude OAuth refresh rejected: {message}"))
    } else {
        AuthError::TokenEndpoint(format!("Claude OAuth refresh failed: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::AccountRecord,
        auth::{AccountAuthMaterialStore, InMemoryAuthMaterialStore, StoredAuthMaterialProvider},
        transport::{TransportError, UsageHttpResponse},
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeTransport {
        response: UsageHttpResponse,
        calls: AtomicUsize,
        body: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl UsageHttpTransport for FakeTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(body) = request.body {
                self.body.lock().unwrap().push(body);
            }
            Ok(self.response.clone())
        }
    }

    fn account() -> AccountRecord {
        AccountRecord::create("Claude", "claude@example.com", None, CLAUDE, None).unwrap()
    }

    #[tokio::test]
    async fn expired_access_token_is_rotated_and_saved_once() {
        let account = account();
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        store
            .save(
                account.id,
                &AccountAuthMaterial {
                    bearer_token: Some("sk-ant-oat-old".to_owned()),
                    oauth_refresh_token: Some("refresh-old".to_owned()),
                    oauth_expires_at_utc: Some(Utc::now() - Duration::hours(1)),
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        let source = Arc::new(StoredAuthMaterialProvider::new(store.clone()))
            as Arc<dyn AccountAuthMaterialProvider>;
        let transport = Arc::new(FakeTransport {
            response: UsageHttpResponse {
                status_code: 200,
                body: r#"{"access_token":"sk-ant-oat-new","refresh_token":"refresh-new","expires_in":3600}"#
                    .to_owned(),
                headers: Default::default(),
            },
            calls: AtomicUsize::new(0),
            body: std::sync::Mutex::new(Vec::new()),
        });
        let provider = ClaudeOAuthRefreshingAuthMaterialProvider::with_options(
            source,
            store.clone() as Arc<dyn AccountAuthMaterialStore>,
            transport.clone(),
            Url::parse("https://example.test/token").unwrap(),
            "test-client",
            Duration::seconds(60),
        );

        let material = provider.get(&account).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("sk-ant-oat-new"));
        assert_eq!(material.oauth_refresh_token.as_deref(), Some("refresh-new"));
        assert!(material.oauth_expires_at_utc.unwrap() > Utc::now());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let fields = url::form_urlencoded::parse(transport.body.lock().unwrap()[0].as_bytes())
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            fields.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            fields.get("refresh_token").map(String::as_str),
            Some("refresh-old")
        );
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some("test-client")
        );

        let stored = store.get(account.id).await.unwrap().unwrap();
        assert_eq!(stored.bearer_token.as_deref(), Some("sk-ant-oat-new"));
    }

    #[tokio::test]
    async fn invalid_grant_requires_reauthentication_without_exposing_response_body() {
        let account = account();
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        store
            .save(
                account.id,
                &AccountAuthMaterial {
                    oauth_refresh_token: Some("refresh-old".to_owned()),
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        let source = Arc::new(StoredAuthMaterialProvider::new(store.clone()))
            as Arc<dyn AccountAuthMaterialProvider>;
        let transport = Arc::new(FakeTransport {
            response: UsageHttpResponse {
                status_code: 400,
                body: r#"{"error":"invalid_grant","refresh_token":"do-not-log"}"#.to_owned(),
                headers: Default::default(),
            },
            calls: AtomicUsize::new(0),
            body: std::sync::Mutex::new(Vec::new()),
        });
        let provider = ClaudeOAuthRefreshingAuthMaterialProvider::with_options(
            source,
            store as Arc<dyn AccountAuthMaterialStore>,
            transport,
            Url::parse("https://example.test/token").unwrap(),
            "test-client",
            Duration::seconds(60),
        );
        let error = provider.get(&account).await.unwrap_err();
        assert!(
            matches!(error, AuthError::ReauthenticationRequired(message) if message.contains("invalid_grant") && !message.contains("do-not-log"))
        );
    }
}
