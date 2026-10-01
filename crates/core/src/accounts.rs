use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

pub const OPENAI: &str = "openai";
pub const CLAUDE: &str = "claude";
pub const OPENCODE_GO: &str = "opencodego";
pub const OPENROUTER: &str = "openrouter";
pub const ANTIGRAVITY: &str = "antigravity";

pub(crate) fn account_reference_prefix(provider_id: &str) -> &'static str {
    match provider_id {
        "codex" | "openai" => "ch",
        "claude" => "cc",
        "openrouter" => "or",
        "opencodego" => "oc",
        "antigravity" => "ag",
        _ => "ac",
    }
}

pub fn normalize_provider_id(value: &str) -> Result<String, AccountError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(AccountError::MissingProviderId);
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub Uuid);

impl AccountId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl FromStr for AccountId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(value)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum AccountStatus {
    Active = 0,
    NeedsReauthentication = 1,
    Paused = 2,
    Disabled = 3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: AccountId,
    pub label: String,
    pub email: String,
    pub provider_account_id: Option<String>,
    pub created_at_utc: DateTime<Utc>,
    pub updated_at_utc: DateTime<Utc>,
    pub status: AccountStatus,
    pub provider_id: String,
    pub workspace_id: Option<String>,
    /// Provider-supplied display name for the selected workspace, when available.
    #[serde(default)]
    pub workspace_name: Option<String>,
    /// Optional user-defined display name. Provider labels and identity remain
    /// unchanged when this is set or cleared.
    #[serde(default)]
    pub alias: Option<String>,
    /// Stable, human-readable reference assigned by the account store. This is
    /// the public selector for CLI and agent workflows; `id` remains internal.
    #[serde(default)]
    pub account_ref: Option<String>,
}

