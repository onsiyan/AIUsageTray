//! Guards Codex weekly usage publication against an unconfirmed reset.

use crate::{
    accounts::{AccountRecord, OPENAI},
    usage::{CodexWeeklyResetCandidate, RateLimitWindow, UsageSnapshot, UsageWindowKind},
};
use chrono::{DateTime, Duration, Utc};

const WEEKLY_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;
const RESET_BOUNDARY_TOLERANCE_SECONDS: i64 = 2 * 60;
const STABLE_RESET_BOUNDARY_TOLERANCE_SECONDS: i64 = 1;
const RESET_THRESHOLD_PERCENT: f64 = 1.0;
const DELAYED_CANDIDATE_MINIMUM_AGE: Duration = Duration::seconds(60);
const DELAYED_CANDIDATE_MAXIMUM_AGE: Duration = Duration::minutes(30);
const CODEX_RESET_EVIDENCE_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelayedResetDecision {
    PublishCurrent,
    RetainCandidate,
    DiscardCandidate(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AvailableResetCredit {
    id: String,
    reset_type: String,
    expires_at_utc: Option<DateTime<Utc>>,
}

/// A sudden weekly drop is ambiguous until a second request using the same
/// account-scoped Codex credential confirms it.
pub(crate) fn is_codex_account(account: &AccountRecord) -> bool {
    account.provider_id.eq_ignore_ascii_case(OPENAI)
        || account.provider_id.eq_ignore_ascii_case("codex")
}

pub(crate) fn needs_weekly_reset_confirmation(
    account: &AccountRecord,
    previous: Option<&UsageSnapshot>,
    candidate: &UsageSnapshot,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if !is_codex_account(account)
        || !is_trusted_codex_snapshot(account, previous)
        || !is_trusted_codex_snapshot(account, candidate)
    {
        return false;
    }
    let (Some(previous_weekly), Some(candidate_weekly)) =
        (weekly_window(previous), weekly_window(candidate))
    else {
        return false;
    };
    previous_weekly.used_percent.is_finite()
        && candidate_weekly.used_percent.is_finite()
        && previous_weekly.used_percent > RESET_THRESHOLD_PERCENT
        && candidate_weekly.used_percent <= RESET_THRESHOLD_PERCENT
}

/// Determines whether a second observation makes a suspicious weekly reset
/// publishable. A low reading requires matching reset boundaries and either a
/// naturally reached reset boundary or corroborated reset-credit evidence.
pub(crate) fn confirms_weekly_reset(
    account: &AccountRecord,
    previous: &UsageSnapshot,
    initial: &UsageSnapshot,
    confirmation: &UsageSnapshot,
) -> bool {
    if !is_trusted_codex_snapshot(account, previous)
        || !is_trusted_codex_snapshot(account, initial)
        || !is_trusted_codex_snapshot(account, confirmation)
        || !compatible_identity_and_plan(previous, initial, confirmation)
        || initial.observed_at_utc <= previous.observed_at_utc
        || confirmation.observed_at_utc <= initial.observed_at_utc
    {
        return false;
    }

    let (Some(previous_weekly), Some(initial_weekly), Some(confirmation_weekly)) = (
        weekly_window(previous),
        weekly_window(initial),
        weekly_window(confirmation),
    ) else {
        return false;
    };
    if !previous_weekly.used_percent.is_finite()
        || !initial_weekly.used_percent.is_finite()
        || !confirmation_weekly.used_percent.is_finite()
        || initial_weekly.used_percent > RESET_THRESHOLD_PERCENT
        || regressed_reset_boundary(previous_weekly, confirmation_weekly)
    {
        return false;
    }
    let (Some(initial_boundary), Some(confirmation_boundary)) = (
        initial_weekly.reset_at_utc,
        confirmation_weekly.reset_at_utc,
    ) else {
        return false;
    };
    if initial_boundary <= initial.observed_at_utc
        || confirmation_boundary <= confirmation.observed_at_utc
    {
        return false;
    }

    // A rebound in the independently fetched sample shows that the first low
    // value was transient. This mirrors CodexBar's immediate confirmation path.
    if confirmation_weekly.used_percent > RESET_THRESHOLD_PERCENT {
        return true;
    }

    let Some(previous_boundary) = previous_weekly.reset_at_utc else {
        return false;
    };
    if (initial_boundary - confirmation_boundary)
        .num_seconds()
        .abs()
        >= RESET_BOUNDARY_TOLERANCE_SECONDS
    {
        return false;
    }

    let reset_credit_evidence = reset_credit_evidence(previous, initial, confirmation);
    if confirmation.observed_at_utc
        < previous_boundary - Duration::seconds(RESET_BOUNDARY_TOLERANCE_SECONDS)
        && !reset_credit_evidence
    {
        return false;
    }
    let boundary_advanced = initial_boundary
        >= previous_boundary + Duration::seconds(RESET_BOUNDARY_TOLERANCE_SECONDS)
        && confirmation_boundary
            >= previous_boundary + Duration::seconds(RESET_BOUNDARY_TOLERANCE_SECONDS);
    boundary_advanced || reset_credit_evidence
}

/// Builds a delayed candidate when two fresh OAuth reads agree on an early
/// weekly reset but the previous reset boundary has not arrived yet.
pub(crate) fn create_delayed_reset_candidate(
    account: &AccountRecord,
    previous: &UsageSnapshot,
    initial: &UsageSnapshot,
    confirmation: &UsageSnapshot,
    created_at_utc: DateTime<Utc>,
) -> Result<CodexWeeklyResetCandidate, &'static str> {
    if !is_exact_codex_oauth_snapshot(account, previous)
        || !is_exact_codex_oauth_snapshot(account, initial)
        || !is_exact_codex_oauth_snapshot(account, confirmation)
    {
        return Err("source_not_exact_codex_oauth");
    }
    if !compatible_identity_and_plan(previous, initial, confirmation) {
        return Err("account_or_plan_mismatch");
    }
    if initial.observed_at_utc <= previous.observed_at_utc
        || confirmation.observed_at_utc <= initial.observed_at_utc
        || created_at_utc < confirmation.observed_at_utc
    {
        return Err("non_monotonic_observation_time");
    }

    let (previous_weekly, initial_weekly, confirmation_weekly) =
        require_weekly_windows(previous, initial, confirmation)?;
    if !previous_weekly.used_percent.is_finite()
        || !initial_weekly.used_percent.is_finite()
        || !confirmation_weekly.used_percent.is_finite()
    {
        return Err("invalid_weekly_usage");
    }
    if previous_weekly.used_percent <= RESET_THRESHOLD_PERCENT
        || initial_weekly.used_percent > RESET_THRESHOLD_PERCENT
        || confirmation_weekly.used_percent > RESET_THRESHOLD_PERCENT
    {
        return Err("weekly_usage_does_not_match_reset");
    }

    let (Some(previous_boundary), Some(initial_boundary), Some(confirmation_boundary)) = (
        valid_weekly_boundary(previous, previous_weekly),
        valid_weekly_boundary(initial, initial_weekly),
        valid_weekly_boundary(confirmation, confirmation_weekly),
    ) else {
        return Err("invalid_weekly_reset_boundary");
    };
    if regressed_reset_boundary(previous_weekly, confirmation_weekly) {
        return Err("weekly_reset_boundary_moved_backwards");
    }
    if (initial_boundary - confirmation_boundary)
        .num_seconds()
        .abs()
        >= RESET_BOUNDARY_TOLERANCE_SECONDS
    {
        return Err("immediate_reset_boundaries_disagree");
    }
    if !supports_delayed_reset_boundary(previous_boundary, initial_boundary)
        || !supports_delayed_reset_boundary(previous_boundary, confirmation_boundary)
    {
        return Err("weekly_reset_boundary_not_supported");
    }
    if !unchanged_positive_reset_credit_inventory(previous, initial, confirmation) {
        return Err("reset_credit_inventory_missing_or_changed");
    }

    Ok(CodexWeeklyResetCandidate {
        evidence_version: CODEX_RESET_EVIDENCE_VERSION,
        first_observed_at_utc: initial.observed_at_utc,
        created_at_utc,
        snapshot: confirmation.clone(),
    })
}

/// Revalidates a persisted early-reset candidate against a later provider
/// observation. Candidate age prevents two immediate retries from counting as
/// independent confirmation.
pub(crate) fn evaluate_delayed_reset_candidate(
    account: &AccountRecord,
    previous: &UsageSnapshot,
    candidate: &CodexWeeklyResetCandidate,
    current: &UsageSnapshot,
    now: DateTime<Utc>,
) -> DelayedResetDecision {
    if candidate.evidence_version != CODEX_RESET_EVIDENCE_VERSION {
        return DelayedResetDecision::DiscardCandidate("candidate_version_mismatch");
    }
    if candidate.created_at_utc > now
        || now - candidate.created_at_utc > DELAYED_CANDIDATE_MAXIMUM_AGE
        || candidate.first_observed_at_utc > candidate.snapshot.observed_at_utc
        || candidate.created_at_utc < candidate.snapshot.observed_at_utc
    {
        return DelayedResetDecision::DiscardCandidate("candidate_time_invalid_or_expired");
    }
    if !is_exact_codex_oauth_snapshot(account, previous)
        || !is_exact_codex_oauth_snapshot(account, &candidate.snapshot)
        || !is_exact_codex_oauth_snapshot(account, current)
    {
        return DelayedResetDecision::DiscardCandidate("source_not_exact_codex_oauth");
    }
    if !compatible_identity_and_plan(previous, &candidate.snapshot, current) {
        return DelayedResetDecision::DiscardCandidate("account_or_plan_mismatch");
    }
    if current.observed_at_utc <= candidate.snapshot.observed_at_utc {
        return DelayedResetDecision::DiscardCandidate("non_monotonic_observation_time");
    }

    let (previous_weekly, candidate_weekly, current_weekly) =
        match require_weekly_windows(previous, &candidate.snapshot, current) {
            Ok(windows) => windows,
            Err(reason) => return DelayedResetDecision::DiscardCandidate(reason),
        };
    if !previous_weekly.used_percent.is_finite()
        || !candidate_weekly.used_percent.is_finite()
        || !current_weekly.used_percent.is_finite()
    {
        return DelayedResetDecision::DiscardCandidate("invalid_weekly_usage");
    }
    if previous_weekly.used_percent <= RESET_THRESHOLD_PERCENT
        || candidate_weekly.used_percent > RESET_THRESHOLD_PERCENT
        || current_weekly.used_percent > RESET_THRESHOLD_PERCENT
    {
        return DelayedResetDecision::DiscardCandidate("weekly_usage_does_not_match_reset");
    }

    let (Some(previous_boundary), Some(candidate_boundary), Some(current_boundary)) = (
        valid_weekly_boundary(previous, previous_weekly),
        valid_weekly_boundary(&candidate.snapshot, candidate_weekly),
        valid_weekly_boundary(current, current_weekly),
    ) else {
        return DelayedResetDecision::DiscardCandidate("invalid_weekly_reset_boundary");
    };
    if regressed_reset_boundary(previous_weekly, current_weekly) {
        return DelayedResetDecision::DiscardCandidate("weekly_reset_boundary_moved_backwards");
    }

    let boundaries_match = (candidate_boundary - current_boundary).num_seconds().abs()
        < RESET_BOUNDARY_TOLERANCE_SECONDS;
    let unused_weekly_windows_roll_forward =
        is_unused_weekly_window_rolling_forward(candidate_weekly, &candidate.snapshot)
            && is_unused_weekly_window_rolling_forward(current_weekly, current)
            && current_boundary >= candidate_boundary;
    if !boundaries_match && !unused_weekly_windows_roll_forward {
        return DelayedResetDecision::DiscardCandidate("delayed_reset_boundaries_disagree");
    }
    if !supports_delayed_reset_boundary(previous_boundary, candidate_boundary)
        || !supports_delayed_reset_boundary(previous_boundary, current_boundary)
    {
        return DelayedResetDecision::DiscardCandidate("weekly_reset_boundary_not_supported");
    }
    if !unchanged_positive_reset_credit_inventory(previous, &candidate.snapshot, current) {
        return DelayedResetDecision::DiscardCandidate("reset_credit_inventory_missing_or_changed");
    }

    if now - candidate.created_at_utc < DELAYED_CANDIDATE_MINIMUM_AGE {
        DelayedResetDecision::RetainCandidate
    } else {
        DelayedResetDecision::PublishCurrent
    }
}

fn require_weekly_windows<'a>(
    previous: &'a UsageSnapshot,
    initial: &'a UsageSnapshot,
    confirmation: &'a UsageSnapshot,
) -> Result<
    (
        &'a RateLimitWindow,
        &'a RateLimitWindow,
        &'a RateLimitWindow,
    ),
    &'static str,
