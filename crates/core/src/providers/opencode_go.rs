//! OpenCode Go usage sources.
//!
//! The provider intentionally mirrors the source separation used by the
//! reference implementation: the Zen API is authoritative for API keys, the
//! signed-in console is authoritative for browser sessions, and the local
//! SQLite history is a device-local estimate that can enrich (but never
//! replace) account usage.

use crate::{
    accounts::{AccountRecord, OPENCODE_GO, VerifiedIdentity},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError},
    providers::{
        opencode_go_local::{
            OpenCodeGoLocalUsage, OpenCodeGoLocalUsageError, OpenCodeGoLocalUsageReader,
        },
        shared::{
            bearer_headers, invalid_payload, json_number, json_string, map_http_error, missing_auth,
        },
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, CreditLimitSnapshot, CreditsSnapshot, RateLimitWindow,
        SpendSnapshot, UsageAdapter, UsageAdapterError, UsageAdapterErrorCode, UsageMetric,
        UsageProbeResult, UsageSnapshot, UsageSourceDiagnostic, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use regex::Regex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, env, sync::Arc, time::Duration as StdDuration};
use tokio::task::JoinHandle;
use url::Url;

const API_USAGE_PATH: &str = "zen/go/v1/usage";
const CONSOLE_ORGS_PATH: &str = "console/api/orgs";
const CONSOLE_STATUS_PATH: &str = "console/api/go/status";
const CONSOLE_BILLING_PATH: &str = "console/api/billing/status";
const MICRO_CENTS_PER_USD: f64 = 100_000_000.0;
const LEGACY_SERVER_PATH: &str = "_server";
const WORKSPACES_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
const BILLING_SERVER_ID: &str = "c83b78a614689c38ebee981f9b39a8b377716db85c1fd7dbab604adc02d3313d";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const LOCAL_SOURCE: &str = "local-estimate";
const MAX_OPEN_CODE_REDIRECTS: usize = 10;
const CONSOLE_BILLING_OPTIONAL_JOIN_TIMEOUT: StdDuration = StdDuration::from_millis(250);
const CONSOLE_BILLING_REQUIRED_TIMEOUT: StdDuration = StdDuration::from_secs(5);

type ConsoleBillingTask = JoinHandle<Result<UsageHttpResponse, TransportError>>;
mod balance;
mod console;
mod snapshot;

use balance::*;
use console::*;
use snapshot::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OpenCodeGoSourceMode {
    /// Local estimate + API for unscoped accounts; web first for cookie-scoped
    /// accounts, then local history and API as fallbacks.
    #[default]
    Automatic,
    /// Require the Zen Go API and a bearer key. No browser or local fallback.
    Api,
    /// Require a browser session and use console/web endpoints only.
    Web,
}

pub struct OpenCodeGoUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    base_url: Url,
    source_mode: OpenCodeGoSourceMode,
    local_reader: OpenCodeGoLocalUsageReader,
}

#[derive(Debug, Clone)]
struct ParsedWindow {
    window: RateLimitWindow,
    used_amount: Option<f64>,
    limit_amount: Option<f64>,
}

impl OpenCodeGoUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            base_url: Url::parse("https://opencode.ai/")
                .map_err(|error| TransportError::InvalidUrl(error.to_string()))?,
            source_mode: OpenCodeGoSourceMode::Automatic,
            local_reader: OpenCodeGoLocalUsageReader::from_process(),
        })
    }

    pub fn with_source_mode(mut self, source_mode: OpenCodeGoSourceMode) -> Self {
        self.source_mode = source_mode;
        self
    }

    pub fn with_local_reader(mut self, local_reader: OpenCodeGoLocalUsageReader) -> Self {
        self.local_reader = local_reader;
        self
    }

    async fn request(
        &self,
        path: &str,
        material: &AccountAuthMaterial,
        extra_headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<UsageHttpResponse, TransportError> {
        let request = self.build_request(path, material, extra_headers)?;
        send_with_guarded_redirects(self.transport.as_ref(), request).await
    }

    fn build_request(
        &self,
        path: &str,
        material: &AccountAuthMaterial,
        extra_headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<UsageHttpRequest, TransportError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let mut headers = bearer_headers(material, USER_AGENT);
        for (name, value) in extra_headers {
            headers.insert(name, value);
        }
        Ok(UsageHttpRequest {
            method: Method::GET,
            url,
            headers,
            body: None,
        })
    }

    async fn probe_api(
        &self,
        account: &AccountRecord,
        material: &AccountAuthMaterial,
    ) -> Result<UsageProbeResult, TransportError> {
        let Some(token) = material
            .bearer_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(missing_auth("OpenCode Go API"));
        };
        let response = self
            .request(
                API_USAGE_PATH,
                material,
                [("Authorization".to_owned(), format!("Bearer {token}"))],
            )
            .await?;
        if !response.is_success() {
            return Ok(map_http_error(&response, "OpenCode Go API"));
        }
        let root = parse_json_document(&response.body)
            .map_err(|error| TransportError::Serialization(error.to_string()))?;
        Ok(parse_api_snapshot(&root, account))
    }

    fn local_snapshot(
        &self,
        account: &AccountRecord,
        local: &OpenCodeGoLocalUsage,
    ) -> UsageProbeResult {
        let mut metrics = local.metrics.clone();
        metrics.push(window_metric("local-5h", &local.primary, None, None));
        metrics.push(window_metric("local-weekly", &local.secondary, None, None));
        metrics.push(window_metric(
            "local-monthly",
            &local.monthly.window,
            None,
            None,
        ));
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: Utc::now(),
            response_account_id: account.provider_account_id.clone(),
            plan_type: None,
            primary: Some(local.primary.clone()),
            primary_window_kind: None,
            primary_window_is_synthetic: false,
            secondary: Some(local.secondary.clone()),
            additional_windows: vec![local.monthly.clone()],
            credits: None,
            credit_inventory: None,
            spend: Some(local.spend.clone()),
            observed_email: (!account.email.trim().is_empty()).then(|| account.email.clone()),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: vec![],
            provider_id: OPENCODE_GO.to_owned(),
            source: Some(LOCAL_SOURCE.to_owned()),
            data_confidence: "estimated".to_owned(),
        };
        UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: Some(account.email.clone()),
                provider_account_id: account.provider_account_id.clone(),
                plan_type: None,
            }),
        )
    }
}

