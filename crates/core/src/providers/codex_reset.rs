//! Guards Codex weekly usage publication against an unconfirmed reset.

use crate::{
    accounts::{AccountRecord, OPENAI},
    usage::{RateLimitWindow, UsageSnapshot, UsageWindowKind},
};
use chrono::Duration;

const WEEKLY_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;
const RESET_BOUNDARY_TOLERANCE_SECONDS: i64 = 2 * 60;
const RESET_THRESHOLD_PERCENT: f64 = 1.0;

/// A sudden weekly drop is ambiguous until a second request using the same
/// account-scoped Codex credential confirms it.
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

fn is_codex_account(account: &AccountRecord) -> bool {
    account.provider_id.eq_ignore_ascii_case(OPENAI)
        || account.provider_id.eq_ignore_ascii_case("codex")
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

    if previous_inventory.available_count == 0 {
        return initial_inventory.available_count == 0
            && confirmation_inventory.available_count == 0;
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
}
