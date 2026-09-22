//! Read-only local OpenCode Go usage history.
//!
//! OpenCode stores assistant messages in a SQLite database.  The local reader
//! deliberately treats this as device-scoped observed cost, not as account
//! truth: rows are useful for a cost history and an estimate when the account
//! API is unavailable, but they must never replace an authoritative API/web
//! snapshot when one is available.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{BTreeMap, HashMap},
    env,
    path::{Path, PathBuf},
    time::Duration as StdDuration,
};
use thiserror::Error;

use crate::usage::{
    AdditionalRateLimitWindow, RateLimitWindow, SpendSnapshot, UsageMetric, UsageWindowKind,
};

const FIVE_HOUR_LIMIT_USD: f64 = 12.0;
const WEEKLY_LIMIT_USD: f64 = 30.0;
const MONTHLY_LIMIT_USD: f64 = 60.0;
const FIVE_HOUR_SECONDS: i64 = 5 * 60 * 60;
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OpenCodeGoLocalUsageError {
    #[error("OpenCode Go local history was not detected")]
    NotDetected,
    #[error("OpenCode Go local history is unavailable: {0}")]
    HistoryUnavailable(String),
    #[error("OpenCode Go SQLite read failed: {0}")]
    SqliteFailed(String),
}

/// Device-local quota/cost data produced from OpenCode assistant records.
#[derive(Debug, Clone)]
pub struct OpenCodeGoLocalUsage {
    pub primary: RateLimitWindow,
    pub secondary: RateLimitWindow,
    pub monthly: AdditionalRateLimitWindow,
    pub spend: SpendSnapshot,
    pub metrics: Vec<UsageMetric>,
    pub row_count: usize,
    pub earliest_row_at_utc: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct OpenCodeGoLocalUsageReader {
    database_path: PathBuf,
}

impl OpenCodeGoLocalUsageReader {
    /// Uses the provider's documented XDG path first, then Windows data roots.
    /// `OPENCODE_GO_DB_PATH` is an explicit diagnostic/test override.
    pub fn from_process() -> Self {
        let paths = Self::database_paths_from_environment();
        Self {
            database_path: paths
                .iter()
                .find(|path| path.is_file())
                .cloned()
                .or_else(|| paths.into_iter().next())
                .unwrap_or_else(|| PathBuf::from("opencode.db")),
        }
    }

