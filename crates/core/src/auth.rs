use crate::accounts::{AccountId, AccountRecord, normalize_provider_id};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieValue {
    pub name: String,
    pub value: String,
}

impl CookieValue {
    pub fn to_header_pair(&self) -> String {
        format!("{}={}", self.name, self.value)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountAuthMaterial {
    pub bearer_token: Option<String>,
    pub cookies: Vec<CookieValue>,
    pub user_agent: Option<String>,
    pub secondary_bearer_token: Option<String>,
    /// OAuth rotation state for providers that expose a refresh token. These
    /// fields are account-scoped and are serialized only into the secure auth
    /// store, never into the usage snapshot database.
    #[serde(default)]
    pub oauth_refresh_token: Option<String>,
    #[serde(default)]
    pub oauth_expires_at_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    pub oauth_scopes: Vec<String>,
    /// An account-scoped OAuth access token retained separately when a
    /// provider credential source supplies both an access token and another
    /// credential.
    #[serde(default)]
    pub oauth_access_token: Option<String>,
}

impl AccountAuthMaterial {
    pub fn has_bearer_token(&self) -> bool {
        self.bearer_token
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    }

    pub fn cookie_header(&self) -> Option<String> {
        let header = self
            .cookies
            .iter()
            .filter(|cookie| !cookie.name.trim().is_empty() && !cookie.value.trim().is_empty())
            .map(CookieValue::to_header_pair)
            .collect::<Vec<_>>()
            .join("; ");
        (!header.is_empty()).then_some(header)
    }

    pub fn from_cookie_header(value: &str, user_agent: Option<String>) -> Self {
        let cookies = value
            .split(';')
            .filter_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                (!name.trim().is_empty()).then_some(CookieValue {
                    name: name.trim().to_owned(),
                    value: value.trim().to_owned(),
                })
            })
            .collect();
        Self {
            cookies,
            user_agent,
            ..Self::default()
        }
    }
}

#[async_trait]
pub trait AccountAuthMaterialProvider: Send + Sync {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError>;
}

/// Durable storage for provider credentials that are not part of the account
/// metadata database. Implementations must use an OS-protected secret store;
/// the core crate deliberately does not provide a plaintext-file implementation.
#[async_trait]
pub trait AccountAuthMaterialStore: Send + Sync {
    async fn get(&self, account_id: AccountId) -> Result<Option<AccountAuthMaterial>, AuthError>;
    async fn save(
        &self,
        account_id: AccountId,
        material: &AccountAuthMaterial,
    ) -> Result<(), AuthError>;
    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError>;
}

#[derive(Debug, Default)]
pub struct EmptyAuthMaterialProvider;

#[async_trait]
impl AccountAuthMaterialProvider for EmptyAuthMaterialProvider {
    async fn get(
        &self,
        _account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(None)
    }
}

#[derive(Debug, Default)]
pub struct InMemoryAuthMaterialStore {
    materials: RwLock<HashMap<AccountId, AccountAuthMaterial>>,
}

#[async_trait]
impl AccountAuthMaterialStore for InMemoryAuthMaterialStore {
    async fn get(&self, account_id: AccountId) -> Result<Option<AccountAuthMaterial>, AuthError> {
        Ok(self.materials.read().await.get(&account_id).cloned())
    }

    async fn save(
        &self,
        account_id: AccountId,
        material: &AccountAuthMaterial,
    ) -> Result<(), AuthError> {
        if material.is_empty() {
            return Err(AuthError::CredentialStore(
                "refusing to persist empty authentication material".to_owned(),
            ));
        }
        self.materials
            .write()
            .await
            .insert(account_id, material.clone());
        Ok(())
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError> {
        self.materials.write().await.remove(&account_id);
        Ok(())
    }
}

impl AccountAuthMaterial {
    pub fn is_empty(&self) -> bool {
        !self.has_bearer_token()
            && !self
                .cookies
                .iter()
                .any(|cookie| !cookie.name.trim().is_empty() && !cookie.value.trim().is_empty())
            && !self
                .secondary_bearer_token
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && !self
                .oauth_refresh_token
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && !self
                .oauth_access_token
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }

    /// Merge a lower-priority source without allowing it to replace a value
    /// already supplied by a higher-priority source. Cookie names are merged
    /// independently so a browser session can supply cookies while a secure
    /// OAuth source supplies the bearer token.
    pub fn fill_missing_from(&mut self, lower_priority: &Self) {
        if !self.has_bearer_token() {
            self.bearer_token = lower_priority
                .bearer_token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
        }
        if self.user_agent.is_none() {
            self.user_agent = lower_priority.user_agent.clone();
        }
        if self
            .secondary_bearer_token
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            self.secondary_bearer_token = lower_priority
                .secondary_bearer_token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
        }
        if self
            .oauth_refresh_token
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            self.oauth_refresh_token = lower_priority
                .oauth_refresh_token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
        }
        if self.oauth_expires_at_utc.is_none() {
            self.oauth_expires_at_utc = lower_priority.oauth_expires_at_utc;
        }
        if self.oauth_scopes.is_empty() {
            self.oauth_scopes = lower_priority.oauth_scopes.clone();
        }
        if self
            .oauth_access_token
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            self.oauth_access_token = lower_priority
                .oauth_access_token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
        }
        for cookie in &lower_priority.cookies {
            if cookie.name.trim().is_empty() || cookie.value.trim().is_empty() {
                continue;
            }
            if !self
                .cookies
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&cookie.name))
            {
                self.cookies.push(cookie.clone());
            }
        }
    }
}

