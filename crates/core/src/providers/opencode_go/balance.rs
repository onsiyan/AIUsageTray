//! Zen balance and credit limit from the console billing endpoints, legacy server calls, or page text.

use super::*;

pub(super) fn find_balance(value: &Value) -> Option<f64> {
    if let Some(number) = json_number(
        value,
        &[
            "zenBalance",
            "zen_balance",
            "zenCurrentBalance",
            "zen_current_balance",
            "currentBalance",
            "current_balance",
            "currentBalanceUSD",
            "current_balance_usd",
            "balanceUSD",
            "balance_usd",
            "usdBalance",
            "usd_balance",
            "creditBalance",
            "credit_balance",
        ],
    ) {
        return Some(number);
    }
    if let Some(balance) = value.get("balance") {
        let currency =
            json_string(value, &["currency", "unit"]).map(|value| value.to_ascii_lowercase());
        if currency
            .as_deref()
            .is_some_and(|value| value.contains("usd") || value.contains('$'))
            && let Some(number) = balance
                .as_f64()
                .or_else(|| balance.as_str().and_then(|v| v.parse().ok()))
        {
            return Some(number);
        }
        if let Some(found) = find_balance(balance) {
            return Some(found);
        }
    }
    match value {
        Value::Object(object) => object.values().find_map(find_balance),
        Value::Array(array) => array.iter().find_map(find_balance),
        _ => None,
    }
}

pub(super) fn find_console_billing_balance(value: &Value) -> Option<f64> {
    json_number(value, &["balanceMicroCents", "balance_micro_cents"])
        .map(|micro_cents| micro_cents / MICRO_CENTS_PER_USD)
}

pub(super) async fn fetch_console_balance(
    task: &mut Option<ConsoleBillingTask>,
    wait: StdDuration,
) -> Result<(f64, Value), UsageSourceDiagnostic> {
    let Some(mut task) = task.take() else {
        return Err(diagnostic(
            "web.console.billing",
            UsageAdapterErrorCode::InvalidPayload,
            "console billing request was not started",
            None,
        ));
    };

    let response = match tokio::time::timeout(wait, &mut task).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(error))) => return Err(transport_diagnostic("web.console.billing", &error)),
        Ok(Err(error)) => {
            return Err(diagnostic(
                "web.console.billing",
                UsageAdapterErrorCode::TransientHttp,
                format!("console billing task failed: {error}"),
                None,
            ));
        }
        Err(_) => {
            task.abort();
            return Err(diagnostic(
                "web.console.billing",
                UsageAdapterErrorCode::TransientHttp,
                format!("console billing request exceeded its {wait:?} wait bound"),
                None,
            ));
        }
    };

    if !response.is_success() {
        return Err(diagnostic(
            "web.console.billing",
            http_error_code(response.status_code),
            format!(
                "OpenCode console billing request failed (HTTP {})",
                response.status_code
            ),
            Some(response.status_code),
        ));
    }
    let root = parse_json_document(&response.body).map_err(|_| {
        diagnostic(
            "web.console.billing",
            UsageAdapterErrorCode::InvalidPayload,
            "console billing response was not valid JSON",
            Some(response.status_code),
        )
    })?;
    let balance = find_console_billing_balance(&root)
        .or_else(|| find_balance(&root))
        .ok_or_else(|| {
            diagnostic(
                "web.console.billing",
                UsageAdapterErrorCode::InvalidPayload,
                format!(
                    "console billing had no recognized balance: {}",
                    json_shape_summary(&root)
                ),
                Some(response.status_code),
            )
        })?;
    Ok((balance, root))
}

pub(super) fn find_legacy_billing_balance(value: &Value) -> Option<f64> {
    match value {
        Value::Object(object) => {
            let has_customer = object
                .get("customerID")
                .or_else(|| object.get("customerId"))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            if has_customer
                && let Some(raw) = object
                    .get("balance")
                    .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
            {
                return Some(raw / 100_000_000.0);
            }
            object.values().find_map(find_legacy_billing_balance)
        }
        Value::Array(array) => array.iter().find_map(find_legacy_billing_balance),
        _ => None,
    }
}

pub(super) fn find_balance_from_text(body: &str) -> Option<f64> {
    let explicit = Regex::new(
        r#"(?i)(?:zenBalance|zenCurrentBalance|currentBalance|currentBalanceUSD|balanceUSD|usdBalance|creditBalance)\s*["']?\s*[:=]\s*["']?([0-9]+(?:\.[0-9]+)?)"#,
    )
    .ok()?
    .captures(body)
    .and_then(|captures| captures.get(1))
    .and_then(|value| value.as_str().parse().ok());
    explicit.or_else(|| {
        Regex::new(r#"(?i)(?:balance|zen\s+balance)[\s\S]{0,120}?\$\s*([0-9][0-9,]*(?:\.[0-9]+)?)"#)
            .ok()?
            .captures(body)
            .and_then(|captures| captures.get(1))
            .and_then(|value| value.as_str().replace(',', "").parse().ok())
    })
}

pub(super) fn find_credit_limit(value: &Value) -> Option<CreditLimitSnapshot> {
    let limit = json_number(value, &["limit", "monthlyLimit", "monthly_limit"])?;
    let used = json_number(value, &["used", "usage", "consumed"]);
    Some(CreditLimitSnapshot {
        limit: Some(limit),
        used,
        remaining: used.map(|used| (limit - used).max(0.0)),
        used_percent: used.map(|used| percent(used, limit)),
        reset_at_utc: parse_reset_at(value, Utc::now(), &["resetAt", "reset_at", "resetsAt"]),
        unit: Some("USD".to_owned()),
        read_succeeded: true,
    })
}
