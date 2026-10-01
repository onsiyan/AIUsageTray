//! Provider-owned authentication sources.
//!
//! The runtime never asks a provider for credentials by reaching into another
//! provider's usage database. Sources in this module are limited to explicit
//! account-scoped material, the provider's documented local credential files,
//! and deliberate environment overrides. Browser sessions are imported by the
//! host once and persisted through [`AccountAuthMaterialStore`]; polling does
//! not keep a browser/WebView alive.

use crate::{
    accounts::{ANTIGRAVITY, AccountId, AccountRecord, CLAUDE, OPENCODE_GO, OPENROUTER},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError, CookieValue},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

/// Persists a session imported by the host's browser-login flow. The host may
/// use its own browser automation/cookie reader, but the polling backend only
/// receives the resulting header and stores it in the OS-protected store.
/// No browser process is retained by this function.
pub async fn store_imported_browser_session<S>(
    store: &S,
    account: &AccountRecord,
    cookie_header: &str,
    user_agent: Option<String>,
) -> Result<(), AuthError>
where
    S: crate::auth::AccountAuthMaterialStore + ?Sized,
{
    let mut imported = AccountAuthMaterial::from_cookie_header(cookie_header, user_agent);
    if imported.cookie_header().is_none() {
        return Err(AuthError::CredentialStore(
            "browser session cookie header was empty or invalid".to_owned(),
        ));
    }
    if let Some(existing) = store.get(account.id).await? {
        imported.fill_missing_from(&existing);
    }
    store.save(account.id, &imported).await
}

/// Environment variables are deliberately bound to one account. A global
/// `OPENROUTER_API_KEY` must never be silently applied to every OpenRouter
/// account in a multi-account installation.
#[derive(Debug, Clone)]
pub struct EnvironmentAuthMaterialProvider {
    account_id: AccountId,
    environment: HashMap<String, String>,
}

impl EnvironmentAuthMaterialProvider {
    pub fn from_process(account_id: AccountId) -> Self {
        Self::with_environment(account_id, env::vars())
    }

    pub fn with_environment(
        account_id: AccountId,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            account_id,
            environment: environment.into_iter().collect(),
        }
    }

    fn value(&self, key: &str) -> Option<String> {
        let value = self.environment.get(key)?.trim();
        if value.is_empty() {
            return None;
        }
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value[1..value.len() - 1].trim()
        } else {
            value
        };
        (!value.is_empty()).then_some(value.to_owned())
    }
}

#[async_trait]
impl AccountAuthMaterialProvider for EnvironmentAuthMaterialProvider {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        if account.id != self.account_id {
            return Ok(None);
        }

        let mut material = AccountAuthMaterial::default();
        match account.provider_id.as_str() {
            OPENROUTER => {
                material.bearer_token = self.value("OPENROUTER_API_KEY");
                material.secondary_bearer_token = self.value("OPENROUTER_MANAGEMENT_API_KEY");
            }
            OPENCODE_GO => {
                material.bearer_token = self.value("OPENCODE_API_KEY");
            }
            CLAUDE => {
                // The explicit web credential is a session cookie. We
                // expose a similarly explicit override for hosts that cannot
                // import a browser cookie database; it is never inferred from
                // an Anthropic API key, which is not a Claude web session.
                if let Some(session_key) = self.value("CLAUDE_SESSION_KEY") {
                    material.cookies.push(CookieValue {
                        name: "sessionKey".to_owned(),
                        value: session_key,
                    });
                }
                if let Some(access_token) = self
                    .value("CLAUDE_OAUTH_TOKEN")
                    .and_then(|value| normalize_claude_oauth_token(&value))
                {
                    material.bearer_token = Some(access_token);
                }
                if let Some(admin_key) = self.value("ANTHROPIC_ADMIN_KEY") {
                    material.bearer_token = Some(admin_key);
                }
            }
            // OpenAI usage is intentionally not sourced from OPENAI_API_KEY
            // (the WHAM endpoint requires the ChatGPT OAuth session), and
            // Antigravity has no environment credential.
            _ => {}
        }

        Ok((!material.is_empty()).then_some(material))
    }
}

