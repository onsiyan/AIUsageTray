//! Anthropic Admin API source: organization usage totals.

use super::*;

impl ClaudeUsageAdapter {
    pub(super) async fn get_admin(
        &self,
        path: &str,
        api_key: &str,
        start: chrono::DateTime<Utc>,
        end: chrono::DateTime<Utc>,
        group_by: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let mut url = self
            .oauth_base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair(
                "starting_at",
                &start.to_rfc3339_opts(SecondsFormat::Secs, true),
            )
            .append_pair("ending_at", &end.to_rfc3339_opts(SecondsFormat::Secs, true))
            .append_pair("bucket_width", "1d")
            .append_pair("limit", "31")
            .append_pair("group_by[]", group_by);
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
                    ("x-api-key".to_owned(), api_key.to_owned()),
                    ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }
}

impl ClaudeUsageAdapter {
    pub(super) async fn probe_admin(
        &self,
        account: &AccountRecord,
        api_key: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        let end = Utc::now();
        let start = end - Duration::days(30);
        let costs = self
            .get_admin(
                "v1/organizations/cost_report",
                api_key,
                start,
                end,
                "description",
            )
            .await?;
        if !costs.is_success() {
            return Ok(map_claude_http_error(&costs, "Claude Admin API"));
        }
        let messages = self
            .get_admin(
                "v1/organizations/usage_report/messages",
                api_key,
                start,
                end,
                "model",
            )
            .await?;
        if !messages.is_success() {
            return Ok(map_claude_http_error(&messages, "Claude Admin API"));
        }
        let mut totals = parse_admin_usage(&costs.body).ok_or_else(|| {
            TransportError::Serialization("Claude Admin API cost report was invalid".to_owned())
        })?;
        if let Some(message_totals) = parse_admin_usage(&messages.body) {
            totals.merge_messages(message_totals);
        }
        let now = Utc::now();
        let mut metrics = Vec::new();
        if let Some(cost_usd) = totals.cost_usd {
            metrics.push(UsageMetric {
                key: "admin-cost-30d".to_owned(),
                name: "API spend (30d)".to_owned(),
                used_percent: None,
                used_amount: Some(cost_usd),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("USD".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata: HashMap::new(),
            });
        }
        if totals.total_tokens > 0 {
            metrics.push(UsageMetric {
                key: "admin-tokens-30d".to_owned(),
                name: "Tokens (30d)".to_owned(),
                used_percent: None,
                used_amount: Some(totals.total_tokens as f64),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("tokens".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata: HashMap::new(),
            });
        }
        for model in totals.models {
            let mut metadata = HashMap::new();
            metadata.insert("input_tokens".to_owned(), model.input_tokens.to_string());
            metadata.insert(
                "cache_creation_tokens".to_owned(),
                model.cache_creation_tokens.to_string(),
            );
            metadata.insert(
                "cache_read_tokens".to_owned(),
                model.cache_read_tokens.to_string(),
            );
            metadata.insert("output_tokens".to_owned(), model.output_tokens.to_string());
            metrics.push(UsageMetric {
                key: format!("admin-model-{}", slugify(&model.name)),
                name: model.name,
                used_percent: None,
                used_amount: Some(model.total_tokens as f64),
                limit_amount: None,
                remaining_amount: None,
                unit: Some("tokens".to_owned()),
                reset_at_utc: None,
                reset_label: None,
                metadata,
            });
        }
        let spend = totals.cost_usd.map(|cost_usd| SpendSnapshot {
            monthly_usage: Some(cost_usd),
            monthly_limit: None,
            used_percent: None,
            limit_enabled: Some(false),
            currency_code: Some("USD".to_owned()),
        });
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: account.provider_account_id.clone(),
            plan_type: Some("Admin API".to_owned()),
            primary: None,
            primary_window_kind: None,
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: None,
            credit_inventory: None,
            spend,
            observed_email: Some(account.email.clone()),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: CLAUDE.to_owned(),
            source: Some("admin-api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        Ok(UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: Some(account.email.clone()),
                provider_account_id: account.provider_account_id.clone(),
                plan_type: Some("Admin API".to_owned()),
            }),
        ))
    }
}

#[derive(Debug, Default)]
pub(super) struct AdminUsageTotals {
    pub(super) cost_usd: Option<f64>,
    pub(super) total_tokens: u64,
    pub(super) models: Vec<AdminModelTotals>,
}

#[derive(Debug)]
pub(super) struct AdminModelTotals {
    pub(super) name: String,
    pub(super) input_tokens: u64,
    pub(super) cache_creation_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) total_tokens: u64,
}

impl AdminUsageTotals {
    fn merge_messages(&mut self, messages: Self) {
        self.total_tokens = self.total_tokens.saturating_add(messages.total_tokens);
        self.models.extend(messages.models);
    }
}

pub(super) fn parse_admin_usage(body: &str) -> Option<AdminUsageTotals> {
    let root: Value = serde_json::from_str(body).ok()?;
    let buckets = root.get("data")?.as_array()?;
    let mut totals = AdminUsageTotals::default();
    let mut models = HashMap::<String, AdminModelTotals>::new();
    for bucket in buckets {
        // An empty day bucket must not invalidate the whole 30-day report.
        let Some(results) = bucket.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            if let Some(amount) = json_number(result, &["amount"]) {
                totals.cost_usd = Some(totals.cost_usd.unwrap_or_default() + amount / 100.0);
            }
            let input = json_u64(result, &["uncached_input_tokens"]);
            let cache_creation = result
                .get("cache_creation")
                .map(|value| {
                    json_u64(value, &["ephemeral_1h_input_tokens"])
                        .saturating_add(json_u64(value, &["ephemeral_5m_input_tokens"]))
                })
                .unwrap_or_default();
            let cache_read = json_u64(result, &["cache_read_input_tokens"]);
            let output = json_u64(result, &["output_tokens"]);
            let total = input
                .saturating_add(cache_creation)
                .saturating_add(cache_read)
                .saturating_add(output);
            if total > 0 {
                totals.total_tokens = totals.total_tokens.saturating_add(total);
                let name =
                    json_string(result, &["model"]).unwrap_or_else(|| "Claude API".to_owned());
                let model = models
                    .entry(name.clone())
                    .or_insert_with(|| AdminModelTotals {
                        name,
                        input_tokens: 0,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        output_tokens: 0,
                        total_tokens: 0,
                    });
                model.input_tokens = model.input_tokens.saturating_add(input);
                model.cache_creation_tokens =
                    model.cache_creation_tokens.saturating_add(cache_creation);
                model.cache_read_tokens = model.cache_read_tokens.saturating_add(cache_read);
                model.output_tokens = model.output_tokens.saturating_add(output);
                model.total_tokens = model.total_tokens.saturating_add(total);
            }
        }
    }
    totals.models = models.into_values().collect();
    Some(totals)
}

pub(super) fn json_u64(value: &Value, names: &[&str]) -> u64 {
    names
        .iter()
        .find_map(|name| {
            value.get(*name).and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
                    .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
            })
        })
        .unwrap_or_default()
}

pub(super) fn normalize_claude_admin_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let token = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();
    token
        .to_ascii_lowercase()
        .starts_with("sk-ant-admin")
        .then_some(token.to_owned())
}
