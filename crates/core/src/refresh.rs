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
    providers::{
        codex_reset::{
            DelayedResetDecision, confirms_weekly_reset, create_delayed_reset_candidate,
            evaluate_delayed_reset_candidate, is_codex_account, needs_weekly_reset_confirmation,
        },
        registry::ProviderRegistry,
    },
    transport::TransportError,
    usage::{
        StorageError, UsageAdapter, UsageAdapterError, UsageAdapterErrorCode, UsageSnapshot,
        UsageSnapshotStore,
    },
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore, watch};

/// How often `RefreshCadence::Automatic` refreshes every account.
const AUTOMATIC_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
const RESET_BOUNDARY_GRACE: Duration = Duration::from_secs(30);
const RESET_BOUNDARY_MINIMUM_DELAY: Duration = Duration::from_secs(5);

/// Background refresh cadence. `Automatic` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshCadence {
    /// Only explicit refreshes.
    Manual,
    /// Every account on a fixed interval.
    Fixed(Duration),
    /// Every account every 30 minutes, plus each account again right after
    /// one of its known usage windows resets.
    Automatic,
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

/// Configuration for the coordinator and its optional background loop.
#[derive(Debug, Clone)]
pub struct RefreshCoordinatorConfig {
    pub cadence: RefreshCadence,
    pub max_concurrency: usize,
    pub reset_boundary_grace: Duration,
    pub reset_boundary_minimum_delay: Duration,
}