impl AccountRecord {
    pub fn create(
        label: impl Into<String>,
        email: impl Into<String>,
        provider_account_id: Option<String>,
        provider_id: &str,
        workspace_id: Option<String>,
    ) -> Result<Self, AccountError> {
        let label = required(label.into(), "label")?;
        let email = required(email.into(), "email")?.to_ascii_lowercase();
        let provider_id = normalize_provider_id(provider_id)?;
        let now = Utc::now();

        Ok(Self {
            id: AccountId::new(),
            label,
            email,
            provider_account_id: normalize_optional(provider_account_id),
            created_at_utc: now,
            updated_at_utc: now,
            status: AccountStatus::Active,
            provider_id,
            workspace_id: normalize_optional(workspace_id),
            workspace_name: None,
            alias: None,
            account_ref: None,
        })
    }

    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.label)
    }

    pub fn with_identity(
        &self,
        email: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> Result<Self, AccountError> {
        let mut next = self.clone();
        if let Some(email) = email.filter(|value| !value.trim().is_empty()) {
            next.email = email.trim().to_ascii_lowercase();
        }
        if let Some(provider_account_id) =
            provider_account_id.filter(|value| !value.trim().is_empty())
        {
            next.provider_account_id = Some(provider_account_id.trim().to_owned());
        }
        next.updated_at_utc = Utc::now();
        next.status = AccountStatus::Active;
        Ok(next)
    }

    /// Applies a provider-verified identity observed during a refresh.
    ///
    /// Returns `None` when the record would not change. A user-selected
    /// `Paused` or `Disabled` state is kept; only an automatic
    /// reauthentication marker is cleared by a successful verification.
    pub fn with_verified_identity(
        &self,
        email: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> Result<Option<Self>, AccountError> {
        let mut next = self.with_identity(email, provider_account_id)?;
        if matches!(self.status, AccountStatus::Paused | AccountStatus::Disabled) {
            next.status = self.status;
        }
        let changed = next.email != self.email
            || next.provider_account_id != self.provider_account_id
            || next.status != self.status;
        Ok(changed.then_some(next))
    }

    /// Pins a browser-backed account to the workspace that was selected by
    /// the provider during the first authoritative probe.  Keeping this on
    /// the account record prevents a later refresh from silently switching to
    /// another workspace in the same browser session.
    pub fn with_workspace_id(&self, workspace_id: Option<&str>) -> Self {
        let mut next = self.clone();
        next.workspace_id = normalize_optional(workspace_id.map(str::to_owned));
        next.updated_at_utc = Utc::now();
        next
    }

    pub fn with_workspace_name(&self, workspace_name: Option<&str>) -> Self {
        let mut next = self.clone();
        next.workspace_name = normalize_optional(workspace_name.map(str::to_owned));
        next.updated_at_utc = Utc::now();
        next
    }

    pub fn with_alias(&self, alias: Option<&str>) -> Self {
        let mut next = self.clone();
        next.alias = normalize_optional(alias.map(str::to_owned));
        next.updated_at_utc = Utc::now();
        next
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedIdentity {
    pub email: Option<String>,
    pub provider_account_id: Option<String>,
    pub plan_type: Option<String>,
}

#[async_trait]
pub trait AccountStore: Send + Sync {
    async fn list(&self) -> Result<Vec<AccountRecord>, AccountStoreError>;
    async fn get(&self, account_id: AccountId) -> Result<Option<AccountRecord>, AccountStoreError>;
    async fn upsert(&self, account: &AccountRecord) -> Result<(), AccountStoreError>;
    /// Saves a provider account without creating a second local row when that
    /// provider's stable identity and selected workspace are already linked.
    /// Providers that do not supply a stable identity continue to use the local
    /// account id.
    async fn upsert_or_get_by_provider_identity(
        &self,
        account: &AccountRecord,
    ) -> Result<AccountRecord, AccountStoreError>;
    /// Sets or clears a user-defined display name without changing provider
    /// identity or other account metadata. Whitespace-only values clear it.
    async fn set_alias(
        &self,
        account_id: AccountId,
        alias: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError>;
    /// Applies a provider-verified identity to the account's *current* stored
    /// record. Unlike `upsert`, this never recreates an account that was
    /// removed while a refresh was in flight and never overwrites metadata
    /// (alias, status, workspace) changed concurrently. Returns `None` when
    /// the account no longer exists.
    async fn apply_verified_identity(
        &self,
        account_id: AccountId,
        email: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError>;
    async fn remove(&self, account_id: AccountId) -> Result<(), AccountStoreError>;
}

#[derive(Debug, Default)]
struct InMemoryAccountState {
    accounts: std::collections::HashMap<AccountId, AccountRecord>,
    next_reference: std::collections::HashMap<&'static str, u64>,
}

impl InMemoryAccountState {
    fn allocate_reference(&mut self, provider_id: &str) -> String {
        let prefix = account_reference_prefix(provider_id);
        let next = self.next_reference.entry(prefix).or_insert(1);
        let reference = format!("{prefix}{next}");
        *next += 1;
        reference
    }
}

#[derive(Debug, Default)]
pub struct InMemoryAccountStore {
    state: tokio::sync::RwLock<InMemoryAccountState>,
}

#[async_trait]
impl AccountStore for InMemoryAccountStore {
    async fn list(&self) -> Result<Vec<AccountRecord>, AccountStoreError> {
        let mut accounts = self
            .state
            .read()
            .await
            .accounts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        accounts.sort_by(|left, right| {
            left.display_name()
                .to_ascii_lowercase()
                .cmp(&right.display_name().to_ascii_lowercase())
        });
        Ok(accounts)
    }

    async fn get(&self, account_id: AccountId) -> Result<Option<AccountRecord>, AccountStoreError> {
        Ok(self.state.read().await.accounts.get(&account_id).cloned())
    }

    async fn upsert(&self, account: &AccountRecord) -> Result<(), AccountStoreError> {
        let mut state = self.state.write().await;
        if account
            .provider_account_id
            .as_deref()
            .is_some_and(|identity| {
                state.accounts.values().any(|existing| {
                    existing.id != account.id
                        && existing.provider_id == account.provider_id
                        && existing.provider_account_id.as_deref() == Some(identity)
                        && existing.workspace_id == account.workspace_id
                })
            })
        {
            return Err(AccountStoreError::DuplicateProviderIdentity);
        }
        let existing = state.accounts.get(&account.id).cloned();
        let mut saved = account.clone();
        saved.account_ref = Some(
            existing
                .as_ref()
                .and_then(|existing| existing.account_ref.clone())
                .unwrap_or_else(|| state.allocate_reference(&account.provider_id)),
        );
        if saved.alias.is_none() {
            saved.alias = existing.and_then(|existing| existing.alias);
        }
        state.accounts.insert(account.id, saved);
        Ok(())
    }

    async fn upsert_or_get_by_provider_identity(
        &self,
        account: &AccountRecord,
    ) -> Result<AccountRecord, AccountStoreError> {
        let mut state = self.state.write().await;
        let existing = account
            .provider_account_id
            .as_deref()
            .and_then(|identity| {
                state
                    .accounts
                    .values()
                    .filter(|existing| {
                        existing.provider_id == account.provider_id
                            && existing.provider_account_id.as_deref() == Some(identity)
                            && existing.workspace_id == account.workspace_id
                    })
                    .max_by(|left, right| {
                        left.updated_at_utc
                            .cmp(&right.updated_at_utc)
                            .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
                    })
                    .cloned()
            })
            .or_else(|| state.accounts.get(&account.id).cloned());

        if let Some(existing) = existing {
            let resolved = if existing.id == account.id {
                if account.alias.is_none() && existing.alias.is_some() {
                    account.with_alias(existing.alias.as_deref())
                } else {
                    account.clone()
                }
            } else {
                existing
                    .with_identity(Some(&account.email), account.provider_account_id.as_deref())
                    .map_err(|error| AccountStoreError::InvalidData(error.to_string()))?
            };
            let mut resolved = resolved;
            resolved.account_ref = Some(
                existing
                    .account_ref
                    .unwrap_or_else(|| state.allocate_reference(&resolved.provider_id)),
            );
            state.accounts.insert(resolved.id, resolved.clone());
            return Ok(resolved);
        }

        let mut saved = account.clone();
        saved.account_ref = Some(state.allocate_reference(&saved.provider_id));
        state.accounts.insert(saved.id, saved.clone());
        Ok(saved)
    }

    async fn set_alias(
        &self,
        account_id: AccountId,
        alias: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError> {
        let mut accounts = self.state.write().await;
        let accounts = &mut accounts.accounts;
        let Some(account) = accounts.get_mut(&account_id) else {
            return Ok(None);
        };
        let updated = account.with_alias(alias);
        *account = updated.clone();
        Ok(Some(updated))
    }

    async fn apply_verified_identity(
        &self,
        account_id: AccountId,
        email: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError> {
        let mut state = self.state.write().await;
        let Some(current) = state.accounts.get(&account_id).cloned() else {
            return Ok(None);
        };
        let Some(updated) = current
            .with_verified_identity(email, provider_account_id)
            .map_err(|error| AccountStoreError::InvalidData(error.to_string()))?
        else {
            return Ok(Some(current));
        };
        if updated
            .provider_account_id
            .as_deref()
            .is_some_and(|identity| {
                state.accounts.values().any(|existing| {
                    existing.id != updated.id
                        && existing.provider_id == updated.provider_id
                        && existing.provider_account_id.as_deref() == Some(identity)
                        && existing.workspace_id == updated.workspace_id
                })
            })
        {
            return Err(AccountStoreError::DuplicateProviderIdentity);
        }
        state.accounts.insert(account_id, updated.clone());
        Ok(Some(updated))
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AccountStoreError> {
        self.state.write().await.accounts.remove(&account_id);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("{0} is required")]
    MissingField(&'static str),
    #[error("provider id is required")]
    MissingProviderId,
}

#[derive(Debug, thiserror::Error)]
pub enum AccountStoreError {
    #[error("account store failed: {0}")]
    Storage(String),
    #[error("an account with this provider identity already exists")]
    DuplicateProviderIdentity,
    #[error("invalid account data: {0}")]
    InvalidData(String),
}

fn required(value: String, field: &'static str) -> Result<String, AccountError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AccountError::MissingField(field));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim().to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}