#[async_trait]
impl UsageAdapter for OpenCodeGoUsageAdapter {
    fn adapter_id(&self) -> &str {
        OPENCODE_GO
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) => return Ok(missing_auth("OpenCode Go")),
            Err(AuthError::ReauthenticationRequired(_)) => return Ok(missing_auth("OpenCode Go")),
            Err(error) => return Ok(invalid_payload("OpenCode Go", error.to_string())),
        };

        match self.source_mode {
            OpenCodeGoSourceMode::Api => return self.probe_api(account, &material).await,
            OpenCodeGoSourceMode::Web => return self.probe_web(account, &material).await,
            OpenCodeGoSourceMode::Automatic
                if material
                    .oauth_refresh_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty()) =>
            {
                return self.probe_web(account, &material).await;
            }
            OpenCodeGoSourceMode::Automatic => {}
        }

        let scoped = !material.cookies.is_empty() || account.workspace_id.is_some();
        // Reading the local history scans every stored message; keep that
        // synchronous SQLite work off the async worker threads.
        let reader = self.local_reader.clone();
        let local = tokio::task::spawn_blocking(move || reader.read_from_process(Utc::now()))
            .await
            .unwrap_or_else(|error| {
                Err(OpenCodeGoLocalUsageError::HistoryUnavailable(
                    error.to_string(),
                ))
            });
        let mut diagnostics = Vec::new();
        let mut last_failure = None;

        if scoped {
            if !material.cookies.is_empty() {
                match self.probe_web(account, &material).await {
                    Ok(result) if result.succeeded() => return Ok(result),
                    Ok(result) => {
                        if let Some(error) = result.error.clone() {
                            diagnostics.push(diagnostic_from_error("web", &error));
                            last_failure = Some(result);
                        }
                    }
                    Err(error) => diagnostics.push(transport_diagnostic("web", &error)),
                }
            }
            if let Ok(local) = local.as_ref() {
                let mut result = self.local_snapshot(account, local);
                if let Some(snapshot) = result.snapshot.as_mut() {
                    snapshot.source_diagnostics = diagnostics;
                }
                return Ok(result);
            }
            if let Err(error) = local {
                diagnostics.push(diagnostic_from_local_error(&error));
            }
            if material.has_bearer_token() {
                match self.probe_api(account, &material).await {
                    Ok(mut result) if result.succeeded() => {
                        if let Some(snapshot) = result.snapshot.as_mut() {
                            snapshot.source_diagnostics.extend(diagnostics);
                        }
                        return Ok(result);
                    }
                    Ok(result) => last_failure = Some(result),
                    Err(error) => diagnostics.push(transport_diagnostic("api", &error)),
                }
            }
        } else {
            if let Ok(local) = local.as_ref() {
                if material.has_bearer_token() {
                    match self.probe_api(account, &material).await {
                        Ok(mut result) if result.succeeded() => {
                            let local_result = self.local_snapshot(account, local);
                            if let (Some(remote), Some(local_snapshot)) =
                                (result.snapshot.as_mut(), local_result.snapshot)
                            {
                                merge_local_estimate(remote, &local_snapshot);
                            }
                            return Ok(result);
                        }
                        Ok(result) => {
                            if let Some(error) = result.error.as_ref() {
                                diagnostics.push(diagnostic_from_error("api", error));
                            }
                            let mut local_result = self.local_snapshot(account, local);
                            if let Some(snapshot) = local_result.snapshot.as_mut() {
                                snapshot.source_diagnostics = diagnostics;
                            }
                            return Ok(local_result);
                        }
                        Err(error) => {
                            diagnostics.push(transport_diagnostic("api", &error));
                            let mut local_result = self.local_snapshot(account, local);
                            if let Some(snapshot) = local_result.snapshot.as_mut() {
                                snapshot.source_diagnostics = diagnostics;
                            }
                            return Ok(local_result);
                        }
                    }
                } else {
                    return Ok(self.local_snapshot(account, local));
                }
            } else if let Err(error) = local {
                diagnostics.push(diagnostic_from_local_error(&error));
            }
            if material.has_bearer_token() {
                match self.probe_api(account, &material).await {
                    Ok(mut result) if result.succeeded() => {
                        if let Some(snapshot) = result.snapshot.as_mut() {
                            snapshot.source_diagnostics.extend(diagnostics);
                        }
                        return Ok(result);
                    }
                    Ok(result) => last_failure = Some(result),
                    Err(error) => diagnostics.push(transport_diagnostic("api", &error)),
                }
            }
            if !material.cookies.is_empty() {
                match self.probe_web(account, &material).await {
                    Ok(mut result) if result.succeeded() => {
                        if let Some(snapshot) = result.snapshot.as_mut() {
                            snapshot.source_diagnostics.extend(diagnostics);
                        }
                        return Ok(result);
                    }
                    Ok(result) => last_failure = Some(result),
                    Err(error) => diagnostics.push(transport_diagnostic("web", &error)),
                }
            }
        }

        if let Some(mut result) = last_failure {
            if let Some(error) = result.error.as_ref() {
                diagnostics.push(diagnostic_from_error("opencode-go", error));
            }
            if let Some(snapshot) = result.snapshot.as_mut() {
                snapshot.source_diagnostics.extend(diagnostics);
            }
            return Ok(result);
        }
        if material.has_bearer_token() || !material.cookies.is_empty() {
            let mut result = invalid_payload("OpenCode Go", "all configured sources failed");
            if let Some(error) = result.error.as_mut() {
                let details = diagnostics
                    .iter()
                    .map(|item| item.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                if !details.is_empty() {
                    error.message.push_str(": ");
                    error.message.push_str(&details);
                }
            }
            return Ok(result);
        }
        Ok(missing_auth("OpenCode Go"))
    }
}

