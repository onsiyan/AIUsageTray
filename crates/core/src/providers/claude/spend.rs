//! Extra-usage spend reported by the OAuth usage API.

use super::*;

pub(super) fn parse_extra_usage(root: &Value) -> (Option<SpendSnapshot>, Option<CreditsSnapshot>) {
    let Some(extra) = root.get("extra_usage") else {
        return (None, None);
    };
    if extra
        .get("is_enabled")
        .and_then(Value::as_bool)
        .is_some_and(|enabled| !enabled)
    {
        return (None, None);
    }
    let used = json_number(extra, &["used_credits", "usedCredits"]);
    let limit = json_number(
        extra,
        &["monthly_limit", "monthly_credit_limit", "monthlyLimit"],
    );
    // The provider's own implementation treats an incomplete pair as
    // unusable rather than displaying a misleading partial spend balance.
    let (Some(used), Some(limit)) = (
        used.filter(|value| value.is_finite() && *value >= 0.0),
        limit.filter(|value| value.is_finite() && *value > 0.0),
    ) else {
        return (None, None);
    };
    // Claude reports extra-usage amounts in cents. Normalize to major units
    // before exposing them through the provider-neutral spend contract.
    let used = used / 100.0;
    let limit = limit / 100.0;
    if limit <= 0.0 {
        return (None, None);
    }
    let currency_code = normalized_claude_currency(json_string(extra, &["currency"]))
        .unwrap_or_else(|| "USD".to_owned());
    let percent = json_number(extra, &["utilization", "used_percent", "usedPercent"])
        .filter(|value| value.is_finite())
        .map(normalize_percent)
        .or_else(|| {
            let percent = used / limit * 100.0;
            percent.is_finite().then(|| normalize_percent(percent))
        });
    (
        Some(SpendSnapshot {
            monthly_usage: Some(used),
            monthly_limit: Some(limit),
            used_percent: percent,
            limit_enabled: Some(true),
            currency_code: Some(currency_code),
        }),
        None,
    )
}

pub(super) fn normalized_claude_currency(value: Option<String>) -> Option<String> {
    let value = value?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_ascii_uppercase())
}
