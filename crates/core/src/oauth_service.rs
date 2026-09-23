use crate::{
    accounts::{AccountId, OPENAI},
    auth::{
        AuthError, OAuthBrowserLauncher, OAuthCallbackListenerFactory, OAuthCredentialStore,
        OAuthLoginResult, OAuthPkcePair, OAuthProviderDefinition, OAuthTokenProvider,
        OAuthTokenSet, OAuthUserIdentity, StoredOAuthCredential,
    },
    transport::{UsageHttpRequest, UsageHttpTransport},
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration as StdDuration,
};
use tokio::sync::{Mutex, RwLock};
use url::Url;

pub struct OAuthAuthorizationService<T, S, F, B> {
    transport: Arc<T>,
    credentials: Arc<S>,
    callback_factory: Arc<F>,
    browser: Arc<B>,
    access_tokens: RwLock<HashMap<AccountId, OAuthTokenSet>>,
    refresh_locks: Mutex<HashMap<AccountId, Arc<Mutex<()>>>>,
    refresh_skew: Duration,
}

impl<T, S, F, B> OAuthAuthorizationService<T, S, F, B>
where
    T: UsageHttpTransport + 'static,
    S: OAuthCredentialStore + 'static,
    F: OAuthCallbackListenerFactory + 'static,
    B: OAuthBrowserLauncher + 'static,
{
    pub fn new(
        transport: Arc<T>,
        credentials: Arc<S>,
        callback_factory: Arc<F>,
        browser: Arc<B>,
    ) -> Self {
        Self {
            transport,
            credentials,
            callback_factory,
            browser,
            access_tokens: RwLock::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            refresh_skew: Duration::minutes(1),
        }
    }

    pub async fn login(
        &self,
        account_id: AccountId,
        provider: &OAuthProviderDefinition,
        timeout: StdDuration,
    ) -> Result<OAuthLoginResult, AuthError> {
        let pkce = OAuthPkcePair::create();
        let state = random_url_safe();
        let mut listener = self.callback_factory.create(&provider.redirect_uri).await?;
        listener.start().await?;

        let redirect_uri = listener.redirect_uri().clone();
        let authorization_uri = build_authorization_uri(provider, &redirect_uri, &state, &pkce);
        self.browser.open(&authorization_uri).await?;
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
            .ok_or_else(|| AuthError::Callback("authorization code was missing".to_owned()))?;

        let tokens = self
            .exchange_authorization_code(provider, &code, &redirect_uri, &pkce.verifier)
            .await?;
        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            AuthError::TokenEndpoint("authorization did not return a refresh credential".to_owned())
        })?;
        let identity = self.fetch_identity(provider, &tokens).await?;
        let credential = StoredOAuthCredential {
            provider_id: provider.provider_id.clone(),
            refresh_token,
            client_id: Some(provider.client_id.clone()),
            client_secret: provider.client_secret.clone(),
            id_token: tokens.id_token.clone(),
            provider_account_id: identity
                .as_ref()
                .and_then(|identity| identity.provider_account_id.clone()),
            workspace_id: identity
                .as_ref()
                .and_then(|identity| identity.workspace_id.clone()),
            metadata: BTreeMap::new(),
        };
        self.credentials.save(account_id, &credential).await?;
        self.access_tokens
            .write()
            .await
            .insert(account_id, tokens.clone());
        Ok(OAuthLoginResult {
            provider_id: provider.provider_id.clone(),
            tokens,
            credential,
            identity,
        })
    }

    async fn exchange_authorization_code(
        &self,
        provider: &OAuthProviderDefinition,
        code: &str,
        redirect_uri: &Url,
        verifier: &str,
    ) -> Result<OAuthTokenSet, AuthError> {
        let mut fields = provider.token_parameters.clone();
        fields.insert("grant_type".to_owned(), "authorization_code".to_owned());
        fields.insert("code".to_owned(), code.to_owned());
        fields.insert("redirect_uri".to_owned(), redirect_uri.to_string());
        fields.insert("client_id".to_owned(), provider.client_id.clone());
        fields.insert("code_verifier".to_owned(), verifier.to_owned());
        if let Some(secret) = provider.client_secret.as_deref() {
            fields.insert("client_secret".to_owned(), secret.to_owned());
        }
        self.request_token(provider, fields).await
    }

    pub async fn access_token(
        &self,
        account_id: AccountId,
        provider: &OAuthProviderDefinition,
    ) -> Result<OAuthTokenSet, AuthError> {
        let account_lock = {
            let mut locks = self.refresh_locks.lock().await;
            locks
                .entry(account_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = account_lock.lock().await;

        if let Some(tokens) = self.access_tokens.read().await.get(&account_id).cloned()
            && tokens.is_usable(Utc::now(), self.refresh_skew)
        {
            return Ok(tokens);
        }

        let credential = self.credentials.get(account_id).await?.ok_or_else(|| {
            AuthError::ReauthenticationRequired("no stored refresh credential".to_owned())
        })?;
        if credential.provider_id != provider.provider_id {
            return Err(AuthError::ReauthenticationRequired(
                "stored OAuth credential belongs to a different provider".to_owned(),
            ));
        }
        let mut fields = provider.token_parameters.clone();
        fields.insert("grant_type".to_owned(), "refresh_token".to_owned());
        fields.insert("refresh_token".to_owned(), credential.refresh_token.clone());
        fields.insert(
            "client_id".to_owned(),
            credential
                .client_id
                .clone()
                .unwrap_or_else(|| provider.client_id.clone()),
        );
        if let Some(secret) = credential
            .client_secret
            .as_deref()
            .or(provider.client_secret.as_deref())
        {
            fields.insert("client_secret".to_owned(), secret.to_owned());
        }
        let mut tokens = self.request_token(provider, fields).await?;
        if tokens.refresh_token.is_none() {
            tokens.refresh_token = Some(credential.refresh_token.clone());
        }
        if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            let mut updated_credential = credential;
            updated_credential.refresh_token = refresh_token.to_owned();
            if let Some(id_token) = tokens.id_token.as_deref() {
                updated_credential.id_token = Some(id_token.to_owned());
            }
            self.credentials
                .save(account_id, &updated_credential)
                .await?;
        }
        self.access_tokens
            .write()
            .await
            .insert(account_id, tokens.clone());
        Ok(tokens)
    }

    async fn request_token(
        &self,
        provider: &OAuthProviderDefinition,
        fields: BTreeMap<String, String>,
    ) -> Result<OAuthTokenSet, AuthError> {
        let body = form_encode(fields);
        let response = self
            .transport
            .send(UsageHttpRequest {
                method: Method::POST,
                url: provider.token_endpoint.clone(),
                headers: BTreeMap::from([
                    ("Accept".to_owned(), "application/json".to_owned()),
                    (
                        "Content-Type".to_owned(),
                        "application/x-www-form-urlencoded".to_owned(),
                    ),
                ]),
                body: Some(body),
            })
            .await
            .map_err(|error| AuthError::Transport(error.to_string()))?;
        if !response.is_success() {
            let error_code = response.body.parse::<Value>().ok().and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let message = error_code.unwrap_or_else(|| {
                format!("token endpoint returned HTTP {}", response.status_code)
            });
            if matches!(response.status_code, 400 | 401 | 403)
                || matches!(
                    message.as_str(),
                    "invalid_grant" | "invalid_client" | "unauthorized_client"
                )
            {
                return Err(AuthError::ReauthenticationRequired(message));
            }
            return Err(AuthError::TokenEndpoint(message));
        }
        parse_token_response(&response.body)
    }

    async fn fetch_identity(
        &self,
        provider: &OAuthProviderDefinition,
        tokens: &OAuthTokenSet,
    ) -> Result<Option<OAuthUserIdentity>, AuthError> {
        let token_identity =
            identity_from_id_token(&provider.provider_id, tokens.id_token.as_deref());
        let Some(url) = provider.user_info_endpoint.clone() else {
            return Ok(token_identity);
        };
        let mut headers = provider.user_info_headers.clone();
        headers.insert("Accept".to_owned(), "application/json".to_owned());
        headers.insert(
            "Authorization".to_owned(),
            format!("Bearer {}", tokens.access_token),
        );
        let response = self
            .transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers,
                body: None,
            })
            .await
            .map_err(|error| AuthError::Transport(error.to_string()))?;
        if !response.is_success() {
            return Ok(token_identity);
        }
        let value: Value = match serde_json::from_str(&response.body) {
            Ok(value) => value,
            Err(_) => return Ok(token_identity),
        };
        Ok(Some(OAuthUserIdentity {
            email: value
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    token_identity
                        .as_ref()
                        .and_then(|identity| identity.email.clone())
                }),
            provider_account_id: value
                .get("id")
                .or_else(|| value.get("sub"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    token_identity
                        .as_ref()
                        .and_then(|identity| identity.provider_account_id.clone())
                }),
            workspace_id: token_identity
                .as_ref()
                .and_then(|identity| identity.workspace_id.clone()),
            display_name: value
                .get("name")
                .or_else(|| value.get("display_name"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    token_identity
                        .as_ref()
                        .and_then(|identity| identity.display_name.clone())
                }),
        }))
    }
}

