//! JSON values and human-readable tables for command results.

use super::*;

pub(super) fn account_value(account: &AccountRecord) -> Value {
    json!({
        "account_ref": account.account_ref,
        "provider": provider_name(&account.provider_id),
        "provider_id": account.provider_id,
        "name": account.display_name(),
        "label": account.label,
        "alias": account.alias,
        "email": account.email,
        "workspace": account.workspace_id.as_ref().map(|id| json!({
            "id": id,
            "name": account.workspace_name,
        })),
        "status": account_status_name(account),
    })
}

pub(super) fn snapshot_value(snapshot: &UsageSnapshot) -> Value {
    let mut windows = Vec::new();
    if let Some(window) = &snapshot.primary {
        windows.push(window_value("primary", window));
    }
    if let Some(window) = &snapshot.secondary {
        windows.push(window_value("secondary", window));
    }
    windows.extend(
        snapshot
            .additional_windows
            .iter()
            .map(|additional| window_value(&additional.key, &additional.window)),
    );
    json!({
        "observed_at_utc": snapshot.observed_at_utc,
        "provider": provider_name(&snapshot.provider_id),
        "provider_id": snapshot.provider_id,
        "source": snapshot.source,
        "data_confidence": snapshot.data_confidence,
        "plan_type": snapshot.plan_type,
        "observed_email": snapshot.observed_email,
        "response_account_id": snapshot.response_account_id,
        "primary_window_kind": snapshot.primary_window_kind,
        "primary_window_is_synthetic": snapshot.primary_window_is_synthetic,
        "is_stale": snapshot.is_stale,
        "stale_reason": snapshot.stale_reason,
        "stale_at_utc": snapshot.stale_at_utc,
        "windows": windows,
        "metrics": snapshot.metrics,
        "credits": snapshot.credits,
        "credit_inventory": snapshot.credit_inventory,
        "spend": snapshot.spend,
        "source_diagnostics": snapshot.source_diagnostics,
    })
}

pub(super) fn window_value(
    key: &str,
    window: &usage_monitor_core::usage::RateLimitWindow,
) -> Value {
    json!({
        "key": key,
        "kind": window_kind_name(window.kind),
        "name": window.name,
        "used_percent": window.used_percent,
        "remaining_percent": window.remaining_percent(),
        "reset_at_utc": window.reset_at_utc,
        "limit_window_seconds": window.limit_window_seconds,
    })
}

pub(super) fn window_kind_name(kind: UsageWindowKind) -> &'static str {
    match kind {
        UsageWindowKind::Primary => "primary",
        UsageWindowKind::Secondary => "secondary",
        UsageWindowKind::Additional => "additional",
    }
}

pub(super) fn refresh_result_value(
    account: &AccountRecord,
    outcome: &usage_monitor_core::refresh::RefreshOutcome,
) -> Value {
    json!({
        "account_ref": account.account_ref,
        "provider": provider_name(&account.provider_id),
        "provider_id": account.provider_id,
        "status": refresh_status_name(outcome.status),
        "completed_at_utc": outcome.completed_at_utc,
        "snapshot": outcome.snapshot.as_ref().map(snapshot_value),
        "error": outcome.error.as_ref().map(refresh_error_value),
        "storage_error": outcome.storage_error,
    })
}

pub(super) fn usage_get_value(account: &AccountRecord, outcome: &RefreshOutcome) -> Value {
    json!({
        "schema_version": 1,
        "account": account_value(account),
        "snapshot": outcome.snapshot.as_ref().map(snapshot_value),
        "refresh": {
            "status": refresh_status_name(outcome.status),
            "completed_at_utc": outcome.completed_at_utc,
            "error": outcome.error.as_ref().map(refresh_error_value),
            "storage_error": outcome.storage_error,
        },
    })
}

pub(super) fn refresh_error_value(error: &UsageAdapterError) -> Value {
    json!({
        "code": format!("{:?}", error.code).to_ascii_lowercase(),
        "message": error.message,
        "http_status_code": error.http_status_code,
        "retry_after_seconds": error.retry_after_seconds,
    })
}

pub(super) fn usage_get_exit_code(status: RefreshStatus) -> i32 {
    if status == RefreshStatus::Updated {
        0
    } else {
        EXIT_REFRESH_FAILED
    }
}

