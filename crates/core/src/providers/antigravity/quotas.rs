//! Per-model quotas: parsing, verification merges, pools, and the model snapshot.

use super::*;

pub(super) fn should_verify_remote_quotas(quotas: &[Quota]) -> bool {
    !quotas.is_empty()
        && quotas.iter().all(|quota| {
            quota
                .remaining_fraction
                .is_some_and(|remaining| remaining >= 0.999)
        })
}

pub(super) fn has_usable_remote_quotas(quotas: &[Quota]) -> bool {
    quotas
        .iter()
        .any(|quota| quota.remaining_fraction.is_some())
}

pub(super) fn parse_remote_quota_buckets(root: &Value) -> Vec<Quota> {
    let buckets = root
        .get("buckets")
        .or_else(|| {
            root.get("response")
                .and_then(|response| response.get("buckets"))
        })
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let mut by_model = HashMap::<String, Quota>::new();

    for bucket in buckets {
        let Some(model_id) = json_string(bucket, &["modelId", "model_id", "model", "id"]) else {
            continue;
        };
        let quota_info = bucket.get("quotaInfo").or_else(|| bucket.get("quota_info"));
        let remaining =
            json_number(bucket, &["remainingFraction", "remaining_fraction"]).or_else(|| {
                quota_info.and_then(|info| {
                    json_number(info, &["remainingFraction", "remaining_fraction"])
                })
            });
        let reset = parse_date_value(
            bucket
                .get("resetTime")
                .or_else(|| bucket.get("reset_time"))
                .or_else(|| quota_info.and_then(|info| info.get("resetTime")))
                .or_else(|| quota_info.and_then(|info| info.get("reset_time"))),
        );
        let quota = to_quota(&model_id, model_id.clone(), remaining, reset);
        let key = model_id.to_ascii_lowercase();
        let replace = by_model.get(&key).is_none_or(|existing| {
            match (existing.remaining_fraction, quota.remaining_fraction) {
                (None, Some(_)) => true,
                (Some(left), Some(right)) => right < left,
                _ => false,
            }
        });
        if replace {
            by_model.insert(key, quota);
        }
    }

    let mut quotas = by_model.into_values().collect::<Vec<_>>();
    quotas.sort_by(|left, right| left.key.cmp(&right.key));
    quotas
}

pub(super) fn merge_verified_quotas(catalog: &[Quota], verified: &[Quota]) -> Vec<Quota> {
    let mut verified_by_key = verified
        .iter()
        .map(|quota| (quota.key.to_ascii_lowercase(), quota.clone()))
        .collect::<HashMap<_, _>>();
    let mut merged = catalog
        .iter()
        .filter_map(|catalog_quota| {
            let verified_quota = verified_by_key.remove(&catalog_quota.key.to_ascii_lowercase())?;
            let mut merged = catalog_quota.clone();
            if verified_quota.remaining_fraction.is_some() {
                merged.remaining_fraction = verified_quota.remaining_fraction;
                merged.used_percent = verified_quota.used_percent;
                merged.window.used_percent = verified_quota.used_percent;
            }
            if verified_quota.window.reset_at_utc.is_some() {
                merged.window.reset_at_utc = verified_quota.window.reset_at_utc;
            }
            merged.window.limit_window_seconds = verified_quota.window.limit_window_seconds;
            Some(merged)
        })
        .collect::<Vec<_>>();
    merged.extend(
        verified_by_key
            .into_values()
            .filter(|quota| quota.remaining_fraction.is_some()),
    );
    merged
}

pub(super) fn quota_metric(quota: &Quota) -> UsageMetric {
    let mut metadata = HashMap::from([
        ("source".to_owned(), "model-quota".to_owned()),
        ("model_id".to_owned(), quota.key.clone()),
    ]);
    if let Some(remaining) = quota.remaining_fraction {
        metadata.insert("remaining_fraction".to_owned(), remaining.to_string());
    }
    UsageMetric {
        key: quota.key.clone(),
        name: quota.name.clone(),
        used_percent: quota.remaining_fraction.map(|_| quota.used_percent),
        used_amount: quota.remaining_fraction.map(|_| quota.used_percent),
        limit_amount: quota.remaining_fraction.map(|_| 100.0),
        remaining_amount: quota.remaining_fraction.map(|value| value * 100.0),
        unit: quota.remaining_fraction.map(|_| "percent".to_owned()),
        reset_at_utc: quota.window.reset_at_utc,
        reset_label: None,
        metadata,
    }
}

