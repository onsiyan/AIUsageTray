//! Provider-independent refresh orchestration.
//!
//! This module deliberately knows nothing about HTTP, OAuth, or a provider's
//! response format. Adapters own those concerns; this layer owns the things
//! that are easy to get subtly wrong when several accounts are refreshed at
//! once: coalescing, bounded concurrency, generation-free publication through
//! a single shared result, stale-result retention, cadence, and reset-boundary
//! refreshes.

use crate::{
    accounts::{
        AccountId, AccountRecord, AccountStatus, AccountStore, AccountStoreError, OPENAI,
        VerifiedIdentity,
    },
    providers::registry::ProviderRegistry,
    transport::TransportError,
    usage::{
        StorageError, UsageAdapter, UsageAdapterError, UsageAdapterErrorCode, UsageSnapshot,
        UsageSnapshotStore,
    },
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Notify, Semaphore, watch};

const LOW_POWER_MINIMUM_INTERVAL: Duration = Duration::from_secs(30 * 60);
const RESET_BOUNDARY_GRACE: Duration = Duration::from_secs(30);
const RESET_BOUNDARY_MINIMUM_DELAY: Duration = Duration::from_secs(5);
const MAX_ATTEMPTED_RESET_BOUNDARIES: usize = 64;

/// Automatic refresh cadence. `Adaptive` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshCadence {
    Manual,
    Fixed(Duration),
    Adaptive,
}

impl RefreshCadence {
    fn normalized(self) -> Self {
        match self {
            Self::Fixed(interval) if interval.is_zero() => Self::Fixed(Duration::from_secs(1)),
            cadence => cadence,
        }
    }
}

/// Why a refresh was started. The reason is observable by the host/UI but is
/// never interpreted by an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    Manual,
    Scheduled,
    ResetBoundary,
}

