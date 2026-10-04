//! Hands a saved Codex account to the Codex desktop app.
//!
//! OpenAI refresh tokens are single use, so the app and this monitor must
//! never hold independent copies of one. Switching therefore *links* the
//! account to Codex's `auth.json`: while the link is valid, that file is the
//! source of truth for the account's tokens. The credential store reads the
//! latest refresh token from it, writes any rotation back into it, and uses
//! Codex's own access token instead of refreshing when that token is still
//! valid.

use crate::{
    accounts::AccountId,
    auth::{AuthError, OAuthTokenSet, StoredOAuthCredential},
    providers::openai::{OpenAiTokenUser, openai_token_user},
    storage::default_accounts_database_path,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const AUTH_FILE: &str = "auth.json";
const LINK_FILE: &str = "codex-desktop-link.json";
const BACKUP_DIRECTORY: &str = "codex-auth-backups";

#[derive(Debug, Clone)]
pub struct CodexDesktopPaths {
    pub codex_home: PathBuf,
    pub state_directory: PathBuf,
}

impl CodexDesktopPaths {
    /// Codex's home (`CODEX_HOME`, else `~/.codex`) and this monitor's data
    /// directory, next to the accounts database.
    pub fn from_environment() -> Self {
        let codex_home = env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .or_else(|| env::var_os("HOME"))
                    .map(|home| PathBuf::from(home).join(".codex"))
            })
            .unwrap_or_else(|| PathBuf::from(".codex"));
        let state_directory = default_accounts_database_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(env::temp_dir);
        Self {
            codex_home,
            state_directory,
        }
    }

    pub fn auth_file(&self) -> PathBuf {
        self.codex_home.join(AUTH_FILE)
    }

    fn link_file(&self) -> PathBuf {
        self.state_directory.join(LINK_FILE)
    }

    fn backup_directory(&self) -> PathBuf {
        self.state_directory.join(BACKUP_DIRECTORY)
    }
}