pub(super) fn canonical_model_id(model_id: &str) -> String {
    let normalized = model_id.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "gemini-3.6-flash"
        | "gemini-3.6-flash-low"
        | "gemini-3.6-flash-medium"
        | "gemini-3.6-flash-high"
        | "gemini-3.5-flash-extra-low"
        | "gemini-3.5-flash-low"
        | "gemini-3.5-flash-mid"
        | "gemini-3.5-flash-high"
        | "gemini-3-flash-agent" => "gemini-3.7-flash".to_owned(),
        _ => model_id.to_owned(),
    }
}

pub(super) fn compare_canonical_quota_candidates(
    left: &Quota,
    right: &Quota,
) -> std::cmp::Ordering {
    let usage_order = match (
        known_remaining_fraction(left),
        known_remaining_fraction(right),
    ) {
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => left.total_cmp(&right),
        (None, None) => std::cmp::Ordering::Equal,
    };
    usage_order
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.key.cmp(&right.key))
}

pub(super) fn canonicalize_and_deduplicate_model_quotas(models: &[Quota]) -> Vec<Quota> {
    let mut by_canonical_id = BTreeMap::<String, Quota>::new();
    for model in models {
        let mut canonical = model.clone();
        canonical.key = canonical_model_id(&model.key);
        let canonical_key = canonical.key.to_ascii_lowercase();
        match by_canonical_id.get(&canonical_key) {
            Some(existing)
                if compare_canonical_quota_candidates(&canonical, existing)
                    != std::cmp::Ordering::Less => {}
            _ => {
                by_canonical_id.insert(canonical_key, canonical);
            }
        }
    }
    by_canonical_id.into_values().collect()
}

pub(super) fn snapshot_from_model_quotas(
    account: &AccountRecord,
    models: &[Quota],
    observed_email: Option<String>,
    plan_type: Option<String>,
    source: &str,
    data_confidence: &str,
) -> UsageSnapshot {
    let mut ordered = models.to_vec();
    ordered.sort_by(|left, right| {
        right
            .remaining_fraction
            .is_some()
            .cmp(&left.remaining_fraction.is_some())
            .then_with(|| right.used_percent.total_cmp(&left.used_percent))
            .then_with(|| left.key.cmp(&right.key))
    });
    let metrics = ordered.iter().map(quota_metric).collect::<Vec<_>>();
    let mut ordered = canonicalize_and_deduplicate_model_quotas(&ordered);
    ordered.sort_by(|left, right| {
        right
            .remaining_fraction
            .is_some()
            .cmp(&left.remaining_fraction.is_some())
            .then_with(|| right.used_percent.total_cmp(&left.used_percent))
            .then_with(|| left.key.cmp(&right.key))
            .then_with(|| left.name.cmp(&right.name))
    });

    let gemini_index = model_quota_representative_index(&ordered, AntigravityQuotaPool::Gemini);
    let claude_gpt_index =
        model_quota_representative_index(&ordered, AntigravityQuotaPool::ClaudeGpt);
    let local_unknown_fallback = if gemini_index.is_none()
        && claude_gpt_index.is_none()
        && matches!(source, "local-legacy" | "local-command-models")
    {
        local_unknown_model_representative_index(&ordered)
    } else {
        None
    };
    let primary_index = gemini_index.or(local_unknown_fallback);
    let primary = primary_index.map(|index| {
        let mut window = ordered[index].window.clone();
        window.kind = UsageWindowKind::Primary;
        window
    });
    let secondary = claude_gpt_index.map(|index| {
        let mut window = ordered[index].window.clone();
        window.kind = UsageWindowKind::Secondary;
        window
    });
    let is_remote = matches!(source, "api-model-catalog" | "api-verified-quota");
    let gemini_pool_quota = gemini_index.map(|index| &ordered[index]);
    let claude_gpt_pool_quota = claude_gpt_index.map(|index| &ordered[index]);
    let additional_windows = ordered
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != primary_index && Some(*index) != claude_gpt_index)
        .filter(|(_, quota)| {
            should_show_model_quota_window(
                quota,
                is_remote,
                gemini_pool_quota,
                claude_gpt_pool_quota,
            )
        })
        .map(|(_, quota)| AdditionalRateLimitWindow {
            key: quota.key.clone(),
            name: quota.name.clone(),
            window: quota.window.clone(),
        })
        .collect::<Vec<_>>();
    UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: account.workspace_id.clone(),
        plan_type,
        primary,
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary,
        additional_windows,
        credits: None,
        credit_inventory: None,
        spend: None,
        observed_email,
        is_stale: false,
        stale_reason: None,
        stale_at_utc: None,
        metrics,
        source_diagnostics: Vec::new(),
        provider_id: ANTIGRAVITY.to_owned(),
        source: Some(source.to_owned()),
        data_confidence: data_confidence.to_owned(),
    }
}

