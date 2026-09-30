use crate::{
    accounts::{
        AccountId, AccountRecord, AccountStatus, AccountStore, AccountStoreError, OPENAI,
        account_reference_prefix,
    },
    usage::{CodexWeeklyResetCandidate, StorageError, UsageSnapshot, UsageSnapshotStore},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{collections::HashSet, env, path::Path, path::PathBuf, str::FromStr, sync::Mutex};

/// Most recent observations retained per account.
const SNAPSHOT_HISTORY_LIMIT: i64 = 200;

/// The shared default account and usage database used by every host and CLI.
pub fn default_accounts_database_path() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("CodexUsageMonitor-Rust")
        .join("accounts.db")
}

pub struct SqliteStore {
    connection: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens the existing account database without running schema migrations
    /// or enabling any write path. This is intended for read-only consumers
    /// such as status panels that must not initialize or mutate user storage.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite_error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StorageError::Backend(error.to_string()))?;
        }
        let mut connection = Connection::open(path).map_err(sqlite_error)?;
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS accounts (
                    account_id TEXT PRIMARY KEY,
                    label TEXT NOT NULL,
                    email TEXT NOT NULL,
                    provider_id TEXT NOT NULL DEFAULT 'openai',
                    provider_account_id TEXT NULL,
                    browser_kind TEXT NULL,
                    browser_profile_id TEXT NULL,
                    workspace_id TEXT NULL,
                    workspace_name TEXT NULL,
                    codex_home TEXT NULL,
                    openai_account_id TEXT NULL,
                    alias TEXT NULL,
                    account_ref TEXT NULL,
                    status INTEGER NOT NULL,
                    created_at_utc TEXT NOT NULL,
                    updated_at_utc TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS usage_snapshots (
                    snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    account_id TEXT NOT NULL,
                    observed_at_utc TEXT NOT NULL,
                    response_account_id TEXT NULL,
                    plan_type TEXT NULL,
                    primary_used_percent REAL NULL,
                    primary_reset_at_utc TEXT NULL,
                    primary_limit_window_seconds INTEGER NULL,
                    secondary_used_percent REAL NULL,
                    secondary_reset_at_utc TEXT NULL,
                    secondary_limit_window_seconds INTEGER NULL,
                    observed_email TEXT NULL,
                    credits_has_credits INTEGER NULL,
                    credits_unlimited INTEGER NULL,
                    credits_balance TEXT NULL,
                    credits_approximate_message_cost TEXT NULL,
                    spend_monthly_usage TEXT NULL,
                    spend_monthly_limit TEXT NULL,
                    spend_used_percent REAL NULL,
                    spend_limit_enabled INTEGER NULL,
                    provider_id TEXT NOT NULL DEFAULT 'openai',
                    source TEXT NULL,
                    data_confidence TEXT NOT NULL DEFAULT 'authoritative',
                    metrics_json TEXT NULL,
                    FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS additional_rate_windows (
                    snapshot_id INTEGER NOT NULL,
                    window_key TEXT NOT NULL,
                    window_name TEXT NOT NULL,
                    used_percent REAL NOT NULL,
                    reset_at_utc TEXT NULL,
                    limit_window_seconds INTEGER NOT NULL,
                    PRIMARY KEY(snapshot_id, window_key),
                    FOREIGN KEY(snapshot_id) REFERENCES usage_snapshots(snapshot_id) ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS codex_weekly_reset_candidates (
                    account_id TEXT PRIMARY KEY,
                    candidate_json TEXT NOT NULL,
                    FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS account_ref_sequences (
                    prefix TEXT PRIMARY KEY,
                    next_value INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS ix_usage_snapshots_account_observed
                    ON usage_snapshots(account_id, observed_at_utc DESC, snapshot_id DESC);
                "#,
            )
            .map_err(sqlite_error)?;
        ensure_column(
            &connection,
            "accounts",
            "provider_id",
            "TEXT NOT NULL DEFAULT 'openai'",
        )?;
        ensure_column(&connection, "accounts", "provider_account_id", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "browser_kind", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "browser_profile_id", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "workspace_id", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "workspace_name", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "codex_home", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "alias", "TEXT NULL")?;
        ensure_column(&connection, "accounts", "account_ref", "TEXT NULL")?;
        ensure_column(
            &connection,
            "usage_snapshots",
            "provider_id",
            "TEXT NOT NULL DEFAULT 'openai'",
        )?;
        ensure_column(&connection, "usage_snapshots", "source", "TEXT NULL")?;
        ensure_column(
            &connection,
            "usage_snapshots",
            "data_confidence",
            "TEXT NOT NULL DEFAULT 'authoritative'",
        )?;
        ensure_column(&connection, "usage_snapshots", "metrics_json", "TEXT NULL")?;
        migrate_account_references(&mut connection)?;
        connection
            .execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_accounts_account_ref ON accounts(account_ref) WHERE account_ref IS NOT NULL;",
            )
            .map_err(sqlite_error)?;
        connection
            .execute_batch(
                r#"
                DROP TRIGGER IF EXISTS trg_accounts_provider_identity_unique_insert;
                DROP TRIGGER IF EXISTS trg_accounts_provider_identity_unique_update;

                CREATE TRIGGER trg_accounts_provider_identity_unique_insert
                BEFORE INSERT ON accounts
                WHEN NEW.provider_account_id IS NOT NULL
                    AND EXISTS (
                        SELECT 1 FROM accounts
                        WHERE provider_id = NEW.provider_id
                            AND provider_account_id = NEW.provider_account_id
                            AND workspace_id IS NEW.workspace_id
                            AND account_id <> NEW.account_id
                    )
                BEGIN
                    SELECT RAISE(ABORT, 'duplicate provider identity');
                END;

                CREATE TRIGGER trg_accounts_provider_identity_unique_update
                BEFORE UPDATE OF provider_id, provider_account_id, workspace_id ON accounts
                WHEN NEW.provider_account_id IS NOT NULL
                    AND EXISTS (
                        SELECT 1 FROM accounts
                        WHERE provider_id = NEW.provider_id
                            AND provider_account_id = NEW.provider_account_id
                            AND workspace_id IS NEW.workspace_id
                            AND account_id <> NEW.account_id
                    )
                BEGIN
                    SELECT RAISE(ABORT, 'duplicate provider identity');
                END;
                "#,
            )
            .map_err(sqlite_error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.connection
            .lock()
            .map_err(|_| StorageError::Backend("SQLite mutex was poisoned".to_owned()))
    }
}

#[async_trait]
impl AccountStore for SqliteStore {
    async fn list(&self) -> Result<Vec<AccountRecord>, AccountStoreError> {
        let connection = self.lock().map_err(account_error)?;
        let mut statement = connection
            .prepare(
                "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, workspace_name, codex_home, status, created_at_utc, updated_at_utc, alias, account_ref FROM accounts ORDER BY lower(COALESCE(NULLIF(TRIM(alias), ''), label)), account_id",
            )
            .map_err(sqlite_account_error)?;
        let rows = statement
            .query_map([], read_account)
            .map_err(sqlite_account_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_account_error)
    }

    async fn get(&self, account_id: AccountId) -> Result<Option<AccountRecord>, AccountStoreError> {
        let connection = self.lock().map_err(account_error)?;
        connection
            .query_row(
                "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, workspace_name, codex_home, status, created_at_utc, updated_at_utc, alias, account_ref FROM accounts WHERE account_id = ?1",
                [account_id.to_string()],
                read_account,
            )
            .optional()
            .map_err(sqlite_account_error)
    }

    async fn upsert(&self, account: &AccountRecord) -> Result<(), AccountStoreError> {
        let mut connection = self.lock().map_err(account_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_account_error)?;
        write_account(&transaction, account).map_err(sqlite_account_error)?;
        transaction.commit().map_err(sqlite_account_error)
    }

    async fn upsert_or_get_by_provider_identity(
        &self,
        account: &AccountRecord,
    ) -> Result<AccountRecord, AccountStoreError> {
        let mut connection = self.lock().map_err(account_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_account_error)?;

        let existing = if let Some(provider_account_id) = account.provider_account_id.as_deref() {
            transaction
                .query_row(
                    "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, workspace_name, codex_home, status, created_at_utc, updated_at_utc, alias, account_ref FROM accounts WHERE provider_id = ?1 AND provider_account_id = ?2 AND workspace_id IS ?3 ORDER BY updated_at_utc DESC, created_at_utc, account_id LIMIT 1",
                    params![account.provider_id, provider_account_id, account.workspace_id],
                    read_account,
                )
                .optional()
                .map_err(sqlite_account_error)?
        } else {
            None
        };

        let mut resolved = if let Some(existing) = existing {
            if existing.id == account.id {
                if account.alias.is_none() && existing.alias.is_some() {
                    account.with_alias(existing.alias.as_deref())
                } else {
                    account.clone()
                }
            } else {
                existing
                    .with_identity(Some(&account.email), account.provider_account_id.as_deref())
                    .map_err(|error| AccountStoreError::InvalidData(error.to_string()))?
            }
        } else {
            account.clone()
        };

        resolved.account_ref =
            Some(write_account(&transaction, &resolved).map_err(sqlite_account_error)?);
        transaction.commit().map_err(sqlite_account_error)?;
        Ok(resolved)
    }

    async fn set_alias(
        &self,
        account_id: AccountId,
        alias: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError> {
        let connection = self.lock().map_err(account_error)?;
        let Some(account) = connection
            .query_row(
                "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, workspace_name, codex_home, status, created_at_utc, updated_at_utc, alias, account_ref FROM accounts WHERE account_id = ?1",
                [account_id.to_string()],
                read_account,
            )
            .optional()
            .map_err(sqlite_account_error)?
        else {
            return Ok(None);
        };

        let updated = account.with_alias(alias);
        connection
            .execute(
                "UPDATE accounts SET alias = ?1, updated_at_utc = ?2 WHERE account_id = ?3",
                params![
                    updated.alias,
                    updated.updated_at_utc.to_rfc3339(),
                    updated.id.to_string(),
                ],
            )
            .map_err(sqlite_account_error)?;
        Ok(Some(updated))
    }

    async fn apply_verified_identity(
        &self,
        account_id: AccountId,
        email: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> Result<Option<AccountRecord>, AccountStoreError> {
        let mut connection = self.lock().map_err(account_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_account_error)?;
        let Some(current) = transaction
            .query_row(
                "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, workspace_name, codex_home, status, created_at_utc, updated_at_utc, alias, account_ref FROM accounts WHERE account_id = ?1",
                [account_id.to_string()],
                read_account,
            )
            .optional()
            .map_err(sqlite_account_error)?
        else {
            return Ok(None);
        };
        let Some(updated) = current
            .with_verified_identity(email, provider_account_id)
            .map_err(|error| AccountStoreError::InvalidData(error.to_string()))?
        else {
            return Ok(Some(current));
        };
        transaction
            .execute(
                "UPDATE accounts SET email = ?1, provider_account_id = ?2, status = ?3, updated_at_utc = ?4 WHERE account_id = ?5",
                params![
                    updated.email,
                    updated.provider_account_id,
                    updated.status as i32,
                    updated.updated_at_utc.to_rfc3339(),
                    updated.id.to_string(),
                ],
            )
            .map_err(sqlite_account_error)?;
        transaction.commit().map_err(sqlite_account_error)?;
        Ok(Some(updated))
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AccountStoreError> {
        let connection = self.lock().map_err(account_error)?;
        connection
            .execute(
                "DELETE FROM accounts WHERE account_id = ?1",
                [account_id.to_string()],
            )
            .map(|_| ())
            .map_err(sqlite_account_error)
    }
}

fn write_account(
    connection: &Connection,
    account: &AccountRecord,
) -> Result<String, rusqlite::Error> {
    let persisted_reference = connection
        .query_row(
            "SELECT account_ref FROM accounts WHERE account_id = ?1",
            [account.id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let account_ref = match persisted_reference {
        Some(account_ref) => account_ref,
        None => {
            allocate_account_reference(connection, account_reference_prefix(&account.provider_id))?
        }
    };
    connection.execute(
        r#"
        INSERT INTO accounts (
            account_id, label, email, provider_id, provider_account_id,
            browser_kind, browser_profile_id, workspace_id, workspace_name,
            codex_home, openai_account_id, status, created_at_utc, updated_at_utc, alias, account_ref)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
        ON CONFLICT(account_id) DO UPDATE SET
            label = excluded.label,
            email = excluded.email,
            provider_id = excluded.provider_id,
            provider_account_id = excluded.provider_account_id,
            browser_kind = excluded.browser_kind,
            browser_profile_id = excluded.browser_profile_id,
            workspace_id = excluded.workspace_id,
            workspace_name = excluded.workspace_name,
            codex_home = excluded.codex_home,
            openai_account_id = excluded.openai_account_id,
            status = excluded.status,
            updated_at_utc = excluded.updated_at_utc,
            alias = COALESCE(excluded.alias, accounts.alias),
            account_ref = COALESCE(accounts.account_ref, excluded.account_ref)
        "#,
        params![
            account.id.to_string(),
            account.label,
            account.email,
            account.provider_id,
            account.provider_account_id,
            account.browser_kind,
            account.browser_profile_id,
            account.workspace_id,
            account.workspace_name,
            account.codex_home,
            (account.provider_id == OPENAI)
                .then(|| account.workspace_id.clone())
                .flatten(),
            account.status as i32,
            account.created_at_utc.to_rfc3339(),
            account.updated_at_utc.to_rfc3339(),
            account.alias,
            account_ref,
        ],
    )?;
    Ok(account_ref)
}

fn migrate_account_references(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sqlite_error)?;
    let accounts = {
        let mut statement = transaction
            .prepare(
                "SELECT account_id, provider_id, account_ref FROM accounts ORDER BY created_at_utc, account_id",
            )
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };

    let existing_references = accounts
        .iter()
        .filter_map(|(_, _, account_ref)| account_ref.as_deref())
        .collect::<Vec<_>>();
    for account_ref in existing_references {
        if let Some((prefix, number)) = reference_components(account_ref) {
            if number == i64::MAX {
                return Err(StorageError::InvalidData(format!(
                    "account reference sequence is exhausted for {prefix}"
                )));
            }
            reserve_reference_sequence(&transaction, prefix, number + 1).map_err(sqlite_error)?;
        }
    }

    let mut assigned = HashSet::new();
    for (account_id, provider_id, account_ref) in accounts {
        let has_unique_reference = account_ref
            .as_deref()
            .filter(|value| reference_components(value).is_some())
            .is_some_and(|value| assigned.insert(value.to_owned()));
        if has_unique_reference {
            continue;
        }

        let prefix = account_reference_prefix(&provider_id);
        let account_ref = loop {
            let candidate =
                allocate_account_reference(&transaction, prefix).map_err(sqlite_error)?;
            if assigned.insert(candidate.clone()) {
                break candidate;
            }
        };
        transaction
            .execute(
                "UPDATE accounts SET account_ref = ?1 WHERE account_id = ?2",
                rusqlite::params![account_ref, account_id],
            )
            .map_err(sqlite_error)?;
    }

    transaction.commit().map_err(sqlite_error)
}

fn allocate_account_reference(
    connection: &Connection,
    prefix: &str,
) -> Result<String, rusqlite::Error> {
    let next = connection
        .query_row(
            "SELECT next_value FROM account_ref_sequences WHERE prefix = ?1",
            [prefix],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(1);
    if next < 1 || next == i64::MAX {
        return Err(rusqlite::Error::InvalidQuery);
    }
    connection.execute(
        "INSERT INTO account_ref_sequences (prefix, next_value) VALUES (?1, ?2) ON CONFLICT(prefix) DO UPDATE SET next_value = excluded.next_value",
        rusqlite::params![prefix, next + 1],
    )?;
    Ok(format!("{prefix}{next}"))
}

fn reserve_reference_sequence(
    connection: &Connection,
    prefix: &str,
    next_value: i64,
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "INSERT INTO account_ref_sequences (prefix, next_value) VALUES (?1, ?2) ON CONFLICT(prefix) DO UPDATE SET next_value = MAX(next_value, excluded.next_value)",
        rusqlite::params![prefix, next_value],
    )?;
    Ok(())
}

fn reference_components(account_ref: &str) -> Option<(&str, i64)> {
    let split = account_ref.find(|character: char| character.is_ascii_digit())?;
    let (prefix, number) = account_ref.split_at(split);
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|character| character.is_ascii_lowercase())
        || number.is_empty()
        || !number.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let number = number.parse::<i64>().ok()?;
    (number > 0).then_some((prefix, number))
}

#[async_trait]
impl UsageSnapshotStore for SqliteStore {
    async fn get_latest(
        &self,
        account_id: AccountId,
    ) -> Result<Option<UsageSnapshot>, StorageError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT snapshot_id, metrics_json, observed_at_utc, response_account_id, plan_type, observed_email, provider_id, source, data_confidence FROM usage_snapshots WHERE account_id = ?1 ORDER BY observed_at_utc DESC, snapshot_id DESC LIMIT 1",
                [account_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((
            snapshot_id,
            payload,
            observed_at,
            response_account_id,
            plan_type,
            observed_email,
            provider_id,
            source,
            data_confidence,
        )) = row
        else {
            return Ok(None);
        };
        if let Some(payload) = payload.as_deref() {
            if let Ok(snapshot) = serde_json::from_str::<UsageSnapshot>(payload) {
                return Ok(Some(snapshot));
            }
        }
        let observed_at_utc = parse_datetime(&observed_at)?;
        let metrics = payload
            .as_deref()
            .and_then(|payload| serde_json::from_str(payload).ok())
            .unwrap_or_default();
        let additional_windows = read_additional_windows(&connection, snapshot_id)?;
        Ok(Some(UsageSnapshot {
            account_id,
            observed_at_utc,
            response_account_id,
            plan_type,
            primary: None,
            primary_window_kind: None,
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows,
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id,
            source,
            data_confidence,
        }))
    }

    async fn save(&self, snapshot: UsageSnapshot) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let payload = serde_json::to_string(&snapshot)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let transaction = connection.unchecked_transaction().map_err(sqlite_error)?;
        transaction
            .execute(
                r#"
                INSERT INTO usage_snapshots (
                    account_id, observed_at_utc, response_account_id, plan_type,
                    observed_email, provider_id, source, data_confidence, metrics_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    snapshot.account_id.to_string(),
                    snapshot.observed_at_utc.to_rfc3339(),
                    snapshot.response_account_id,
                    snapshot.plan_type,
                    snapshot.observed_email,
                    snapshot.provider_id,
                    snapshot.source,
                    snapshot.data_confidence,
                    payload,
                ],
            )
            .map_err(sqlite_error)?;
        let snapshot_id = transaction.last_insert_rowid();
        for additional in &snapshot.additional_windows {
            transaction
                .execute(
                    "INSERT INTO additional_rate_windows (snapshot_id, window_key, window_name, used_percent, reset_at_utc, limit_window_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        snapshot_id,
                        additional.key,
                        additional.name,
                        additional.window.used_percent,
                        additional.window.reset_at_utc.map(|value| value.to_rfc3339()),
                        additional.window.limit_window_seconds,
                    ],
                )
                .map_err(sqlite_error)?;
        }
        // Every refresh appends a full observation. Keep a bounded recent
        // history per account so the database does not grow without limit
        // (additional windows are removed by the foreign-key cascade).
        transaction
            .execute(
                r#"
                DELETE FROM usage_snapshots
                WHERE account_id = ?1
                  AND snapshot_id NOT IN (
                      SELECT snapshot_id FROM usage_snapshots
                      WHERE account_id = ?1
                      ORDER BY observed_at_utc DESC, snapshot_id DESC
                      LIMIT ?2
                  )
                "#,
                params![snapshot.account_id.to_string(), SNAPSHOT_HISTORY_LIMIT],
            )
            .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)
    }

    async fn mark_latest_stale(
        &self,
        account_id: AccountId,
        reason: &str,
    ) -> Result<Option<UsageSnapshot>, StorageError> {
        {
            let connection = self.lock()?;
            let Some((snapshot_id, payload)) = connection
                .query_row(
                    "SELECT snapshot_id, metrics_json FROM usage_snapshots WHERE account_id = ?1 ORDER BY observed_at_utc DESC, snapshot_id DESC LIMIT 1",
                    [account_id.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?
            else {
                return Ok(None);
            };
            if let Some(snapshot) = payload
                .as_deref()
                .and_then(|payload| serde_json::from_str::<UsageSnapshot>(payload).ok())
            {
                let stale = snapshot.mark_stale(reason);
                let payload = serde_json::to_string(&stale)
                    .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                connection
                    .execute(
                        "UPDATE usage_snapshots SET metrics_json = ?1 WHERE snapshot_id = ?2",
                        params![payload, snapshot_id],
                    )
                    .map_err(sqlite_error)?;
                return Ok(Some(stale));
            }
        }
        // Legacy rows without a full JSON payload cannot be updated in place.
        let Some(latest) = self.get_latest(account_id).await? else {
            return Ok(None);
        };
        let stale = latest.mark_stale(reason);
        self.save(stale.clone()).await?;
        Ok(Some(stale))
    }

    async fn get_codex_weekly_reset_candidate(
        &self,
        account_id: AccountId,
    ) -> Result<Option<CodexWeeklyResetCandidate>, StorageError> {
        let connection = self.lock()?;
        let payload = connection
            .query_row(
                "SELECT candidate_json FROM codex_weekly_reset_candidates WHERE account_id = ?1",
                [account_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        payload
            .map(|payload| {
                serde_json::from_str(&payload)
                    .map_err(|error| StorageError::InvalidData(error.to_string()))
            })
            .transpose()
    }

    async fn save_codex_weekly_reset_candidate(
        &self,
        account_id: AccountId,
        candidate: Option<CodexWeeklyResetCandidate>,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        match candidate {
            Some(candidate) => {
                if candidate.snapshot.account_id != account_id {
                    return Err(StorageError::InvalidData(
                        "Codex reset candidate belongs to a different account".to_owned(),
                    ));
                }
                let payload = serde_json::to_string(&candidate)
                    .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                connection
                    .execute(
                        "INSERT INTO codex_weekly_reset_candidates (account_id, candidate_json) VALUES (?1, ?2) ON CONFLICT(account_id) DO UPDATE SET candidate_json = excluded.candidate_json",
                        params![account_id.to_string(), payload],
                    )
                    .map_err(sqlite_error)?;
            }
            None => {
                connection
                    .execute(
                        "DELETE FROM codex_weekly_reset_candidates WHERE account_id = ?1",
                        [account_id.to_string()],
                    )
                    .map_err(sqlite_error)?;
            }
        }
        Ok(())
    }
}

fn read_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRecord> {
    let id = AccountId::from_str(&row.get::<_, String>(0)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let status = match row.get::<_, i32>(10)? {
        0 => AccountStatus::Active,
        1 => AccountStatus::NeedsReauthentication,
        2 => AccountStatus::Paused,
        3 => AccountStatus::Disabled,
        value => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Integer,
                format!("unknown account status {value}").into(),
            ));
        }
    };
    let created_at = row.get::<_, String>(11)?;
    let updated_at = row.get::<_, String>(12)?;
    let created_at_utc = parse_sql_datetime(&created_at).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let updated_at_utc = parse_sql_datetime(&updated_at).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(AccountRecord {
        id,
        label: row.get(1)?,
        email: row.get(2)?,
        provider_id: row.get(3)?,
        provider_account_id: row.get(4)?,
        browser_kind: row.get(5)?,
        browser_profile_id: row.get(6)?,
        workspace_id: row.get(7)?,
        workspace_name: row.get(8)?,
        codex_home: row.get(9)?,
        alias: row.get(13)?,
        account_ref: row.get(14)?,
        status,
        created_at_utc,
        updated_at_utc,
    })
}

fn read_additional_windows(
    connection: &Connection,
    snapshot_id: i64,
) -> Result<Vec<crate::usage::AdditionalRateLimitWindow>, StorageError> {
    let mut statement = connection
        .prepare("SELECT window_key, window_name, used_percent, reset_at_utc, limit_window_seconds FROM additional_rate_windows WHERE snapshot_id = ?1 ORDER BY window_key")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([snapshot_id], |row| {
            Ok(crate::usage::AdditionalRateLimitWindow {
                key: row.get(0)?,
                name: row.get(1)?,
                window: crate::usage::RateLimitWindow {
                    kind: crate::usage::UsageWindowKind::Additional,
                    name: row.get(1)?,
                    used_percent: row.get(2)?,
                    reset_at_utc: row
                        .get::<_, Option<String>>(3)?
                        .map(|value| {
                            parse_sql_datetime(&value).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    3,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })
                        })
                        .transpose()?,
                    limit_window_seconds: row.get(4)?,
                },
            })
        })
        .map_err(sqlite_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), StorageError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(sqlite_error)?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sqlite_error)?
        .filter_map(Result::ok)
        .any(|name| name == column);
    if exists {
        return Ok(());
    }
    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )
        .map(|_| ())
        .map_err(sqlite_error)
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>, StorageError> {
    parse_sql_datetime(value)
}

fn parse_sql_datetime(value: &str) -> Result<DateTime<Utc>, StorageError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| StorageError::InvalidData(format!("invalid timestamp {value}: {error}")))
}