#[async_trait]
impl<T, S, F, B> OAuthTokenProvider for OAuthAuthorizationService<T, S, F, B>
where
    T: UsageHttpTransport + 'static,
    S: OAuthCredentialStore + 'static,
    F: OAuthCallbackListenerFactory + 'static,
    B: OAuthBrowserLauncher + 'static,
{
    async fn access_token(
        &self,
        account_id: AccountId,
        provider: &OAuthProviderDefinition,
    ) -> Result<OAuthTokenSet, AuthError> {
        OAuthAuthorizationService::access_token(self, account_id, provider).await
    }
}

fn build_authorization_uri(
    provider: &OAuthProviderDefinition,
    redirect_uri: &Url,
    state: &str,
    pkce: &OAuthPkcePair,
) -> Url {
    let mut parameters = provider.authorization_parameters.clone();
    parameters.insert("response_type".to_owned(), "code".to_owned());
    parameters.insert("client_id".to_owned(), provider.client_id.clone());
    parameters.insert("redirect_uri".to_owned(), redirect_uri.to_string());
    parameters.insert("scope".to_owned(), provider.scopes.join(" "));
    parameters.insert("state".to_owned(), state.to_owned());
    parameters.insert("code_challenge".to_owned(), pkce.challenge.clone());
    parameters.insert("code_challenge_method".to_owned(), "S256".to_owned());

    let mut uri = provider.authorization_endpoint.clone();
    {
        let mut query = uri.query_pairs_mut();
        query.clear();
        for (key, value) in parameters {
            query.append_pair(&key, &value);
        }
    }
    uri
}