/// Signals used by the pure adaptive policy. A UI can update these without
/// knowing anything about timers or provider implementations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdaptiveRefreshSignals {
    pub last_interaction_at: Option<DateTime<Utc>>,
    pub last_coding_activity_at: Option<DateTime<Utc>>,
    pub low_power_mode_enabled: bool,
    pub thermal_constrained: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveRefreshReason {
    RecentInteraction,
    CodingActivity,
    Warm,
    Idle,
    LongIdle,
    Constrained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveRefreshDecision {
    pub delay: Duration,
    pub reason: AdaptiveRefreshReason,
}

/// Thresholds tuned for the provider reset windows.
pub struct AdaptiveRefreshPolicy;

impl AdaptiveRefreshPolicy {
    pub fn decide(now: DateTime<Utc>, signals: &AdaptiveRefreshSignals) -> AdaptiveRefreshDecision {
        if signals.low_power_mode_enabled || signals.thermal_constrained {
            return AdaptiveRefreshDecision {
                delay: Duration::from_secs(30 * 60),
                reason: AdaptiveRefreshReason::Constrained,
            };
        }

        let base = match signals.last_interaction_at {
            None => AdaptiveRefreshDecision {
                delay: Duration::from_secs(30 * 60),
                reason: AdaptiveRefreshReason::LongIdle,
            },
            Some(last_interaction_at) => {
                let age = now.signed_duration_since(last_interaction_at);
                if age <= ChronoDuration::minutes(5) {
                    AdaptiveRefreshDecision {
                        delay: Duration::from_secs(2 * 60),
                        reason: AdaptiveRefreshReason::RecentInteraction,
                    }
                } else if age <= ChronoDuration::hours(1) {
                    AdaptiveRefreshDecision {
                        delay: Duration::from_secs(5 * 60),
                        reason: AdaptiveRefreshReason::Warm,
                    }
                } else if age < ChronoDuration::hours(4) {
                    AdaptiveRefreshDecision {
                        delay: Duration::from_secs(15 * 60),
                        reason: AdaptiveRefreshReason::Idle,
                    }
                } else {
                    AdaptiveRefreshDecision {
                        delay: Duration::from_secs(30 * 60),
                        reason: AdaptiveRefreshReason::LongIdle,
                    }
                }
            }
        };

        if let Some(last_coding_activity_at) = signals.last_coding_activity_at {
            let activity_age = now.signed_duration_since(last_coding_activity_at);
            if activity_age < ChronoDuration::minutes(5) && base.delay > Duration::from_secs(5 * 60)
            {
                return AdaptiveRefreshDecision {
                    delay: Duration::from_secs(5 * 60),
                    reason: AdaptiveRefreshReason::CodingActivity,
                };
            }
        }

        base
    }
}

/// Configuration for the coordinator and its optional background loop.
#[derive(Debug, Clone)]
pub struct RefreshCoordinatorConfig {
    pub cadence: RefreshCadence,
    pub max_concurrency: usize,
    pub reset_boundary_grace: Duration,
    pub reset_boundary_minimum_delay: Duration,
    pub max_attempted_reset_boundaries: usize,
}

impl Default for RefreshCoordinatorConfig {
    fn default() -> Self {
        Self {
            cadence: RefreshCadence::Adaptive,
            max_concurrency: 4,
            reset_boundary_grace: RESET_BOUNDARY_GRACE,
            reset_boundary_minimum_delay: RESET_BOUNDARY_MINIMUM_DELAY,
            max_attempted_reset_boundaries: MAX_ATTEMPTED_RESET_BOUNDARIES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStatus {
    Updated,
    RetainedStale,
    Failed,
    Invalidated,
    Skipped,
}

/// A single account refresh result. A retained stale snapshot is deliberately
/// returned to callers so a UI never has to replace a known value with zero
/// merely because a provider was temporarily unavailable.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub account_id: AccountId,
    pub provider_id: String,
    pub reason: RefreshReason,
    pub status: RefreshStatus,
    pub snapshot: Option<UsageSnapshot>,
    pub identity: Option<VerifiedIdentity>,
    pub error: Option<UsageAdapterError>,
    pub storage_error: Option<String>,
    pub completed_at_utc: DateTime<Utc>,
}

impl RefreshOutcome {
    fn new(
        account: &AccountRecord,
        reason: RefreshReason,
        status: RefreshStatus,
        snapshot: Option<UsageSnapshot>,
        identity: Option<VerifiedIdentity>,
        error: Option<UsageAdapterError>,
        storage_error: Option<String>,
    ) -> Self {
        Self {
            account_id: account.id,
            provider_id: account.provider_id.clone(),
            reason,
            status,
            snapshot,
            identity,
            error,
            storage_error,
            completed_at_utc: Utc::now(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshCoordinatorError {
    #[error("account store failed: {0}")]
    AccountStore(String),
}

impl From<AccountStoreError> for RefreshCoordinatorError {
    fn from(error: AccountStoreError) -> Self {
        Self::AccountStore(error.to_string())
    }
}

struct RefreshFlight {
    account_id: AccountId,
    sender: watch::Sender<Option<RefreshOutcome>>,
    receiver: watch::Receiver<Option<RefreshOutcome>>,
}

/// Coordinates every provider without coupling the providers to one another.
/// Construct it behind an `Arc`; background workers and coalesced callers use
/// the same coordinator instance.
pub struct UsageRefreshCoordinator {
    account_store: Arc<dyn AccountStore>,
    snapshot_store: Arc<dyn UsageSnapshotStore>,
    adapters: HashMap<String, Arc<dyn UsageAdapter>>,
    concurrency: Arc<Semaphore>,
    flights: Mutex<HashMap<AccountId, Arc<RefreshFlight>>>,
    config: RefreshCoordinatorConfig,
    signals: Mutex<AdaptiveRefreshSignals>,
    wake: Notify,
    shutdown: watch::Sender<bool>,
    attempted_reset_boundaries: Mutex<VecDeque<DateTime<Utc>>>,
}

impl UsageRefreshCoordinator {
    pub fn new_with_registry(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        registry: &ProviderRegistry,
        config: RefreshCoordinatorConfig,
    ) -> Arc<Self> {
        Self::new(account_store, snapshot_store, registry.adapters(), config)
    }

    pub fn new(
        account_store: Arc<dyn AccountStore>,
        snapshot_store: Arc<dyn UsageSnapshotStore>,
        adapters: impl IntoIterator<Item = Arc<dyn UsageAdapter>>,
        config: RefreshCoordinatorConfig,
    ) -> Arc<Self> {
        let config = RefreshCoordinatorConfig {
            cadence: config.cadence.normalized(),
            max_concurrency: config.max_concurrency.max(1),
            reset_boundary_grace: config.reset_boundary_grace,
            reset_boundary_minimum_delay: config.reset_boundary_minimum_delay,
            max_attempted_reset_boundaries: config.max_attempted_reset_boundaries.max(1),
        };
        let mut adapter_registry = HashMap::new();
        for adapter in adapters {
            let adapter_id = adapter.adapter_id().to_ascii_lowercase();
            adapter_registry.insert(adapter_id.clone(), Arc::clone(&adapter));
            // The OpenAI implementation keeps its internal adapter id
            // (`openai-wham`) distinct from the persisted provider id. Legacy
            // accounts may also use `codex`; both names resolve to that same
            // adapter without making the adapter aware of storage aliases.
            if adapter_id == "openai-wham" {
                adapter_registry.insert(OPENAI.to_owned(), Arc::clone(&adapter));
                adapter_registry.insert("codex".to_owned(), adapter);
            }
        }
        let (shutdown, _) = watch::channel(false);
        Arc::new(Self {
            account_store,
            snapshot_store,
            adapters: adapter_registry,
            concurrency: Arc::new(Semaphore::new(config.max_concurrency)),
            flights: Mutex::new(HashMap::new()),
            config,
            signals: Mutex::new(AdaptiveRefreshSignals::default()),
            wake: Notify::new(),
            shutdown,
            attempted_reset_boundaries: Mutex::new(VecDeque::new()),
        })
    }

    pub fn cadence(&self) -> RefreshCadence {
        self.config.cadence
    }

    /// Starts one worker for a given account if one is not already running.
    /// All concurrent callers then await and receive the same result.
    pub async fn refresh_account(
        self: &Arc<Self>,
        account: AccountRecord,
        reason: RefreshReason,
    ) -> RefreshOutcome {
        let (flight, is_owner) = {
            let mut flights = self.flights.lock().await;
            if let Some(flight) = flights.get(&account.id) {
                (Arc::clone(flight), false)
            } else {
                let (sender, receiver) = watch::channel(None);
                let flight = Arc::new(RefreshFlight {
                    account_id: account.id,
                    sender,
                    receiver,
                });
                flights.insert(account.id, Arc::clone(&flight));
                (flight, true)
            }
        };

        if is_owner {
            let coordinator = Arc::clone(self);
            let worker_flight = Arc::clone(&flight);
            tokio::spawn(async move {
                let outcome = coordinator.refresh_account_inner(account, reason).await;
                let _ = worker_flight.sender.send(Some(outcome));
                let mut flights = coordinator.flights.lock().await;
                if flights
                    .get(&worker_flight.account_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &worker_flight))
                {
                    flights.remove(&worker_flight.account_id);
                }
            });
        }

        let mut receiver = flight.receiver.clone();
        loop {
            if let Some(outcome) = receiver.borrow().clone() {
                return outcome;
            }
            if receiver.changed().await.is_err() {
                return RefreshOutcome::new(
                    &placeholder_account(flight.account_id),
                    reason,
                    RefreshStatus::Failed,
                    None,
                    None,
                    Some(UsageAdapterError {
                        code: UsageAdapterErrorCode::Cancelled,
                        message: "refresh worker ended before publishing a result".to_owned(),
                        http_status_code: None,
                        retry_after_seconds: None,
                    }),
                    None,
                );
            }
        }
    }

    /// Refreshes all active accounts. The semaphore still bounds provider
    /// calls, while the batch itself is launched concurrently.
    pub async fn refresh_all(
        self: &Arc<Self>,
        reason: RefreshReason,
    ) -> Result<Vec<RefreshOutcome>, RefreshCoordinatorError> {
        let accounts = self.account_store.list().await?;
        let handles = accounts
            .into_iter()
            .map(|account| {
                let coordinator = Arc::clone(self);
                tokio::spawn(async move { coordinator.refresh_account(account, reason).await })
            })
            .collect::<Vec<_>>();
        let mut outcomes = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(outcome) = handle.await {
                outcomes.push(outcome);
            }
        }
        Ok(outcomes)
    }

    pub async fn note_interaction(&self, at: DateTime<Utc>) {
        self.signals.lock().await.last_interaction_at = Some(at);
        self.wake.notify_waiters();
    }

    pub async fn note_coding_activity(&self, at: DateTime<Utc>) {
        self.signals.lock().await.last_coding_activity_at = Some(at);
        self.wake.notify_waiters();
    }

    pub async fn set_power_state(&self, low_power_mode_enabled: bool, thermal_constrained: bool) {
        let mut signals = self.signals.lock().await;
        signals.low_power_mode_enabled = low_power_mode_enabled;
        signals.thermal_constrained = thermal_constrained;
        drop(signals);
        self.wake.notify_waiters();
    }

    pub async fn adaptive_decision(&self, now: DateTime<Utc>) -> AdaptiveRefreshDecision {
        let signals = self.signals.lock().await;
        AdaptiveRefreshPolicy::decide(now, &signals)
    }

    /// Requests graceful shutdown of a running background loop.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        self.wake.notify_waiters();
    }

    /// Runs the backend timer loop: an immediate pass, then fixed or
    /// adaptive ticks, with one-shot reset-boundary passes pulled into the
    /// current normal interval.
    pub async fn run(self: Arc<Self>) -> Result<(), RefreshCoordinatorError> {
        let initial_outcomes = self.refresh_all(RefreshReason::Manual).await?;
        let mut snapshots = snapshot_cache(initial_outcomes);
        let mut shutdown = self.shutdown.subscribe();
        if *shutdown.borrow() {
            return Ok(());
        }

        let mut scheduled_at = self
            .normal_interval(Utc::now())
            .await
            .map(|interval| Utc::now() + chrono_from_std(interval));

        loop {
            let Some(interval) = self.normal_interval(Utc::now()).await else {
                tokio::select! {
                    result = shutdown.changed() => {
                        if result.is_err() || *shutdown.borrow() { return Ok(()); }
                    }
                }
                continue;
            };

            let now = Utc::now();
            if scheduled_at.is_none() {
                scheduled_at = Some(now + chrono_from_std(interval));
            }
            let normal_deadline = scheduled_at.expect("scheduled deadline is initialized");
            let attempted = self.attempted_boundaries().await;
            let minimum_automatic_interval = self
                .signals
                .lock()
                .await
                .low_power_mode_enabled
                .then_some(LOW_POWER_MINIMUM_INTERVAL);
            let candidate = next_reset_boundary_refresh_candidate(
                &snapshots.values().cloned().collect::<Vec<_>>(),
                interval,
                self.config.reset_boundary_grace,
                self.config.reset_boundary_minimum_delay,
                minimum_automatic_interval,
                &attempted,
                now,
            );
            let target = candidate
                .map(|candidate| candidate.refresh_at.min(normal_deadline))
                .unwrap_or(normal_deadline);
            let sleep_for = (target - now).to_std().unwrap_or_else(|_| Duration::ZERO);

            tokio::select! {
                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
                _ = self.wake.notified() => {
                    if self.config.cadence == RefreshCadence::Adaptive {
                        if let Some(decision) = self.normal_interval(Utc::now()).await {
                            let earlier = Utc::now() + chrono_from_std(decision);
                            if scheduled_at.is_none_or(|scheduled| earlier < scheduled) {
                                scheduled_at = Some(earlier);
                            }
                        }
                    }
                    continue;
                }
                _ = tokio::time::sleep(sleep_for) => {}
            }

            if *shutdown.borrow() {
                return Ok(());
            }
            let completed_at = Utc::now();
            let reset_due = candidate.is_some_and(|candidate| {
                completed_at >= candidate.refresh_at && candidate.refresh_at <= normal_deadline
            });
            let normal_due = completed_at >= normal_deadline;
            if reset_due {
                let boundary = candidate
                    .expect("reset candidate exists")
                    .boundary_refresh_at;
                self.record_attempted_boundary(boundary).await;
            }
            let reason = if reset_due {
                RefreshReason::ResetBoundary
            } else {
                RefreshReason::Scheduled
            };
            let outcomes = self.refresh_all(reason).await?;
            snapshots = update_snapshot_cache(snapshots, outcomes);

            if normal_due {
                scheduled_at = match self.config.cadence {
                    RefreshCadence::Fixed(_) => Some(next_fixed_scheduled_at(
                        normal_deadline,
                        Utc::now(),
                        interval,
                    )),
                    RefreshCadence::Adaptive => self
                        .normal_interval(Utc::now())
                        .await
                        .map(|next| Utc::now() + chrono_from_std(next)),
                    RefreshCadence::Manual => None,
                };
            }
        }
    }

    async fn normal_interval(&self, now: DateTime<Utc>) -> Option<Duration> {
        let requested = match self.config.cadence {
            RefreshCadence::Manual => None,
            RefreshCadence::Fixed(interval) => Some(interval),
            RefreshCadence::Adaptive => {
                let signals = self.signals.lock().await;
                Some(AdaptiveRefreshPolicy::decide(now, &signals).delay)
            }
        }?;
        let low_power = self.signals.lock().await.low_power_mode_enabled;
        Some(if low_power {
            requested.max(LOW_POWER_MINIMUM_INTERVAL)
        } else {
            requested
        })
    }

    async fn attempted_boundaries(&self) -> HashSet<DateTime<Utc>> {
        self.attempted_reset_boundaries
            .lock()
            .await
            .iter()
            .copied()
            .collect()
    }

    async fn record_attempted_boundary(&self, boundary: DateTime<Utc>) {
        let mut attempted = self.attempted_reset_boundaries.lock().await;
        if !attempted.contains(&boundary) {
            attempted.push_back(boundary);
        }
        while attempted.len() > self.config.max_attempted_reset_boundaries {
            attempted.pop_front();
        }
    }

    async fn refresh_account_inner(
        &self,
        account: AccountRecord,
        reason: RefreshReason,
    ) -> RefreshOutcome {
        if matches!(
            account.status,
            AccountStatus::Paused | AccountStatus::Disabled
        ) {
            return RefreshOutcome::new(
                &account,
                reason,
                RefreshStatus::Skipped,
                None,
                None,
                None,
                None,
            );
        }

        let prior = match self.snapshot_store.get_latest(account.id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return RefreshOutcome::new(
                    &account,
                    reason,
                    RefreshStatus::Failed,
                    None,
                    None,
                    Some(storage_as_adapter_error(&error)),
                    Some(error.to_string()),
                );
            }
        };

        let Some(adapter) = self.adapters.get(&account.provider_id.to_ascii_lowercase()) else {
            return self
                .finish_failure(
                    &account,
                    reason,
                    prior,
                    UsageAdapterError {
                        code: UsageAdapterErrorCode::UnsupportedProvider,
                        message: format!(
                            "no usage adapter is registered for {}",
                            account.provider_id
                        ),
                        http_status_code: None,
                        retry_after_seconds: None,
                    },
                )
                .await;
        };

        let _permit = match Arc::clone(&self.concurrency).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return RefreshOutcome::new(
                    &account,
                    reason,
                    RefreshStatus::Failed,
                    prior,
                    None,
                    Some(UsageAdapterError {
                        code: UsageAdapterErrorCode::Cancelled,
                        message: "refresh concurrency gate was closed".to_owned(),
                        http_status_code: None,
                        retry_after_seconds: None,
                    }),
                    None,
                );
            }
        };

        let probe = match adapter.probe(&account).await {
            Ok(probe) => probe,
            Err(error) => {
                return self
                    .finish_failure(&account, reason, prior, transport_as_adapter_error(error))
                    .await;
            }
        };

        if probe.succeeded() {
            if let Some(snapshot) = probe.snapshot.clone() {
                if snapshot.account_id != account.id
                    || !provider_ids_match(&account.provider_id, &snapshot.provider_id)
                {
                    return self
                        .finish_failure(
                            &account,
                            reason,
                            prior,
                            UsageAdapterError {
                                code: UsageAdapterErrorCode::AccountMismatch,
                                message: "adapter returned a snapshot for a different account or provider".to_owned(),
                                http_status_code: None,
                                retry_after_seconds: None,
                            },
                        )
                        .await;
                }
                if prior
                    .as_ref()
                    .is_some_and(|previous| snapshot.observed_at_utc < previous.observed_at_utc)
                {
                    return self
                        .finish_failure(
                            &account,
                            reason,
                            prior,
                            UsageAdapterError {
                                code: UsageAdapterErrorCode::InvalidPayload,
                                message:
                                    "adapter returned a snapshot older than the stored snapshot"
                                        .to_owned(),
                                http_status_code: None,
                                retry_after_seconds: None,
                            },
                        )
                        .await;
                }
                let identity = probe.identity.clone();
                let identity_storage_error = identity.as_ref().and_then(|identity| {
                    let updated = account
                        .with_identity(
                            identity.email.as_deref(),
                            identity.provider_account_id.as_deref(),
                        )
                        .ok()?;
                    Some(updated)
                });
                let identity_storage_error = match identity_storage_error {
                    Some(updated) => self
                        .account_store
                        .upsert(&updated)
                        .await
                        .err()
                        .map(|error| error.to_string()),
                    None if identity.is_some() => {
                        Some("verified identity could not be normalized".to_owned())
                    }
                    None => None,
                };
                if let Err(error) = self.snapshot_store.save(snapshot.clone()).await {
                    return RefreshOutcome::new(
                        &account,
                        reason,
                        RefreshStatus::Failed,
                        prior,
                        identity,
                        None,
                        Some(combine_storage_errors(
                            identity_storage_error,
                            error.to_string(),
                        )),
                    );
                }
                return RefreshOutcome::new(
                    &account,
                    reason,
                    RefreshStatus::Updated,
                    Some(snapshot),
                    identity,
                    None,
                    identity_storage_error,
                );
            }
        }

        let error = probe.error.unwrap_or(UsageAdapterError {
            code: UsageAdapterErrorCode::InvalidPayload,
            message: "adapter returned neither a usable snapshot nor an error".to_owned(),
            http_status_code: None,
            retry_after_seconds: None,
        });
        self.finish_failure(&account, reason, prior, error).await
    }

    async fn finish_failure(
        &self,
        account: &AccountRecord,
        reason: RefreshReason,
        prior: Option<UsageSnapshot>,
        error: UsageAdapterError,
    ) -> RefreshOutcome {
        if should_retain_stale(&error.code) {
            if let Some(prior) = prior {
                let stale = prior.mark_stale(format!("{}: {}", account.provider_id, error.message));
                let storage_error = self
                    .snapshot_store
                    .save(stale.clone())
                    .await
                    .err()
                    .map(|error| error.to_string());
                return RefreshOutcome::new(
                    account,
                    reason,
                    RefreshStatus::RetainedStale,
                    Some(stale),
                    None,
                    Some(error),
                    storage_error,
                );
            }
        }

        let status = if should_invalidate(&error.code) {
            RefreshStatus::Invalidated
        } else {
            RefreshStatus::Failed
        };
        RefreshOutcome::new(account, reason, status, None, None, Some(error), None)
    }
}

/// Candidate used by reset-boundary scheduling. `boundary_refresh_at` is the
/// deduplication key; `refresh_at` includes minimum-delay and low-power rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetBoundaryRefreshCandidate {
    pub refresh_at: DateTime<Utc>,
    pub boundary_refresh_at: DateTime<Utc>,
}