#[derive(Clone)]
pub(super) struct Quota {
    pub(super) key: String,
    pub(super) name: String,
    pub(super) remaining_fraction: Option<f64>,
    pub(super) used_percent: f64,
    pub(super) window: RateLimitWindow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AntigravityQuotaPool {
    Gemini,
    ClaudeGpt,
}

pub(super) fn model_is_summary_eligible(quota: &Quota) -> bool {
    let model_id = quota.key.to_ascii_lowercase();
    let label = quota.name.to_ascii_lowercase();
    !model_id.starts_with("tab_")
        && ![&model_id, &label].iter().any(|text| {
            text.contains("lite") || text.contains("autocomplete") || text.contains("image")
        })
}

pub(super) fn model_quota_pool(quota: &Quota) -> Option<AntigravityQuotaPool> {
    if !model_is_summary_eligible(quota) {
        return None;
    }
    model_quota_family(quota)
}

pub(super) fn model_quota_family(quota: &Quota) -> Option<AntigravityQuotaPool> {
    let model = format!("{} {}", quota.key, quota.name).to_ascii_lowercase();
    if model.contains("claude") || model.contains("gpt") || model.contains("openai") {
        Some(AntigravityQuotaPool::ClaudeGpt)
    } else if model.contains("gemini") && (model.contains("pro") || model.contains("flash")) {
        Some(AntigravityQuotaPool::Gemini)
    } else {
        None
    }
}

pub(super) fn model_quota_mirror_pool(quota: &Quota) -> Option<AntigravityQuotaPool> {
    model_quota_family(quota).or_else(|| {
        let model_id = quota.key.to_ascii_lowercase();
        let label = quota.name.to_ascii_lowercase();
        (model_id.starts_with("tab_")
            || model_id.contains("autocomplete")
            || label.contains("autocomplete"))
        .then_some(AntigravityQuotaPool::Gemini)
    })
}

pub(super) fn known_remaining_fraction(quota: &Quota) -> Option<f64> {
    quota
        .remaining_fraction
        .filter(|fraction| fraction.is_finite())
        .map(|fraction| fraction.clamp(0.0, 1.0))
}

pub(super) fn compare_quota_representatives(left: &Quota, right: &Quota) -> std::cmp::Ordering {
    let remaining_order = known_remaining_fraction(left)
        .unwrap_or(f64::INFINITY)
        .total_cmp(&known_remaining_fraction(right).unwrap_or(f64::INFINITY));
    if remaining_order != std::cmp::Ordering::Equal {
        return remaining_order;
    }
    match (
        left.window.reset_at_utc.as_ref(),
        right.window.reset_at_utc.as_ref(),
    ) {
        (Some(left_reset), Some(right_reset)) if left_reset != right_reset => {
            return left_reset.cmp(right_reset);
        }
        (Some(_), None) => return std::cmp::Ordering::Less,
        (None, Some(_)) => return std::cmp::Ordering::Greater,
        _ => {}
    }
    left.name
        .to_ascii_lowercase()
        .cmp(&right.name.to_ascii_lowercase())
        .then_with(|| left.key.cmp(&right.key))
}

pub(super) fn model_quota_representative_index(
    models: &[Quota],
    pool: AntigravityQuotaPool,
) -> Option<usize> {
    models
        .iter()
        .enumerate()
        .filter(|(_, quota)| {
            model_quota_pool(quota) == Some(pool) && known_remaining_fraction(quota).is_some()
        })
        .min_by(|(_, left), (_, right)| compare_quota_representatives(left, right))
        .map(|(index, _)| index)
}

pub(super) fn local_unknown_model_representative_index(models: &[Quota]) -> Option<usize> {
    models
        .iter()
        .enumerate()
        .filter(|(_, quota)| {
            model_is_summary_eligible(quota)
                && model_quota_pool(quota).is_none()
                && known_remaining_fraction(quota).is_some()
        })
        .min_by(|(_, left), (_, right)| compare_quota_representatives(left, right))
        .map(|(index, _)| index)
}

pub(super) fn remote_quota_mirrors_pool(quota: &Quota, pool_quota: &Quota) -> bool {
    let (Some(quota_reset), Some(pool_reset)) = (
        quota.window.reset_at_utc.as_ref(),
        pool_quota.window.reset_at_utc.as_ref(),
    ) else {
        return false;
    };
    if quota_reset != pool_reset {
        return false;
    }
    matches!(
        (quota.remaining_fraction, pool_quota.remaining_fraction),
        (Some(quota_fraction), Some(pool_fraction))
            if quota_fraction.is_finite()
                && pool_fraction.is_finite()
                && quota_fraction == pool_fraction
    )
}

pub(super) fn should_show_model_quota_window(
    quota: &Quota,
    is_remote: bool,
    gemini_pool_quota: Option<&Quota>,
    claude_gpt_pool_quota: Option<&Quota>,
) -> bool {
    let pool = model_quota_pool(quota);
    if pool.is_some() && model_is_summary_eligible(quota) {
        return false;
    }
    if is_remote {
        let represented_pool = match model_quota_mirror_pool(quota) {
            Some(AntigravityQuotaPool::Gemini) => gemini_pool_quota,
            Some(AntigravityQuotaPool::ClaudeGpt) => claude_gpt_pool_quota,
            None => None,
        };
        if represented_pool.is_some_and(|pool_quota| remote_quota_mirrors_pool(quota, pool_quota)) {
            return false;
        }
    }
    match quota.remaining_fraction {
        Some(fraction) if fraction.is_finite() => fraction.clamp(0.0, 1.0) * 100.0 < 99.9,
        Some(_) => false,
        None => quota.window.reset_at_utc.is_some(),
    }
}

pub(super) fn parse_model_quotas(root: &Value) -> Vec<Quota> {
    root.get("models")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|models| models.iter())
        .filter_map(|(key, model)| {
            let quota_info = model.get("quotaInfo").or_else(|| model.get("quota_info"))?;
            let remaining = json_number(quota_info, &["remainingFraction", "remaining_fraction"]);
            let reset = parse_date(quota_info, &["resetTime", "reset_time"]);
            let name = json_string(model, &["displayName", "display_name", "label"])
                .unwrap_or_else(|| key.clone());
            Some(to_quota(key, name, remaining, reset))
        })
        .collect()
}

pub(super) fn to_quota(
    key: &str,
    name: String,
    remaining_fraction: Option<f64>,
    reset_at_utc: Option<DateTime<Utc>>,
) -> Quota {
    let used_percent = remaining_fraction
        .map(|remaining| ((1.0 - remaining) * 100.0).clamp(0.0, 100.0))
        .unwrap_or(0.0);
    Quota {
        key: key.to_owned(),
        name: name.clone(),
        remaining_fraction,
        used_percent,
        window: RateLimitWindow {
            kind: UsageWindowKind::Additional,
            name,
            used_percent,
            reset_at_utc,
            // The model catalog exposes only the absolute reset time, not the
            // fixed window duration. Keep the duration unknown instead of
            // storing a moving countdown in this field.
            limit_window_seconds: 0,
        },
    }
}

pub(super) fn parse_date(value: &Value, names: &[&str]) -> Option<DateTime<Utc>> {
    names
        .iter()
        .find_map(|name| parse_date_value(value.get(*name)))
}