> {
    let (Some(previous_weekly), Some(initial_weekly), Some(confirmation_weekly)) = (
        weekly_window(previous),
        weekly_window(initial),
        weekly_window(confirmation),
    ) else {
        return Err("weekly_window_missing");
    };
    if [previous_weekly, initial_weekly, confirmation_weekly]
        .iter()
        .any(|window| window.limit_window_seconds != WEEKLY_WINDOW_SECONDS)
    {
        return Err("weekly_window_duration_mismatch");
    }
    Ok((previous_weekly, initial_weekly, confirmation_weekly))
}

fn is_exact_codex_oauth_snapshot(account: &AccountRecord, snapshot: &UsageSnapshot) -> bool {
    is_trusted_codex_snapshot(account, snapshot)
        && snapshot
            .source
            .as_deref()
            .is_some_and(|source| source.eq_ignore_ascii_case("codex-oauth"))
        && snapshot
            .data_confidence
            .eq_ignore_ascii_case("authoritative")
}

fn valid_weekly_boundary(
    snapshot: &UsageSnapshot,
    window: &RateLimitWindow,
) -> Option<DateTime<Utc>> {
    if window.limit_window_seconds != WEEKLY_WINDOW_SECONDS {
        return None;
    }
    window
        .reset_at_utc
        .filter(|boundary| *boundary > snapshot.observed_at_utc)
}

