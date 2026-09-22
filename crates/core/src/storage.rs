use crate::{
    accounts::{AccountId, AccountRecord, AccountStatus, AccountStore, AccountStoreError, OPENAI},
    usage::{StorageError, UsageSnapshot, UsageSnapshotStore},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, str::FromStr, sync::Mutex};

pub struct SqliteStore {
    connection: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StorageError::Backend(error.to_string()))?;
        }
        let connection = Connection::open(path).map_err(sqlite_error)?;
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
                    codex_home TEXT NULL,
                    openai_account_id TEXT NULL,
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
        ensure_column(&connection, "accounts", "codex_home", "TEXT NULL")?;
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
                "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, codex_home, status, created_at_utc, updated_at_utc FROM accounts ORDER BY lower(label), account_id",
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
                "SELECT account_id, label, email, provider_id, provider_account_id, browser_kind, browser_profile_id, workspace_id, codex_home, status, created_at_utc, updated_at_utc FROM accounts WHERE account_id = ?1",
                [account_id.to_string()],
                read_account,
            )
            .optional()
            .map_err(sqlite_account_error)
    }

    async fn upsert(&self, account: &AccountRecord) -> Result<(), AccountStoreError> {
        let connection = self.lock().map_err(account_error)?;
        connection
            .execute(
                r#"
                INSERT INTO accounts (
                    account_id, label, email, provider_id, provider_account_id,
                    browser_kind, browser_profile_id, workspace_id, codex_home,
                    openai_account_id, status, created_at_utc, updated_at_utc)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(account_id) DO UPDATE SET
                    label = excluded.label,
                    email = excluded.email,
                    provider_id = excluded.provider_id,
                    provider_account_id = excluded.provider_account_id,
                    browser_kind = excluded.browser_kind,
                    browser_profile_id = excluded.browser_profile_id,
                    workspace_id = excluded.workspace_id,
                    codex_home = excluded.codex_home,
                    openai_account_id = excluded.openai_account_id,
                    status = excluded.status,
                    updated_at_utc = excluded.updated_at_utc
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
                    account.codex_home,
                    (account.provider_id == OPENAI)
                        .then(|| account.provider_account_id.clone())
                        .flatten(),
                    account.status as i32,
                    account.created_at_utc.to_rfc3339(),
                    account.updated_at_utc.to_rfc3339(),
                ],
            )
            .map(|_| ())
            .map_err(sqlite_account_error)
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
        transaction.commit().map_err(sqlite_error)
    }
}

fn read_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRecord> {
    let id = AccountId::from_str(&row.get::<_, String>(0)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let status = match row.get::<_, i32>(9)? {
        0 => AccountStatus::Active,
        1 => AccountStatus::NeedsReauthentication,
        2 => AccountStatus::Paused,
        3 => AccountStatus::Disabled,
        value => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                format!("unknown account status {value}").into(),
            ));
        }
    };
    let created_at = row.get::<_, String>(10)?;
    let updated_at = row.get::<_, String>(11)?;
    let created_at_utc = parse_sql_datetime(&created_at).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let updated_at_utc = parse_sql_datetime(&updated_at).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(error))
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
        codex_home: row.get(8)?,
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
    AccountStoreError::Storage(error.to_string())
}
