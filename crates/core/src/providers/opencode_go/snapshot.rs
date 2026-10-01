//! Usage windows (five-hour, weekly, monthly) and the snapshots built from them.

use super::*;

pub(super) fn build_snapshot(
    account: &AccountRecord,
    root: &Value,
    source: &str,
    rolling: ParsedWindow,
    weekly: Option<ParsedWindow>,
    monthly: Option<ParsedWindow>,
    balance: Option<f64>,
    renews_at: Option<DateTime<Utc>>,
    confidence: &str,
    diagnostics: Vec<UsageSourceDiagnostic>,
) -> UsageProbeResult {
    let mut metrics = vec![window_metric(
        "rolling",
        &rolling.window,
        rolling.used_amount,
        rolling.limit_amount,
    )];
    if let Some(window) = weekly.as_ref() {
        metrics.push(window_metric(
            "weekly",
            &window.window,
            window.used_amount,
            window.limit_amount,
        ));
    }
    if let Some(window) = monthly.as_ref() {
        metrics.push(window_metric(
            "monthly",
            &window.window,
            window.used_amount,
            window.limit_amount,
        ));
    }

    let monthly_usage = monthly.as_ref().and_then(|window| window.used_amount);
    let monthly_limit = monthly.as_ref().and_then(|window| window.limit_amount);
    let spend = (monthly_usage.is_some() || monthly_limit.is_some()).then_some(SpendSnapshot {
        monthly_usage,
        monthly_limit,
        used_percent: monthly_usage
            .zip(monthly_limit)
            .map(|(used, limit)| percent(used, limit)),
        limit_enabled: monthly_limit.map(|limit| limit > 0.0),
        currency_code: None,
    });
    let credits = balance.map(|value| CreditsSnapshot {
        has_credits: Some(true),
        unlimited: Some(false),
        balance: Some(value),
        currency_code: None,
        approximate_message_cost: None,
        limit: None,
        balance_read_succeeded: Some(true),
        credits_available: Some(value > 0.0),
    });
    let response_account_id = json_string(
        root,
        &[
            "workspaceId",
            "workspace_id",
            "orgId",
            "org_id",
            "accountId",
            "account_id",
        ],
    )
    .or_else(|| account.provider_account_id.clone());
    let email = find_email(root);
    let plan = json_string(root, &["plan", "planType", "plan_type", "tier"]);
    let mut snapshot = UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: response_account_id.clone(),
        plan_type: plan.clone(),
        primary: Some(rolling.window.clone()),
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary: weekly.map(|window| window.window),
        additional_windows: monthly
            .map(|window| AdditionalRateLimitWindow {
                key: "monthly".to_owned(),
                name: "Monthly".to_owned(),
                window: window.window,
            })
            .into_iter()
            .collect(),
        credits,
        credit_inventory: None,
        spend,
        observed_email: email.clone(),
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics,
        source_diagnostics: diagnostics,
        provider_id: OPENCODE_GO.to_owned(),
        source: Some(source.to_owned()),
        data_confidence: confidence.to_owned(),
    };
    if let Some(renews_at) = renews_at {
        snapshot.metrics.push(UsageMetric {
            key: "subscription-renewal".to_owned(),
            name: "Subscription renewal".to_owned(),
            used_percent: None,
            used_amount: None,
            limit_amount: None,
            remaining_amount: None,
            unit: None,
            reset_at_utc: Some(renews_at),
            reset_label: Some("Subscription renewal".to_owned()),
            metadata: HashMap::new(),
        });
    }
    UsageProbeResult::success(
        snapshot.clone(),
        Some(VerifiedIdentity {
            email,
            provider_account_id: response_account_id,
            plan_type: plan,
        }),
    )
}

