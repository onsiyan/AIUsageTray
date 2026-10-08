//! DeepSeek prepaid balance through the official API.
//!
//! DeepSeek has no rolling session or weekly windows: an API key draws from
//! the account's prepaid balance. `GET /user/balance` reports that balance per
//! currency, split into topped-up (paid) and granted (promotional) funds.

use crate::{
    accounts::{AccountRecord, DEEPSEEK, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{
        bearer_headers, invalid_payload, json_bool, json_number, json_string, map_http_error,
        missing_auth,
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpTransport},
    usage::{
        CreditsSnapshot, UsageAdapter, UsageMetric, UsagePrimaryWindowKind, UsageProbeResult,
        UsageSnapshot,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Method;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};
use url::Url;

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";
const USER_AGENT: &str = "UsageMonitor/0.1";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(6);

pub struct DeepSeekUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    balance_url: Url,
    deadline: Duration,
}

impl DeepSeekUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        let balance_url = Url::parse(BALANCE_URL)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        Ok(Self {
            transport,
            auth,
            balance_url,
            deadline: DEFAULT_DEADLINE,
        })
    }

    /// Bounds the balance request, for hosts with a stricter refresh budget.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

#[async_trait]
impl UsageAdapter for DeepSeekUsageAdapter {
    fn adapter_id(&self) -> &str {
        DEEPSEEK
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => {
                return Ok(missing_auth("DeepSeek"));
            }
            Err(error) => return Ok(invalid_payload("DeepSeek", error.to_string())),
        };
        if material
            .bearer_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        {
            return Ok(missing_auth("DeepSeek"));
        }