fn supports_delayed_reset_boundary(previous: DateTime<Utc>, current: DateTime<Utc>) -> bool {
    let delta = current - previous;
    (delta > -Duration::seconds(STABLE_RESET_BOUNDARY_TOLERANCE_SECONDS)
        && delta < Duration::seconds(STABLE_RESET_BOUNDARY_TOLERANCE_SECONDS))
        || current >= previous + Duration::seconds(RESET_BOUNDARY_TOLERANCE_SECONDS)
}

fn is_unused_weekly_window_rolling_forward(
    window: &RateLimitWindow,
    snapshot: &UsageSnapshot,
) -> bool {
    window.used_percent == 0.0
        && window.limit_window_seconds == WEEKLY_WINDOW_SECONDS
        && window.reset_at_utc.is_some_and(|boundary| {
            (boundary - snapshot.observed_at_utc - Duration::seconds(WEEKLY_WINDOW_SECONDS))
                .num_seconds()
                .abs()
                < RESET_BOUNDARY_TOLERANCE_SECONDS
        })
}

fn unchanged_positive_reset_credit_inventory(
    previous: &UsageSnapshot,
    initial: &UsageSnapshot,
    confirmation: &UsageSnapshot,
) -> bool {
    let Some(previous) = available_reset_credits(previous) else {
        return false;
    };
    let Some(initial) = available_reset_credits(initial) else {
        return false;
    };
    let Some(confirmation) = available_reset_credits(confirmation) else {
        return false;
    };
    !previous.is_empty() && previous == initial && initial == confirmation
}