/// Which saved account currently lives in Codex's `auth.json`. Members of a
/// ChatGPT Team share `chatgpt_account_id`, so the user is recorded too; a
/// link written before that is treated as ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexDesktopLink {
    pub account_id: AccountId,
    pub chatgpt_account_id: String,
    #[serde(default)]
    pub chatgpt_user_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CodexAuthTokens {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

fn load_link(paths: &CodexDesktopPaths) -> Option<CodexDesktopLink> {
    let bytes = fs::read(paths.link_file()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_auth_value(paths: &CodexDesktopPaths) -> Option<Value> {
    let bytes = fs::read(paths.auth_file()).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}

fn read_auth_tokens(paths: &CodexDesktopPaths) -> Option<CodexAuthTokens> {
    let root = read_auth_value(paths)?;
    let tokens = root.get("tokens")?;
    let field = |key: &str| {
        tokens
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Some(CodexAuthTokens {
        id_token: field("id_token"),
        access_token: field("access_token"),
        refresh_token: field("refresh_token"),
        account_id: field("account_id"),
    })
}

impl CodexAuthTokens {
    fn user(&self) -> Option<OpenAiTokenUser> {
        self.id_token
            .as_deref()
            .and_then(openai_token_user)
            .or_else(|| self.access_token.as_deref().and_then(openai_token_user))
    }
}

fn tokens_user(id_token: Option<&str>, access_token: &str) -> Option<OpenAiTokenUser> {
    id_token
        .and_then(openai_token_user)
        .or_else(|| openai_token_user(access_token))
}

/// The link and Codex's tokens, only while `auth.json` still belongs to the
/// linked account: the same workspace *and* the same user. Signing in to
/// another account inside Codex, including a teammate in the same workspace,
/// silently ends the link, and the monitor falls back to its own credential.
fn valid_link(paths: &CodexDesktopPaths) -> Option<(CodexDesktopLink, CodexAuthTokens)> {
    let link = load_link(paths)?;
    let tokens = read_auth_tokens(paths)?;
    let linked_user = OpenAiTokenUser {
        user_id: Some(link.chatgpt_user_id.clone()?),
        email: None,
    };
    (tokens.account_id.as_deref() == Some(link.chatgpt_account_id.as_str())
        && tokens.refresh_token.is_some()
        && tokens
            .user()
            .is_some_and(|user| user.is_same_user(&linked_user)))
    .then_some((link, tokens))
}

/// The saved account Codex is currently signed in with, when it was switched
/// there by this monitor and Codex has not changed account since.
pub fn active_account(paths: &CodexDesktopPaths) -> Option<AccountId> {
    valid_link(paths).map(|(link, _)| link.account_id)
}

/// Replaces a stored credential's tokens with Codex's latest ones when the
/// account is linked. Returns whether anything changed.
pub fn overlay_linked_credential(
    paths: &CodexDesktopPaths,
    account_id: AccountId,
    credential: &mut StoredOAuthCredential,
) -> bool {
    let Some((link, tokens)) = valid_link(paths) else {
        return false;
    };
    if link.account_id != account_id {
        return false;
    }
    // Never adopt tokens of a different user than the stored credential's,
    // whatever the link says.
    if let Some(stored_user) = credential.id_token.as_deref().and_then(openai_token_user)
        && !tokens
            .user()
            .is_some_and(|user| user.is_same_user(&stored_user))
    {
        return false;
    }
    let mut changed = false;
    if let Some(refresh_token) = tokens.refresh_token
        && refresh_token != credential.refresh_token
    {
        credential.refresh_token = refresh_token;
        changed = true;
    }
    if tokens.id_token.is_some() && tokens.id_token != credential.id_token {
        credential.id_token = tokens.id_token;
        changed = true;
    }
    changed
}

/// Writes a rotated credential into `auth.json` when the account is linked,
/// so Codex keeps working with the only valid refresh token.
pub fn propagate_linked_credential(
    paths: &CodexDesktopPaths,
    account_id: AccountId,
    credential: &StoredOAuthCredential,
) -> Result<(), AuthError> {
    let Some((link, tokens)) = valid_link(paths) else {
        return Ok(());
    };
    if link.account_id != account_id
        || tokens.refresh_token.as_deref() == Some(credential.refresh_token.as_str())
    {
        return Ok(());
    }
    let mut root = read_auth_value(paths).unwrap_or_else(|| json!({}));
    let object = root.as_object_mut().expect("auth.json root is an object");
    let tokens = object
        .entry("tokens")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| AuthError::CredentialStore("Codex auth.json tokens are invalid".into()))?;
    tokens.insert(
        "refresh_token".to_owned(),
        Value::String(credential.refresh_token.clone()),
    );
    if let Some(id_token) = &credential.id_token {
        tokens.insert("id_token".to_owned(), Value::String(id_token.clone()));
    }
    write_atomically(&paths.auth_file(), &root)
}

/// Codex's current access token for a linked account, so the monitor can use
/// it instead of spending the shared refresh token.
pub fn linked_access_token(
    paths: &CodexDesktopPaths,
    account_id: AccountId,
) -> Option<OAuthTokenSet> {
    let (link, tokens) = valid_link(paths)?;
    if link.account_id != account_id {
        return None;
    }
    let access_token = tokens.access_token?;
    let expires_at_utc = jwt_expiry(&access_token)?;
    Some(OAuthTokenSet {
        access_token,
        expires_at_utc,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        token_type: "Bearer".to_owned(),
        scope: None,
    })
}

/// Ends the link without touching `auth.json`. Used when the linked account
/// is removed from the monitor.
pub fn forget_link_for(paths: &CodexDesktopPaths, account_id: AccountId) {
    if load_link(paths).is_some_and(|link| link.account_id == account_id) {
        let _ = fs::remove_file(paths.link_file());
    }
}

/// Makes Codex use `account_id` by writing freshly refreshed tokens into
/// `auth.json` and linking the account. The caller must have already synced
/// the previously linked account (reading it through the credential store
/// does that) and must restart Codex afterwards.
pub fn install_account(
    paths: &CodexDesktopPaths,
    account_id: AccountId,
    chatgpt_account_id: Option<&str>,
    tokens: &OAuthTokenSet,
) -> Result<CodexDesktopLink, AuthError> {
    let refresh_token = tokens
        .refresh_token
        .clone()
        .ok_or_else(|| AuthError::CredentialStore("no refresh token to hand to Codex".into()))?;
    let chatgpt_account_id = chatgpt_account_id
        .map(str::to_owned)
        .or_else(|| {
            tokens
                .id_token
                .as_deref()
                .and_then(chatgpt_account_id_from_id_token)
        })
        .ok_or_else(|| {
            AuthError::CredentialStore("the account's ChatGPT workspace is unknown".into())
        })?;
    let chatgpt_user_id = tokens_user(tokens.id_token.as_deref(), &tokens.access_token)
        .and_then(|user| user.user_id)
        .ok_or_else(|| {
            AuthError::CredentialStore("the account's ChatGPT user is unknown".into())
        })?;

    let existing = read_auth_value(paths);
    if existing.is_some() && valid_link(paths).is_none() {
        backup_foreign_auth_file(paths)?;
    }

    let mut root = existing.unwrap_or_else(|| json!({}));
    let object: &mut Map<String, Value> = root.as_object_mut().expect("object root");
    object.insert("auth_mode".to_owned(), Value::String("chatgpt".to_owned()));
    object.insert("OPENAI_API_KEY".to_owned(), Value::Null);
    object.insert(
        "tokens".to_owned(),
        json!({
            "id_token": tokens.id_token,
            "access_token": tokens.access_token,
            "refresh_token": refresh_token,
            "account_id": chatgpt_account_id,
        }),
    );
    object.insert(
        "last_refresh".to_owned(),
        Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)),
    );
    fs::create_dir_all(&paths.codex_home).map_err(store_error)?;
    write_atomically(&paths.auth_file(), &root)?;

    let link = CodexDesktopLink {
        account_id,
        chatgpt_account_id,
        chatgpt_user_id: Some(chatgpt_user_id),
    };
    fs::create_dir_all(&paths.state_directory).map_err(store_error)?;
    let bytes = serde_json::to_vec_pretty(&link).map_err(|error| store_error(error.into()))?;
    fs::write(paths.link_file(), bytes).map_err(store_error)?;
    Ok(link)
}

/// Fails when Codex is configured to keep its sign-in in the OS keyring
/// (`cli_auth_credentials_store = "keyring"` or `"auto"`): it would then
/// ignore `auth.json`, and switching would silently do nothing.
pub fn ensure_file_credential_store(paths: &CodexDesktopPaths) -> Result<(), AuthError> {
    let Ok(config) = fs::read_to_string(paths.codex_home.join("config.toml")) else {
        return Ok(());
    };
    match configured_credential_store(&config).as_deref() {
        Some("keyring" | "auto") => Err(AuthError::Config(
            "Codex keeps its sign-in in the system keyring (cli_auth_credentials_store in \
             config.toml), so switching accounts from here is not supported"
                .to_owned(),
        )),
        _ => Ok(()),
    }
}

/// The top-level `cli_auth_credentials_store` value, ignoring table sections.
fn configured_credential_store(config: &str) -> Option<String> {
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            return None;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "cli_auth_credentials_store" {
            let value = value.split('#').next().unwrap_or_default().trim();
            return Some(value.trim_matches(['"', '\'']).to_ascii_lowercase());
        }
    }
    None
}

