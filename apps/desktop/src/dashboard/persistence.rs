//! Loading and saving accounts and dashboard preferences.

use super::*;

pub(super) fn account_rank(order: &[AccountId], account_id: AccountId) -> usize {
    order
        .iter()
        .position(|id| *id == account_id)
        .unwrap_or(usize::MAX)
}

pub(super) fn provider_tab(provider_id: &str) -> Option<UsageProvider> {
    PROVIDER_TABS
        .iter()
        .map(|tab| tab.provider)
        .find(|provider| belongs_to_provider(provider_id, *provider))
}

pub(super) fn load_account_order() -> Vec<AccountId> {
    crate::theme::preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(ACCOUNT_ORDER_FILE)).ok())
        .map(|contents| {
            contents
                .lines()
                .filter_map(|line| line.trim().parse().ok().map(AccountId))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn save_account_order(order: &[AccountId]) -> io::Result<()> {
    let directory = crate::theme::preference_directory()?;
    fs::create_dir_all(&directory)?;
    let contents = order
        .iter()
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(directory.join(ACCOUNT_ORDER_FILE), contents)
}

pub(super) fn load_hide_antigravity_claude_gpt() -> bool {
    crate::theme::preference_directory()
        .ok()
        .and_then(|directory| {
            fs::read_to_string(directory.join(HIDE_ANTIGRAVITY_CLAUDE_GPT_FILE)).ok()
        })
        .is_some_and(|value| value.trim() == "hide")
}

#[cfg(target_os = "windows")]
pub(super) fn current_antigravity_app_email() -> Option<String> {
    usage_monitor_windows::antigravity_app::signed_in_email()
}

#[cfg(not(target_os = "windows"))]
pub(super) fn current_antigravity_app_email() -> Option<String> {
    None
}

pub(super) fn current_codex_desktop_account() -> Option<AccountId> {
    codex_desktop::active_account(&codex_desktop::CodexDesktopPaths::from_environment())
}

pub async fn load_saved_accounts() -> Result<Vec<AccountUsageEntry>, String> {
    let path = default_accounts_database_path();
    // The read-only open never creates the database. Before the first account
    // is added there is simply nothing to show, which is not an error.
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let store = SqliteStore::open_read_only(path).map_err(|error| error.to_string())?;
    let accounts = store.list().await.map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(accounts.len());

    for account in accounts {
        let snapshot = store
            .get_latest(account.id)
            .await
            .map_err(|error| error.to_string())?;
        entries.push(AccountUsageEntry { account, snapshot });
    }

    Ok(entries)
}

pub(super) fn load_model_visibility_preferences() -> ModelVisibilityPreferences {
    crate::theme::preference_directory()
        .ok()
        .map(|directory| directory.join("model-visibility.txt"))
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| parse_model_visibility_preferences(&contents))
        .unwrap_or_default()
}

pub(super) fn parse_model_visibility_preferences(contents: &str) -> ModelVisibilityPreferences {
    let mut preferences = ModelVisibilityPreferences::default();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(model_id) = line.strip_prefix("visible:") {
            preferences
                .visible_model_ids
                .insert(normalize_model_id(model_id));
        } else {
            let model_id = line.strip_prefix("hidden:").unwrap_or(line);
            preferences
                .hidden_model_ids
                .insert(normalize_model_id(model_id));
        }
    }
    preferences
}

pub(super) fn save_model_visibility_preferences(
    preferences: &ModelVisibilityPreferences,
) -> io::Result<()> {
    let directory = crate::theme::preference_directory()?;
    fs::create_dir_all(&directory)?;

    // Plain legacy lines still mean hidden. Visible overrides use an explicit
    // prefix so old preference files remain readable and user choices survive.
    let mut preferences = preferences
        .hidden_model_ids
        .iter()
        .cloned()
        .chain(
            preferences
                .visible_model_ids
                .iter()
                .map(|model_id| format!("visible:{model_id}")),
        )
        .collect::<Vec<_>>();
    preferences.sort_unstable();
    fs::write(
        directory.join("model-visibility.txt"),
        preferences.join("\n"),
    )
}

pub async fn save_account_alias(
    account_id: AccountId,
    draft: String,
    original_label: String,
) -> Result<Vec<AccountUsageEntry>, String> {
    let store =
        SqliteStore::open(default_accounts_database_path()).map_err(|error| error.to_string())?;
    store
        .set_alias(
            account_id,
            normalized_alias(&draft, &original_label).as_deref(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "The account no longer exists".to_owned())?;
    drop(store);
    load_saved_accounts().await
}

#[cfg(target_os = "windows")]
pub async fn delete_saved_account(account_id: AccountId) -> Result<Vec<AccountUsageEntry>, String> {
    use usage_monitor_core::auth::{AccountAuthMaterialStore, OAuthCredentialStore};
    use usage_monitor_windows::{
        WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    };

    let store =
        SqliteStore::open(default_accounts_database_path()).map_err(|error| error.to_string())?;
    if store
        .get(account_id)
        .await
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err("The account no longer exists".to_owned());
    }

    WindowsCredentialManagerStore
        .remove(account_id)
        .await
        .map_err(|error| format!("Could not remove saved OAuth credentials: {error}"))?;
    WindowsCredentialManagerAuthMaterialStore
        .remove(account_id)
        .await
        .map_err(|error| format!("Could not remove saved sign-in credentials: {error}"))?;
    store
        .remove(account_id)
        .await
        .map_err(|error| error.to_string())?;

    drop(store);
    load_saved_accounts().await
}

#[cfg(not(target_os = "windows"))]
pub async fn delete_saved_account(
    _account_id: AccountId,
) -> Result<Vec<AccountUsageEntry>, String> {
    Err("Account deletion is only available on Windows".to_owned())
}

pub(super) fn normalized_alias(draft: &str, original_label: &str) -> Option<String> {
    let trimmed = draft.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(original_label.trim()) {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

pub(super) fn capitalize_first(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };

    first.to_uppercase().chain(characters).collect()
}
