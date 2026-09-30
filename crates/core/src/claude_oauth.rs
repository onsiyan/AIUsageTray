//! Claude OAuth sign-in and account-scoped access-token rotation.
//!
//! [`login`] signs an account in directly in the default browser with the
//! OAuth authorization-code flow (PKCE + localhost callback), the same shape
//! as the Codex and Antigravity logins. The refreshing provider then rotates
//! only the Claude account being probed, serializes concurrent refreshes per
//! account, and writes the rotated material to the injected secure store. No
//! browser or WebView is needed after the initial sign-in.

use crate::{
    accounts::{AccountId, AccountRecord, CLAUDE},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore, AuthError,
        OAuthBrowserLauncher, OAuthCallbackListenerFactory, OAuthPkcePair,
    },
    transport::{UsageHttpRequest, UsageHttpTransport},
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use url::Url;

pub const DEFAULT_CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const DEFAULT_CLAUDE_OAUTH_TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
/// Authorization endpoint for Claude.ai (Pro/Max/Team) subscriptions.
pub const CLAUDE_AI_OAUTH_AUTHORIZE_ENDPOINT: &str = "https://claude.com/cai/oauth/authorize";
/// Loopback redirect; the port is chosen when the callback listener binds.
const CLAUDE_OAUTH_REDIRECT_URI: &str = "http://localhost:0/callback";
/// The scope set requested by Claude Code's own Claude.ai sign-in.
/// `user:profile` is required by the usage and profile endpoints.
pub const CLAUDE_OAUTH_SCOPES: [&str; 7] = [
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
    "user:plugins",
];

/// Signs a Claude.ai account in through the browser and returns refreshable
/// OAuth material. The caller verifies the identity (for example with
/// [`crate::providers::claude::fetch_oauth_identity`]) before binding the
/// material to a local account.
pub async fn login(
    transport: &dyn UsageHttpTransport,
    callbacks: &dyn OAuthCallbackListenerFactory,
    browser: &dyn OAuthBrowserLauncher,
    timeout: std::time::Duration,
) -> Result<AccountAuthMaterial, AuthError> {
    let pkce = OAuthPkcePair::create();
    let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let redirect = Url::parse(CLAUDE_OAUTH_REDIRECT_URI).expect("static Claude callback URL");
    let mut listener = callbacks.create(&redirect).await?;
    listener.start().await?;
    let redirect_uri = listener.redirect_uri().clone();

    browser
        .open(&claude_authorization_uri(&redirect_uri, &state, &pkce))
        .await?;
    let callback = listener
        .wait(
            &state,
            Duration::from_std(timeout).unwrap_or_else(|_| Duration::minutes(5)),
        )
        .await?;
    if let Some(error) = callback.error {
        return Err(AuthError::Callback(
            callback.error_description.unwrap_or(error),
        ));
    }
    let code = callback
        .code
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| AuthError::Callback("authorization code was missing".to_owned()))?;

    // Claude's token endpoint takes a JSON body that echoes `state`.
    let body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri.as_str(),
        "client_id": DEFAULT_CLAUDE_OAUTH_CLIENT_ID,
        "code_verifier": pkce.verifier,
        "state": state,
    })
    .to_string();
    let response = transport
        .send(UsageHttpRequest {
            method: Method::POST,
            url: Url::parse(DEFAULT_CLAUDE_OAUTH_TOKEN_ENDPOINT).expect("static Claude token URL"),
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
        return Err(AuthError::TokenEndpoint(format!(
            "Claude OAuth code exchange failed (HTTP {})",
            response.status_code
        )));
    }
    parse_login_tokens(&response.body)
}

fn claude_authorization_uri(redirect_uri: &Url, state: &str, pkce: &OAuthPkcePair) -> Url {
    let mut uri =
        Url::parse(CLAUDE_AI_OAUTH_AUTHORIZE_ENDPOINT).expect("static Claude authorize URL");
    uri.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", DEFAULT_CLAUDE_OAUTH_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair("scope", &CLAUDE_OAUTH_SCOPES.join(" "))
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    uri
}