/// Keeps a copy of an `auth.json` this monitor did not write, so switching
/// never loses a sign-in the user made directly in Codex.
fn backup_foreign_auth_file(paths: &CodexDesktopPaths) -> Result<(), AuthError> {
    let directory = paths.backup_directory();
    fs::create_dir_all(&directory).map_err(store_error)?;
    let name = format!("auth-{}.json", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    fs::copy(paths.auth_file(), directory.join(name)).map_err(store_error)?;
    Ok(())
}

fn write_atomically(path: &Path, value: &Value) -> Result<(), AuthError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| store_error(error.into()))?;
    let temporary = path.with_extension("json.monitor-tmp");
    fs::write(&temporary, bytes).map_err(store_error)?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        store_error(error)
    })
}

fn store_error(error: std::io::Error) -> AuthError {
    AuthError::CredentialStore(format!("Codex auth file: {error}"))
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_expiry(token: &str) -> Option<DateTime<Utc>> {
    let exp = jwt_claims(token)?.get("exp")?.as_i64()?;
    Utc.timestamp_opt(exp, 0).single()
}

fn chatgpt_account_id_from_id_token(id_token: &str) -> Option<String> {
    jwt_claims(id_token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn paths(directory: &Path) -> CodexDesktopPaths {
        CodexDesktopPaths {
            codex_home: directory.join("codex"),
            state_directory: directory.join("state"),
        }
    }

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    fn tokens(refresh: &str) -> OAuthTokenSet {
        OAuthTokenSet {
            access_token: jwt(json!({"exp": 4_000_000_000_i64})),
            expires_at_utc: Utc::now(),
            refresh_token: Some(refresh.to_owned()),
            id_token: Some(jwt(json!({
                "email": "me@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "ws-1",
                    "chatgpt_user_id": "user-me",
                },
            }))),
            token_type: "Bearer".to_owned(),
            scope: None,
        }
    }

    fn credential(refresh: &str) -> StoredOAuthCredential {
        StoredOAuthCredential {
            provider_id: "openai".to_owned(),
            refresh_token: refresh.to_owned(),
            client_id: None,
            client_secret: None,
            id_token: None,
            provider_account_id: None,
            workspace_id: None,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn keyring_credential_store_blocks_switching() {
        assert_eq!(
            configured_credential_store(
                "model = \"x\"\ncli_auth_credentials_store = \"keyring\" # os\n"
            ),
            Some("keyring".to_owned())
        );
        assert_eq!(
            configured_credential_store("[profiles.a]\ncli_auth_credentials_store = \"keyring\"\n"),
            None,
            "a value inside a table is not the global setting"
        );

        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        fs::create_dir_all(&paths.codex_home).unwrap();
        assert!(ensure_file_credential_store(&paths).is_ok());
        fs::write(
            paths.codex_home.join("config.toml"),
            "cli_auth_credentials_store = \"auto\"\n",
        )
        .unwrap();
        assert!(ensure_file_credential_store(&paths).is_err());
        fs::write(
            paths.codex_home.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        assert!(ensure_file_credential_store(&paths).is_ok());
    }

    #[test]
    fn install_links_the_account_and_backs_up_a_foreign_sign_in() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        fs::create_dir_all(&paths.codex_home).unwrap();
        fs::write(
            paths.auth_file(),
            r#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"theirs","account_id":"other"},"extra":1}"#,
        )
        .unwrap();
        let account = AccountId(Uuid::new_v4());

        install_account(&paths, account, None, &tokens("mine")).unwrap();

        assert_eq!(active_account(&paths), Some(account));
        let written = read_auth_value(&paths).unwrap();
        assert_eq!(written["tokens"]["refresh_token"], "mine");
        assert_eq!(written["tokens"]["account_id"], "ws-1");
        assert_eq!(written["extra"], 1, "unrelated Codex settings are kept");
        assert_eq!(fs::read_dir(paths.backup_directory()).unwrap().count(), 1);
        assert!(linked_access_token(&paths, account).is_some());
    }

    #[test]
    fn linked_tokens_flow_both_ways_until_codex_changes_account() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        let account = AccountId(Uuid::new_v4());
        install_account(&paths, account, Some("ws-1"), &tokens("first")).unwrap();

        // Codex rotated its token: the monitor picks it up.
        let mut root = read_auth_value(&paths).unwrap();
        root["tokens"]["refresh_token"] = json!("rotated-by-codex");
        write_atomically(&paths.auth_file(), &root).unwrap();
        let mut stored = credential("first");
        assert!(overlay_linked_credential(&paths, account, &mut stored));
        assert_eq!(stored.refresh_token, "rotated-by-codex");

        // The monitor rotated it: Codex gets it.
        propagate_linked_credential(&paths, account, &credential("rotated-by-monitor")).unwrap();
        assert_eq!(
            read_auth_value(&paths).unwrap()["tokens"]["refresh_token"],
            "rotated-by-monitor"
        );

        // Another account signed in inside Codex: the link no longer applies.
        let mut root = read_auth_value(&paths).unwrap();
        root["tokens"]["account_id"] = json!("someone-else");
        write_atomically(&paths.auth_file(), &root).unwrap();
        let mut stored = credential("rotated-by-monitor");
        assert!(!overlay_linked_credential(&paths, account, &mut stored));
        propagate_linked_credential(&paths, account, &credential("ignored")).unwrap();
        assert_eq!(
            read_auth_value(&paths).unwrap()["tokens"]["refresh_token"],
            "rotated-by-monitor"
        );
        assert_eq!(active_account(&paths), None);
        assert!(linked_access_token(&paths, account).is_none());
    }

    #[test]
    fn a_teammate_in_the_same_workspace_ends_the_link() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        let account = AccountId(Uuid::new_v4());
        install_account(&paths, account, Some("ws-1"), &tokens("mine")).unwrap();
        let mut stored = credential("mine");
        stored.id_token = tokens("mine").id_token;

        // A teammate signs in to Codex: same workspace, different user.
        let mut root = read_auth_value(&paths).unwrap();
        root["tokens"]["refresh_token"] = json!("teammate");
        root["tokens"]["id_token"] = json!(jwt(json!({
            "email": "teammate@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "ws-1",
                "chatgpt_user_id": "user-teammate",
            },
        })));
        write_atomically(&paths.auth_file(), &root).unwrap();

        assert_eq!(active_account(&paths), None);
        assert!(!overlay_linked_credential(&paths, account, &mut stored));
        assert_eq!(stored.refresh_token, "mine");
        assert!(linked_access_token(&paths, account).is_none());
    }

    #[test]
    fn a_link_without_a_recorded_user_is_ended() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        let account = AccountId(Uuid::new_v4());
        install_account(&paths, account, Some("ws-1"), &tokens("mine")).unwrap();
        fs::write(
            paths.link_file(),
            format!(
                r#"{{"account_id":"{}","chatgpt_account_id":"ws-1"}}"#,
                account.0
            ),
        )
        .unwrap();

        assert_eq!(active_account(&paths), None);
    }
}
