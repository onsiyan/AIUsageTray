use super::*;
use crate::{
    accounts::{ANTIGRAVITY, AccountStore, CLAUDE, InMemoryAccountStore, OPENAI, VerifiedIdentity},
    usage::{
        CodexWeeklyResetCandidate, CreditsSnapshot, RateLimitWindow, UsageAdapter,
        UsageCreditInventory, UsageCreditRecord, UsageProbeResult, UsageSnapshotStore,
        UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::TimeZone;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
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
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
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

    async fn probe(&self, _account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
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

    async fn probe(&self, _account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        Ok(UsageProbeResult::success(self.0.clone(), None))
    }
}

struct SequencedSnapshotAdapter {
    snapshots: std::sync::Mutex<VecDeque<UsageSnapshot>>,
    calls: AtomicUsize,
}

#[async_trait]
impl UsageAdapter for SequencedSnapshotAdapter {
    fn adapter_id(&self) -> &str {
        OPENAI
    }

    async fn probe(&self, _account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let snapshot = self
            .snapshots
            .lock()
            .unwrap()
            .pop_front()
            .expect("test adapter has a snapshot for each probe");
        Ok(UsageProbeResult::success(snapshot, None))
    }
}

struct ProviderSnapshotAdapter {
    provider_id: &'static str,
    snapshot: UsageSnapshot,
}

#[async_trait]
impl UsageAdapter for ProviderSnapshotAdapter {
    fn adapter_id(&self) -> &str {
        self.provider_id
    }

    async fn probe(&self, _account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        Ok(UsageProbeResult::success(self.snapshot.clone(), None))
    }
}

fn codex_weekly_snapshot(
    account_id: AccountId,
    observed_at: DateTime<Utc>,
    weekly_used: f64,
    weekly_reset: DateTime<Utc>,
) -> UsageSnapshot {
    let mut snapshot = snapshot(
        account_id,
        observed_at,
        observed_at + ChronoDuration::hours(4),
    );
    snapshot.plan_type = Some("plus".to_owned());
    snapshot.secondary = Some(RateLimitWindow {
        kind: UsageWindowKind::Secondary,
        name: "weekly".to_owned(),
        used_percent: weekly_used,
        reset_at_utc: Some(weekly_reset),
        limit_window_seconds: 7 * 24 * 60 * 60,
    });
    snapshot.observed_email = Some("codex@example.com".to_owned());
    snapshot.source = Some("codex-oauth".to_owned());
    snapshot
}

fn with_available_codex_reset_credit(mut snapshot: UsageSnapshot) -> UsageSnapshot {
    let expires_at_utc = snapshot
        .observed_at_utc
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        + ChronoDuration::days(90);
    snapshot.credit_inventory = Some(UsageCreditInventory {
        available_count: 1,
        credits: vec![UsageCreditRecord {
            id: Some("reset-credit".to_owned()),
            reset_type: Some("codex_rate_limits".to_owned()),
            status: Some("available".to_owned()),
            granted_at_utc: None,
            expires_at_utc: Some(expires_at_utc),
            redeem_started_at_utc: None,
            redeemed_at_utc: None,
            title: None,
            description: None,
        }],
    });
    snapshot
}

#[tokio::test]
async fn unconfirmed_weekly_reset_retains_prior_usage_for_the_same_account() {
    let account = AccountRecord::create("test", "codex@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    let prior = codex_weekly_snapshot(account.id, now(), 70.0, now() + ChronoDuration::days(3));
    store.save(prior.clone()).await.unwrap();
    let initial = codex_weekly_snapshot(
        account.id,
        now() + ChronoDuration::seconds(1),
        0.2,
        now() + ChronoDuration::days(10),
    );
    let confirmation = codex_weekly_snapshot(
        account.id,
        now() + ChronoDuration::seconds(2),
        0.8,
        now() + ChronoDuration::days(10),
    );
    let adapter = Arc::new(SequencedSnapshotAdapter {
        snapshots: std::sync::Mutex::new(VecDeque::from([initial, confirmation])),
        calls: AtomicUsize::new(0),
    });
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        store.clone(),
        vec![adapter.clone() as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let outcome = coordinator
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;

    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.status, RefreshStatus::RetainedStale);
    assert_eq!(
        outcome.error.as_ref().map(|error| error.code),
        Some(UsageAdapterErrorCode::InvalidPayload)
    );
    let retained = outcome.snapshot.unwrap();
    assert!(retained.is_stale);
    assert_eq!(retained.secondary.unwrap().used_percent, 70.0);
    let persisted = store.get_latest(account.id).await.unwrap().unwrap();
    assert_eq!(persisted.observed_at_utc, prior.observed_at_utc);
}

#[tokio::test]
async fn early_weekly_reset_is_retained_with_a_persisted_delayed_candidate() {
    let account = AccountRecord::create("test", "codex@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    let observed_at = Utc::now();
    let prior = with_available_codex_reset_credit(codex_weekly_snapshot(
        account.id,
        observed_at - ChronoDuration::minutes(1),
        70.0,
        observed_at + ChronoDuration::days(3),
    ));
    store.save(prior.clone()).await.unwrap();
    let initial = with_available_codex_reset_credit(codex_weekly_snapshot(
        account.id,
        observed_at - ChronoDuration::seconds(2),
        0.0,
        observed_at + ChronoDuration::days(10),
    ));
    let confirmation = with_available_codex_reset_credit(codex_weekly_snapshot(
        account.id,
        observed_at - ChronoDuration::seconds(1),
        0.0,
        observed_at + ChronoDuration::days(10),
    ));
    let adapter = Arc::new(SequencedSnapshotAdapter {
        snapshots: std::sync::Mutex::new(VecDeque::from([initial, confirmation])),
        calls: AtomicUsize::new(0),
    });
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        store.clone(),
        vec![adapter.clone() as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let outcome = coordinator
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;

    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.status, RefreshStatus::RetainedStale);
    assert!(
        outcome
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("awaiting an independent later OAuth observation")
    );
    assert!(
        store
            .get_codex_weekly_reset_candidate(account.id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .get_latest(account.id)
            .await
            .unwrap()
            .unwrap()
            .observed_at_utc,
        prior.observed_at_utc
    );
}

#[tokio::test]
async fn later_oauth_observation_publishes_and_clears_the_pending_reset_candidate() {
    let account = AccountRecord::create("test", "codex@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    let observed_at = Utc::now();
    let prior = with_available_codex_reset_credit(codex_weekly_snapshot(
        account.id,
        observed_at - ChronoDuration::minutes(1),
        70.0,
        observed_at + ChronoDuration::days(3),
    ));
    let candidate_snapshot = with_available_codex_reset_credit(codex_weekly_snapshot(
        account.id,
        observed_at - ChronoDuration::seconds(70),
        0.0,
        observed_at + ChronoDuration::days(10),
    ));
    let candidate = CodexWeeklyResetCandidate {
        evidence_version: 1,
        first_observed_at_utc: candidate_snapshot.observed_at_utc,
        created_at_utc: observed_at - ChronoDuration::seconds(61),
        snapshot: candidate_snapshot,
    };
    store.save(prior).await.unwrap();
    store
        .save_codex_weekly_reset_candidate(account.id, Some(candidate))
        .await
        .unwrap();

    let current = with_available_codex_reset_credit(codex_weekly_snapshot(
        account.id,
        observed_at,
        0.4,
        observed_at + ChronoDuration::days(10),
    ));
    let adapter = Arc::new(SequencedSnapshotAdapter {
        snapshots: std::sync::Mutex::new(VecDeque::from([current.clone()])),
        calls: AtomicUsize::new(0),
    });
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        store.clone(),
        vec![adapter.clone() as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let outcome = coordinator
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;

    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.status, RefreshStatus::Updated);
    assert_eq!(
        outcome.snapshot.unwrap().secondary.unwrap().used_percent,
        0.4
    );
    assert!(
        store
            .get_codex_weekly_reset_candidate(account.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_latest(account.id)
            .await
            .unwrap()
            .unwrap()
            .observed_at_utc,
        current.observed_at_utc
    );
}

#[tokio::test]
async fn confirmed_weekly_reset_publishes_the_second_oauth_observation() {
    let account = AccountRecord::create("test", "codex@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    let prior = codex_weekly_snapshot(account.id, now(), 70.0, now() + ChronoDuration::minutes(1));
    store.save(prior).await.unwrap();
    let initial = codex_weekly_snapshot(
        account.id,
        now() + ChronoDuration::minutes(2),
        0.2,
        now() + ChronoDuration::days(7) + ChronoDuration::minutes(1),
    );
    let confirmation = codex_weekly_snapshot(
        account.id,
        now() + ChronoDuration::minutes(3),
        0.4,
        now() + ChronoDuration::days(7) + ChronoDuration::minutes(1),
    );
    let adapter = Arc::new(SequencedSnapshotAdapter {
        snapshots: std::sync::Mutex::new(VecDeque::from([initial, confirmation.clone()])),
        calls: AtomicUsize::new(0),
    });
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        store.clone(),
        vec![adapter.clone() as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let outcome = coordinator
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;

    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.status, RefreshStatus::Updated);
    let published = outcome.snapshot.unwrap();
    assert_eq!(published.observed_at_utc, confirmation.observed_at_utc);
    assert_eq!(published.secondary.unwrap().used_percent, 0.4);
    let persisted = store.get_latest(account.id).await.unwrap().unwrap();
    assert_eq!(persisted.observed_at_utc, confirmation.observed_at_utc);
}

#[tokio::test]
async fn authoritative_claude_and_antigravity_resets_publish_without_codex_guard() {
    for provider_id in [CLAUDE, ANTIGRAVITY] {
        let email = format!("{provider_id}@example.com");
        let account = AccountRecord::create("test", &email, None, provider_id, None).unwrap();
        let accounts = Arc::new(InMemoryAccountStore::default());
        accounts.upsert(&account).await.unwrap();
        let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
        let observed_at = now();

        let mut previous = snapshot(
            account.id,
            observed_at,
            observed_at + ChronoDuration::hours(4),
        );
        previous.provider_id = provider_id.to_owned();
        previous.source = Some(
            if provider_id == CLAUDE {
                "oauth"
            } else {
                "local"
            }
            .to_owned(),
        );
        previous.observed_email = Some(email.clone());
        previous.primary.as_mut().unwrap().used_percent = 75.0;
        previous.secondary = Some(RateLimitWindow {
            kind: UsageWindowKind::Secondary,
            name: "weekly".to_owned(),
            used_percent: 80.0,
            reset_at_utc: Some(observed_at + ChronoDuration::days(3)),
            limit_window_seconds: 7 * 24 * 60 * 60,
        });
        store.save(previous).await.unwrap();

        let mut reset = snapshot(
            account.id,
            observed_at + ChronoDuration::seconds(1),
            observed_at + ChronoDuration::hours(5),
        );
        reset.provider_id = provider_id.to_owned();
        reset.source = Some(
            if provider_id == CLAUDE {
                "oauth"
            } else {
                "local"
            }
            .to_owned(),
        );
        reset.observed_email = Some(email);
        reset.primary.as_mut().unwrap().used_percent = 0.0;
        reset.secondary = Some(RateLimitWindow {
            kind: UsageWindowKind::Secondary,
            name: "weekly".to_owned(),
            used_percent: 0.0,
            reset_at_utc: Some(observed_at + ChronoDuration::days(7)),
            limit_window_seconds: 7 * 24 * 60 * 60,
        });
        let adapter = Arc::new(ProviderSnapshotAdapter {
            provider_id,
            snapshot: reset,
        });
        let coordinator = UsageRefreshCoordinator::new(
            accounts,
            store.clone(),
            vec![adapter as Arc<dyn UsageAdapter>],
            RefreshCoordinatorConfig::default(),
        );

        let outcome = coordinator
            .refresh_account(account.clone(), RefreshReason::Manual)
            .await;

        assert_eq!(outcome.status, RefreshStatus::Updated, "{provider_id}");
        let published = outcome.snapshot.unwrap();
        assert_eq!(
            published.primary.unwrap().used_percent,
            0.0,
            "{provider_id}"
        );
        assert_eq!(
            published.secondary.unwrap().used_percent,
            0.0,
            "{provider_id}"
        );
        assert!(!published.is_stale, "{provider_id}");
    }
}

#[tokio::test]
async fn older_provider_snapshot_cannot_replace_newer_persisted_usage() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
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
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
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
struct PanicOnceAdapter {
    calls: AtomicUsize,
}

#[async_trait]
impl UsageAdapter for PanicOnceAdapter {
    fn adapter_id(&self) -> &str {
        OPENAI
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("simulated adapter panic");
        }
        Ok(UsageProbeResult::success(
            snapshot(
                account.id,
                Utc::now(),
                Utc::now() + ChronoDuration::hours(1),
            ),
            None,
        ))
    }
}

#[tokio::test]
async fn adapter_panic_fails_the_refresh_without_wedging_the_account() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        Arc::new(crate::usage::InMemoryUsageSnapshotStore::default()),
        vec![Arc::new(PanicOnceAdapter {
            calls: AtomicUsize::new(0),
        }) as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let first = tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.refresh_account(account.clone(), RefreshReason::Manual),
    )
    .await
    .expect("a panicking adapter must not hang the refresh");
    assert_eq!(first.status, RefreshStatus::Failed);

    let second = tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.refresh_account(account, RefreshReason::Manual),
    )
    .await
    .expect("the account must be refreshable after a panic");
    assert_eq!(second.status, RefreshStatus::Updated);
}

#[tokio::test]
async fn account_removed_during_refresh_is_not_recreated() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    let coordinator = UsageRefreshCoordinator::new(
        accounts.clone(),
        store.clone(),
        vec![Arc::new(CountingAdapter {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(200),
        }) as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let refresh = {
        let coordinator = Arc::clone(&coordinator);
        let account = account.clone();
        tokio::spawn(async move {
            coordinator
                .refresh_account(account, RefreshReason::Manual)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    accounts.remove(account.id).await.unwrap();
    let outcome = refresh.await.unwrap();

    assert_eq!(outcome.status, RefreshStatus::Skipped);
    assert!(accounts.get(account.id).await.unwrap().is_none());
    assert!(store.get_latest(account.id).await.unwrap().is_none());
}

#[tokio::test]
async fn verified_identity_keeps_concurrent_alias_and_pause() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let coordinator = UsageRefreshCoordinator::new(
        accounts.clone(),
        Arc::new(crate::usage::InMemoryUsageSnapshotStore::default()),
        vec![Arc::new(CountingAdapter {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(200),
        }) as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let refresh = {
        let coordinator = Arc::clone(&coordinator);
        let account = account.clone();
        tokio::spawn(async move {
            coordinator
                .refresh_account(account, RefreshReason::Manual)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    accounts
        .set_alias(account.id, Some("Renamed"))
        .await
        .unwrap();
    let mut paused = accounts.get(account.id).await.unwrap().unwrap();
    paused.status = AccountStatus::Paused;
    accounts.upsert(&paused).await.unwrap();
    assert_eq!(refresh.await.unwrap().status, RefreshStatus::Updated);

    let stored = accounts.get(account.id).await.unwrap().unwrap();
    assert_eq!(stored.alias.as_deref(), Some("Renamed"));
    assert_eq!(stored.status, AccountStatus::Paused);
    assert_eq!(stored.email, "verified@example.com");
}

struct UnauthorizedAdapter;

#[async_trait]
impl UsageAdapter for UnauthorizedAdapter {
    fn adapter_id(&self) -> &str {
        OPENAI
    }

    async fn probe(&self, _account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        Ok(UsageProbeResult::failure(UsageAdapterError {
            code: UsageAdapterErrorCode::Unauthorized,
            message: "token revoked".to_owned(),
            http_status_code: Some(401),
            retry_after_seconds: None,
        }))
    }
}

#[tokio::test]
async fn invalidated_refresh_marks_the_stored_snapshot_stale() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    store
        .save(snapshot(
            account.id,
            Utc::now() - ChronoDuration::hours(3),
            Utc::now() + ChronoDuration::hours(1),
        ))
        .await
        .unwrap();
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        store.clone(),
        vec![Arc::new(UnauthorizedAdapter) as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let outcome = coordinator
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;
    assert_eq!(outcome.status, RefreshStatus::Invalidated);
    let stored = store.get_latest(account.id).await.unwrap().unwrap();
    assert!(stored.is_stale);
    assert!(
        stored
            .stale_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("token revoked"))
    );
}

#[tokio::test]
async fn future_dated_stored_snapshot_does_not_block_new_usage() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    accounts.upsert(&account).await.unwrap();
    let store = Arc::new(crate::usage::InMemoryUsageSnapshotStore::default());
    store
        .save(snapshot(
            account.id,
            Utc::now() + ChronoDuration::days(2),
            Utc::now() + ChronoDuration::days(3),
        ))
        .await
        .unwrap();
    let current = snapshot(
        account.id,
        Utc::now(),
        Utc::now() + ChronoDuration::hours(1),
    );
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        store.clone(),
        vec![Arc::new(FixedSnapshotAdapter(current.clone())) as Arc<dyn UsageAdapter>],
        RefreshCoordinatorConfig::default(),
    );

    let outcome = coordinator
        .refresh_account(account, RefreshReason::Manual)
        .await;
    assert_eq!(outcome.status, RefreshStatus::Updated);
}

#[test]
fn stale_marker_keeps_the_first_stale_time() {
    let first = snapshot(AccountId::new(), now(), now()).mark_stale("first");
    let second = first.mark_stale("second");
    assert_eq!(second.stale_at_utc, first.stale_at_utc);
    assert_eq!(second.stale_reason.as_deref(), Some("second"));
}

#[tokio::test]
async fn one_reset_pass_covers_every_past_boundary_of_a_stale_snapshot() {
    let account = AccountRecord::create("test", "test@example.com", None, OPENAI, None).unwrap();
    let accounts = Arc::new(InMemoryAccountStore::default());
    let coordinator = UsageRefreshCoordinator::new(
        accounts,
        Arc::new(crate::usage::InMemoryUsageSnapshotStore::default()),
        Vec::<Arc<dyn UsageAdapter>>::new(),
        RefreshCoordinatorConfig::default(),
    );
    let at = Utc::now();
    let mut stale = snapshot(
        account.id,
        at - ChronoDuration::hours(10),
        at - ChronoDuration::hours(1),
    );
    stale.additional_windows = (2..7)
        .map(|hours| crate::usage::AdditionalRateLimitWindow {
            key: format!("model-{hours}"),
            name: format!("model-{hours}"),
            window: RateLimitWindow {
                kind: UsageWindowKind::Additional,
                name: format!("model-{hours}"),
                used_percent: 10.0,
                reset_at_utc: Some(at - ChronoDuration::hours(hours)),
                limit_window_seconds: 18_000,
            },
        })
        .collect();
    let snapshots = HashMap::from([(account.id, stale)]);

    let due = coordinator
        .record_due_reset_boundaries(&snapshots, at)
        .await;
    assert_eq!(due, HashSet::from([account.id]));

    let attempted = coordinator.attempted_reset_boundaries.lock().await.clone();
    let next = earliest_reset_boundary_candidate(
        pending_reset_boundaries(
            snapshots.values(),
            RESET_BOUNDARY_GRACE,
            |snapshot, boundary| boundary_already_attempted(&attempted, snapshot, boundary),
        ),
        Duration::from_secs(30 * 60),
        RESET_BOUNDARY_MINIMUM_DELAY,
        at,
    );
    assert!(
        next.is_none(),
        "past boundaries must not schedule more passes"
    );
}

#[test]
fn huge_reset_delay_does_not_panic() {
    let value = serde_json::json!({ "resetInSec": 1e30 });
    assert!(crate::providers::shared::reset_at(&value, now()).is_none());
    let value = serde_json::json!({ "resetInSec": "NaN" });
    assert!(crate::providers::shared::reset_at(&value, now()).is_none());
}