/// Reads credentials saved by the host in the secure per-account store.
pub struct StoredAuthMaterialProvider<S> {
    pub store: Arc<S>,
}

impl<S> StoredAuthMaterialProvider<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl<S> AccountAuthMaterialProvider for StoredAuthMaterialProvider<S>
where
    S: AccountAuthMaterialStore + 'static,
{
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        self.store.get(account.id).await
    }
}

/// Composes account-scoped authentication sources in a deterministic order.
/// A source may contribute only one part (for example browser cookies); the
/// result is filled from lower-priority sources without replacing values that
/// were already found. This keeps multi-account credentials isolated while
/// still allowing a secure OAuth token and imported cookies to coexist.
pub struct CompositeAuthMaterialProvider {
    sources: Vec<Arc<dyn AccountAuthMaterialProvider>>,
}

impl CompositeAuthMaterialProvider {
    pub fn new(sources: impl IntoIterator<Item = Arc<dyn AccountAuthMaterialProvider>>) -> Self {
        Self {
            sources: sources.into_iter().collect(),
        }
    }

    pub fn empty() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    pub fn push(&mut self, source: Arc<dyn AccountAuthMaterialProvider>) {
        self.sources.push(source);
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }
}

#[async_trait]
impl AccountAuthMaterialProvider for CompositeAuthMaterialProvider {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let mut merged = AccountAuthMaterial::default();
        let mut deferred_reauthentication = None;

        for source in &self.sources {
            match source.get(account).await {
                Ok(Some(material)) => merged.fill_missing_from(&material),
                Ok(None) => {}
                Err(error @ AuthError::ReauthenticationRequired(_)) => {
                    // A stale source must not prevent a valid lower-priority
                    // source (for example imported browser cookies) from being
                    // used. If nothing else resolves, preserve the useful
                    // reauthentication signal for the provider adapter.
                    deferred_reauthentication.get_or_insert(error);
                }
                Err(error) => return Err(error),
            }
        }