/// Reads the provider-owned files that are useful as a compatibility fallback
/// after a provider's own login flow has completed. The provider file is bound
/// to one account id by the host; it is not a global multi-account resolver.
#[derive(Debug, Clone)]
pub struct LocalFileAuthMaterialProvider {
    account_id: AccountId,
    home_directory: PathBuf,
    environment: HashMap<String, String>,
}

impl LocalFileAuthMaterialProvider {
    pub fn from_process(account_id: AccountId) -> Self {
        Self::with_home_directory(account_id, default_home_directory(), env::vars())
    }

    pub fn with_home_directory(
        account_id: AccountId,
        home_directory: impl Into<PathBuf>,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            account_id,
            home_directory: home_directory.into(),
            environment: environment.into_iter().collect(),
        }
    }

    fn load(&self, account: &AccountRecord) -> Option<AccountAuthMaterial> {
        match account.provider_id.as_str() {
            ANTIGRAVITY => self.load_antigravity(account),
            OPENCODE_GO => self.load_opencode_go(),
            CLAUDE => self.load_claude_web_session(account),
            _ => None,
        }
    }

    fn load_antigravity(&self, account: &AccountRecord) -> Option<AccountAuthMaterial> {
        let credentials_path = self.home_directory.join(".gemini").join("oauth_creds.json");
        let root = read_json(&credentials_path)?;
        let access_token = string_at(&root, &["access_token", "accessToken"])?;
        let active_email = read_json(
            &self
                .home_directory
                .join(".gemini")
                .join("google_accounts.json"),
        )
        .and_then(|value| string_at(&value, &["active"]))
        .or_else(|| string_at(&root, &["email", "email_address"]));
        // The file belongs to whichever Google account is signed in on this
        // machine. Without a matching email it cannot be attributed to this
        // account and must not be used.
        if !active_email.is_some_and(|email| email.eq_ignore_ascii_case(&account.email)) {
            return None;
        }

        Some(AccountAuthMaterial {
            bearer_token: Some(access_token),
            ..AccountAuthMaterial::default()
        })
    }

    fn load_opencode_go(&self) -> Option<AccountAuthMaterial> {
        let path = self
            .opencode_data_roots()
            .into_iter()
            .map(|root| root.join("opencode").join("auth.json"))
            .find(|path| path.is_file())?;
        let root = read_json(&path)?;
        let entry = root
            .get("opencode-go")
            .or_else(|| root.get("opencode_go"))
            .or_else(|| root.get("opencodeGo"))?;
        let key = string_at(entry, &["key", "apiKey", "api_key", "token", "access"])?;
        Some(AccountAuthMaterial {
            bearer_token: Some(key),
            ..AccountAuthMaterial::default()
        })
    }

    fn load_claude_web_session(&self, account: &AccountRecord) -> Option<AccountAuthMaterial> {
        let root = self.claude_credentials_root();
        let path = root.join(".credentials.json");
        let value = read_json(&path)?;
        if let Some(email) = find_string_by_key(&value, &["email", "email_address", "emailAddress"])
            && !email.eq_ignore_ascii_case(&account.email)
        {
            return None;
        }
        let session_key = find_string_by_key(
            &value,
            &["sessionKey", "session_key", "sessionToken", "session_token"],
        );
        if let Some(session_key) = session_key {
            return Some(AccountAuthMaterial {
                cookies: vec![CookieValue {
                    name: "sessionKey".to_owned(),
                    value: session_key,
                }],
                ..AccountAuthMaterial::default()
            });
        }
        Self::parse_claude_oauth_material(&value)
    }

    /// Reads the OAuth material written by the official Claude Code login
    /// command without binding it to an account record first. A host uses this
    /// only during the explicit add-account flow to call Claude's profile
    /// endpoint, obtain the verified identity, and then create the durable
    /// account-bound provider chain.
    pub fn read_claude_oauth_material(&self) -> Option<AccountAuthMaterial> {
        let path = self.claude_credentials_root().join(".credentials.json");
        let value = read_json(&path)?;
        Self::parse_claude_oauth_material(&value)
    }

    fn parse_claude_oauth_material(value: &Value) -> Option<AccountAuthMaterial> {
        let oauth = value.get("claudeAiOauth").unwrap_or(value);
        let access_token = find_string_by_key(oauth, &["accessToken", "access_token"])
            .and_then(|value| normalize_claude_oauth_token(&value))?;
        let refresh_token = find_string_by_key(oauth, &["refreshToken", "refresh_token"]);
        let expires_at_utc = find_number_by_key(oauth, &["expiresAt", "expires_at"])
            .and_then(|millis| DateTime::<Utc>::from_timestamp_millis(millis as i64));
        let oauth_scopes = find_string_array_by_key(oauth, &["scopes", "scope"]);
        Some(AccountAuthMaterial {
            bearer_token: Some(access_token),
            oauth_refresh_token: refresh_token,
            oauth_expires_at_utc: expires_at_utc,
            oauth_scopes,
            ..AccountAuthMaterial::default()
        })
    }

    fn claude_credentials_root(&self) -> PathBuf {
        let configured = self
            .environment
            .get("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .or_else(|| self.environment.get("CLAUDE_CONFIG_DIR"))
            .filter(|value| !value.is_empty());
        configured
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home_directory.join(".claude"))
    }

    fn opencode_data_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(value) = self
            .environment
            .get("XDG_DATA_HOME")
            .filter(|value| !value.trim().is_empty())
        {
            roots.push(PathBuf::from(value));
        }
        // The provider-owned XDG path is the primary source. Windows OpenCode builds
        // commonly use LOCALAPPDATA/APPDATA, so both are accepted as fallbacks.
        roots.push(self.home_directory.join(".local").join("share"));
        for key in ["LOCALAPPDATA", "APPDATA"] {
            if let Some(value) = self
                .environment
                .get(key)
                .filter(|value| !value.trim().is_empty())
            {
                roots.push(PathBuf::from(value));
            }
        }
        deduplicate_paths(roots)
    }
}