pub(super) fn balance_only_snapshot(
    account: &AccountRecord,
    workspace: &str,
    balance: f64,
    source: &str,
) -> UsageProbeResult {
    let snapshot = UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: Some(workspace.to_owned()),
        plan_type: None,
        primary: None,
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary: None,
        additional_windows: Vec::new(),
        credits: Some(CreditsSnapshot {
            has_credits: Some(true),
            unlimited: Some(false),
            balance: Some(balance),
            currency_code: None,
            approximate_message_cost: None,
            limit: None,
            balance_read_succeeded: Some(true),
            credits_available: Some(balance > 0.0),
        }),
        credit_inventory: None,
        spend: None,
        observed_email: (!account.email.is_empty()).then(|| account.email.clone()),
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics: Vec::new(),
        source_diagnostics: Vec::new(),
        provider_id: OPENCODE_GO.to_owned(),
        source: Some(source.to_owned()),
        data_confidence: "authoritative".to_owned(),
    };
    UsageProbeResult::success(snapshot, None)
}

pub(super) fn enrich_balance(result: &mut UsageProbeResult, balance: Option<f64>, root: &Value) {
    let Some(snapshot) = result.snapshot.as_mut() else {
        return;
    };
    let Some(balance) = balance else {
        return;
    };
    snapshot.credits = Some(CreditsSnapshot {
        has_credits: Some(true),
        unlimited: Some(false),
        balance: Some(balance),
        currency_code: None,
        approximate_message_cost: None,
        limit: find_credit_limit(root),
        balance_read_succeeded: Some(true),
        credits_available: Some(balance > 0.0),
    });
}

pub(super) fn parse_window(
    value: &Value,
    kind: UsageWindowKind,
    name: &str,
    direct_percent: bool,
    micro_cents: bool,
) -> Option<ParsedWindow> {
    let now = Utc::now();
    let direct = json_number(
        value,
        &[
            "usagePercent",
            "usage_percent",
            "usedPercent",
            "used_percent",
            "percentUsed",
            "percent",
            "utilization",
            "utilizationPercent",
            "utilization_percent",
        ],
    );
    let (mut used_amount, mut limit_amount) = (
        json_number(value, &["usedAmount", "used_amount", "usedUSD", "used_usd"]),
        json_number(
            value,
            &["limitAmount", "limit_amount", "limitUSD", "limit_usd"],
        ),
    );
    let used_raw = json_number(
        value,
        &[
            "used",
            "usage",
            "consumed",
            "count",
            "usedTokens",
            "usedMicroCents",
            "used_micro_cents",
        ],
    );
    let limit_raw = json_number(
        value,
        &[
            "limit",
            "total",
            "quota",
            "max",
            "cap",
            "tokenLimit",
            "limitMicroCents",
            "limit_micro_cents",
        ],
    );
    if used_amount.is_none() {
        used_amount = used_raw;
    }
    if limit_amount.is_none() {
        limit_amount = limit_raw;
    }
    let has_micro_keys = has_key(
        value,
        &[
            "usedMicroCents",
            "used_micro_cents",
            "limitMicroCents",
            "limit_micro_cents",
        ],
    ) || value
        .get("unit")
        .and_then(Value::as_str)
        .is_some_and(|unit| unit.to_ascii_lowercase().contains("micro"));
    if micro_cents || has_micro_keys {
        used_amount = used_amount.map(|value| value / MICRO_CENTS_PER_USD);
        limit_amount = limit_amount.map(|value| value / MICRO_CENTS_PER_USD);
    }
    let computed = match (used_amount, limit_amount) {
        (Some(used), Some(limit)) if limit > 0.0 => Some(percent(used, limit)),
        _ => None,
    };
    let used_percent = direct
        .map(|value| {
            if !direct_percent && (0.0..=1.0).contains(&value) {
                value * 100.0
            } else {
                value
            }
        })
        .or(computed)
        .map(|value| value.clamp(0.0, 100.0))?;
    let reset = parse_reset_at(
        value,
        now,
        &[
            "resetAt",
            "reset_at",
            "resetTime",
            "reset_time",
            "resetsAt",
            "resets_at",
            "nextReset",
            "next_reset",
            "renewAt",
            "renew_at",
        ],
    )
    .or_else(|| parse_reset_in(value, now));
    Some(ParsedWindow {
        window: RateLimitWindow {
            kind,
            name: name.to_owned(),
            used_percent,
            reset_at_utc: reset,
            limit_window_seconds: fixed_window_seconds(kind),
        },
        used_amount,
        limit_amount,
    })
}