fn parse_token_response(body: &str) -> Result<OAuthTokenSet, AuthError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| AuthError::TokenEndpoint(format!("invalid token response: {error}")))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AuthError::TokenEndpoint("token response omitted access_token".to_owned())
        })?;
    let expires_in = value
        .get("expires_in")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(3600)
        .max(1);
    Ok(OAuthTokenSet {
        access_token: access_token.to_owned(),
        expires_at_utc: Utc::now() + Duration::seconds(expires_in),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
        id_token: value
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
        token_type: value
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_owned(),
        scope: value
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn identity_from_id_token(provider_id: &str, id_token: Option<&str>) -> Option<OAuthUserIdentity> {
    let payload = id_token?.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let openai_auth = value.get("https://api.openai.com/auth");
    let openai_profile = value.get("https://api.openai.com/profile");
    Some(OAuthUserIdentity {
        email: value
            .get("email")
            .and_then(Value::as_str)
            .or_else(|| {
                (provider_id == OPENAI)
                    .then(|| openai_profile.and_then(|profile| profile.get("email")))
                    .flatten()
                    .and_then(Value::as_str)
            })
            .map(str::to_owned),
        provider_account_id: if provider_id == OPENAI {
            openai_auth
                .and_then(|auth| auth.get("chatgpt_user_id").or_else(|| auth.get("user_id")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            value.get("sub").and_then(Value::as_str).map(str::to_owned)
        },
        workspace_id: (provider_id == OPENAI)
            .then(|| openai_auth.and_then(|auth| auth.get("chatgpt_account_id")))
            .flatten()
            .and_then(Value::as_str)
            .map(str::to_owned),
        display_name: value.get("name").and_then(Value::as_str).map(str::to_owned),
    })
}

fn form_encode(fields: BTreeMap<String, String>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        serializer.append_pair(&key, &value);
    }
    serializer.finish()
}

fn random_url_safe() -> String {
    let bytes: [u8; 32] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        antigravity::oauth_definition, openai::oauth_definition as codex_oauth_definition,
    };

    #[test]
    fn authorization_uri_contains_required_google_parameters() {
        let provider = oauth_definition();
        let pkce = OAuthPkcePair {
            verifier: "verifier".to_owned(),
            challenge: OAuthPkcePair::compute_s256_challenge("verifier"),
        };
        let redirect_uri = provider.redirect_uri.clone();
        let uri = build_authorization_uri(&provider, &redirect_uri, "state-123", &pkce);
        let query: BTreeMap<String, String> = uri.query_pairs().into_owned().collect();

        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some(provider.client_id.as_str())
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some(redirect_uri.as_str())
        );
        assert_eq!(query.get("state").map(String::as_str), Some("state-123"));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(query.contains_key("code_challenge"));
        assert!(
            query
                .get("scope")
                .is_some_and(|scope| {
                    scope == "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email"
                })
        );
        assert_eq!(
            query.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(
            query.get("prompt").map(String::as_str),
            Some("select_account consent")
        );
        assert!(!query.contains_key("include_granted_scopes"));
    }

    #[test]
    fn codex_authorization_uri_uses_direct_openai_oauth_with_loopback_pkce() {
        let provider = codex_oauth_definition();
        let pkce = OAuthPkcePair {
            verifier: "codex-verifier".to_owned(),
            challenge: OAuthPkcePair::compute_s256_challenge("codex-verifier"),
        };
        let redirect_uri = Url::parse("http://localhost:1457/auth/callback").unwrap();
        let uri = build_authorization_uri(&provider, &redirect_uri, "codex-state", &pkce);
        let query: BTreeMap<String, String> = uri.query_pairs().into_owned().collect();

        assert_eq!(
            uri.origin().ascii_serialization(),
            "https://auth.openai.com"
        );
        assert_eq!(uri.path(), "/oauth/authorize");
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some(provider.client_id.as_str())
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1457/auth/callback")
        );
        assert_eq!(query.get("state").map(String::as_str), Some("codex-state"));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(pkce.challenge.as_str())
        );
        assert_eq!(
            query.get("originator").map(String::as_str),
            Some("codex_usage_monitor_rust")
        );
    }

    #[test]
    fn codex_id_token_separates_workspace_id_from_chatgpt_user_identity() {
        let provider = codex_oauth_definition();
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"email":"codex@example.com","sub":"oidc-subject","https://api.openai.com/auth":{"chatgpt_user_id":"chatgpt-user","chatgpt_account_id":"team-workspace"}}"#,
        );
        let token = format!("header.{payload}.signature");

        let identity = identity_from_id_token(&provider.provider_id, Some(&token)).unwrap();

        assert_eq!(identity.email.as_deref(), Some("codex@example.com"));
        assert_eq!(
            identity.provider_account_id.as_deref(),
            Some("chatgpt-user")
        );
        assert_eq!(identity.workspace_id.as_deref(), Some("team-workspace"));
    }

    #[test]
    fn codex_id_token_uses_legacy_user_id_claim_as_user_identity() {
        let provider = codex_oauth_definition();
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"user_id":"chatgpt-user","chatgpt_account_id":"team-workspace"}}"#,
        );
        let token = format!("header.{payload}.signature");

        let identity = identity_from_id_token(&provider.provider_id, Some(&token)).unwrap();

        assert_eq!(
            identity.provider_account_id.as_deref(),
            Some("chatgpt-user")
        );
        assert_eq!(identity.workspace_id.as_deref(), Some("team-workspace"));
    }

    #[test]
    fn codex_id_token_can_read_email_from_openai_profile_claims() {
        let provider = codex_oauth_definition();
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"sub":"user-subject","https://api.openai.com/profile":{"email":"codex@example.com"},"https://api.openai.com/auth":{}}"#,
        );
        let token = format!("header.{payload}.signature");

        let identity = identity_from_id_token(&provider.provider_id, Some(&token)).unwrap();

        assert_eq!(identity.email.as_deref(), Some("codex@example.com"));
        assert_eq!(identity.provider_account_id, None);
        assert_eq!(identity.workspace_id, None);
    }
}