fn available_reset_credits(snapshot: &UsageSnapshot) -> Option<Vec<AvailableResetCredit>> {
    let inventory = snapshot.credit_inventory.as_ref()?;
    if inventory.available_count == 0 {
        return None;
    }
    let credits = inventory
        .credits
        .iter()
        .filter(|credit| {
            credit
                .status
                .as_deref()
                .is_some_and(|status| status.eq_ignore_ascii_case("available"))
                && credit
                    .expires_at_utc
                    .is_none_or(|expires_at| expires_at > snapshot.observed_at_utc)
        })
        .map(|credit| {
            Some(AvailableResetCredit {
                id: credit.id.as_deref()?.trim().to_owned(),
                reset_type: credit.reset_type.as_deref()?.trim().to_owned(),
                expires_at_utc: credit.expires_at_utc,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if credits.is_empty()
        || credits.len() != inventory.available_count as usize
        || credits
            .iter()
            .any(|credit| credit.id.is_empty() || credit.reset_type.is_empty())
    {
        return None;
    }
    let mut credits = credits;
    credits.sort();
    Some(credits)
}

fn is_trusted_codex_snapshot(account: &AccountRecord, snapshot: &UsageSnapshot) -> bool {
    is_codex_account(account)
        && snapshot.account_id == account.id
        && (snapshot.provider_id.eq_ignore_ascii_case(OPENAI)
            || snapshot.provider_id.eq_ignore_ascii_case("codex"))
        && snapshot.source.as_deref().is_some_and(|source| {
            source.eq_ignore_ascii_case("codex-oauth")
                    // Accept snapshots written by the previous Rust build so
                    // the reset guard survives this auth-source migration.
                    || source.eq_ignore_ascii_case("browser-session")
        })
        && snapshot
            .observed_email
            .as_deref()
            .is_some_and(|email| email.trim().eq_ignore_ascii_case(account.email.trim()))
}

fn weekly_window(snapshot: &UsageSnapshot) -> Option<&RateLimitWindow> {
    let is_weekly = |window: &&RateLimitWindow| {
        window.kind != UsageWindowKind::Additional
            && window.limit_window_seconds == WEEKLY_WINDOW_SECONDS
    };
    snapshot
        .primary
        .iter()
        .chain(snapshot.secondary.iter())
        .find(is_weekly)
}

fn compatible_identity_and_plan(
    previous: &UsageSnapshot,
    initial: &UsageSnapshot,
    confirmation: &UsageSnapshot,
) -> bool {
    let snapshots = [previous, initial, confirmation];
    let response_accounts = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.response_account_id.as_deref());
    let mut response_accounts = response_accounts.map(str::trim);
    let first_response_account = response_accounts.next();
    if response_accounts
        .any(|value| first_response_account.is_some_and(|first| !value.eq_ignore_ascii_case(first)))
    {
        return false;
    }

    let plans = snapshots
        .iter()
        .map(|snapshot| {
            snapshot
                .plan_type
                .as_deref()
                .map(|plan| plan.trim().to_ascii_lowercase())
        })
        .collect::<Vec<_>>();
    let Some(first_plan) = plans.first().and_then(Option::as_ref) else {
        return false;
    };
    plans.iter().all(|plan| plan.as_ref() == Some(first_plan))
}

fn regressed_reset_boundary(previous: &RateLimitWindow, current: &RateLimitWindow) -> bool {
    previous
        .reset_at_utc
        .zip(current.reset_at_utc)
        .is_some_and(|(previous, current)| {
            current < previous - Duration::seconds(RESET_BOUNDARY_TOLERANCE_SECONDS)
        })
}

fn reset_credit_evidence(
    previous: &UsageSnapshot,
    initial: &UsageSnapshot,
    confirmation: &UsageSnapshot,
) -> bool {
    let (Some(previous_inventory), Some(initial_inventory), Some(confirmation_inventory)) = (
        previous.credit_inventory.as_ref(),
        initial.credit_inventory.as_ref(),
        confirmation.credit_inventory.as_ref(),
    ) else {
        return false;
    };

    // With no available credit before the drop, nothing can have been
    // consumed, so an empty inventory is not evidence of a manual reset.
    if previous_inventory.available_count == 0
        || initial_inventory.available_count > previous_inventory.available_count
        || confirmation_inventory.available_count > previous_inventory.available_count
    {
        return false;
    }

    previous_inventory.credits.iter().any(|credit| {
        let Some(id) = credit.id.as_deref() else {
            return false;
        };
        if !credit
            .status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("available"))
        {
            return false;
        }
        inventory_confirms_consumption(
            id,
            credit.expires_at_utc,
            previous_inventory.available_count,
            initial,
        ) && inventory_confirms_consumption(
            id,
            credit.expires_at_utc,
            previous_inventory.available_count,
            confirmation,
        )
    })
}

fn inventory_confirms_consumption(
    credit_id: &str,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    previous_available_count: u32,
    current: &UsageSnapshot,
) -> bool {
    let Some(inventory) = current.credit_inventory.as_ref() else {
        return false;
    };
    if let Some(credit) = inventory
        .credits
        .iter()
        .find(|credit| credit.id.as_deref() == Some(credit_id))
    {
        return credit.status.as_deref().is_some_and(|status| {
            status.eq_ignore_ascii_case("redeeming") || status.eq_ignore_ascii_case("redeemed")
        });
    }
    expires_at.is_none_or(|expires_at| expires_at > current.observed_at_utc)
        && inventory.available_count < previous_available_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::AccountRecord,
        usage::{UsageCreditInventory, UsageCreditRecord, UsagePrimaryWindowKind},
    };
    use chrono::{TimeZone, Utc};

    fn instant(day: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, day, 12, 0, 0).unwrap()
    }

    fn account() -> AccountRecord {
        AccountRecord::create("test", "codex@example.com", None, OPENAI, None).unwrap()
    }

    fn snapshot(
        account: &AccountRecord,
        observed_at: chrono::DateTime<Utc>,
        weekly_used: f64,
        weekly_reset: chrono::DateTime<Utc>,
    ) -> UsageSnapshot {
        UsageSnapshot {
            account_id: account.id,
            observed_at_utc: observed_at,
            response_account_id: Some("provider-account".to_owned()),
            plan_type: Some("plus".to_owned()),
            primary: Some(RateLimitWindow {
                kind: UsageWindowKind::Primary,
                name: "5 hour".to_owned(),
                used_percent: 20.0,
                reset_at_utc: Some(observed_at + Duration::hours(4)),
                limit_window_seconds: 5 * 60 * 60,
            }),
            primary_window_kind: Some(UsagePrimaryWindowKind::Session),
            primary_window_is_synthetic: false,
            secondary: Some(RateLimitWindow {
                kind: UsageWindowKind::Secondary,
                name: "weekly".to_owned(),
                used_percent: weekly_used,
                reset_at_utc: Some(weekly_reset),
                limit_window_seconds: WEEKLY_WINDOW_SECONDS,
            }),
            additional_windows: Vec::new(),
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: Some(account.email.clone()),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: Vec::new(),
            source_diagnostics: Vec::new(),
            provider_id: OPENAI.to_owned(),
            source: Some("codex-oauth".to_owned()),
            data_confidence: "authoritative".to_owned(),
        }
    }

    #[test]
    fn sudden_low_usage_requires_same_account_codex_confirmation() {
        let account = account();
        let previous = snapshot(&account, instant(1), 70.0, instant(5));
        let candidate = snapshot(&account, instant(2), 0.2, instant(12));
        let confirmation = snapshot(
            &account,
            instant(2) + Duration::seconds(1),
            0.8,
            instant(12),
        );

        assert!(needs_weekly_reset_confirmation(
            &account,
            Some(&previous),
            &candidate
        ));
        assert!(!confirms_weekly_reset(
            &account,
            &previous,
            &candidate,
            &confirmation
        ));
    }

    #[test]
    fn natural_reset_is_accepted_only_after_old_boundary_with_matching_samples() {
        let account = account();
        let previous = snapshot(&account, instant(1), 70.0, instant(2));
        let initial = snapshot(&account, instant(2) + Duration::minutes(1), 0.2, instant(9));
        let confirmation = snapshot(
            &account,
            instant(2) + Duration::minutes(2),
            0.4,
            instant(9) + Duration::seconds(30),
        );

        assert!(confirms_weekly_reset(
            &account,
            &previous,
            &initial,
            &confirmation
        ));
    }

    #[test]
    fn rebound_in_confirmation_rejects_the_initial_low_reading() {
        let account = account();
        let previous = snapshot(&account, instant(1), 70.0, instant(5));
        let initial = snapshot(&account, instant(2), 0.2, instant(12));
        let confirmation = snapshot(
            &account,
            instant(2) + Duration::seconds(1),
            63.0,
            instant(5),
        );

        assert!(confirms_weekly_reset(
            &account,
            &previous,
            &initial,
            &confirmation
        ));
    }

    #[test]
    fn confirmation_from_a_different_provider_account_is_rejected() {
        let account = account();
        let previous = snapshot(&account, instant(1), 70.0, instant(5));
        let initial = snapshot(&account, instant(2), 0.2, instant(12));
        let mut confirmation = snapshot(
            &account,
            instant(2) + Duration::seconds(1),
            0.8,
            instant(12),
        );
        confirmation.response_account_id = Some("another-provider-account".to_owned());

        assert!(!confirms_weekly_reset(
            &account,
            &previous,
            &initial,
            &confirmation
        ));
    }

    #[test]
    fn confirmation_with_an_expired_reset_boundary_is_rejected() {
        let account = account();
        let previous = snapshot(&account, instant(1), 70.0, instant(5));
        let initial = snapshot(&account, instant(2), 0.2, instant(12));
        let confirmation = snapshot(&account, instant(6), 63.0, instant(5));

        assert!(!confirms_weekly_reset(
            &account,
            &previous,
            &initial,
            &confirmation
        ));
    }

    #[test]
    fn manual_reset_requires_a_consumed_credit_seen_in_both_samples() {
        let account = account();
        let mut previous = snapshot(&account, instant(1), 70.0, instant(5));
        let mut initial = snapshot(&account, instant(2), 0.2, instant(12));
        let mut confirmation = snapshot(
            &account,
            instant(2) + Duration::seconds(1),
            0.8,
            instant(12),
        );
        previous.credit_inventory = Some(credit_inventory("available", 1));
        initial.credit_inventory = Some(credit_inventory("redeemed", 0));
        confirmation.credit_inventory = Some(credit_inventory("redeemed", 0));

        assert!(confirms_weekly_reset(
            &account,
            &previous,
            &initial,
            &confirmation
        ));
    }

    #[test]
    fn early_reset_candidate_requires_unchanged_positive_credit_inventory() {
        let account = account();
        let mut previous = snapshot(&account, instant(1), 70.0, instant(5));
        let mut initial = snapshot(&account, instant(2), 0.0, instant(9));
        let mut confirmation =
            snapshot(&account, instant(2) + Duration::seconds(1), 0.0, instant(9));
        previous.credit_inventory = Some(credit_inventory("available", 1));
        initial.credit_inventory = Some(credit_inventory("available", 1));
        confirmation.credit_inventory = Some(credit_inventory("available", 1));

        let candidate = create_delayed_reset_candidate(
            &account,
            &previous,
            &initial,
            &confirmation,
            instant(2) + Duration::seconds(2),
        )
        .unwrap();
        assert_eq!(
            candidate.snapshot.observed_at_utc,
            confirmation.observed_at_utc
        );

        confirmation.credit_inventory = Some(credit_inventory("redeemed", 0));
        assert_eq!(
            create_delayed_reset_candidate(
                &account,
                &previous,
                &initial,
                &confirmation,
                instant(2) + Duration::seconds(2),
            )
            .unwrap_err(),
            "reset_credit_inventory_missing_or_changed"
        );
    }

    #[test]
    fn delayed_candidate_accepts_a_stable_boundary_but_rejects_a_small_shift() {
        let account = account();
        let mut previous = snapshot(&account, instant(1), 70.0, instant(5));
        let mut initial = snapshot(&account, instant(2), 0.0, instant(5));
        let mut confirmation =
            snapshot(&account, instant(2) + Duration::seconds(1), 0.0, instant(5));
        previous.credit_inventory = Some(credit_inventory("available", 1));
        initial.credit_inventory = Some(credit_inventory("available", 1));
        confirmation.credit_inventory = Some(credit_inventory("available", 1));

        create_delayed_reset_candidate(
            &account,
            &previous,
            &initial,
            &confirmation,
            instant(2) + Duration::seconds(2),
        )
        .unwrap();

        initial.secondary.as_mut().unwrap().reset_at_utc = Some(instant(5) + Duration::seconds(30));
        confirmation.secondary.as_mut().unwrap().reset_at_utc =
            Some(instant(5) + Duration::seconds(30));
        assert_eq!(
            create_delayed_reset_candidate(
                &account,
                &previous,
                &initial,
                &confirmation,
                instant(2) + Duration::seconds(2),
            )
            .unwrap_err(),
            "weekly_reset_boundary_not_supported"
        );
    }

    #[test]
    fn stable_boundary_reset_publishes_after_an_independent_observation() {
        let account = account();
        let mut previous = snapshot(&account, instant(1), 70.0, instant(5));
        let mut initial = snapshot(&account, instant(2), 0.0, instant(5));
        let mut confirmation =
            snapshot(&account, instant(2) + Duration::seconds(1), 0.0, instant(5));
        previous.credit_inventory = Some(credit_inventory("available", 1));
        initial.credit_inventory = Some(credit_inventory("available", 1));
        confirmation.credit_inventory = Some(credit_inventory("available", 1));
        let created_at = confirmation.observed_at_utc;
        let candidate = create_delayed_reset_candidate(
            &account,
            &previous,
            &initial,
            &confirmation,
            created_at,
        )
        .unwrap();

        let mut current = snapshot(&account, created_at + Duration::minutes(2), 0.0, instant(5));
        current.credit_inventory = Some(credit_inventory("available", 1));
        assert_eq!(
            evaluate_delayed_reset_candidate(
                &account,
                &previous,
                &candidate,
                &current,
                created_at + Duration::minutes(2),
            ),
            DelayedResetDecision::PublishCurrent
        );
    }

    #[test]
    fn delayed_reset_requires_a_later_matching_observation_and_minimum_age() {
        let account = account();
        let mut previous = snapshot(&account, instant(1), 70.0, instant(5));
        let mut initial = snapshot(&account, instant(2), 0.0, instant(9));
        let mut confirmation = snapshot(
            &account,
            instant(2) + Duration::seconds(1),
            0.0,
            instant(9) + Duration::seconds(1),
        );
        previous.credit_inventory = Some(credit_inventory("available", 1));
        initial.credit_inventory = Some(credit_inventory("available", 1));
        confirmation.credit_inventory = Some(credit_inventory("available", 1));
        let created_at = confirmation.observed_at_utc;
        let candidate = create_delayed_reset_candidate(
            &account,
            &previous,
            &initial,
            &confirmation,
            created_at,
        )
        .unwrap();

        let mut current = snapshot(
            &account,
            created_at + Duration::seconds(30),
            0.0,
            created_at + Duration::days(7) + Duration::seconds(30),
        );
        current.credit_inventory = Some(credit_inventory("available", 1));
        assert_eq!(
            evaluate_delayed_reset_candidate(
                &account,
                &previous,
                &candidate,
                &current,
                created_at + Duration::seconds(30),
            ),
            DelayedResetDecision::RetainCandidate
        );

        current.observed_at_utc = created_at + Duration::minutes(3);
        current.secondary.as_mut().unwrap().reset_at_utc =
            Some(current.observed_at_utc + Duration::days(7));
        assert_eq!(
            evaluate_delayed_reset_candidate(
                &account,
                &previous,
                &candidate,
                &current,
                created_at + Duration::minutes(3),
            ),
            DelayedResetDecision::PublishCurrent
        );
    }

    fn credit_inventory(status: &str, available_count: u32) -> UsageCreditInventory {
        UsageCreditInventory {
            available_count,
            credits: vec![UsageCreditRecord {
                id: Some("weekly-reset-credit".to_owned()),
                reset_type: Some("codex_rate_limits".to_owned()),
                status: Some(status.to_owned()),
                granted_at_utc: Some(instant(1) - Duration::days(1)),
                expires_at_utc: Some(instant(13)),
                redeem_started_at_utc: None,
                redeemed_at_utc: None,
                title: None,
                description: None,
            }],
        }
    }
    #[test]
    fn empty_reset_credit_inventory_is_not_evidence_of_an_early_reset() {
        let account = account();
        let mut previous = snapshot(&account, instant(1), 70.0, instant(5));
        let mut initial = snapshot(&account, instant(2), 0.2, instant(5));
        let mut confirmation =
            snapshot(&account, instant(2) + Duration::seconds(1), 0.3, instant(5));
        previous.credit_inventory = Some(credit_inventory("redeemed", 0));
        initial.credit_inventory = Some(credit_inventory("redeemed", 0));
        confirmation.credit_inventory = Some(credit_inventory("redeemed", 0));

        assert!(!confirms_weekly_reset(
            &account,
            &previous,
            &initial,
            &confirmation
        ));
    }
}
