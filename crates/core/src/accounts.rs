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

pub const KNOWN_PROVIDER_IDS: &[&str] = &[
    "codex",
    "openai",
    "azureopenai",
    "claude",
    "clinepass",
    "cursor",
    "opencode",
    "opencodego",
    "alibaba",
    "alibabatokenplan",
    "qwencloud",
    "factory",
    "fireworks",
    "gemini",
    "antigravity",
    "copilot",
    "devin",
    "zai",
    "minimax",
    "manus",
    "kimi",
    "kilo",
    "kiro",
    "vertexai",
    "augment",
    "jetbrains",
    "moonshot",
    "amp",
    "t3chat",
    "ollama",
    "synthetic",
    "openrouter",
    "elevenlabs",
    "warp",
    "windsurf",
    "zed",
    "perplexity",
    "mimo",
    "doubao",
    "sakana",
    "abacus",
    "mistral",
    "deepseek",
    "deepinfra",
    "codebuff",
    "crof",
    "venice",
    "commandcode",
    "qoder",
    "stepfun",
    "bedrock",
    "grok",
    "groq",
    "llmproxy",
    "litellm",
    "deepgram",
    "poe",
    "chutes",
    "neuralwatt",
    "clawrouter",
    "longcat",
    "sub2api",
    "wayfinder",
    "zenmux",
    "aiand",
    "zoommate",
    "xai",
    "notion",
    "ibmbob",
    "nous",
    "muse",
    "coderabbit",
    "replicate",
    "huggingface",
];

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
    /// Browser family paired with `browser_profile_id` for account-scoped,
    /// non-interactive session re-import. Stored as a stable id so core does
    /// not depend on a platform-specific browser crate.
    #[serde(default)]
    pub browser_kind: Option<String>,
    pub browser_profile_id: Option<String>,
    pub workspace_id: Option<String>,
    /// Provider-supplied display name for the selected workspace, when available.
    #[serde(default)]
    pub workspace_name: Option<String>,
    /// Legacy metadata retained for SQLite compatibility. The Codex adapter
    /// does not read this path or use native Codex processes as a source.
    #[serde(default)]
    pub codex_home: Option<String>,
    /// Optional user-defined display name. Provider labels and identity remain
    /// unchanged when this is set or cleared.
    #[serde(default)]
    pub alias: Option<String>,
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
            browser_kind: None,
            browser_profile_id: None,
            workspace_id: normalize_optional(workspace_id),
            workspace_name: None,
            codex_home: None,
            alias: None,
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

    pub fn with_codex_home(&self, codex_home: Option<&str>) -> Self {
        let mut next = self.clone();
        next.codex_home = codex_home
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        next.updated_at_utc = Utc::now();
        next
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
    async fn remove(&self, account_id: AccountId) -> Result<(), AccountStoreError>;
}

#[derive(Debug, Default)]
pub struct InMemoryAccountStore {
    accounts: tokio::sync::RwLock<std::collections::HashMap<AccountId, AccountRecord>>,
}

#[async_trait]
impl AccountStore for InMemoryAccountStore {
    async fn list(&self) -> Result<Vec<AccountRecord>, AccountStoreError> {
        let mut accounts = self
            .accounts
            .read()
            .await
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
        Ok(self.accounts.read().await.get(&account_id).cloned())
    }

    async fn upsert(&self, account: &AccountRecord) -> Result<(), AccountStoreError> {
        let mut accounts = self.accounts.write().await;
        if account
            .provider_account_id
            .as_deref()
            .is_some_and(|identity| {
                accounts.values().any(|existing| {
                    existing.id != account.id
                        && existing.provider_id == account.provider_id
                        && existing.provider_account_id.as_deref() == Some(identity)
                        && existing.workspace_id == account.workspace_id
                })
            })
        {
            return Err(AccountStoreError::DuplicateProviderIdentity);
        }
        let mut saved = account.clone();
        if saved.alias.is_none() {
            saved.alias = accounts
                .get(&account.id)
                .and_then(|existing| existing.alias.clone());
        }
        accounts.insert(account.id, saved);
        Ok(())
    }

    async fn upsert_or_get_by_provider_identity(
        &self,
        account: &AccountRecord,
    ) -> Result<AccountRecord, AccountStoreError> {
        let mut accounts = self.accounts.write().await;
        let existing = account.provider_account_id.as_deref().and_then(|identity| {
            accounts
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
        });

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
            accounts.insert(resolved.id, resolved.clone());
            return Ok(resolved);
        }

        accounts.insert(account.id, account.clone());
        Ok(account.clone())
    }

    async fn set_alias(
        &self,
        account_id: AccountId,
        alias: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError> {
        let mut accounts = self.accounts.write().await;
        let Some(account) = accounts.get_mut(&account_id) else {
            return Ok(None);
        };
        let updated = account.with_alias(alias);
        *account = updated.clone();
        Ok(Some(updated))
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AccountStoreError> {
        self.accounts.write().await.remove(&account_id);
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