fn sqlite_error(error: rusqlite::Error) -> StorageError {
    StorageError::Backend(error.to_string())
}

fn account_error(error: StorageError) -> AccountStoreError {
    AccountStoreError::Storage(error.to_string())
}

fn sqlite_account_error(error: rusqlite::Error) -> AccountStoreError {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(_, Some(message))
            if message.contains("duplicate provider identity")
    ) {
        AccountStoreError::DuplicateProviderIdentity
    } else {
        AccountStoreError::Storage(error.to_string())
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::SqliteStore;
    use tempfile::tempdir;

    #[test]
    fn read_only_connection_can_read_but_cannot_modify_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("accounts.db");
        drop(SqliteStore::open(&path).unwrap());

        let store = SqliteStore::open_read_only(&path).unwrap();
        let connection = store.lock().unwrap();
        let account_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(account_count, 0);
        assert!(
            connection
                .execute_batch("CREATE TABLE read_only_probe (id INTEGER)")
                .is_err()
        );
    }

    #[test]
    fn read_only_open_does_not_create_a_missing_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing.db");

        assert!(SqliteStore::open_read_only(&path).is_err());
        assert!(!path.exists());
    }
}

#[cfg(test)]
mod refresh_write_tests {
    use super::SqliteStore;
    use crate::{
        accounts::{AccountRecord, AccountStore, OPENAI},
        usage::{UsageSnapshot, UsageSnapshotStore},
    };
    use chrono::Utc;
    use tempfile::tempdir;