    pub fn with_database_path(path: impl Into<PathBuf>) -> Self {
        Self {
            database_path: path.into(),
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Try every known data root, retaining the first usable database.  This
    /// matters on Windows where CLI builds commonly use the XDG location while
    /// desktop builds may place provider data below LOCALAPPDATA/APPDATA.
    pub fn read_from_process(
        &self,
        now: DateTime<Utc>,
    ) -> Result<OpenCodeGoLocalUsage, OpenCodeGoLocalUsageError> {
        let mut paths = vec![self.database_path.clone()];
        paths.extend(Self::database_paths_from_environment());
        let paths = deduplicate_paths(paths);
        let mut found = false;
        let mut failures = Vec::new();
        for path in paths {
            if !path.is_file() {
                continue;
            }
            found = true;
            match Self::with_database_path(path.clone()).read(now) {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        if found {
            Err(OpenCodeGoLocalUsageError::HistoryUnavailable(
                failures.join("; "),
            ))
        } else {
            Err(OpenCodeGoLocalUsageError::NotDetected)
        }
    }

    pub fn read(
        &self,
        now: DateTime<Utc>,
    ) -> Result<OpenCodeGoLocalUsage, OpenCodeGoLocalUsageError> {
        if !self.database_path.is_file() {
            return Err(OpenCodeGoLocalUsageError::HistoryUnavailable(format!(
                "database not found at {}",
                self.database_path.display()
            )));
        }
        let rows = self.read_rows()?;
        if rows.is_empty() {
            return Err(OpenCodeGoLocalUsageError::HistoryUnavailable(
                "no OpenCode Go assistant cost rows were found".to_owned(),
            ));
        }
        Ok(build_snapshot(&rows, now))
    }

    pub fn database_paths_from_environment() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(value) = non_empty_env("OPENCODE_GO_DB_PATH") {
            paths.push(PathBuf::from(value));
        }

        let home = env::var_os("USERPROFILE")
            .or_else(|| env::var_os("HOME"))
            .map(PathBuf::from);
        if let Some(home) = home {
            paths.push(
                home.join(".local")
                    .join("share")
                    .join("opencode")
                    .join("opencode.db"),
            );
        }

        for key in ["XDG_DATA_HOME", "LOCALAPPDATA", "APPDATA"] {
            if let Some(root) = non_empty_env(key) {
                paths.push(PathBuf::from(root).join("opencode").join("opencode.db"));
            }
        }

        deduplicate_paths(paths)
    }

    fn read_rows(&self) -> Result<Vec<UsageRow>, OpenCodeGoLocalUsageError> {
        let connection =
            Connection::open_with_flags(&self.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
        connection
            .busy_timeout(StdDuration::from_millis(250))
            .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;

        let has_part = has_table(&connection, "part")?;
        let sql = if has_part {
            MESSAGE_AND_PART_USAGE_SQL
        } else {
            MESSAGE_USAGE_SQL
        };
        let mut statement = connection
            .prepare(sql)
            .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
        let mut result = Vec::new();
        let mut rows = statement
            .query([])
            .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?
        {
            let created_raw = row
                .get::<_, i64>(0)
                .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
            let cost = row
                .get::<_, f64>(1)
                .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
            let request_count = row
                .get::<_, i64>(2)
                .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
            let model = row
                .get::<_, Option<String>>(3)
                .ok()
                .flatten()
                .unwrap_or_default();
            let Some(created_ms) = normalize_timestamp_ms(created_raw) else {
                continue;
            };
            if !cost.is_finite() || cost < 0.0 {
                continue;
            }
            result.push(UsageRow {
                created_ms,
                cost,
                request_count: request_count.max(1) as usize,
                model,
            });
        }
        Ok(result)
    }
}

#[derive(Debug, Clone)]
struct UsageRow {
    created_ms: i64,
    cost: f64,
    request_count: usize,
    model: String,
}

const MESSAGE_USAGE_SQL: &str = r#"
    SELECT
      CAST(COALESCE(json_extract(data, '$.time.created'), time_created) AS INTEGER) AS created_ms,
      CAST(json_extract(data, '$.cost') AS REAL) AS cost,
      1 AS request_count,
      COALESCE(json_extract(data, '$.modelID'), '') AS model_id
    FROM message
    WHERE json_valid(data)
      AND json_extract(data, '$.providerID') = 'opencode-go'
      AND json_extract(data, '$.role') = 'assistant'
      AND json_type(data, '$.cost') IN ('integer', 'real')
"#;

const MESSAGE_AND_PART_USAGE_SQL: &str = r#"
    WITH provider_messages AS (
      SELECT
        id AS message_id,
        CAST(COALESCE(json_extract(data, '$.time.created'), time_created) AS INTEGER) AS created_ms,
        CAST(json_extract(data, '$.cost') AS REAL) AS cost,
        json_type(data, '$.cost') IN ('integer', 'real') AS has_cost,
        COALESCE(json_extract(data, '$.modelID'), '') AS model_id
      FROM message
      WHERE json_valid(data)
        AND json_extract(data, '$.providerID') = 'opencode-go'
        AND json_extract(data, '$.role') = 'assistant'
    )
    SELECT
      CAST(COALESCE(json_extract(p.data, '$.time.created'), p.time_created, m.created_ms) AS INTEGER)
        AS created_ms,
      CAST(json_extract(p.data, '$.cost') AS REAL) AS cost,
      1 AS request_count,
      m.model_id AS model_id
    FROM part p
    JOIN provider_messages m ON m.message_id = p.message_id
    WHERE json_valid(p.data)
      AND json_extract(p.data, '$.type') = 'step-finish'
      AND json_type(p.data, '$.cost') IN ('integer', 'real')
    UNION ALL
    SELECT created_ms, cost, 1 AS request_count, model_id
    FROM provider_messages m
    WHERE has_cost
      AND NOT EXISTS (
        SELECT 1
        FROM part p
        WHERE p.message_id = m.message_id
          AND json_valid(p.data)
          AND json_extract(p.data, '$.type') = 'step-finish'
          AND json_type(p.data, '$.cost') IN ('integer', 'real')
      )
"#;

fn has_table(connection: &Connection, name: &str) -> Result<bool, OpenCodeGoLocalUsageError> {
    let found = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| OpenCodeGoLocalUsageError::SqliteFailed(error.to_string()))?;
    Ok(found != 0)
}

fn build_snapshot(rows: &[UsageRow], now: DateTime<Utc>) -> OpenCodeGoLocalUsage {
    let now_ms = now.timestamp_millis();
    let session_start_ms = now_ms - FIVE_HOUR_SECONDS * 1_000;
    let week_start = start_of_utc_week(now);
    let week_end = week_start + Duration::seconds(WEEK_SECONDS);
    let earliest = rows
        .iter()
        .filter_map(|row| Utc.timestamp_millis_opt(row.created_ms).single())
        .min();
    let (month_start, month_end) = month_bounds(now, earliest);

    let mut session_cost = 0.0;
    let mut weekly_cost = 0.0;
    let mut monthly_cost = 0.0;
    let mut oldest_session = None;
    let mut daily: BTreeMap<String, BTreeMap<String, (f64, usize)>> = BTreeMap::new();

    for row in rows {
        if row.created_ms >= session_start_ms && row.created_ms < now_ms {
            session_cost += row.cost;
            oldest_session = Some(
                oldest_session.map_or(row.created_ms, |oldest: i64| oldest.min(row.created_ms)),
            );
        }
        if let Some(created) = Utc.timestamp_millis_opt(row.created_ms).single() {
            if created >= week_start && created < week_end {
                weekly_cost += row.cost;
            }
            if created >= month_start && created < month_end {
                monthly_cost += row.cost;
            }

            let local = created.with_timezone(&Local);
            let day = local.format("%Y-%m-%d").to_string();
            let model = if row.model.trim().is_empty() {
                "unknown".to_owned()
            } else {
                row.model.trim().to_owned()
            };
            let bucket = daily
                .entry(day)
                .or_default()
                .entry(model)
                .or_insert((0.0, 0));
            bucket.0 += row.cost;
            bucket.1 += row.request_count;
        }
    }

    let rolling_reset = oldest_session
        .map(|created| created + FIVE_HOUR_SECONDS * 1_000)
        .or_else(|| Some(now_ms + FIVE_HOUR_SECONDS * 1_000));
    let primary = amount_window(
        UsageWindowKind::Primary,
        "Rolling 5 hours (local estimate)",
        session_cost,
        FIVE_HOUR_LIMIT_USD,
        rolling_reset.and_then(|value| Utc.timestamp_millis_opt(value).single()),
        FIVE_HOUR_SECONDS,
    );
    let secondary = amount_window(
        UsageWindowKind::Secondary,
        "Weekly (local estimate)",
        weekly_cost,
        WEEKLY_LIMIT_USD,
        Some(week_end),
        WEEK_SECONDS,
    );
    let monthly_window = amount_window(
        UsageWindowKind::Additional,
        "Monthly (local estimate)",
        monthly_cost,
        MONTHLY_LIMIT_USD,
        Some(month_end),
        (month_end - month_start).num_seconds().max(0),
    );

    let mut metrics = vec![
        amount_metric(
            "local-cost-5h",
            "Observed cost (5h)",
            session_cost,
            FIVE_HOUR_LIMIT_USD,
            primary.reset_at_utc,
            &[("scope", "device-local"), ("window", "5h")],
        ),
        amount_metric(
            "local-cost-weekly",
            "Observed cost (weekly)",
            weekly_cost,
            WEEKLY_LIMIT_USD,
            secondary.reset_at_utc,
            &[("scope", "device-local"), ("window", "weekly")],
        ),
        amount_metric(
            "local-cost-monthly",
            "Observed cost (monthly)",
            monthly_cost,
            MONTHLY_LIMIT_USD,
            monthly_window.reset_at_utc,
            &[("scope", "device-local"), ("window", "monthly")],
        ),
    ];
    for (day, models) in &daily {
        for (model, (cost, requests)) in models {
            let mut metadata = HashMap::new();
            metadata.insert("scope".to_owned(), "device-local".to_owned());
            metadata.insert("day".to_owned(), day.clone());
            metadata.insert("model".to_owned(), model.clone());
            metadata.insert("request_count".to_owned(), requests.to_string());
            metrics.push(UsageMetric {
                key: format!("local-cost-day:{day}:{model}"),
                name: format!("Observed cost {day} ({model})"),
                used_percent: None,
                used_amount: Some(*cost),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("USD".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata,
            });
        }
    }

    OpenCodeGoLocalUsage {
        primary,
        secondary,
        monthly: AdditionalRateLimitWindow {
            key: "monthly".to_owned(),
            name: "Monthly (local estimate)".to_owned(),
            window: monthly_window,
        },
        spend: SpendSnapshot {
            monthly_usage: Some(monthly_cost),
            monthly_limit: Some(MONTHLY_LIMIT_USD),
            used_percent: Some(percent(monthly_cost, MONTHLY_LIMIT_USD)),
            limit_enabled: Some(true),
        },
        metrics: {
            metrics.shrink_to_fit();
            metrics
        },
        row_count: rows.len(),
        earliest_row_at_utc: earliest,
    }
}

fn amount_window(
    kind: UsageWindowKind,
    name: &str,
    used: f64,
    limit: f64,
    reset_at_utc: Option<DateTime<Utc>>,
    limit_window_seconds: i64,
) -> RateLimitWindow {
    RateLimitWindow {
        kind,
        name: name.to_owned(),
        used_percent: percent(used, limit),
        reset_at_utc,
        limit_window_seconds,
    }
}

fn amount_metric(
    key: &str,
    name: &str,
    used: f64,
    limit: f64,
    reset_at_utc: Option<DateTime<Utc>>,
    metadata: &[(&str, &str)],
) -> UsageMetric {
    let mut values = HashMap::new();
    for (key, value) in metadata {
        values.insert(key.to_string(), (*value).to_owned());
    }
    UsageMetric {
        key: key.to_owned(),
        name: name.to_owned(),
        used_percent: Some(percent(used, limit)),
        used_amount: Some(used),
        limit_amount: Some(limit),
        remaining_amount: Some((limit - used).max(0.0)),
        unit: Some("USD".to_owned()),
        reset_at_utc,
        reset_label: None,
        metadata: values,
    }
}

fn percent(used: f64, limit: f64) -> f64 {
    if !used.is_finite() || !limit.is_finite() || limit <= 0.0 {
        return 0.0;
    }
    (used.max(0.0) / limit * 100.0).clamp(0.0, 100.0)
}

fn start_of_utc_week(now: DateTime<Utc>) -> DateTime<Utc> {
    let date = now.date_naive();
    let days_since_monday = date.weekday().num_days_from_monday() as i64;
    let monday = date - Duration::days(days_since_monday);
    Utc.from_utc_datetime(&monday.and_hms_opt(0, 0, 0).expect("midnight is valid"))
}

fn month_bounds(
    now: DateTime<Utc>,
    anchor: Option<DateTime<Utc>>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let Some(anchor) = anchor else {
        let start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now);
        return (start, add_month(start));
    };
    let anchor_day = anchor.day();
    let mut start = make_anchored_month(now.year(), now.month(), anchor, anchor_day);
    if start > now {
        let (year, month) = previous_month(now.year(), now.month());
        start = make_anchored_month(year, month, anchor, anchor_day);
    }
    (start, add_month(start))
}

fn make_anchored_month(
    year: i32,
    month: u32,
    anchor: DateTime<Utc>,
    anchor_day: u32,
) -> DateTime<Utc> {
    let day = anchor_day.min(days_in_month(year, month));
    Utc.with_ymd_and_hms(
        year,
        month,
        day,
        anchor.hour(),
        anchor.minute(),
        anchor.second(),
    )
    .single()
    .unwrap_or_else(|| {
        Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
            .single()
            .unwrap()
    })
}

fn add_month(value: DateTime<Utc>) -> DateTime<Utc> {
    let (year, month) = next_month(value.year(), value.month());
    make_anchored_month(year, month, value, value.day())
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = next_month(year, month);
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|date| date.pred_opt())
        .map_or(28, |date| date.day())
}

fn normalize_timestamp_ms(value: i64) -> Option<i64> {
    if value <= 0 {
        return None;
    }
    if value < 100_000_000_000 {
        value.checked_mul(1_000)
    } else if value > 100_000_000_000_000 {
        Some(value / 1_000)
    } else {
        Some(value)
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    #[test]
    fn reads_message_costs_and_builds_rolling_weekly_monthly_windows() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE message (id TEXT, time_created INTEGER, data TEXT);")
            .unwrap();
        let now = Utc
            .with_ymd_and_hms(2026, 9, 22, 12, 0, 0)
            .single()
            .unwrap();
        let created = now.timestamp_millis() - 60_000;
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3)",
                params![
                    "message-1",
                    created,
                    format!(
                        r#"{{"role":"assistant","providerID":"opencode-go","modelID":"glm-5.3-flash","cost":6.0,"time":{{"created":{created}}}}}"#
                    )
                ],
            )
            .unwrap();

        let snapshot = OpenCodeGoLocalUsageReader::with_database_path(database)
            .read(now)
            .unwrap();
        assert_eq!(snapshot.row_count, 1);
        assert_eq!(snapshot.primary.used_percent, 50.0);
        assert_eq!(snapshot.secondary.used_percent, 20.0);
        assert_eq!(snapshot.monthly.window.used_percent, 10.0);
        assert!(
            snapshot
                .metrics
                .iter()
                .any(|metric| metric.key.starts_with("local-cost-day:"))
        );
    }