pub(super) fn print_usage_get(account: &AccountRecord, outcome: &RefreshOutcome) {
    if let Some(snapshot) = &outcome.snapshot {
        print_usage(account, snapshot);
    } else {
        println!(
            "{}\t{}\t{}",
            account.account_ref.as_deref().unwrap_or("?"),
            provider_name(&account.provider_id),
            account.display_name()
        );
        println!("No current usage snapshot is available.");
    }
    println!("Refresh: {}", refresh_status_name(outcome.status));
    if let Some(error) = &outcome.error {
        println!(
            "Refresh error: {}: {}",
            format!("{:?}", error.code).to_ascii_lowercase(),
            error.message
        );
    }
    if let Some(error) = &outcome.storage_error {
        println!("Storage error: {error}");
    }
}

pub(super) fn account_status_name(account: &AccountRecord) -> &'static str {
    use usage_monitor_core::accounts::AccountStatus;
    match account.status {
        AccountStatus::Active => "active",
        AccountStatus::NeedsReauthentication => "needs_reauthentication",
        AccountStatus::Paused => "paused",
        AccountStatus::Disabled => "disabled",
    }
}

pub(super) fn provider_name(provider_id: &str) -> &str {
    match provider_id {
        "openai" | "codex" => "codex",
        "opencodego" => "opencode-go",
        _ => provider_id,
    }
}

pub(super) fn refresh_status_name(status: RefreshStatus) -> &'static str {
    match status {
        RefreshStatus::Updated => "updated",
        RefreshStatus::RetainedStale => "retained_stale",
        RefreshStatus::Failed => "failed",
        RefreshStatus::Invalidated => "invalidated",
        RefreshStatus::Skipped => "skipped",
    }
}

pub(super) fn print_json(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).expect("CLI JSON values serialize")
    );
}

pub(super) fn emit_failure(failure: &CliFailure, json_output: bool) {
    if json_output {
        print_json(json!({
            "schema_version": 1,
            "error": {
                "code": failure.code,
                "message": failure.message,
                "candidates": failure.candidates,
            }
        }));
    } else {
        eprintln!("{}: {}", failure.code, failure.message);
        if !failure.candidates.is_empty() {
            eprintln!("Candidates: {}", failure.candidates.join(", "));
        }
    }
}

pub(super) fn print_account_table(accounts: &[&AccountRecord]) {
    println!("REF\tPROVIDER\tNAME\tEMAIL\tWORKSPACE\tSTATUS");
    for account in accounts {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            account.account_ref.as_deref().unwrap_or("?"),
            provider_name(&account.provider_id),
            account.display_name(),
            account.email,
            account.workspace_name.as_deref().unwrap_or(""),
            account_status_name(account),
        );
    }
}

pub(super) fn print_account_details(account: &AccountRecord) {
    println!(
        "Reference: {}",
        account.account_ref.as_deref().unwrap_or("?")
    );
    println!("Provider: {}", provider_name(&account.provider_id));
    println!("Name: {}", account.display_name());
    println!("Email: {}", account.email);
    println!(
        "Workspace: {}",
        account.workspace_name.as_deref().unwrap_or("none")
    );
    println!("Status: {}", account_status_name(account));
}

pub(super) fn print_usage(account: &AccountRecord, snapshot: &UsageSnapshot) {
    println!(
        "{}\t{}\t{}",
        account.account_ref.as_deref().unwrap_or("?"),
        provider_name(&account.provider_id),
        account.display_name()
    );
    println!("Observed (UTC): {}", snapshot.observed_at_utc.to_rfc3339());
    println!(
        "Plan: {}",
        snapshot.plan_type.as_deref().unwrap_or("unknown")
    );
    println!("Stale: {}", snapshot.is_stale);
    if let Some(window) = &snapshot.primary {
        print_window("primary", window);
    }
    if let Some(window) = &snapshot.secondary {
        print_window("secondary", window);
    }
    for additional in &snapshot.additional_windows {
        print_window(&additional.key, &additional.window);
    }
}

pub(super) fn print_window(key: &str, window: &usage_monitor_core::usage::RateLimitWindow) {
    println!(
        "{key}\t{}\t{:.1}% used\t{:.1}% remaining\treset={}",
        window.name,
        window.used_percent,
        window.remaining_percent(),
        window
            .reset_at_utc
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_owned())
    );
}