        let response = tokio::time::timeout(
            self.deadline,
            self.transport.send(UsageHttpRequest {
                method: Method::GET,
                url: self.balance_url.clone(),
                headers: bearer_headers(&material, USER_AGENT),
                body: None,
            }),
        )
        .await
        .map_err(|_| TransportError::Timeout("balance".to_owned()))??;
        if !response.is_success() {
            return Ok(map_http_error(&response, "DeepSeek"));
        }
        let balance = match parse_balance(&response.body) {
            Ok(balance) => balance,
            Err(reason) => return Ok(invalid_payload("DeepSeek", reason)),
        };

        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: Utc::now(),
            response_account_id: None,
            plan_type: None,
            primary: None,
            primary_window_kind: Some(UsagePrimaryWindowKind::Spend),
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: Some(balance.credits()),
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: balance.metrics(),
            source_diagnostics: Vec::new(),
            provider_id: DEEPSEEK.to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        // The balance endpoint does not say who owns the key, so the account
        // keeps the label it was added with.
        let identity = VerifiedIdentity {
            email: None,
            provider_account_id: None,
            plan_type: None,
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

/// The balance in the one currency shown for the account.
#[derive(Debug, Clone, PartialEq)]
struct DeepSeekBalance {
    currency: String,
    total: f64,
    topped_up: f64,
    granted: f64,
    /// Whether DeepSeek accepts API calls against this balance.
    is_available: bool,
}

impl DeepSeekBalance {
    fn credits(&self) -> CreditsSnapshot {
        CreditsSnapshot {
            has_credits: Some(self.total > 0.0),
            unlimited: Some(false),
            balance: Some(self.total),
            currency_code: Some(self.currency.clone()),
            approximate_message_cost: None,
            limit: None,
            balance_read_succeeded: Some(true),
            credits_available: Some(self.is_available && self.total > 0.0),
        }
    }

    fn metrics(&self) -> Vec<UsageMetric> {
        let amount = |key: &str, name: &str, value: f64| UsageMetric {
            key: key.to_owned(),
            name: name.to_owned(),
            used_percent: None,
            used_amount: None,
            limit_amount: None,
            remaining_amount: Some(value),
            unit: Some(self.currency.clone()),
            reset_at_utc: None,
            reset_label: None,
            metadata: HashMap::new(),
        };
        let mut metrics = vec![amount("balance", "Balance", self.total)];
        // Without granted funds the paid amount equals the balance; showing
        // it again would only repeat the same number.
        if self.granted > 0.0 {
            metrics.push(amount("balance.topped_up", "Paid", self.topped_up));
            metrics.push(amount("balance.granted", "Granted", self.granted));
        }
        let notice = if self.total <= 0.0 {
            Some(("balance.empty", "Add credits at platform.deepseek.com"))
        } else if !self.is_available {
            Some(("balance.unavailable", "Balance unavailable for API calls"))
        } else {
            None
        };
        if let Some((key, name)) = notice {
            metrics.push(UsageMetric {
                remaining_amount: None,
                unit: None,
                ..amount(key, name, 0.0)
            });
        }
        metrics
    }
}

/// Picks the one currency to show for mixed accounts: a
/// funded USD balance, then any funded balance, then USD, then
/// whatever is listed first.
fn parse_balance(body: &str) -> Result<DeepSeekBalance, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| format!("balance JSON could not be parsed: {error}"))?;
    let infos = root
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or_else(|| "balance response is missing balance_infos".to_owned())?;
    let balances = infos
        .iter()
        .filter_map(|info| {
            let currency = json_string(info, &["currency"])?
                .trim()
                .to_ascii_uppercase();
            let total = finite(json_number(info, &["total_balance"]))?;
            Some((
                currency,
                total,
                finite(json_number(info, &["topped_up_balance"])).unwrap_or(0.0),
                finite(json_number(info, &["granted_balance"])).unwrap_or(0.0),
            ))
        })
        .collect::<Vec<_>>();
    let is_usd = |entry: &&(String, f64, f64, f64)| entry.0 == "USD";
    let funded = |entry: &&(String, f64, f64, f64)| entry.1 > 0.0;
    let chosen = balances
        .iter()
        .find(|entry| is_usd(entry) && funded(entry))
        .or_else(|| balances.iter().find(funded))
        .or_else(|| balances.iter().find(is_usd))
        .or_else(|| balances.first())
        .ok_or_else(|| "balance response lists no balance".to_owned())?;
    let (currency, total, topped_up, granted) = chosen.clone();
    Ok(DeepSeekBalance {
        currency,
        total,
        topped_up,
        granted,
        is_available: json_bool(&root, &["is_available"]).unwrap_or(true),
    })
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AccountAuthMaterial, transport::UsageHttpResponse, usage::UsageAdapterErrorCode,
    };
    use std::{collections::BTreeMap, sync::Mutex};

    #[test]
    fn a_funded_usd_balance_wins_over_other_currencies() {
        let balance = parse_balance(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"CNY","total_balance":"50.00","granted_balance":"0.00","topped_up_balance":"50.00"},
                {"currency":"USD","total_balance":"12.34","granted_balance":"2.00","topped_up_balance":"10.34"}]}"#,
        )
        .unwrap();
        assert_eq!(balance.currency, "USD");
        assert_eq!(balance.total, 12.34);
        assert_eq!(balance.topped_up, 10.34);
        assert_eq!(balance.granted, 2.0);
        assert!(balance.is_available);
    }

    #[test]
    fn an_empty_usd_balance_yields_to_a_funded_one() {
        let balance = parse_balance(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"USD","total_balance":"0.00","granted_balance":"0.00","topped_up_balance":"0.00"},
                {"currency":"CNY","total_balance":"8.50","granted_balance":"0.00","topped_up_balance":"8.50"}]}"#,
        )
        .unwrap();
        assert_eq!(balance.currency, "CNY");
        assert_eq!(balance.total, 8.5);
    }

    #[test]
    fn with_nothing_funded_usd_is_shown_and_the_user_is_told_to_top_up() {
        let balance = parse_balance(
            r#"{"is_available":false,"balance_infos":[
                {"currency":"CNY","total_balance":"0","granted_balance":"0","topped_up_balance":"0"},
                {"currency":"USD","total_balance":"0","granted_balance":"0","topped_up_balance":"0"}]}"#,
        )
        .unwrap();
        assert_eq!(balance.currency, "USD");
        let metrics = balance.metrics();
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[1].key, "balance.empty");
        assert_eq!(balance.credits().credits_available, Some(false));
    }

    #[test]
    fn granted_funds_are_broken_out_and_an_unusable_balance_is_flagged() {
        let balance = DeepSeekBalance {
            currency: "USD".to_owned(),
            total: 5.0,
            topped_up: 3.0,
            granted: 2.0,
            is_available: false,
        };
        let keys = balance
            .metrics()
            .into_iter()
            .map(|metric| metric.key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                "balance",
                "balance.topped_up",
                "balance.granted",
                "balance.unavailable"
            ]
        );
    }

    #[test]
    fn a_response_without_balances_is_rejected() {
        assert!(parse_balance(r#"{"is_available":true}"#).is_err());
        assert!(parse_balance(r#"{"balance_infos":[]}"#).is_err());
        assert!(parse_balance("not json").is_err());
    }

    struct KeyAuth;

    #[async_trait]
    impl AccountAuthMaterialProvider for KeyAuth {
        async fn get(
            &self,
            _account: &AccountRecord,
        ) -> Result<Option<AccountAuthMaterial>, AuthError> {
            Ok(Some(AccountAuthMaterial {
                bearer_token: Some("sk-test".to_owned()),
                ..AccountAuthMaterial::default()
            }))
        }
    }

    struct CannedTransport {
        status_code: u16,
        body: &'static str,
        requests: Mutex<Vec<UsageHttpRequest>>,
    }

    #[async_trait]
    impl UsageHttpTransport for CannedTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            Ok(UsageHttpResponse {
                status_code: self.status_code,
                body: self.body.to_owned(),
                headers: BTreeMap::new(),
            })
        }
    }

    fn probe(status_code: u16, body: &'static str) -> (UsageProbeResult, Vec<UsageHttpRequest>) {
        let transport = Arc::new(CannedTransport {
            status_code,
            body,
            requests: Mutex::new(Vec::new()),
        });
        let adapter = DeepSeekUsageAdapter::new(transport.clone(), Arc::new(KeyAuth)).unwrap();
        let account = AccountRecord::create("ds", "ds@example.com", None, DEEPSEEK, None).unwrap();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(adapter.probe(&account))
            .unwrap();
        let requests = std::mem::take(&mut *transport.requests.lock().unwrap());
        (result, requests)
    }

    #[test]
    fn the_probe_reads_the_official_balance_endpoint_with_the_key() {
        let (result, requests) = probe(
            200,
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"7.25","granted_balance":"0.00","topped_up_balance":"7.25"}]}"#,
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.as_str(), BALANCE_URL);
        assert_eq!(
            requests[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer sk-test")
        );
        let snapshot = result.snapshot.expect("balance snapshot");
        assert_eq!(snapshot.provider_id, DEEPSEEK);
        let credits = snapshot.credits.unwrap();
        assert_eq!(credits.balance, Some(7.25));
        assert_eq!(credits.currency_code.as_deref(), Some("USD"));
        assert_eq!(snapshot.metrics.len(), 1);
    }

    #[test]
    fn a_rejected_key_is_reported_as_unauthorized() {
        let (result, _) = probe(401, r#"{"error":{"message":"Authentication Fails"}}"#);
        assert!(result.snapshot.is_none());
        assert_eq!(
            result.error.unwrap().code,
            UsageAdapterErrorCode::Unauthorized
        );
    }
}