impl Default for RefreshCoordinatorConfig {
    fn default() -> Self {
        Self {
            cadence: RefreshCadence::Automatic,
            max_concurrency: 4,
            reset_boundary_grace: RESET_BOUNDARY_GRACE,
            reset_boundary_minimum_delay: RESET_BOUNDARY_MINIMUM_DELAY,
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

/// Shared state for one in-flight account refresh. Only the worker task owns
/// the sender, so waiters observe a closed channel (instead of hanging) if the
/// worker is dropped before publishing.
struct RefreshFlight {
    account_id: AccountId,
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
    shutdown: watch::Sender<bool>,
    /// Per-account high-water mark of reset boundaries already refreshed.
    attempted_reset_boundaries: Mutex<HashMap<AccountId, DateTime<Utc>>>,
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
            shutdown,
            attempted_reset_boundaries: Mutex::new(HashMap::new()),
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
        let (flight, sender) = {
            let mut flights = self.flights.lock().await;
            if let Some(flight) = flights.get(&account.id) {
                (Arc::clone(flight), None)
            } else {
                let (sender, receiver) = watch::channel(None);
                let flight = Arc::new(RefreshFlight {
                    account_id: account.id,
                    receiver,
                });
                flights.insert(account.id, Arc::clone(&flight));
                (flight, Some(sender))
            }
        };

        if let Some(sender) = sender {
            let coordinator = Arc::clone(self);
            let worker_flight = Arc::clone(&flight);
            tokio::spawn(async move {
                // Run the provider work in its own task so a panic inside an
                // adapter is contained: the flight is still published and
                // removed instead of wedging this account (and every batch
                // that waits for it) forever.
                let work = {
                    let coordinator = Arc::clone(&coordinator);
                    let account = account.clone();
                    tokio::spawn(
                        async move { coordinator.refresh_account_inner(account, reason).await },
                    )
                };
                let outcome = match work.await {
                    Ok(outcome) => outcome,
                    Err(error) => RefreshOutcome::new(
                        &account,
                        reason,
                        RefreshStatus::Failed,
                        None,
                        None,
                        Some(UsageAdapterError {
                            code: UsageAdapterErrorCode::Unknown,
                            message: if error.is_panic() {
                                "refresh worker panicked".to_owned()
                            } else {
                                "refresh worker was cancelled".to_owned()
                            },
                            http_status_code: None,
                            retry_after_seconds: None,
                        }),
                        None,
                    ),
                };
                let _ = sender.send(Some(outcome));
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
        self.refresh_where(reason, |_| true).await
    }

    /// Refreshes only the selected accounts, e.g. those whose reset boundary
    /// has passed, without touching every other provider account.
    async fn refresh_accounts(
        self: &Arc<Self>,
        account_ids: &HashSet<AccountId>,
        reason: RefreshReason,
    ) -> Result<Vec<RefreshOutcome>, RefreshCoordinatorError> {
        self.refresh_where(reason, |account| account_ids.contains(&account.id))
            .await
    }

    async fn refresh_where(
        self: &Arc<Self>,
        reason: RefreshReason,
        include: impl Fn(&AccountRecord) -> bool,
    ) -> Result<Vec<RefreshOutcome>, RefreshCoordinatorError> {
        let accounts = self.account_store.list().await?;
        let handles = accounts
            .into_iter()
            .filter(|account| include(account))
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

    /// Requests graceful shutdown of a running background loop.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Runs the backend timer loop: an immediate pass, then regular passes on
    /// the configured cadence, with one-shot reset-boundary passes pulled into
    /// the current interval.
    pub async fn run(self: Arc<Self>) -> Result<(), RefreshCoordinatorError> {
        let mut snapshots = self
            .full_refresh_pass(HashMap::new(), RefreshReason::Manual)
            .await;
        let mut shutdown = self.shutdown.subscribe();
        if *shutdown.borrow() {
            return Ok(());
        }

        let mut scheduled_at = self
            .normal_interval()
            .map(|interval| Utc::now() + chrono_from_std(interval));

        loop {
            let Some(interval) = self.normal_interval() else {
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
            let attempted_through = self.attempted_reset_boundaries.lock().await.clone();
            let candidate = earliest_reset_boundary_candidate(
                pending_reset_boundaries(
                    snapshots.values(),
                    self.config.reset_boundary_grace,
                    |snapshot, boundary| {
                        boundary_already_attempted(&attempted_through, snapshot, boundary)
                    },
                ),
                interval,
                self.config.reset_boundary_minimum_delay,
                now,
            );
            let target = candidate
                .map(|candidate| candidate.refresh_at.min(normal_deadline))
                .unwrap_or(normal_deadline);
            let sleep_for = (target - now).to_std().unwrap_or(Duration::ZERO);

            tokio::select! {
                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow() { return Ok(()); }
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
            // Every boundary that has already passed is covered by this one
            // pass. Recording them all prevents a stale snapshot with many
            // past reset times from triggering one pass per window.
            let reset_accounts = if reset_due {
                self.record_due_reset_boundaries(&snapshots, completed_at)
                    .await
            } else {
                HashSet::new()
            };
            if normal_due {
                let reason = if reset_due {
                    RefreshReason::ResetBoundary
                } else {
                    RefreshReason::Scheduled
                };
                snapshots = self.full_refresh_pass(snapshots, reason).await;
            } else if !reset_accounts.is_empty() {
                // Only the accounts whose window reset need a refresh.
                if let Ok(outcomes) = self
                    .refresh_accounts(&reset_accounts, RefreshReason::ResetBoundary)
                    .await
                {
                    snapshots = update_snapshot_cache(snapshots, outcomes);
                }
            }

            if normal_due {
                scheduled_at = match self.config.cadence {
                    RefreshCadence::Fixed(_) => Some(next_fixed_scheduled_at(
                        normal_deadline,
                        Utc::now(),
                        interval,
                    )),
                    RefreshCadence::Automatic => Some(Utc::now() + chrono_from_std(interval)),
                    RefreshCadence::Manual => None,
                };
            }
        }
    }

    /// Refreshes every account and rebuilds the loop's snapshot cache from the
    /// accounts that still exist. A transient account-store failure (for
    /// example a locked database) keeps the previous cache so the background
    /// loop survives and retries on its next tick.
    async fn full_refresh_pass(
        self: &Arc<Self>,
        mut snapshots: HashMap<AccountId, UsageSnapshot>,
        reason: RefreshReason,
    ) -> HashMap<AccountId, UsageSnapshot> {
        let Ok(outcomes) = self.refresh_all(reason).await else {
            return snapshots;
        };
        let present = outcomes
            .iter()
            .map(|outcome| outcome.account_id)
            .collect::<HashSet<_>>();
        snapshots.retain(|account_id, _| present.contains(account_id));
        self.attempted_reset_boundaries
            .lock()
            .await
            .retain(|account_id, _| present.contains(account_id));
        update_snapshot_cache(snapshots, outcomes)
    }

    fn normal_interval(&self) -> Option<Duration> {
        match self.config.cadence {
            RefreshCadence::Manual => None,
            RefreshCadence::Fixed(interval) => Some(interval),
            RefreshCadence::Automatic => Some(AUTOMATIC_REFRESH_INTERVAL),
        }
    }

    /// Records every reset boundary that has passed by `at` and returns the
    /// accounts that own them. Boundaries are tracked per account as a
    /// high-water mark, so an old boundary is never retried.
    async fn record_due_reset_boundaries(
        &self,
        snapshots: &HashMap<AccountId, UsageSnapshot>,
        at: DateTime<Utc>,
    ) -> HashSet<AccountId> {
        let mut attempted = self.attempted_reset_boundaries.lock().await;
        let due = pending_reset_boundaries(
            snapshots.values(),
            self.config.reset_boundary_grace,
            |snapshot, boundary| boundary_already_attempted(&attempted, snapshot, boundary),
        )
        .filter(|(_, boundary)| *boundary <= at)
        .map(|(snapshot, boundary)| (snapshot.account_id, boundary))
        .collect::<Vec<_>>();
        let mut accounts = HashSet::new();
        for (account_id, boundary) in due {
            let watermark = attempted.entry(account_id).or_insert(boundary);
            if boundary > *watermark {
                *watermark = boundary;
            }
            accounts.insert(account_id);
        }
        accounts
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

        if probe.succeeded()
            && let Some(mut snapshot) = probe.snapshot.clone()
        {
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
                            message:
                                "adapter returned a snapshot for a different account or provider"
                                    .to_owned(),
                            http_status_code: None,
                            retry_after_seconds: None,
                        },
                    )
                    .await;
            }
            let mut identity = probe.identity.clone();
            // A stored snapshot from the future (for example after the
            // system clock was corrected backwards) must not block every
            // later refresh until wall-clock time catches up with it.
            if prior.as_ref().is_some_and(|previous| {
                snapshot.observed_at_utc < previous.observed_at_utc
                    && previous.observed_at_utc <= Utc::now()
            }) {
                return self
                    .finish_failure(
                        &account,
                        reason,
                        prior,
                        UsageAdapterError {
                            code: UsageAdapterErrorCode::InvalidPayload,
                            message: "adapter returned a snapshot older than the stored snapshot"
                                .to_owned(),
                            http_status_code: None,
                            retry_after_seconds: None,
                        },
                    )
                    .await;
            }

            let mut delayed_reset_confirmed = false;
            if is_codex_account(&account) {
                let pending_candidate = match self
                    .snapshot_store
                    .get_codex_weekly_reset_candidate(account.id)
                    .await
                {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        return self
                                .finish_failure(
                                    &account,
                                    reason,
                                    prior,
                                    UsageAdapterError {
                                        code: UsageAdapterErrorCode::InvalidPayload,
                                        message: format!(
                                            "could not read pending Codex weekly reset evidence: {error}"
                                        ),
                                        http_status_code: None,
                                        retry_after_seconds: None,
                                    },
                                )
                                .await;
                    }
                };
                if let (Some(previous), Some(candidate)) = (prior.as_ref(), pending_candidate) {
                    match evaluate_delayed_reset_candidate(
                        &account,
                        previous,
                        &candidate,
                        &snapshot,
                        Utc::now(),
                    ) {
                        DelayedResetDecision::PublishCurrent => {
                            delayed_reset_confirmed = true;
                        }
                        DelayedResetDecision::RetainCandidate => {
                            return self
                                    .finish_failure(
                                        &account,
                                        reason,
                                        prior,
                                        UsageAdapterError {
                                            code: UsageAdapterErrorCode::InvalidPayload,
                                            message: "Codex weekly reset is awaiting an independent later OAuth observation".to_owned(),
                                            http_status_code: None,
                                            retry_after_seconds: None,
                                        },
                                    )
                                    .await;
                        }
                        DelayedResetDecision::DiscardCandidate(_) => {
                            if let Err(error) = self
                                .snapshot_store
                                .save_codex_weekly_reset_candidate(account.id, None)
                                .await
                            {
                                return self
                                        .finish_failure(
                                            &account,
                                            reason,
                                            prior,
                                            UsageAdapterError {
                                                code: UsageAdapterErrorCode::InvalidPayload,
                                                message: format!(
                                                    "could not discard invalid Codex weekly reset evidence: {error}"
                                                ),
                                                http_status_code: None,
                                                retry_after_seconds: None,
                                            },
                                        )
                                        .await;
                            }
                        }
                    }
                }
            }

            if !delayed_reset_confirmed
                && needs_weekly_reset_confirmation(&account, prior.as_ref(), &snapshot)
            {
                let confirmation = match adapter.probe(&account).await {
                    Ok(confirmation) if confirmation.succeeded() => confirmation,
                    Ok(confirmation) => {
                        let error = confirmation.error.unwrap_or(UsageAdapterError {
                            code: UsageAdapterErrorCode::InvalidPayload,
                            message: "Codex weekly reset confirmation returned no snapshot"
                                .to_owned(),
                            http_status_code: None,
                            retry_after_seconds: None,
                        });
                        return self.finish_failure(&account, reason, prior, error).await;
                    }
                    Err(error) => {
                        return self
                            .finish_failure(
                                &account,
                                reason,
                                prior,
                                transport_as_adapter_error(error),
                            )
                            .await;
                    }
                };
                let Some(confirmed_snapshot) = confirmation.snapshot else {
                    return self
                        .finish_failure(
                            &account,
                            reason,
                            prior,
                            UsageAdapterError {
                                code: UsageAdapterErrorCode::InvalidPayload,
                                message: "Codex weekly reset confirmation returned no snapshot"
                                    .to_owned(),
                                http_status_code: None,
                                retry_after_seconds: None,
                            },
                        )
                        .await;
                };
                let previous = prior
                    .as_ref()
                    .expect("confirmation requires a prior snapshot");
                let confirmation_rejection = if confirmed_snapshot.account_id != account.id
                    || !provider_ids_match(&account.provider_id, &confirmed_snapshot.provider_id)
                {
                    Some("account_or_provider_mismatch")
                } else if !confirms_weekly_reset(&account, previous, &snapshot, &confirmed_snapshot)
                {
                    Some("immediate_confirmation_rejected")
                } else {
                    None
                };
                if let Some(rejection) = confirmation_rejection {
                    let delayed_candidate = create_delayed_reset_candidate(
                        &account,
                        previous,
                        &snapshot,
                        &confirmed_snapshot,
                        Utc::now(),
                    );
                    match delayed_candidate {
                        Ok(candidate) => {
                            if let Err(error) = self
                                .snapshot_store
                                .save_codex_weekly_reset_candidate(account.id, Some(candidate))
                                .await
                            {
                                return self
                                        .finish_failure(
                                            &account,
                                            reason,
                                            prior,
                                            UsageAdapterError {
                                                code: UsageAdapterErrorCode::InvalidPayload,
                                                message: format!(
                                                    "could not persist pending Codex weekly reset evidence: {error}"
                                                ),
                                                http_status_code: None,
                                                retry_after_seconds: None,
                                            },
                                        )
                                        .await;
                            }
                            return self
                                    .finish_failure(
                                        &account,
                                        reason,
                                        prior,
                                        UsageAdapterError {
                                            code: UsageAdapterErrorCode::InvalidPayload,
                                            message: "Codex weekly reset is awaiting an independent later OAuth observation".to_owned(),
                                            http_status_code: None,
                                            retry_after_seconds: None,
                                        },
                                    )
                                    .await;
                        }
                        Err(delayed_rejection) => {
                            return self
                                    .finish_failure(
                                        &account,
                                        reason,
                                        prior,
                                        UsageAdapterError {
                                            code: UsageAdapterErrorCode::InvalidPayload,
                                            message: format!(
                                                "Codex weekly reset rejected ({rejection}); delayed confirmation unavailable ({delayed_rejection})"
                                            ),
                                            http_status_code: None,
                                            retry_after_seconds: None,
                                        },
                                    )
                                    .await;
                        }
                    }
                }
                snapshot = confirmed_snapshot;
                identity = confirmation.identity.or(identity);
            }
            // Apply the identity to the *current* stored record. The
            // `account` value was read before the provider call, so
            // writing it back would resurrect an account removed during
            // the refresh and undo concurrent alias/status changes.
            let current = match identity.as_ref() {
                Some(identity) => {
                    self.account_store
                        .apply_verified_identity(
                            account.id,
                            identity.email.as_deref(),
                            identity.provider_account_id.as_deref(),
                        )
                        .await
                }
                None => self.account_store.get(account.id).await,
            };
            let identity_storage_error = match current {
                Ok(Some(_)) => None,
                Ok(None) => {
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
                Err(error) => Some(error.to_string()),
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
            let candidate_clear_error = if is_codex_account(&account) {
                self.snapshot_store
                    .save_codex_weekly_reset_candidate(account.id, None)
                    .await
                    .err()
                    .map(|error| error.to_string())
            } else {
                None
            };
            return RefreshOutcome::new(
                &account,
                reason,
                RefreshStatus::Updated,
                Some(snapshot),
                identity,
                None,
                combine_optional_storage_errors(identity_storage_error, candidate_clear_error),
            );
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
        let stale_reason = format!("{}: {}", account.provider_id, error.message);
        if should_retain_stale(&error.code)
            && let Some(prior) = prior
        {
            // Mark the stored latest snapshot stale in place instead of
            // appending a duplicate history row for every failure.
            let (stale, storage_error) = match self
                .snapshot_store
                .mark_latest_stale(account.id, &stale_reason)
                .await
            {
                Ok(Some(stale)) => (stale, None),
                Ok(None) => (prior.mark_stale(&stale_reason), None),
                Err(storage_error) => (
                    prior.mark_stale(&stale_reason),
                    Some(storage_error.to_string()),
                ),
            };
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

        let status = if should_invalidate(&error.code) {
            RefreshStatus::Invalidated
        } else {
            RefreshStatus::Failed
        };
        // Hosts reload the stored snapshot after a refresh. Without a stale
        // marker, an account whose credentials were revoked would keep
        // showing its last values as current data.
        let storage_error = if prior.is_some() {
            self.snapshot_store
                .mark_latest_stale(account.id, &stale_reason)
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        RefreshOutcome::new(
            account,
            reason,
            status,
            None,
            None,
            Some(error),
            storage_error,
        )
    }
}

/// Candidate used by reset-boundary scheduling. `boundary_refresh_at` is the
/// deduplication key; `refresh_at` applies the minimum delay.
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
    attempted_boundary_refreshes: &HashSet<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<ResetBoundaryRefreshCandidate> {
    earliest_reset_boundary_candidate(
        pending_reset_boundaries(snapshots, reset_boundary_grace, |_, boundary| {
            attempted_boundary_refreshes.contains(&boundary)
        }),
        normal_refresh_interval,
        reset_boundary_minimum_delay,
        now,
    )
}

/// Yields `(snapshot, boundary_refresh_at)` for every window reset that the
/// snapshot predates and that has not been attempted yet.
fn pending_reset_boundaries<'a>(
    snapshots: impl IntoIterator<Item = &'a UsageSnapshot>,
    reset_boundary_grace: Duration,
    is_attempted: impl Fn(&UsageSnapshot, DateTime<Utc>) -> bool,
) -> impl Iterator<Item = (&'a UsageSnapshot, DateTime<Utc>)> {
    let grace = chrono_from_std(reset_boundary_grace);
    snapshots
        .into_iter()
        .flat_map(|snapshot| {
            snapshot
                .all_rate_windows()
                .map(move |window| (snapshot, window))
        })
        .filter_map(move |(snapshot, window)| {
            let boundary_refresh_at = window.reset_at_utc?.checked_add_signed(grace)?;
            (snapshot.observed_at_utc < boundary_refresh_at
                && !is_attempted(snapshot, boundary_refresh_at))
            .then_some((snapshot, boundary_refresh_at))
        })
}

fn earliest_reset_boundary_candidate<'a>(
    boundaries: impl Iterator<Item = (&'a UsageSnapshot, DateTime<Utc>)>,
    normal_refresh_interval: Duration,
    reset_boundary_minimum_delay: Duration,
    now: DateTime<Utc>,
) -> Option<ResetBoundaryRefreshCandidate> {
    let normal_deadline = now + chrono_from_std(normal_refresh_interval);
    let earliest_allowed = now + chrono_from_std(reset_boundary_minimum_delay);
    boundaries
        .filter_map(|(_, boundary_refresh_at)| {
            if boundary_refresh_at > normal_deadline {
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

fn boundary_already_attempted(
    attempted_through: &HashMap<AccountId, DateTime<Utc>>,
    snapshot: &UsageSnapshot,
    boundary_refresh_at: DateTime<Utc>,
) -> bool {
    attempted_through
        .get(&snapshot.account_id)
        .is_some_and(|watermark| boundary_refresh_at <= *watermark)
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

fn update_snapshot_cache(
    mut snapshots: HashMap<AccountId, UsageSnapshot>,
    outcomes: Vec<RefreshOutcome>,
) -> HashMap<AccountId, UsageSnapshot> {
    for outcome in outcomes {
        if let Some(snapshot) = outcome.snapshot {
            snapshots.insert(outcome.account_id, snapshot);
        } else if matches!(
            outcome.status,
            RefreshStatus::Invalidated | RefreshStatus::Skipped
        ) {
            // Paused, disabled, removed, or unauthenticated accounts must not
            // keep scheduling reset-boundary refreshes.
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

fn combine_optional_storage_errors(
    first: Option<String>,
    second: Option<String>,
) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
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
        workspace_id: None,
        workspace_name: None,
        alias: None,
        account_ref: None,
    }
}

#[cfg(test)]
mod tests;