    fn snapshot(account: &AccountRecord) -> UsageSnapshot {
        serde_json::from_value(serde_json::json!({
            "account_id": account.id,
            "observed_at_utc": Utc::now(),
            "response_account_id": null,
            "plan_type": null,
            "primary": null,
            "secondary": null,
            "additional_windows": [],
            "credits": null,
            "spend": null,
            "observed_email": null,
            "is_stale": false,
            "stale_reason": null,
            "stale_at_utc": null,
            "metrics": [],
            "provider_id": OPENAI,
            "source": null,
            "data_confidence": "authoritative"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn repeated_stale_marks_update_the_latest_row_in_place() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("accounts.db")).unwrap();
        let account = AccountRecord::create("c", "c@example.com", None, OPENAI, None).unwrap();
        store.upsert(&account).await.unwrap();
        store.save(snapshot(&account)).await.unwrap();

        let first = store
            .mark_latest_stale(account.id, "first")
            .await
            .unwrap()
            .unwrap();
        let second = store
            .mark_latest_stale(account.id, "second")
            .await
            .unwrap()
            .unwrap();

        let rows: i64 = store
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM usage_snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        let latest = store.get_latest(account.id).await.unwrap().unwrap();
        assert!(latest.is_stale);
        assert_eq!(latest.stale_reason.as_deref(), Some("second"));
        assert_eq!(second.stale_at_utc, first.stale_at_utc);
    }

    #[tokio::test]
    async fn verified_identity_does_not_recreate_a_removed_account() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("accounts.db")).unwrap();
        let account = AccountRecord::create("c", "c@example.com", None, OPENAI, None).unwrap();
        store.upsert(&account).await.unwrap();
        store.remove(account.id).await.unwrap();

        let applied = store
            .apply_verified_identity(account.id, Some("new@example.com"), Some("acct"))
            .await
            .unwrap();
        assert!(applied.is_none());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn snapshot_history_is_bounded_per_account() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("accounts.db")).unwrap();
        let account = AccountRecord::create("c", "c@example.com", None, OPENAI, None).unwrap();
        store.upsert(&account).await.unwrap();
        for _ in 0..(super::SNAPSHOT_HISTORY_LIMIT + 25) {
            store.save(snapshot(&account)).await.unwrap();
        }
        let rows: i64 = store
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM usage_snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, super::SNAPSHOT_HISTORY_LIMIT);
        assert!(store.get_latest(account.id).await.unwrap().is_some());
    }
}