#[async_trait]
impl AccountAuthMaterialProvider for LocalFileAuthMaterialProvider {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        if account.id != self.account_id {
            return Ok(None);
        }
        Ok(self.load(account))
    }
}

fn default_home_directory() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn string_at(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    keys.iter().find_map(|key| {
        object.get(*key).and_then(Value::as_str).and_then(|raw| {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then_some(trimmed.to_owned())
        })
    })
}

fn find_string_by_key(value: &Value, keys: &[&str]) -> Option<String> {
    if let Some(found) = string_at(value, keys) {
        return Some(found);
    }
    match value {
        Value::Object(object) => object
            .values()
            .find_map(|child| find_string_by_key(child, keys)),
        Value::Array(array) => array
            .iter()
            .find_map(|child| find_string_by_key(child, keys)),
        _ => None,
    }
}

fn find_number_by_key(value: &Value, keys: &[&str]) -> Option<f64> {
    if let Some(found) = keys.iter().find_map(|key| {
        value.get(*key).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_i64().map(|number| number as f64))
                .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
        })
    }) {
        return Some(found);
    }
    match value {
        Value::Object(object) => object
            .values()
            .find_map(|child| find_number_by_key(child, keys)),
        Value::Array(array) => array
            .iter()
            .find_map(|child| find_number_by_key(child, keys)),
        _ => None,
    }
}