fn parse_api_snapshot(root: &Value, account: &AccountRecord) -> UsageProbeResult {
    let usage = root.get("usage").unwrap_or(root);
    let rolling = find_window(usage, WindowRole::Rolling).and_then(|value| {
        parse_window(
            value,
            UsageWindowKind::Primary,
            "Rolling 5 hours",
            true,
            false,
        )
    });
    let weekly = find_window(usage, WindowRole::Weekly)
        .and_then(|value| parse_window(value, UsageWindowKind::Secondary, "Weekly", true, false));
    let monthly = find_window(usage, WindowRole::Monthly)
        .and_then(|value| parse_window(value, UsageWindowKind::Additional, "Monthly", true, false));
    let renew_keys = ["renewAt", "renew_at", "renewsAt", "renews_at"];
    let renews_at = parse_reset_at(usage, Utc::now(), &renew_keys)
        .or_else(|| parse_reset_at(root, Utc::now(), &renew_keys));
    let Some(rolling) = rolling else {
        return invalid_payload("OpenCode Go API", "rolling usage was not found");
    };
    build_snapshot(
        account,
        root,
        "api",
        rolling,
        weekly,
        monthly,
        find_balance(root),
        renews_at,
        "authoritative",
        Vec::new(),
    )
}

fn http_error_code(status: u16) -> UsageAdapterErrorCode {
    match status {
        401 => UsageAdapterErrorCode::Unauthorized,
        403 => UsageAdapterErrorCode::Forbidden,
        429 => UsageAdapterErrorCode::RateLimited,
        500..=599 => UsageAdapterErrorCode::TransientHttp,
        _ => UsageAdapterErrorCode::HttpError,
    }
}

fn diagnostic(
    source: impl Into<String>,
    code: UsageAdapterErrorCode,
    message: impl Into<String>,
    status: Option<u16>,
) -> UsageSourceDiagnostic {
    UsageSourceDiagnostic {
        source: source.into(),
        code,
        message: message.into(),
        http_status_code: status,
        retry_after_seconds: None,
    }
}

fn transport_diagnostic(source: &str, error: &TransportError) -> UsageSourceDiagnostic {
    diagnostic(
        source,
        UsageAdapterErrorCode::TransientHttp,
        error.to_string(),
        None,
    )
}

fn diagnostic_from_error(
    source: &str,
    error: &crate::usage::UsageAdapterError,
) -> UsageSourceDiagnostic {
    diagnostic(
        source,
        error.code,
        error.message.clone(),
        error.http_status_code,
    )
}

fn diagnostic_from_local_error(error: &OpenCodeGoLocalUsageError) -> UsageSourceDiagnostic {
    diagnostic(
        LOCAL_SOURCE,
        UsageAdapterErrorCode::UnsupportedProvider,
        error.to_string(),
        None,
    )
}

fn add_snapshot_diagnostic(result: &mut UsageProbeResult, item: UsageSourceDiagnostic) {
    if let Some(snapshot) = result.snapshot.as_mut() {
        snapshot.source_diagnostics.push(item);
    }
}

#[cfg(test)]
mod tests;