/// Reset-boundary rules: only pull a refresh forward when
/// the boundary is inside the next normal interval and the stored snapshot
/// predates it.
pub fn next_reset_boundary_refresh_candidate(
    snapshots: &[UsageSnapshot],
    normal_refresh_interval: Duration,
    reset_boundary_grace: Duration,
    reset_boundary_minimum_delay: Duration,
    minimum_automatic_refresh_interval: Option<Duration>,
    attempted_boundary_refreshes: &HashSet<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<ResetBoundaryRefreshCandidate> {
    let normal_deadline = now + chrono_from_std(normal_refresh_interval);
    let earliest_allowed = (now + chrono_from_std(reset_boundary_minimum_delay)).max(
        minimum_automatic_refresh_interval
            .map(|interval| now + chrono_from_std(interval))
            .unwrap_or(now),
    );
    snapshots
        .iter()
        .flat_map(|snapshot| {
            snapshot
                .all_rate_windows()
                .map(move |window| (snapshot, window))
        })
        .filter_map(|(snapshot, window)| {
            let reset_at = window.reset_at_utc?;
            let boundary_refresh_at = reset_at + chrono_from_std(reset_boundary_grace);
            if attempted_boundary_refreshes.contains(&boundary_refresh_at)
                || boundary_refresh_at > normal_deadline
                || snapshot.observed_at_utc >= boundary_refresh_at
            {
                return None;
            }
            let refresh_at = boundary_refresh_at.max(earliest_allowed);
            (refresh_at <= normal_deadline).then_some(ResetBoundaryRefreshCandidate {
                refresh_at,
                boundary_refresh_at,
            })
        })
        .min_by_key(|candidate| candidate.refresh_at)
}

/// Advances from the previous scheduled tick and skips missed ticks instead
/// of creating a catch-up burst after a slow provider request.
pub fn next_fixed_scheduled_at(
    previous_scheduled_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    interval: Duration,
) -> DateTime<Utc> {
    assert!(
        !interval.is_zero(),
        "fixed refresh interval must be positive"
    );
    let increment = chrono_from_std(interval);
    let mut next = previous_scheduled_at + increment;
    while next <= completed_at {
        next += increment;
    }
    next
}

fn snapshot_cache(outcomes: Vec<RefreshOutcome>) -> HashMap<AccountId, UsageSnapshot> {
    update_snapshot_cache(HashMap::new(), outcomes)
}

fn update_snapshot_cache(
    mut snapshots: HashMap<AccountId, UsageSnapshot>,
    outcomes: Vec<RefreshOutcome>,
) -> HashMap<AccountId, UsageSnapshot> {
    for outcome in outcomes {
        if let Some(snapshot) = outcome.snapshot {
            snapshots.insert(outcome.account_id, snapshot);
        } else if outcome.status == RefreshStatus::Invalidated {
            snapshots.remove(&outcome.account_id);
        }
    }
    snapshots
}

fn should_retain_stale(code: &UsageAdapterErrorCode) -> bool {
    matches!(
        code,
        UsageAdapterErrorCode::QuotaExhausted
            | UsageAdapterErrorCode::RateLimited
            | UsageAdapterErrorCode::CloudflareChallenge
            | UsageAdapterErrorCode::TransientHttp
            | UsageAdapterErrorCode::HttpError
            | UsageAdapterErrorCode::NetworkFailure
            | UsageAdapterErrorCode::InvalidPayload
    )
}

fn should_invalidate(code: &UsageAdapterErrorCode) -> bool {
    matches!(
        code,
        UsageAdapterErrorCode::AuthenticationUnavailable
            | UsageAdapterErrorCode::Unauthorized
            | UsageAdapterErrorCode::Forbidden
            | UsageAdapterErrorCode::AccountMismatch
            | UsageAdapterErrorCode::UnsupportedProvider
            | UsageAdapterErrorCode::Cancelled
    )
}

fn transport_as_adapter_error(error: TransportError) -> UsageAdapterError {
    UsageAdapterError {
        code: UsageAdapterErrorCode::NetworkFailure,
        message: error.to_string(),
        http_status_code: None,
        retry_after_seconds: None,
    }
}

fn storage_as_adapter_error(error: &StorageError) -> UsageAdapterError {
    UsageAdapterError {
        code: UsageAdapterErrorCode::Unknown,
        message: error.to_string(),
        http_status_code: None,
        retry_after_seconds: None,
    }
}

fn combine_storage_errors(first: Option<String>, second: String) -> String {
    match first {
        Some(first) => format!("{first}; {second}"),
        None => second,
    }
}

fn chrono_from_std(duration: Duration) -> ChronoDuration {
    ChronoDuration::from_std(duration).expect("refresh duration must fit in chrono")
}

fn provider_ids_match(account_provider_id: &str, snapshot_provider_id: &str) -> bool {
    account_provider_id == snapshot_provider_id
        || (account_provider_id == "codex" && snapshot_provider_id == OPENAI)
}

fn placeholder_account(account_id: AccountId) -> AccountRecord {
    let now = Utc::now();
    AccountRecord {
        id: account_id,
        label: "unknown".to_owned(),
        email: String::new(),
        provider_account_id: None,
        created_at_utc: now,
        updated_at_utc: now,
        status: AccountStatus::Active,
        provider_id: "unknown".to_owned(),
        browser_kind: None,
        browser_profile_id: None,
        workspace_id: None,
        codex_home: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{AccountStore, InMemoryAccountStore, OPENAI, VerifiedIdentity},
        usage::{
            CreditsSnapshot, RateLimitWindow, UsageAdapter, UsageProbeResult, UsageSnapshotStore,
            UsageWindowKind,
        },
    };
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn adaptive_policy_matches_reset_window_thresholds() {
        let at = now();
        let mut signals = AdaptiveRefreshSignals::default();
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals),
            AdaptiveRefreshDecision {
                delay: Duration::from_secs(30 * 60),
                reason: AdaptiveRefreshReason::LongIdle,
            }
        );
        signals.last_interaction_at = Some(at - ChronoDuration::minutes(5));
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals).delay,
            Duration::from_secs(2 * 60)
        );
        signals.last_interaction_at = Some(at - ChronoDuration::minutes(6));
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals).delay,
            Duration::from_secs(5 * 60)
        );
        signals.last_interaction_at = Some(at - ChronoDuration::hours(2));
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals).delay,
            Duration::from_secs(15 * 60)
        );
        signals.last_interaction_at = Some(at - ChronoDuration::hours(4));
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals).delay,
            Duration::from_secs(30 * 60)
        );
        signals.last_coding_activity_at = Some(at - ChronoDuration::minutes(1));
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals).reason,
            AdaptiveRefreshReason::CodingActivity
        );
        signals.low_power_mode_enabled = true;
        assert_eq!(
            AdaptiveRefreshPolicy::decide(at, &signals).reason,
            AdaptiveRefreshReason::Constrained
        );
    }

    #[test]
    fn cloudflare_challenge_retains_usage_without_invalidating_auth() {
        assert!(should_retain_stale(
            &UsageAdapterErrorCode::CloudflareChallenge
        ));
        assert!(!should_invalidate(
            &UsageAdapterErrorCode::CloudflareChallenge
        ));
    }

    #[test]
    fn fixed_timer_skips_missed_ticks() {
        let previous = now();
        let completed = previous + ChronoDuration::minutes(7);
        assert_eq!(
            next_fixed_scheduled_at(previous, completed, Duration::from_secs(5 * 60)),
            previous + ChronoDuration::minutes(10)
        );
    }

    fn snapshot(
        account_id: AccountId,
        observed_at: DateTime<Utc>,
        reset_at: DateTime<Utc>,
    ) -> UsageSnapshot {
        UsageSnapshot {
            account_id,
            observed_at_utc: observed_at,
            response_account_id: None,
            plan_type: Some("test".to_owned()),
            primary: Some(RateLimitWindow {
                kind: UsageWindowKind::Primary,
                name: "session".to_owned(),
                used_percent: 20.0,
                reset_at_utc: Some(reset_at),
                limit_window_seconds: 18_000,
            }),
            primary_window_kind: None,
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: Some(CreditsSnapshot {
                has_credits: Some(true),
                unlimited: Some(false),
                balance: None,
                currency_code: None,
                approximate_message_cost: None,
                limit: None,
                balance_read_succeeded: None,
                credits_available: None,
            }),
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: Vec::new(),
            source_diagnostics: Vec::new(),
            provider_id: OPENAI.to_owned(),
            source: Some("test".to_owned()),
            data_confidence: "authoritative".to_owned(),
        }
    }

    #[test]
    fn reset_boundary_candidate_uses_grace_and_minimum_delay() {
        let at = now();
        let account_id = AccountId::new();
        let reset_at = at + ChronoDuration::minutes(4);
        let candidate = next_reset_boundary_refresh_candidate(
            &[snapshot(
                account_id,
                at - ChronoDuration::minutes(1),
                reset_at,
            )],
            Duration::from_secs(5 * 60),
            RESET_BOUNDARY_GRACE,
            RESET_BOUNDARY_MINIMUM_DELAY,
            None,
            &HashSet::new(),
            at,
        )
        .unwrap();
        assert_eq!(
            candidate.boundary_refresh_at,
            reset_at + ChronoDuration::seconds(30)
        );
        assert_eq!(candidate.refresh_at, reset_at + ChronoDuration::seconds(30));
    }

    #[derive(Default)]
    struct CountingAdapter {
        calls: AtomicUsize,
        delay: Duration,
    }

    #[async_trait]
    impl UsageAdapter for CountingAdapter {
        fn adapter_id(&self) -> &str {
            "openai-wham"
        }

        async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(UsageProbeResult::success(
                snapshot(
                    account.id,
                    Utc::now(),
                    Utc::now() + ChronoDuration::hours(1),
                ),
                Some(VerifiedIdentity {
                    email: Some("verified@example.com".to_owned()),
                    provider_account_id: Some("provider-account".to_owned()),
                    plan_type: Some("pro".to_owned()),
                }),
            ))
        }
    }

    #[tokio::test]
    async fn concurrent_calls_are_coalesced_to_one_probe() {
        let account =
            AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
        let accounts = Arc::new(InMemoryAccountStore::default());
        accounts.upsert(&account).await.unwrap();
        let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
        let adapter = Arc::new(CountingAdapter {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(30),
        });
        let coordinator = UsageRefreshCoordinator::new(
            accounts.clone(),
            store,
            vec![adapter.clone() as Arc<dyn UsageAdapter>],
            RefreshCoordinatorConfig::default(),
        );
        let first = coordinator.refresh_account(account.clone(), RefreshReason::Manual);
        let second = coordinator.refresh_account(account, RefreshReason::Manual);
        let (first, second) = tokio::join!(first, second);
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.status, RefreshStatus::Updated);
        assert_eq!(second.status, RefreshStatus::Updated);
        let persisted_account = accounts.get(first.account_id).await.unwrap().unwrap();
        assert_eq!(persisted_account.email, "verified@example.com");
        assert_eq!(
            persisted_account.provider_account_id.as_deref(),
            Some("provider-account")
        );
    }

    struct RateLimitedAdapter;

    #[async_trait]
    impl UsageAdapter for RateLimitedAdapter {
        fn adapter_id(&self) -> &str {
            OPENAI
        }

        async fn probe(
            &self,
            _account: &AccountRecord,
        ) -> Result<UsageProbeResult, TransportError> {
            Ok(UsageProbeResult::failure(UsageAdapterError {
                code: UsageAdapterErrorCode::RateLimited,
                message: "retry later".to_owned(),
                http_status_code: Some(429),
                retry_after_seconds: Some(10),
            }))
        }
    }

    struct FixedSnapshotAdapter(UsageSnapshot);

    #[async_trait]
    impl UsageAdapter for FixedSnapshotAdapter {
        fn adapter_id(&self) -> &str {
            OPENAI
        }

        async fn probe(
            &self,
            _account: &AccountRecord,
        ) -> Result<UsageProbeResult, TransportError> {
            Ok(UsageProbeResult::success(self.0.clone(), None))
        }
    }

    #[tokio::test]
    async fn older_provider_snapshot_cannot_replace_newer_persisted_usage() {
        let account =
            AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
        let accounts = Arc::new(InMemoryAccountStore::default());
        accounts.upsert(&account).await.unwrap();
        let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
        let mut prior = snapshot(account.id, now(), now() + ChronoDuration::hours(1));
        prior.primary.as_mut().unwrap().used_percent = 88.0;
        store.save(prior.clone()).await.unwrap();
        let adapter = FixedSnapshotAdapter(snapshot(
            account.id,
            now() - ChronoDuration::minutes(1),
            now() + ChronoDuration::hours(2),
        ));
        let coordinator = UsageRefreshCoordinator::new(
            accounts,
            store.clone(),
            vec![Arc::new(adapter) as Arc<dyn UsageAdapter>],
            RefreshCoordinatorConfig::default(),
        );

        let outcome = coordinator
            .refresh_account(account.clone(), RefreshReason::Scheduled)
            .await;

        assert_eq!(outcome.status, RefreshStatus::RetainedStale);
        assert_eq!(
            outcome.error.as_ref().map(|error| error.code),
            Some(UsageAdapterErrorCode::InvalidPayload)
        );
        let retained = outcome.snapshot.unwrap();
        assert!(retained.is_stale);
        assert_eq!(retained.observed_at_utc, prior.observed_at_utc);
        assert_eq!(retained.primary.unwrap().used_percent, 88.0);
        let persisted = store.get_latest(account.id).await.unwrap().unwrap();
        assert_eq!(persisted.observed_at_utc, prior.observed_at_utc);
        assert_eq!(persisted.primary.unwrap().used_percent, 88.0);
    }

    #[tokio::test]
    async fn transient_failure_retains_and_persists_the_last_snapshot() {
        let account =
            AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
        let accounts = Arc::new(InMemoryAccountStore::default());
        accounts.upsert(&account).await.unwrap();
        let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
        let prior = snapshot(
            account.id,
            now() - ChronoDuration::minutes(1),
            now() + ChronoDuration::hours(1),
        );
        store.save(prior.clone()).await.unwrap();
        let coordinator = UsageRefreshCoordinator::new(
            accounts,
            store.clone(),
            vec![Arc::new(RateLimitedAdapter) as Arc<dyn UsageAdapter>],
            RefreshCoordinatorConfig::default(),
        );

        let outcome = coordinator
            .refresh_account(account.clone(), RefreshReason::Scheduled)
            .await;
        assert_eq!(outcome.status, RefreshStatus::RetainedStale);
        assert!(
            outcome
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.is_stale)
        );
        assert_eq!(
            outcome.error.as_ref().map(|error| error.code),
            Some(UsageAdapterErrorCode::RateLimited)
        );

        let persisted = store.get_latest(account.id).await.unwrap().unwrap();
        assert!(persisted.is_stale);
        assert_eq!(persisted.observed_at_utc, prior.observed_at_utc);
    }
}