pub(super) fn parse_text_window(
    body: &str,
    object_name: &str,
    kind: UsageWindowKind,
    name: &str,
) -> Option<ParsedWindow> {
    let object = regex::escape(object_name);
    let percent = Regex::new(&format!(
        r"(?is){object}.{{0,900}}?(?:usagePercent|usage_percent|usedPercent|percentUsed)\s*[:=]\s*([0-9]+(?:\.[0-9]+)?)"
    ))
    .ok()?
    .captures(body)
    .and_then(|captures| captures.get(1))
    .and_then(|value| value.as_str().parse::<f64>().ok())?;
    let seconds = Regex::new(&format!(
        r"(?is){object}.{{0,900}}?(?:resetInSec|resetInSeconds|resetSeconds|reset_in_sec)\s*[:=]\s*([0-9]+)"
    ))
    .ok()
    .and_then(|regex| regex.captures(body))
    .and_then(|captures| captures.get(1))
    .and_then(|value| value.as_str().parse::<i64>().ok());
    let now = Utc::now();
    Some(ParsedWindow {
        window: RateLimitWindow {
            kind,
            name: name.to_owned(),
            used_percent: percent.clamp(0.0, 100.0),
            reset_at_utc: seconds.and_then(|seconds| checked_after(now, seconds as f64)),
            limit_window_seconds: fixed_window_seconds(kind),
        },
        used_amount: None,
        limit_amount: None,
    })
}

#[derive(Debug, Clone, Copy)]
pub(super) enum WindowRole {
    Rolling,
    Weekly,
    Monthly,
}

pub(super) fn find_window(value: &Value, role: WindowRole) -> Option<&Value> {
    let names: &[&str] = match role {
        WindowRole::Rolling => &[
            "rolling",
            "rollingUsage",
            "rolling_usage",
            "rollingWindow",
            "rolling_window",
            "fiveHour",
            "five_hour",
            "5h",
        ],
        WindowRole::Weekly => &[
            "weekly",
            "weeklyUsage",
            "weekly_usage",
            "weeklyWindow",
            "weekly_window",
            "week",
        ],
        WindowRole::Monthly => &[
            "monthly",
            "monthlyUsage",
            "monthly_usage",
            "monthlyWindow",
            "monthly_window",
            "month",
        ],
    };
    if let Value::Object(object) = value {
        for name in names {
            if let Some(candidate) = object.get(*name)
                && candidate.is_object()
            {
                return Some(candidate);
            }
        }
        for (key, child) in object {
            let lower = key.to_ascii_lowercase();
            let matches = match role {
                WindowRole::Rolling => {
                    lower.contains("rolling")
                        || lower.contains("fivehour")
                        || lower.contains("5h")
                        || lower.contains("5-hour")
                }
                WindowRole::Weekly => lower.contains("weekly") || lower.contains("week"),
                WindowRole::Monthly => lower.contains("monthly") || lower.contains("month"),
            };
            if matches && child.is_object() {
                return Some(child);
            }
        }
        for child in object.values() {
            if let Some(found) = find_window(child, role) {
                return Some(found);
            }
        }
    } else if let Value::Array(array) = value {
        for child in array {
            if let Some(found) = find_window(child, role) {
                return Some(found);
            }
        }
    }
    None
}

pub(super) fn first_named<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| value.get(*name))
}

pub(super) fn parse_json_document(body: &str) -> Result<Value, serde_json::Error> {
    let trimmed = body.trim().trim_start_matches(")]}',");
    serde_json::from_str(trimmed).or_else(|_| {
        let start = trimmed.find(['{', '[']).unwrap_or(0);
        let end = trimmed
            .rfind(['}', ']'])
            .map(|index| index + 1)
            .unwrap_or(trimmed.len());
        serde_json::from_str(&trimmed[start..end])
    })
}