        if !merged.is_empty() {
            return Ok(Some(merged));
        }
        if let Some(error) = deferred_reauthentication {
            return Err(error);
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderDefinition {
    pub provider_id: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub redirect_uri: Url,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub authorization_parameters: BTreeMap<String, String>,
    pub token_parameters: BTreeMap<String, String>,
    pub user_info_endpoint: Option<Url>,
    pub user_info_headers: BTreeMap<String, String>,
}

impl OAuthProviderDefinition {
    pub fn new(
        provider_id: &str,
        authorization_endpoint: Url,
        token_endpoint: Url,
        redirect_uri: Url,
        client_id: &str,
        client_secret: Option<&str>,
        scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, AuthError> {
        Ok(Self {
            provider_id: normalize_provider_id(provider_id)
                .map_err(|error| AuthError::Config(error.to_string()))?,
            authorization_endpoint,
            token_endpoint,
            redirect_uri,
            client_id: client_id.to_owned(),
            client_secret: client_secret.map(str::to_owned),
            scopes: scopes.into_iter().map(Into::into).collect(),
            authorization_parameters: BTreeMap::new(),
            token_parameters: BTreeMap::new(),
            user_info_endpoint: None,
            user_info_headers: BTreeMap::new(),
        })
    }

    pub fn callback_path(&self) -> &str {
        self.redirect_uri.path().trim_end_matches('/')
    }

    pub fn callback_port(&self) -> Option<u16> {
        self.redirect_uri.port_or_known_default()
    }
}

#[derive(Debug, Clone)]
pub struct OAuthTokenSet {
    pub access_token: String,
    pub expires_at_utc: DateTime<Utc>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub scope: Option<String>,
}

impl OAuthTokenSet {
    pub fn is_usable(&self, now: DateTime<Utc>, refresh_skew: Duration) -> bool {
        !self.access_token.trim().is_empty() && self.expires_at_utc > now + refresh_skew
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredOAuthCredential {
    pub provider_id: String,
    pub refresh_token: String,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    pub provider_account_id: Option<String>,
    pub workspace_id: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct OAuthCallbackResult {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

impl OAuthCallbackResult {
    pub fn succeeded(&self) -> bool {
        self.code.as_deref().is_some_and(|value| !value.is_empty()) && self.error.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct OAuthUserIdentity {
    pub email: Option<String>,
    pub provider_account_id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OAuthLoginResult {
    pub provider_id: String,
    pub tokens: OAuthTokenSet,
    pub credential: StoredOAuthCredential,
    pub identity: Option<OAuthUserIdentity>,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("OAuth configuration error: {0}")]
    Config(String),
    #[error("OAuth callback failed: {0}")]
    Callback(String),
    #[error("OAuth authorization requires re-authentication: {0}")]
    ReauthenticationRequired(String),
    #[error("OAuth token endpoint failed: {0}")]
    TokenEndpoint(String),
    #[error("credential store failed: {0}")]
    CredentialStore(String),
    #[error("transport failed: {0}")]
    Transport(String),
    #[error("operation cancelled")]
    Cancelled,
}

#[async_trait]
pub trait OAuthCredentialStore: Send + Sync {
    async fn get(&self, account_id: AccountId) -> Result<Option<StoredOAuthCredential>, AuthError>;
    async fn save(
        &self,
        account_id: AccountId,
        credential: &StoredOAuthCredential,
    ) -> Result<(), AuthError>;
    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError>;
}

#[derive(Debug, Default)]
pub struct InMemoryOAuthCredentialStore {
    credentials: RwLock<HashMap<AccountId, StoredOAuthCredential>>,
}

#[async_trait]
impl OAuthCredentialStore for InMemoryOAuthCredentialStore {
    async fn get(&self, account_id: AccountId) -> Result<Option<StoredOAuthCredential>, AuthError> {
        Ok(self.credentials.read().await.get(&account_id).cloned())
    }

    async fn save(
        &self,
        account_id: AccountId,
        credential: &StoredOAuthCredential,
    ) -> Result<(), AuthError> {
        self.credentials
            .write()
            .await
            .insert(account_id, credential.clone());
        Ok(())
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError> {
        self.credentials.write().await.remove(&account_id);
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct OAuthCredentialProviderRegistry {
    definitions: HashMap<String, OAuthProviderDefinition>,
}

impl OAuthCredentialProviderRegistry {
    pub fn new(definitions: impl IntoIterator<Item = OAuthProviderDefinition>) -> Self {
        let definitions = definitions
            .into_iter()
            .map(|definition| (definition.provider_id.clone(), definition))
            .collect();
        Self { definitions }
    }

    pub fn get(&self, provider_id: &str) -> Option<&OAuthProviderDefinition> {
        self.definitions
            .get(&provider_id.trim().to_ascii_lowercase())
    }
}

#[derive(Debug, Clone)]
pub struct OAuthPkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl OAuthPkcePair {
    pub fn create() -> Self {
        let bytes: [u8; 32] = rand::random();
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = Self::compute_s256_challenge(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    pub fn compute_s256_challenge(verifier: &str) -> String {
        let digest = Sha256::digest(verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(digest)
    }
}

#[async_trait]
pub trait OAuthCallbackListener: Send {
    fn redirect_uri(&self) -> &Url;
    async fn start(&mut self) -> Result<(), AuthError>;
    async fn wait(
        &mut self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<OAuthCallbackResult, AuthError>;
}

#[async_trait]
pub trait OAuthCallbackListenerFactory: Send + Sync {
    async fn create(&self, redirect_uri: &Url)
    -> Result<Box<dyn OAuthCallbackListener>, AuthError>;
}

#[async_trait]
pub trait OAuthBrowserLauncher: Send + Sync {
    async fn open(&self, authorization_uri: &Url) -> Result<(), AuthError>;
}

#[derive(Debug, Clone)]
pub struct AccountOAuthMaterialProvider<S, R> {
    pub authorization: Arc<R>,
    pub credentials: Arc<S>,
    pub providers: Arc<OAuthCredentialProviderRegistry>,
}

#[async_trait]
impl<S, R> AccountAuthMaterialProvider for AccountOAuthMaterialProvider<S, R>
where
    S: OAuthCredentialStore + 'static,
    R: OAuthTokenProvider + 'static,
{
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let Some(definition) = self.providers.get(&account.provider_id) else {
            // This provider can be composed with other account sources. An
            // account owned by a non-OAuth provider is simply not handled by
            // this source; it must not block the next source in the chain.
            return Ok(None);
        };
        let tokens = self
            .authorization
            .access_token(account.id, definition)
            .await?;
        Ok(Some(AccountAuthMaterial {
            bearer_token: Some(tokens.access_token),
            ..AccountAuthMaterial::default()
        }))
    }
}

#[async_trait]
pub trait OAuthTokenProvider: Send + Sync {
    async fn access_token(
        &self,
        account_id: AccountId,
        provider: &OAuthProviderDefinition,
    ) -> Result<OAuthTokenSet, AuthError>;
}