fn parse_login_tokens(body: &str) -> Result<AccountAuthMaterial, AuthError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| AuthError::TokenEndpoint(format!("invalid token response: {error}")))?;
    let access_token = root
        .get("access_token")
        .and_then(Value::as_str)
        .and_then(normalize_oauth_token)
        .ok_or_else(|| {
            AuthError::TokenEndpoint(
                "token response omitted a Claude OAuth access token".to_owned(),
            )
        })?;
    let refresh_token = non_empty(&root, "refresh_token").ok_or_else(|| {
        AuthError::TokenEndpoint("token response omitted a refresh token".to_owned())
    })?;
    let expires_in = root
        .get("expires_in")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(3600)
        .clamp(1, 366 * 24 * 60 * 60);
    let oauth_scopes = non_empty(&root, "scope")
        .map(|scope| scope.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| {
            CLAUDE_OAUTH_SCOPES
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect()
        });
    Ok(AccountAuthMaterial {
        bearer_token: Some(access_token),
        oauth_refresh_token: Some(refresh_token),
        oauth_expires_at_utc: Some(Utc::now() + Duration::seconds(expires_in)),
        oauth_scopes,
        ..AccountAuthMaterial::default()
    })
}

fn non_empty(root: &Value, key: &str) -> Option<String> {
    root.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

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
        // Same JSON request shape as Claude Code's own refresh.
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
            // Bound untrusted values; an enormous lifetime would overflow chrono.
            .clamp(1, 366 * 24 * 60 * 60);
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
        let fallback = material.clone();
        match self.refresh(account.id, material).await {
            Ok(material) => {
                self.clear_refresh_failure(account.id).await;
                Ok(Some(material))
            }
            Err(AuthError::ReauthenticationRequired(message)) => {
                // Another process (CLI, tray, `usage watch`) may have used and
                // rotated the same single-use refresh token after we read it.
                // Use its stored result instead of reporting a sign-out.
                let Some(latest) = self
                    .rotated_material(account.id, &refresh_token, &fallback)
                    .await?
                else {
                    let error = AuthError::ReauthenticationRequired(message);
                    self.record_refresh_failure(account.id, &refresh_token, &error)
                        .await;
                    return Err(error);
                };
                if !self.needs_refresh(&latest) {
                    self.clear_refresh_failure(account.id).await;
                    return Ok(Some(latest));
                }
                let latest_token = latest
                    .oauth_refresh_token
                    .as_deref()
                    .unwrap_or_default()
                    .to_owned();
                match self.refresh(account.id, latest).await {
                    Ok(material) => {
                        self.clear_refresh_failure(account.id).await;
                        Ok(Some(material))
                    }
                    Err(error) => {
                        self.record_refresh_failure(account.id, &latest_token, &error)
                            .await;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.record_refresh_failure(account.id, &refresh_token, &error)
                    .await;
                Err(error)
            }
        }
    }
}

impl ClaudeOAuthRefreshingAuthMaterialProvider {
    /// Returns the stored material when its refresh token differs from the one
    /// that was just rejected.
    async fn rotated_material(
        &self,
        account_id: AccountId,
        rejected_refresh_token: &str,
        chain_material: &AccountAuthMaterial,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let Some(mut stored) = self.store.get(account_id).await? else {
            return Ok(None);
        };
        stored.fill_missing_from(chain_material);
        let rotated = stored
            .oauth_refresh_token
            .as_deref()
            .map(str::trim)
            .is_some_and(|token| !token.is_empty() && token != rejected_refresh_token.trim());
        Ok(rotated.then_some(stored))
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
    let parsed_error_code = serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    // Without an OAuth error body a 400/403 is typically an edge/WAF block,
    // which is temporary and must not permanently block refresh.
    let oauth_rejection = parsed_error_code.is_some();
    let error_code = parsed_error_code.unwrap_or_else(|| "unknown_error".to_owned());
    let message = format!("HTTP {} ({error_code})", response.status_code);
    if response.status_code == 401
        || (oauth_rejection && matches!(response.status_code, 400 | 403))
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
        let fields: Value = serde_json::from_str(&transport.body.lock().unwrap()[0]).unwrap();
        assert_eq!(fields["grant_type"], "refresh_token");
        assert_eq!(fields["refresh_token"], "refresh-old");
        assert_eq!(fields["client_id"], "test-client");

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
    struct RotatedElsewhereTransport {
        account_id: AccountId,
        store: Arc<InMemoryAuthMaterialStore>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl UsageHttpTransport for RotatedElsewhereTransport {
        async fn send(
            &self,
            _request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Another process refreshes first and stores the rotated token.
            self.store
                .save(
                    self.account_id,
                    &AccountAuthMaterial {
                        bearer_token: Some("sk-ant-oat-from-other-process".to_owned()),
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
    async fn refresh_token_rotated_by_another_process_is_used_instead_of_signing_out() {
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
        let transport = Arc::new(RotatedElsewhereTransport {
            account_id: account.id,
            store: store.clone(),
            calls: AtomicUsize::new(0),
        });
        let provider = ClaudeOAuthRefreshingAuthMaterialProvider::with_options(
            source,
            store as Arc<dyn AccountAuthMaterialStore>,
            transport.clone(),
            Url::parse("https://example.test/token").unwrap(),
            "test-client",
            Duration::seconds(60),
        );

        let material = provider.get(&account).await.unwrap().unwrap();
        assert_eq!(
            material.bearer_token.as_deref(),
            Some("sk-ant-oat-from-other-process")
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn html_block_from_the_token_endpoint_is_temporary() {
        let error = refresh_error(&UsageHttpResponse {
            status_code: 403,
            body: "<html>Just a moment...</html>".to_owned(),
            headers: Default::default(),
        });
        assert!(matches!(error, AuthError::TokenEndpoint(_)));
    }

    type OpenedUrl = Arc<std::sync::Mutex<Option<Url>>>;

    struct RecordingBrowser(OpenedUrl);

    #[async_trait]
    impl crate::auth::OAuthBrowserLauncher for RecordingBrowser {
        async fn open(&self, authorization_uri: &Url) -> Result<(), AuthError> {
            *self.0.lock().unwrap() = Some(authorization_uri.clone());
            Ok(())
        }
    }

    /// Answers the callback as the browser would after the user approves.
    struct ApprovingCallbacks(OpenedUrl);

    struct ApprovingListener {
        opened: OpenedUrl,
        redirect: Url,
    }

    #[async_trait]
    impl crate::auth::OAuthCallbackListenerFactory for ApprovingCallbacks {
        async fn create(
            &self,
            redirect_uri: &Url,
        ) -> Result<Box<dyn crate::auth::OAuthCallbackListener>, AuthError> {
            assert_eq!(redirect_uri.path(), "/callback");
            let mut redirect = redirect_uri.clone();
            redirect.set_port(Some(54545)).unwrap();
            Ok(Box::new(ApprovingListener {
                opened: Arc::clone(&self.0),
                redirect,
            }))
        }
    }

    #[async_trait]
    impl crate::auth::OAuthCallbackListener for ApprovingListener {
        fn redirect_uri(&self) -> &Url {
            &self.redirect
        }

        async fn start(&mut self) -> Result<(), AuthError> {
            Ok(())
        }

        async fn wait(
            &mut self,
            expected_state: &str,
            _timeout: Duration,
        ) -> Result<crate::auth::OAuthCallbackResult, AuthError> {
            let opened = self.opened.lock().unwrap().clone().unwrap();
            let state = opened
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned());
            assert_eq!(state.as_deref(), Some(expected_state));
            Ok(crate::auth::OAuthCallbackResult {
                code: Some("auth-code".to_owned()),
                state,
                error: None,
                error_description: None,
            })
        }
    }

    #[tokio::test]
    async fn browser_login_exchanges_the_code_for_refreshable_material() {
        let opened = OpenedUrl::default();
        let transport = FakeTransport {
            response: UsageHttpResponse {
                status_code: 200,
                body: r#"{"access_token":"sk-ant-oat-new","refresh_token":"refresh-new","expires_in":28800,"scope":"user:profile user:inference"}"#.to_owned(),
                headers: Default::default(),
            },
            calls: AtomicUsize::new(0),
            body: std::sync::Mutex::new(Vec::new()),
        };

        let material = login(
            &transport,
            &ApprovingCallbacks(Arc::clone(&opened)),
            &RecordingBrowser(Arc::clone(&opened)),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        let opened = opened.lock().unwrap().clone().unwrap();
        let query = opened.query_pairs().into_owned().collect::<HashMap<_, _>>();
        assert_eq!(opened.origin().ascii_serialization(), "https://claude.com");
        assert_eq!(opened.path(), "/cai/oauth/authorize");
        assert_eq!(query["client_id"], DEFAULT_CLAUDE_OAUTH_CLIENT_ID);
        assert_eq!(query["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(
            query["scope"]
                .split(' ')
                .any(|scope| scope == "user:profile")
        );

        let exchange: Value = serde_json::from_str(&transport.body.lock().unwrap()[0]).unwrap();
        assert_eq!(exchange["grant_type"], "authorization_code");
        assert_eq!(exchange["code"], "auth-code");
        assert_eq!(exchange["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(exchange["state"], query["state"].as_str());
        assert!(
            exchange["code_verifier"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );

        assert_eq!(material.bearer_token.as_deref(), Some("sk-ant-oat-new"));
        assert_eq!(material.oauth_refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(material.oauth_scopes, ["user:profile", "user:inference"]);
        assert!(material.oauth_expires_at_utc.unwrap() > Utc::now());
    }
}