pub(super) fn parse_reset_at(
    value: &Value,
    now: DateTime<Utc>,
    keys: &[&str],
) -> Option<DateTime<Utc>> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(|value| parse_date_value(value, now))
}

pub(super) fn parse_reset_in(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    json_number(
        value,
        &[
            "resetInSec",
            "resetInSeconds",
            "resetSeconds",
            "reset_sec",
            "reset_in_sec",
            "resetsInSec",
            "resetsInSeconds",
            "resetIn",
            "resetSec",
        ],
    )
    .and_then(|seconds| checked_after(now, seconds))
}

/// Provider values are untrusted: an out-of-range delay must not panic.
pub(super) fn checked_after(now: DateTime<Utc>, seconds: f64) -> Option<DateTime<Utc>> {
    if !seconds.is_finite() {
        return None;
    }
    Duration::try_seconds(seconds.max(0.0) as i64).and_then(|delay| now.checked_add_signed(delay))
}

/// `limit_window_seconds` is the fixed length of the quota window, never the
/// time remaining until reset (that is derived from `reset_at_utc`). OpenCode
/// Go uses a rolling 5-hour, a weekly, and a monthly window.
pub(super) fn fixed_window_seconds(kind: UsageWindowKind) -> i64 {
    match kind {
        UsageWindowKind::Primary => 5 * 60 * 60,
        UsageWindowKind::Secondary => 7 * 24 * 60 * 60,
        UsageWindowKind::Additional => 30 * 24 * 60 * 60,
    }
}

pub(super) fn parse_date_value(value: &Value, _now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(number) => epoch_to_date(number.as_f64()?),
        Value::String(text) => {
            if let Ok(number) = text.trim().parse::<f64>() {
                return epoch_to_date(number);
            }
            DateTime::parse_from_rfc3339(text.trim())
                .ok()
                .map(|date| date.with_timezone(&Utc))
        }
        _ => None,
    }
}

pub(super) fn epoch_to_date(value: f64) -> Option<DateTime<Utc>> {
    if value > 1_000_000_000_000.0 {
        Utc.timestamp_millis_opt(value as i64).single()
    } else if value > 1_000_000_000.0 {
        Utc.timestamp_opt(value as i64, 0).single()
    } else {
        None
    }
}

pub(super) fn find_email(value: &Value) -> Option<String> {
    json_string(value, &["email", "emailAddress", "email_address"]).or_else(|| match value {
        Value::Object(object) => object.values().find_map(find_email),
        Value::Array(array) => array.iter().find_map(find_email),
        _ => None,
    })
}

pub(super) fn has_key(value: &Value, names: &[&str]) -> bool {
    names.iter().any(|name| value.get(*name).is_some())
}

pub(super) fn window_metric(
    key: &str,
    window: &RateLimitWindow,
    used: Option<f64>,
    limit: Option<f64>,
) -> UsageMetric {
    let mut metadata = HashMap::new();
    metadata.insert("provider".to_owned(), OPENCODE_GO.to_owned());
    UsageMetric {
        key: key.to_owned(),
        name: window.name.clone(),
        used_percent: Some(window.used_percent),
        used_amount: used,
        limit_amount: limit,
        remaining_amount: used.zip(limit).map(|(used, limit)| (limit - used).max(0.0)),
        unit: used.or(limit).map(|_| "USD".to_owned()),
        reset_at_utc: window.reset_at_utc,
        reset_label: None,
        metadata,
    }
}

pub(super) fn percent(used: f64, limit: f64) -> f64 {
    if !used.is_finite() || !limit.is_finite() || limit <= 0.0 {
        0.0
    } else {
        (used.max(0.0) / limit * 100.0).clamp(0.0, 100.0)
    }
}
