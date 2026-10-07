use crate::accounts::{AccountId, VerifiedIdentity};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageWindowKind {
    Primary,
    Secondary,
    Additional,
}

/// Semantic identity of the selected primary lane. Providers may expose a
/// weekly lane as their only usable quota, so callers must not infer this
/// solely from `RateLimitWindow.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsagePrimaryWindowKind {
    Session,
    Weekly,
    Spend,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub kind: UsageWindowKind,
    pub name: String,
    pub used_percent: f64,
    pub reset_at_utc: Option<DateTime<Utc>>,
    pub limit_window_seconds: i64,
}

impl RateLimitWindow {
    /// Returns the percentage available to the user for display.
    ///
    /// Provider payloads and persistence keep `used_percent` as the canonical
    /// raw value. Hosts should use this method when showing quota to users.
    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }

    /// Returns the live countdown derived from the absolute reset timestamp.
    ///
    /// `limit_window_seconds` is the fixed duration of the provider window;
    /// it must never be used as a countdown because doing so makes the value
    /// change on every refresh and destroys the meaning of the persisted
    /// snapshot.
    pub fn seconds_until_reset(&self, now: DateTime<Utc>) -> Option<i64> {
        self.reset_at_utc
            .map(|reset| (reset - now).num_seconds().max(0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdditionalRateLimitWindow {
    pub key: String,
    pub name: String,
    pub window: RateLimitWindow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditsSnapshot {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<f64>,
    /// ISO 4217 currency for a monetary balance, when supplied by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency_code: Option<String>,
    pub approximate_message_cost: Option<f64>,
    /// The provider's monthly/individual credit cap when it is reported
    /// separately from the spend-control lane.
    #[serde(default)]
    pub limit: Option<CreditLimitSnapshot>,
    /// Distinguishes an unread balance from a confirmed zero balance.
    #[serde(default)]
    pub balance_read_succeeded: Option<bool>,
    /// Provider-reported availability, when it is explicit.
    #[serde(default)]
    pub credits_available: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditLimitSnapshot {
    pub limit: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub used_percent: Option<f64>,
    pub reset_at_utc: Option<DateTime<Utc>>,
    pub unit: Option<String>,
    pub read_succeeded: bool,
}

impl CreditLimitSnapshot {
    /// Converts the canonical used percentage into the user-facing remaining
    /// percentage without changing the stored/provider value.
    pub fn remaining_percent(&self) -> Option<f64> {
        self.used_percent
            .map(|value| (100.0 - value).clamp(0.0, 100.0))
    }
}

/// A non-mutating view of provider-issued reset credits. Providers may expose
/// these separately from their ordinary balance; keeping the inventory in the
/// normalized snapshot lets the UI show expiry/reset information without
/// coupling itself to a provider payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageCreditInventory {
    pub available_count: u32,
    pub credits: Vec<UsageCreditRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageCreditRecord {
    pub id: Option<String>,
    pub reset_type: Option<String>,
    pub status: Option<String>,
    pub granted_at_utc: Option<DateTime<Utc>>,
    pub expires_at_utc: Option<DateTime<Utc>>,
    pub redeem_started_at_utc: Option<DateTime<Utc>>,
    pub redeemed_at_utc: Option<DateTime<Utc>>,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendSnapshot {
    pub monthly_usage: Option<f64>,
    pub monthly_limit: Option<f64>,
    pub used_percent: Option<f64>,
    pub limit_enabled: Option<bool>,
    /// ISO 4217 currency for monetary amounts, when supplied by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency_code: Option<String>,
}

impl SpendSnapshot {
    /// Returns the user-facing remaining percentage when a spend limit exists.
    pub fn remaining_percent(&self) -> Option<f64> {
        self.used_percent
            .map(|value| (100.0 - value).clamp(0.0, 100.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageMetric {
    pub key: String,
    pub name: String,
    pub used_percent: Option<f64>,
    pub used_amount: Option<f64>,
    pub limit_amount: Option<f64>,
    pub remaining_amount: Option<f64>,
    pub unit: Option<String>,
    pub reset_at_utc: Option<DateTime<Utc>>,
    pub reset_label: Option<String>,
    pub metadata: HashMap<String, String>,
}

impl UsageMetric {
    /// Returns the user-facing remaining percentage.
    ///
    /// `used_percent` remains the canonical normalized/provider value so
    /// parsers, persistence, and scheduling retain their existing semantics.
    pub fn remaining_percent(&self) -> Option<f64> {
        self.used_percent
            .map(|value| (100.0 - value).clamp(0.0, 100.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub account_id: AccountId,
    pub observed_at_utc: DateTime<Utc>,
    pub response_account_id: Option<String>,
    pub plan_type: Option<String>,
    pub primary: Option<RateLimitWindow>,
    /// The provider-level meaning of `primary`, independent of its generic
    /// display role. Missing on legacy/non-Claude snapshots.
    #[serde(default)]
    pub primary_window_kind: Option<UsagePrimaryWindowKind>,
    /// True when the provider supplied no real primary lane and the adapter
    /// created a display-safe placeholder (for example Claude Web's missing
    /// five-hour lane).
    #[serde(default)]
    pub primary_window_is_synthetic: bool,
    pub secondary: Option<RateLimitWindow>,
    pub additional_windows: Vec<AdditionalRateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    #[serde(default)]
    pub credit_inventory: Option<UsageCreditInventory>,
    pub spend: Option<SpendSnapshot>,
    pub observed_email: Option<String>,
    pub is_stale: bool,
    pub stale_reason: Option<String>,
    pub stale_at_utc: Option<DateTime<Utc>>,
    pub metrics: Vec<UsageMetric>,
    /// Non-fatal source-level diagnostics collected while building this
    /// snapshot.  A provider can therefore expose valid quota data while
    /// recording that an optional enrichment (for example credits or
    /// activity) timed out, was forbidden, or returned an invalid payload.
    #[serde(default)]
    pub source_diagnostics: Vec<UsageSourceDiagnostic>,
    pub provider_id: String,
    pub source: Option<String>,
    pub data_confidence: String,
}

/// A low-usage Codex weekly observation that is plausible but not yet safe to
/// publish as a reset. It is kept separately from the last trusted snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWeeklyResetCandidate {
    pub evidence_version: u16,
    pub first_observed_at_utc: DateTime<Utc>,
    pub created_at_utc: DateTime<Utc>,
    pub snapshot: UsageSnapshot,
}

/// Put in a stale reason when the account must sign in again (or get a new
/// key), so hosts can offer that instead of a plain "couldn't update".
pub const SIGN_IN_REQUIRED: &str = "sign-in required";

impl UsageSnapshot {
    /// Returns every rate-limit window in display-independent order.
    ///
    /// The refresh coordinator uses this rather than knowing which provider
    /// calls a window "primary" or "secondary". Providers are free to add
    /// more windows without changing scheduler logic.
    pub fn all_rate_windows(&self) -> impl Iterator<Item = &RateLimitWindow> {
        self.primary
            .iter()
            .chain(self.secondary.iter())
            .chain(self.additional_windows.iter().map(|window| &window.window))
    }

    /// Shows every window and quota whose reset time passed after this reading
    /// as unused: usage read before a reset is gone once the reset passes,
    /// including when the provider resets early and the stored reading is
    /// the last one available. Returns whether anything changed.
    pub fn clear_elapsed_resets(&mut self, now: DateTime<Utc>) -> bool {
        let observed_at = self.observed_at_utc;
        let elapsed = |reset_at: Option<DateTime<Utc>>| {
            reset_at.is_some_and(|reset_at| observed_at < reset_at && reset_at <= now)
        };
        let mut changed = false;
        for window in self
            .primary
            .iter_mut()
            .chain(self.secondary.iter_mut())
            .chain(
                self.additional_windows
                    .iter_mut()
                    .map(|window| &mut window.window),
            )
        {
            if elapsed(window.reset_at_utc) && window.used_percent != 0.0 {
                window.used_percent = 0.0;
                changed = true;
            }
        }
        for metric in &mut self.metrics {
            if elapsed(metric.reset_at_utc) && metric.used_percent.is_some_and(|used| used != 0.0) {
                metric.used_percent = Some(0.0);
                changed = true;
            }
        }
        changed
    }

    /// Whether the reading went stale because the saved sign-in (or key)
    /// stopped working, and the provider's words for it.
    pub fn sign_in_required(&self) -> Option<&str> {
        let reason = self.stale_reason.as_deref().filter(|_| self.is_stale)?;
        let marker = format!(": {SIGN_IN_REQUIRED}: ");
        reason
            .find(&marker)
            .map(|start| reason[start + marker.len()..].trim())
    }

    /// Marks the snapshot stale. `stale_at_utc` records when the data first
    /// became stale, so repeated failures keep the original timestamp.
    pub fn mark_stale(&self, reason: impl Into<String>) -> Self {
        let mut next = self.clone();
        let first_stale_at = self.stale_at_utc.filter(|_| self.is_stale);
        next.is_stale = true;
        next.stale_reason = Some(reason.into());
        next.stale_at_utc = Some(first_stale_at.unwrap_or_else(Utc::now));
        next
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageAdapterErrorCode {
    AuthenticationUnavailable,
    Unauthorized,
    Forbidden,
    CloudflareChallenge,
    QuotaExhausted,
    RateLimited,
    TransientHttp,
    HttpError,
    NetworkFailure,
    InvalidPayload,
    NoSubscription,
    AccountMismatch,
    UnsupportedProvider,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageAdapterError {
    pub code: UsageAdapterErrorCode,
    pub message: String,
    pub http_status_code: Option<u16>,
    pub retry_after_seconds: Option<u64>,
}

/// A diagnostic attached to a successful snapshot for one provider source.
/// This is deliberately separate from `UsageAdapterError`: optional endpoint
/// failures must not turn an otherwise usable snapshot into a failed refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSourceDiagnostic {
    pub source: String,
    pub code: UsageAdapterErrorCode,
    pub message: String,
    pub http_status_code: Option<u16>,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageProbeResult {
    pub snapshot: Option<UsageSnapshot>,
    pub error: Option<UsageAdapterError>,
    pub identity: Option<VerifiedIdentity>,
    pub session_token_was_refreshed: bool,
}

impl UsageProbeResult {
    pub fn success(snapshot: UsageSnapshot, identity: Option<VerifiedIdentity>) -> Self {
        Self {
            snapshot: Some(snapshot),
            error: None,
            identity,
            session_token_was_refreshed: false,
        }
    }

    pub fn failure(error: UsageAdapterError) -> Self {
        Self {
            snapshot: None,
            error: Some(error),
            identity: None,
            session_token_was_refreshed: false,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.snapshot.is_some() && self.error.is_none()
    }
}

#[async_trait]
pub trait UsageAdapter: Send + Sync {
    fn adapter_id(&self) -> &str;

    async fn probe(
        &self,
        account: &crate::accounts::AccountRecord,
    ) -> Result<UsageProbeResult, crate::transport::TransportError>;
}

#[async_trait]
pub trait UsageSnapshotStore: Send + Sync {
    async fn get_latest(
        &self,
        account_id: AccountId,
    ) -> Result<Option<UsageSnapshot>, StorageError>;
    async fn save(&self, snapshot: UsageSnapshot) -> Result<(), StorageError>;
    /// Marks the latest stored snapshot stale and returns it. Stores that keep
    /// history should update the latest row in place rather than appending a
    /// duplicate observation for every failed refresh. Returns `None` when the
    /// account has no stored snapshot.
    async fn mark_latest_stale(
        &self,
        account_id: AccountId,
        reason: &str,
    ) -> Result<Option<UsageSnapshot>, StorageError> {
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
    ) -> Result<Option<CodexWeeklyResetCandidate>, StorageError>;
    /// Passing `None` removes any pending candidate for this account.
    async fn save_codex_weekly_reset_candidate(
        &self,
        account_id: AccountId,
        candidate: Option<CodexWeeklyResetCandidate>,
    ) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::{CreditsSnapshot, RateLimitWindow, SpendSnapshot, UsageMetric, UsageWindowKind};
    use std::collections::HashMap;

    #[test]
    fn display_percentages_are_remaining_not_used() {
        let window = RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "primary".to_owned(),
            used_percent: 76.0,
            reset_at_utc: None,
            limit_window_seconds: 18_000,
        };
        assert_eq!(window.remaining_percent(), 24.0);

        let metric = UsageMetric {
            key: "primary".to_owned(),
            name: "primary".to_owned(),
            used_percent: Some(76.0),
            used_amount: None,
            limit_amount: None,
            remaining_amount: None,
            unit: None,
            reset_at_utc: None,
            reset_label: None,
            metadata: HashMap::new(),
        };
        assert_eq!(metric.remaining_percent(), Some(24.0));

        let spend = SpendSnapshot {
            monthly_usage: None,
            monthly_limit: None,
            used_percent: Some(76.0),
            limit_enabled: Some(true),
            currency_code: None,
        };
        assert_eq!(spend.remaining_percent(), Some(24.0));
    }

    #[test]
    fn currency_fields_are_backward_compatible_and_omitted_when_unknown() {
        let spend: SpendSnapshot = serde_json::from_value(serde_json::json!({
            "monthly_usage": 5.0,
            "monthly_limit": 10.0,
            "used_percent": 50.0,
            "limit_enabled": true
        }))
        .unwrap();
        assert_eq!(spend.currency_code, None);
        assert!(
            serde_json::to_value(spend)
                .unwrap()
                .get("currency_code")
                .is_none()
        );

        let credits: CreditsSnapshot = serde_json::from_value(serde_json::json!({
            "has_credits": true,
            "unlimited": false,
            "balance": 2.0,
            "approximate_message_cost": null
        }))
        .unwrap();
        assert_eq!(credits.currency_code, None);
        assert!(
            serde_json::to_value(credits)
                .unwrap()
                .get("currency_code")
                .is_none()
        );
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage failed: {0}")]
    Backend(String),
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
}

#[derive(Default)]
pub struct InMemoryUsageSnapshotStore {
    snapshots: RwLock<HashMap<AccountId, UsageSnapshot>>,
    codex_weekly_reset_candidates: RwLock<HashMap<AccountId, CodexWeeklyResetCandidate>>,
}

#[async_trait]
impl UsageSnapshotStore for InMemoryUsageSnapshotStore {
    async fn get_latest(
        &self,
        account_id: AccountId,
    ) -> Result<Option<UsageSnapshot>, StorageError> {
        Ok(self.snapshots.read().await.get(&account_id).cloned())
    }

    async fn save(&self, snapshot: UsageSnapshot) -> Result<(), StorageError> {
        self.snapshots
            .write()
            .await
            .insert(snapshot.account_id, snapshot);
        Ok(())
    }

    async fn get_codex_weekly_reset_candidate(
        &self,
        account_id: AccountId,
    ) -> Result<Option<CodexWeeklyResetCandidate>, StorageError> {
        Ok(self
            .codex_weekly_reset_candidates
            .read()
            .await
            .get(&account_id)
            .cloned())
    }

    async fn save_codex_weekly_reset_candidate(
        &self,
        account_id: AccountId,
        candidate: Option<CodexWeeklyResetCandidate>,
    ) -> Result<(), StorageError> {
        let mut candidates = self.codex_weekly_reset_candidates.write().await;
        if let Some(candidate) = candidate {
            if candidate.snapshot.account_id != account_id {
                return Err(StorageError::InvalidData(
                    "Codex reset candidate belongs to a different account".to_owned(),
                ));
            }
            candidates.insert(account_id, candidate);
        } else {
            candidates.remove(&account_id);
        }
        Ok(())
    }
}