fn find_string_array_by_key(value: &Value, keys: &[&str]) -> Vec<String> {
    if let Some(values) = keys.iter().find_map(|key| {
        value.get(*key).and_then(|value| {
            value.as_array().map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
        })
    }) {
        return values;
    }
    match value {
        Value::Object(object) => object
            .values()
            .find_map(|child| {
                let values = find_string_array_by_key(child, keys);
                (!values.is_empty()).then_some(values)
            })
            .unwrap_or_default(),
        Value::Array(array) => array
            .iter()
            .find_map(|child| {
                let values = find_string_array_by_key(child, keys);
                (!values.is_empty()).then_some(values)
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn deduplicate_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for path in paths {
        if !result.iter().any(|existing: &PathBuf| existing == &path) {
            result.push(path);
        }
    }
    result
}

fn normalize_claude_oauth_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let token = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();
    token
        .to_ascii_lowercase()
        .starts_with("sk-ant-oat")
        .then_some(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{ANTIGRAVITY, AccountRecord, CLAUDE, OPENAI, OPENCODE_GO, OPENROUTER},
        auth::{
            AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
            CompositeAuthMaterialProvider, InMemoryAuthMaterialStore, StoredAuthMaterialProvider,
        },
    };
    use std::sync::Arc;
    use tempfile::tempdir;

    fn account(provider_id: &str) -> AccountRecord {
        AccountRecord::create("test", "user@example.com", None, provider_id, None).unwrap()
    }

    #[tokio::test]
    async fn environment_source_is_scoped_to_one_account_and_maps_management_key() {
        let primary = account(OPENROUTER);
        let other = account(OPENROUTER);
        let source = EnvironmentAuthMaterialProvider::with_environment(
            primary.id,
            [
                ("OPENROUTER_API_KEY".to_owned(), "sk-primary".to_owned()),
                (
                    "OPENROUTER_MANAGEMENT_API_KEY".to_owned(),
                    "sk-management".to_owned(),
                ),
            ],
        );

        let material = source.get(&primary).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("sk-primary"));
        assert_eq!(
            material.secondary_bearer_token.as_deref(),
            Some("sk-management")
        );
        assert!(source.get(&other).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn claude_oauth_environment_token_strips_bearer_scheme() {
        let account = account(CLAUDE);
        let source = EnvironmentAuthMaterialProvider::with_environment(
            account.id,
            [(
                "CLAUDE_OAUTH_TOKEN".to_owned(),
                "Bearer sk-ant-oat-test".to_owned(),
            )],
        );
        let material = source.get(&account).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("sk-ant-oat-test"));
    }

    #[tokio::test]
    async fn claude_admin_environment_key_is_account_scoped() {
        let account = account(CLAUDE);
        let source = EnvironmentAuthMaterialProvider::with_environment(
            account.id,
            [(
                "ANTHROPIC_ADMIN_KEY".to_owned(),
                "sk-ant-admin-test".to_owned(),
            )],
        );
        let material = source.get(&account).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("sk-ant-admin-test"));
    }

    #[tokio::test]
    async fn local_claude_oauth_file_keeps_refresh_state_and_expiry() {
        let directory = tempdir().unwrap();
        let claude = directory.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let expires_at = Utc::now().timestamp_millis() + 3_600_000;
        std::fs::write(
            claude.join(".credentials.json"),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"Bearer sk-ant-oat-test","refreshToken":"refresh-test","expiresAt":{expires_at},"scopes":["user:profile","user:inference"]}}}}"#
            ),
        )
        .unwrap();

        let account = account(CLAUDE);
        let source = LocalFileAuthMaterialProvider::with_home_directory(
            account.id,
            directory.path(),
            std::iter::empty(),
        );
        let material = source.get(&account).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("sk-ant-oat-test"));
        assert_eq!(
            material.oauth_refresh_token.as_deref(),
            Some("refresh-test")
        );
        assert_eq!(material.oauth_scopes, ["user:profile", "user:inference"]);
        assert!(material.oauth_expires_at_utc.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn local_antigravity_file_rejects_a_different_active_email() {
        let directory = tempdir().unwrap();
        let gemini = directory.path().join(".gemini");
        std::fs::create_dir_all(&gemini).unwrap();
        std::fs::write(
            gemini.join("oauth_creds.json"),
            r#"{"access_token":"token-a"}"#,
        )
        .unwrap();
        std::fs::write(
            gemini.join("google_accounts.json"),
            r#"{"active":"other@example.com","old":[]}"#,
        )
        .unwrap();

        let account = account(ANTIGRAVITY);
        let source = LocalFileAuthMaterialProvider::with_home_directory(
            account.id,
            directory.path(),
            std::iter::empty(),
        );
        assert!(source.get(&account).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn local_opencode_go_file_reads_provider_owned_key() {
        let directory = tempdir().unwrap();
        let root = directory
            .path()
            .join(".local")
            .join("share")
            .join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("auth.json"),
            r#"{"opencode-go":{"key":"go-key"}}"#,
        )
        .unwrap();

        let account = account(OPENCODE_GO);
        let source = LocalFileAuthMaterialProvider::with_home_directory(
            account.id,
            directory.path(),
            std::iter::empty(),
        );
        let material = source.get(&account).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("go-key"));
    }

    #[tokio::test]
    async fn codex_native_auth_files_are_not_a_credential_source() {
        let directory = tempdir().unwrap();
        let native_home = directory.path().join("native-codex-home");
        std::fs::create_dir_all(&native_home).unwrap();
        std::fs::write(
            native_home.join("auth.json"),
            r#"{"tokens":{"access_token":"must-not-be-read"}}"#,
        )
        .unwrap();
        let account = AccountRecord::create("codex", "user@example.com", None, OPENAI, None)
            .unwrap()
            .with_codex_home(Some(native_home.to_string_lossy().as_ref()));
        let source = LocalFileAuthMaterialProvider::with_home_directory(
            account.id,
            directory.path(),
            [(
                "CODEX_HOME".to_owned(),
                native_home.to_string_lossy().into_owned(),
            )],
        );
        assert!(source.get(&account).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn imported_browser_session_is_persisted_without_replacing_existing_bearer() {
        let account = account(OPENROUTER);
        let store = InMemoryAuthMaterialStore::default();
        store
            .save(
                account.id,
                &AccountAuthMaterial {
                    bearer_token: Some("api-key".to_owned()),
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        store_imported_browser_session(
            &store,
            &account,
            "auth=browser-session; theme=dark",
            Some("Browser/1".to_owned()),
        )
        .await
        .unwrap();

        let material = store.get(account.id).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("api-key"));
        assert_eq!(material.user_agent.as_deref(), Some("Browser/1"));
        assert_eq!(
            material.cookie_header().as_deref(),
            Some("auth=browser-session; theme=dark")
        );
    }

    #[tokio::test]
    async fn composite_provider_merges_cookie_and_bearer_sources_without_cross_account_leak() {
        let selected = account(OPENROUTER);
        let other = account(OPENROUTER);
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        store
            .save(
                selected.id,
                &AccountAuthMaterial {
                    cookies: vec![CookieValue {
                        name: "auth".to_owned(),
                        value: "browser-session".to_owned(),
                    }],
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        let stored = Arc::new(StoredAuthMaterialProvider::new(store))
            as Arc<dyn AccountAuthMaterialProvider>;
        let environment = Arc::new(EnvironmentAuthMaterialProvider::with_environment(
            selected.id,
            [("OPENROUTER_API_KEY".to_owned(), "api-key".to_owned())],
        )) as Arc<dyn AccountAuthMaterialProvider>;
        let composite = CompositeAuthMaterialProvider::new([stored, environment]);

        let material = composite.get(&selected).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("api-key"));
        assert_eq!(material.cookies[0].name, "auth");
        assert!(composite.get(&other).await.unwrap().is_none());
    }

    #[test]
    fn source_can_be_instantiated_as_a_trait_object() {
        let account = account(OPENCODE_GO);
        let source = Arc::new(EnvironmentAuthMaterialProvider::with_environment(
            account.id,
            std::iter::empty(),
        )) as Arc<dyn AccountAuthMaterialProvider>;
        assert!(Arc::strong_count(&source) >= 1);
    }

    #[tokio::test]
    async fn local_antigravity_file_without_an_identity_is_not_attributed() {
        let directory = tempdir().unwrap();
        let gemini = directory.path().join(".gemini");
        std::fs::create_dir_all(&gemini).unwrap();
        std::fs::write(
            gemini.join("oauth_creds.json"),
            r#"{"access_token":"token-a"}"#,
        )
        .unwrap();
        let account = account(ANTIGRAVITY);
        let source = LocalFileAuthMaterialProvider::with_home_directory(
            account.id,
            directory.path(),
            std::iter::empty(),
        );
        assert!(source.get(&account).await.unwrap().is_none());
    }
}
