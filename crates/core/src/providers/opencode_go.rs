//! OpenCode Go usage sources.
//!
//! An account signed in through the OpenCode console (device authorization)
//! reads its usage from the console; an OpenCode API key reads it from the
//! Zen Go API. Each source is strict: one never substitutes for the other.

use crate::{
    accounts::{AccountRecord, OPENCODE_GO, VerifiedIdentity},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError},
    providers::shared::{
        bearer_headers, invalid_payload, json_number, json_string, map_http_error, missing_auth,
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
    /// The signed-in OpenCode console (the account's console OAuth token).
    #[default]
    Web,
    /// The Zen Go API with an OpenCode API key.
    Api,
}

pub struct OpenCodeGoUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    base_url: Url,
    source_mode: OpenCodeGoSourceMode,
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
            source_mode: OpenCodeGoSourceMode::default(),
        })
    }

    pub fn with_source_mode(mut self, source_mode: OpenCodeGoSourceMode) -> Self {
        self.source_mode = source_mode;
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
            OpenCodeGoSourceMode::Api => self.probe_api(account, &material).await,
            OpenCodeGoSourceMode::Web => self.probe_web(account, &material).await,
        }
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

fn add_snapshot_diagnostic(result: &mut UsageProbeResult, item: UsageSourceDiagnostic) {
    if let Some(snapshot) = result.snapshot.as_mut() {
        snapshot.source_diagnostics.push(item);
    }
}

#[cfg(test)]
mod tests;