    #[test]
    fn step_finish_parts_are_counted_once_when_messages_have_no_cost() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message (id TEXT, time_created INTEGER, data TEXT);\
                 CREATE TABLE part (id TEXT, message_id TEXT, time_created INTEGER, data TEXT);",
            )
            .unwrap();
        let now = Utc
            .with_ymd_and_hms(2026, 9, 22, 12, 0, 0)
            .single()
            .unwrap();
        let created = now.timestamp_millis() - 60_000;
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3)",
                params![
                    "message-1",
                    created,
                    r#"{"role":"assistant","providerID":"opencode-go","modelID":"kimi-k3"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4)",
                params![
                    "part-1",
                    "message-1",
                    created,
                    r#"{"type":"step-finish","cost":3.0}"#
                ],
            )
            .unwrap();

        let snapshot = OpenCodeGoLocalUsageReader::with_database_path(database)
            .read(now)
            .unwrap();
        assert_eq!(snapshot.row_count, 1);
        assert_eq!(snapshot.primary.used_percent, 25.0);
    }

    #[test]
    fn timestamp_normalization_accepts_seconds_millis_and_micros() {
        assert_eq!(
            normalize_timestamp_ms(1_700_000_000),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            normalize_timestamp_ms(1_700_000_000_000),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            normalize_timestamp_ms(1_700_000_000_000_000),
            Some(1_700_000_000_000)
        );
    }
}
