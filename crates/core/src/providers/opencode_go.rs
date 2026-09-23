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
use std::{collections::HashMap, env, sync::Arc};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenCodeGoSourceMode {
    /// Local estimate + API for unscoped accounts; web first for cookie-scoped
    /// accounts, then local history and API as fallbacks.
    Automatic,
    /// Require the Zen Go API and a bearer key. No browser or local fallback.
    Api,
    /// Require a browser session and use console/web endpoints only.
    Web,
}

impl Default for OpenCodeGoSourceMode {
    fn default() -> Self {
        Self::Automatic
    }
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
        let url = self
            .base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let mut headers = bearer_headers(material, USER_AGENT);
        for (name, value) in extra_headers {
            headers.insert(name, value);
        }
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers,
                body: None,
            })
            .await
    }

    async fn request_server(
        &self,
        server_id: &str,
        args: Option<&str>,
        material: &AccountAuthMaterial,
        referer: &str,
    ) -> Result<UsageHttpResponse, TransportError> {
        self.request_server_method(server_id, args, material, referer, Method::GET)
            .await
    }

    async fn request_server_method(
        &self,
        server_id: &str,
        args: Option<&str>,
        material: &AccountAuthMaterial,
        referer: &str,
        method: Method,
    ) -> Result<UsageHttpResponse, TransportError> {
        let mut url = self
            .base_url
            .join(LEGACY_SERVER_PATH)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        if method == Method::GET {
            let mut query = url.query_pairs_mut();
            query.append_pair("id", server_id);
            if let Some(args) = args.filter(|value| !value.is_empty()) {
                query.append_pair("args", args);
            }
        }
        let headers = [
            ("X-Server-Id".to_owned(), server_id.to_owned()),
            (
                "X-Server-Instance".to_owned(),
                format!("server-fn:{}", uuid::Uuid::new_v4()),
            ),
            (
                "Origin".to_owned(),
                self.base_url.origin().ascii_serialization(),
            ),
            ("Referer".to_owned(), referer.to_owned()),
            (
                "Accept".to_owned(),
                "text/javascript, application/json;q=0.9, */*;q=0.8".to_owned(),
            ),
        ];
        let mut request_headers = bearer_headers(material, USER_AGENT);
        request_headers.extend(headers);
        let body = if method == Method::GET {
            None
        } else {
            request_headers.insert("Content-Type".to_owned(), "application/json".to_owned());
            args.map(str::to_owned)
        };
        self.transport
            .send(UsageHttpRequest {
                method,
                url,
                headers: request_headers,
                body,
            })
            .await
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

    async fn probe_web(
        &self,
        account: &AccountRecord,
        material: &AccountAuthMaterial,
    ) -> Result<UsageProbeResult, TransportError> {
        if material.cookie_header().is_none() {
            return Ok(missing_auth("OpenCode Go web"));
        }

        let mut diagnostics = Vec::new();
        let workspace = if let Some(workspace) = account
            .workspace_id
            .as_deref()
            .and_then(normalize_workspace_id)
        {
            Some(workspace)
        } else if let Some(workspace) = env::var("OPENCODE_GO_WORKSPACE_ID")
            .ok()
            .and_then(|value| normalize_workspace_id(&value))
        {
            Some(workspace)
        } else {
            match self.discover_workspace_id(material).await {
                Ok(Some(workspace)) => Some(workspace),
                Ok(None) => {
                    diagnostics.push(diagnostic(
                        "web.workspace",
                        UsageAdapterErrorCode::InvalidPayload,
                        "no OpenCode workspace id was found",
                        None,
                    ));
                    None
                }
                Err(error) => {
                    diagnostics.push(transport_diagnostic("web.workspace", &error));
                    None
                }
            }
        };

        let mut no_go_subscription = false;
        if let Some(workspace) = workspace.as_deref() {
            match self.fetch_console_status(material, workspace).await {
                Ok(response) if response.is_success() => {
                    if let Ok(root) = parse_json_document(&response.body) {
                        if root.is_null() || root.get("access").is_some_and(Value::is_null) {
                            no_go_subscription = true;
                            diagnostics.push(diagnostic(
                                "web.console.status",
                                UsageAdapterErrorCode::NoSubscription,
                                "OpenCode reports no active Go subscription",
                                Some(response.status_code),
                            ));
                            match self.fetch_console_billing(material, workspace).await {
                                Ok(billing) if billing.is_success() => {
                                    if let Ok(billing_root) = parse_json_document(&billing.body) {
                                        if let Some(balance) =
                                            find_console_billing_balance(&billing_root)
                                                .or_else(|| find_balance(&billing_root))
                                        {
                                            let mut result = balance_only_snapshot(
                                                account,
                                                workspace,
                                                balance,
                                                "web-console",
                                            );
                                            if let Some(snapshot) = result.snapshot.as_mut() {
                                                snapshot
                                                    .source_diagnostics
                                                    .append(&mut diagnostics);
                                            }
                                            return Ok(result);
                                        }
                                        diagnostics.push(diagnostic(
                                            "web.console.billing",
                                            UsageAdapterErrorCode::InvalidPayload,
                                            format!(
                                                "console billing had no recognized balance: {}",
                                                json_shape_summary(&billing_root)
                                            ),
                                            Some(billing.status_code),
                                        ));
                                    } else {
                                        diagnostics.push(diagnostic(
                                            "web.console.billing",
                                            UsageAdapterErrorCode::InvalidPayload,
                                            "console billing response was not valid JSON",
                                            Some(billing.status_code),
                                        ));
                                    }
                                }
                                Ok(billing) => diagnostics.push(diagnostic(
                                    "web.console.billing",
                                    http_error_code(billing.status_code),
                                    format!(
                                        "OpenCode console billing request failed (HTTP {})",
                                        billing.status_code
                                    ),
                                    Some(billing.status_code),
                                )),
                                Err(error) => diagnostics
                                    .push(transport_diagnostic("web.console.billing", &error)),
                            }
                        } else if let Some(mut result) =
                            parse_console_snapshot(&root, account, workspace)
                        {
                            if let Some(snapshot) = result.snapshot.as_mut() {
                                snapshot.source_diagnostics.append(&mut diagnostics);
                            }
                            match self.fetch_console_billing(material, workspace).await {
                                Ok(billing) if billing.is_success() => {
                                    if let Ok(root) = parse_json_document(&billing.body) {
                                        let balance = find_console_billing_balance(&root)
                                            .or_else(|| find_balance(&root));
                                        enrich_balance(&mut result, balance, &root);
                                    } else {
                                        add_snapshot_diagnostic(
                                            &mut result,
                                            diagnostic(
                                                "web.console.billing",
                                                UsageAdapterErrorCode::InvalidPayload,
                                                "billing response was not valid JSON",
                                                Some(billing.status_code),
                                            ),
                                        );
                                    }
                                }
                                Ok(billing) => add_snapshot_diagnostic(
                                    &mut result,
                                    diagnostic(
                                        "web.console.billing",
                                        http_error_code(billing.status_code),
                                        "OpenCode console billing request failed",
                                        Some(billing.status_code),
                                    ),
                                ),
                                Err(error) => add_snapshot_diagnostic(
                                    &mut result,
                                    transport_diagnostic("web.console.billing", &error),
                                ),
                            }
                            if result.succeeded() {
                                return Ok(result);
                            }
                        } else {
                            diagnostics.push(diagnostic(
                                "web.console.status",
                                UsageAdapterErrorCode::InvalidPayload,
                                console_status_shape_error(&root),
                                Some(response.status_code),
                            ));
                        }
                    } else {
                        diagnostics.push(diagnostic(
                            "web.console.status",
                            UsageAdapterErrorCode::InvalidPayload,
                            "console status response was not valid JSON",
                            Some(response.status_code),
                        ));
                    }
                }
                Ok(response) => diagnostics.push(diagnostic(
                    "web.console.status",
                    http_error_code(response.status_code),
                    format!(
                        "OpenCode console status request failed (HTTP {})",
                        response.status_code
                    ),
                    Some(response.status_code),
                )),
                Err(error) => diagnostics.push(transport_diagnostic("web.console.status", &error)),
            }

            if !no_go_subscription {
                let page_path = format!("workspace/{workspace}/go");
                match self
                    .request(
                        &page_path,
                        material,
                        web_headers(&self.base_url, &page_path),
                    )
                    .await
                {
                    Ok(response) if response.is_success() => {
                        if let Some(mut result) = parse_web_page(&response.body, account, workspace)
                        {
                            if let Some(snapshot) = result.snapshot.as_mut() {
                                snapshot.source_diagnostics.extend(diagnostics.clone());
                            }
                            if result.succeeded() {
                                return Ok(result);
                            }
                        }
                        diagnostics.push(diagnostic(
                            "web.dashboard",
                            UsageAdapterErrorCode::InvalidPayload,
                            "OpenCode Go dashboard did not contain usage fields",
                            Some(response.status_code),
                        ));
                    }
                    Ok(response) => diagnostics.push(diagnostic(
                        "web.dashboard",
                        http_error_code(response.status_code),
                        format!(
                            "OpenCode Go dashboard request failed (HTTP {})",
                            response.status_code
                        ),
                        Some(response.status_code),
                    )),
                    Err(error) => diagnostics.push(transport_diagnostic("web.dashboard", &error)),
                }
            }

            let args = serde_json::to_string(&[workspace]).unwrap_or_else(|_| "[]".to_owned());
            match self
                .request_server(
                    BILLING_SERVER_ID,
                    Some(&args),
                    material,
                    &format!("{}console/{workspace}/go", self.base_url),
                )
                .await
            {
                Ok(response) if response.is_success() => {
                    if let Ok(root) = parse_json_document(&response.body) {
                        if let Some(balance) =
                            find_legacy_billing_balance(&root).or_else(|| find_balance(&root))
                        {
                            let mut result =
                                balance_only_snapshot(account, workspace, balance, "web-legacy");
                            if let Some(snapshot) = result.snapshot.as_mut() {
                                snapshot.source_diagnostics = diagnostics;
                            }
                            return Ok(result);
                        }
                    }
                    diagnostics.push(diagnostic(
                        "web.legacy.billing",
                        UsageAdapterErrorCode::InvalidPayload,
                        "legacy billing response did not contain a balance",
                        Some(response.status_code),
                    ));
                }
                Ok(response) => diagnostics.push(diagnostic(
                    "web.legacy.billing",
                    http_error_code(response.status_code),
                    "legacy billing request failed",
                    Some(response.status_code),
                )),
                Err(error) => diagnostics.push(transport_diagnostic("web.legacy.billing", &error)),
            }
        }

        let mut result = if no_go_subscription {
            UsageProbeResult::failure(UsageAdapterError {
                code: UsageAdapterErrorCode::NoSubscription,
                message: "No OpenCode Go subscription or supported prepaid balance is available."
                    .to_owned(),
                http_status_code: None,
                retry_after_seconds: None,
            })
        } else {
            invalid_payload(
                "OpenCode Go web",
                "no authoritative usage payload was found",
            )
        };
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
        Ok(result)
    }

    async fn fetch_console_status(
        &self,
        material: &AccountAuthMaterial,
        workspace: &str,
    ) -> Result<UsageHttpResponse, TransportError> {
        self.request(
            CONSOLE_STATUS_PATH,
            material,
            [("x-org-id".to_owned(), workspace.to_owned())],
        )
        .await
    }

    async fn fetch_console_billing(
        &self,
        material: &AccountAuthMaterial,
        workspace: &str,
    ) -> Result<UsageHttpResponse, TransportError> {
        self.request(
            CONSOLE_BILLING_PATH,
            material,
            [("x-org-id".to_owned(), workspace.to_owned())],
        )
        .await
    }

    async fn discover_workspace_id(
        &self,
        material: &AccountAuthMaterial,
    ) -> Result<Option<String>, TransportError> {
        let response = self.request(CONSOLE_ORGS_PATH, material, []).await?;
        if response.is_success() {
            if let Ok(root) = parse_json_document(&response.body) {
                if let Some(workspace) = find_console_workspace_id(&root) {
                    return Ok(Some(workspace));
                }
            }
        }

        let legacy = self
            .request_server(
                WORKSPACES_SERVER_ID,
                None,
                material,
                &self.base_url.to_string(),
            )
            .await?;
        if legacy.is_success() {
            if let Ok(root) = parse_json_document(&legacy.body) {
                if let Some(workspace) = find_workspace_id(&root) {
                    return Ok(Some(workspace));
                }
            }
            if let Some(workspace) = find_workspace_in_text(&legacy.body) {
                return Ok(Some(workspace));
            }
        }

        // The current server function accepts GET, while older deployments
        // only expose the same workspace function through a JSON POST.
        let legacy_post = self
            .request_server_method(
                WORKSPACES_SERVER_ID,
                Some("[]"),
                material,
                &self.base_url.to_string(),
                Method::POST,
            )
            .await?;
        if legacy_post.is_success() {
            if let Ok(root) = parse_json_document(&legacy_post.body) {
                if let Some(workspace) = find_workspace_id(&root) {
                    return Ok(Some(workspace));
                }
            }
            if let Some(workspace) = find_workspace_in_text(&legacy_post.body) {
                return Ok(Some(workspace));
            }
        }
        Ok(None)
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
            OpenCodeGoSourceMode::Automatic => {}
        }

        let scoped = !material.cookies.is_empty() || account.workspace_id.is_some();
        let local = self.local_reader.read_from_process(Utc::now());
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
    let renews_at = parse_reset_at(
        root,
        Utc::now(),
        &["renewAt", "renew_at", "renewsAt", "renews_at"],
    );
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

fn parse_console_snapshot(
    root: &Value,
    account: &AccountRecord,
    workspace: &str,
) -> Option<UsageProbeResult> {
    let meters = root
        .get("access")
        .and_then(|value| value.get("meters"))
        .or_else(|| root.get("meters"))
        .unwrap_or(root);
    let rolling = first_named(meters, &["fiveHour", "five_hour", "rolling", "session"])
        .and_then(|value| {
            parse_window(
                value,
                UsageWindowKind::Primary,
                "Rolling 5 hours",
                false,
                true,
            )
        })
        .or_else(|| {
            find_window(root, WindowRole::Rolling).and_then(|value| {
                parse_window(
                    value,
                    UsageWindowKind::Primary,
                    "Rolling 5 hours",
                    false,
                    true,
                )
            })
        });
    let weekly = first_named(meters, &["week", "weekly"])
        .and_then(|value| parse_window(value, UsageWindowKind::Secondary, "Weekly", false, true))
        .or_else(|| {
            find_window(root, WindowRole::Weekly).and_then(|value| {
                parse_window(value, UsageWindowKind::Secondary, "Weekly", false, true)
            })
        });
    let mut monthly = first_named(meters, &["month", "monthly"])
        .and_then(|value| parse_window(value, UsageWindowKind::Additional, "Monthly", false, true))
        .or_else(|| {
            find_window(root, WindowRole::Monthly).and_then(|value| {
                parse_window(value, UsageWindowKind::Additional, "Monthly", false, true)
            })
        });
    let renews_at = root
        .get("access")
        .and_then(|access| access.get("endsAt"))
        .and_then(|value| parse_date_value(value, Utc::now()));
    if let (Some(monthly), Some(renews_at)) = (monthly.as_mut(), renews_at) {
        if monthly.window.reset_at_utc.is_none() {
            monthly.window.reset_at_utc = Some(renews_at);
            monthly.window.limit_window_seconds = (renews_at - Utc::now()).num_seconds().max(0);
        }
    }
    let mut identity_root = root.clone();
    if let Value::Object(object) = &mut identity_root {
        object.insert(
            "workspaceId".to_owned(),
            Value::String(workspace.to_owned()),
        );
    }
    Some(build_snapshot(
        account,
        &identity_root,
        "web-console",
        rolling?,
        weekly,
        monthly,
        None,
        renews_at,
        "authoritative",
        Vec::new(),
    ))
}

fn parse_web_page(
    body: &str,
    account: &AccountRecord,
    workspace: &str,
) -> Option<UsageProbeResult> {
    if let Ok(root) = parse_json_document(body) {
        let mut identity_root = root.clone();
        if let Value::Object(object) = &mut identity_root {
            object.insert(
                "workspaceId".to_owned(),
                Value::String(workspace.to_owned()),
            );
        }
        let rolling = find_window(&root, WindowRole::Rolling).and_then(|value| {
            parse_window(
                value,
                UsageWindowKind::Primary,
                "Rolling 5 hours",
                false,
                false,
            )
        });
        let weekly = find_window(&root, WindowRole::Weekly).and_then(|value| {
            parse_window(value, UsageWindowKind::Secondary, "Weekly", false, false)
        });
        let monthly = find_window(&root, WindowRole::Monthly).and_then(|value| {
            parse_window(value, UsageWindowKind::Additional, "Monthly", false, false)
        });
        if let Some(rolling) = rolling {
            return Some(build_snapshot(
                account,
                &identity_root,
                "web-dashboard",
                rolling,
                weekly,
                monthly,
                find_balance(&root),
                None,
                "authoritative",
                Vec::new(),
            ));
        }
    }

    let rolling = parse_text_window(
        body,
        "rollingUsage",
        UsageWindowKind::Primary,
        "Rolling 5 hours",
    )?;
    let weekly = parse_text_window(body, "weeklyUsage", UsageWindowKind::Secondary, "Weekly");
    let monthly = parse_text_window(body, "monthlyUsage", UsageWindowKind::Additional, "Monthly");
    let root = json!({"workspaceId": workspace});
    Some(build_snapshot(
        account,
        &root,
        "web-dashboard",
        rolling,
        weekly,
        monthly,
        find_balance_from_text(body),
        None,
        "authoritative",
        Vec::new(),
    ))
}

fn build_snapshot(
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

fn balance_only_snapshot(
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

fn enrich_balance(result: &mut UsageProbeResult, balance: Option<f64>, root: &Value) {
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

fn merge_local_estimate(remote: &mut UsageSnapshot, local: &UsageSnapshot) {
    let local_metrics = local
        .metrics
        .iter()
        .filter(|metric| metric.metadata.get("scope").map(String::as_str) == Some("device-local"))
        .cloned();
    remote.metrics.extend(local_metrics);
    if remote.spend.is_none() {
        remote.spend = local.spend.clone();
    }
    if remote.primary.is_none() {
        remote.primary = local.primary.clone();
    }
    if remote.secondary.is_none() {
        remote.secondary = local.secondary.clone();
    }
    if remote.additional_windows.is_empty() {
        remote.additional_windows = local.additional_windows.clone();
    }
    remote.source = Some(format!(
        "{}+{}",
        remote.source.as_deref().unwrap_or("api"),
        LOCAL_SOURCE
    ));
}

fn parse_window(
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
            limit_window_seconds: reset
                .map(|value| (value - now).num_seconds().max(0))
                .unwrap_or_default(),
        },
        used_amount,
        limit_amount,
    })
}

fn parse_text_window(
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
            reset_at_utc: seconds.map(|seconds| now + Duration::seconds(seconds.max(0))),
            limit_window_seconds: seconds.unwrap_or_default().max(0),
        },
        used_amount: None,
        limit_amount: None,
    })
}

#[derive(Debug, Clone, Copy)]
enum WindowRole {
    Rolling,
    Weekly,
    Monthly,
}

fn find_window<'a>(value: &'a Value, role: WindowRole) -> Option<&'a Value> {
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
            if let Some(candidate) = object.get(*name) {
                if candidate.is_object() {
                    return Some(candidate);
                }
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

fn first_named<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| value.get(*name))
}

fn parse_json_document(body: &str) -> Result<Value, serde_json::Error> {
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

fn find_workspace_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in [
                "workspaceId",
                "workspace_id",
                "orgId",
                "org_id",
                "organizationId",
                "organization_id",
                "id",
            ] {
                if let Some(candidate) = object
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(normalize_workspace_id)
                {
                    return Some(candidate);
                }
            }
            object.values().find_map(find_workspace_id)
        }
        Value::Array(array) => array.iter().find_map(find_workspace_id),
        _ => None,
    }
}

// The Console endpoint returns workspace/org rows. Match CodexBar's current
// behavior: only a row's own `id` with a recognized Console prefix is valid.
// Recursively accepting arbitrary `id` fields can select a user or nested
// resource ID and make the subsequent usage request target the wrong scope.
fn find_console_workspace_id(value: &Value) -> Option<String> {
    value.as_array()?.iter().find_map(|row| {
        let id = row.get("id")?.as_str()?;
        is_console_workspace_id(id).then(|| id.to_owned())
    })
}

fn is_console_workspace_id(value: &str) -> bool {
    ["wrk_", "org_"].iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    })
}

fn console_status_shape_error(root: &Value) -> String {
    if root.is_null() || root.get("access").is_some_and(Value::is_null) {
        return "the signed-in account has no active OpenCode Go subscription".to_owned();
    }
    if !root.is_object() {
        return format!(
            "console status returned {}; expected an object",
            json_shape_summary(root)
        );
    }
    let Some(access) = root.get("access") else {
        return format!(
            "console status has no access field; top-level fields: {}",
            object_field_names(root)
        );
    };
    let Some(meters) = access.get("meters") else {
        return format!(
            "console access has no meters field; access fields: {}",
            object_field_names(access)
        );
    };
    let Some(rolling) = meters.get("fiveHour") else {
        return format!(
            "console access has no fiveHour meter; meter shape: {}",
            json_shape_summary(meters)
        );
    };
    if parse_window(
        rolling,
        UsageWindowKind::Primary,
        "Rolling 5 hours",
        false,
        true,
    )
    .is_none()
    {
        return format!(
            "console fiveHour meter has an unsupported shape: {}",
            json_shape_summary(rolling)
        );
    }

    "console status contained a fiveHour meter but no usable rolling quota".to_owned()
}

fn object_field_names(value: &Value) -> String {
    value
        .as_object()
        .map(|object| {
            if object.is_empty() {
                "<empty>".to_owned()
            } else {
                object
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|| "<not an object>".to_owned())
}

fn json_shape_summary(value: &Value) -> String {
    match value {
        Value::Object(_) => format!("object fields [{}]", object_field_names(value)),
        Value::Array(values) => {
            let first_object_fields = values
                .iter()
                .find(|value| value.is_object())
                .map(|object| format!("; object fields [{}]", object_field_names(object)))
                .unwrap_or_default();
            format!("array length {}{first_object_fields}", values.len())
        }
        Value::String(value) => format!("string of {} characters", value.chars().count()),
        Value::Number(_) => "number".to_owned(),
        Value::Bool(_) => "boolean".to_owned(),
        Value::Null => "null".to_owned(),
    }
}

fn find_workspace_in_text(body: &str) -> Option<String> {
    Regex::new(r#"(?i)(?:workspace(?:Id|_id)?|org(?:Id|_id)?)\s*["']?\s*[:=]\s*["']([^"']+)["']"#)
        .ok()?
        .captures(body)
        .and_then(|captures| captures.get(1))
        .and_then(|value| normalize_workspace_id(value.as_str()))
}

fn normalize_workspace_id(value: &str) -> Option<String> {
    let mut value = value.trim().trim_matches('/').to_owned();
    if value.is_empty() {
        return None;
    }
    if let Some(index) = value.find("/workspace/") {
        value = value[index + "/workspace/".len()..].to_owned();
    }
    if let Some(index) = value.find("/org/") {
        value = value[index + "/org/".len()..].to_owned();
    }
    value = value
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(&value)
        .to_owned();
    (value.len() >= 4).then_some(value)
}

fn find_balance(value: &Value) -> Option<f64> {
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
        {
            if let Some(number) = balance
                .as_f64()
                .or_else(|| balance.as_str().and_then(|v| v.parse().ok()))
            {
                return Some(number);
            }
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

fn find_console_billing_balance(value: &Value) -> Option<f64> {
    json_number(value, &["balanceMicroCents", "balance_micro_cents"])
        .map(|micro_cents| micro_cents / MICRO_CENTS_PER_USD)
}

fn find_legacy_billing_balance(value: &Value) -> Option<f64> {
    match value {
        Value::Object(object) => {
            let has_customer = object
                .get("customerID")
                .or_else(|| object.get("customerId"))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            if has_customer {
                if let Some(raw) = object
                    .get("balance")
                    .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
                {
                    return Some(raw / 100_000_000.0);
                }
            }
            object.values().find_map(find_legacy_billing_balance)
        }
        Value::Array(array) => array.iter().find_map(find_legacy_billing_balance),
        _ => None,
    }
}

fn find_balance_from_text(body: &str) -> Option<f64> {
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

fn find_credit_limit(value: &Value) -> Option<CreditLimitSnapshot> {
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

fn parse_reset_at(value: &Value, now: DateTime<Utc>, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(|value| parse_date_value(value, now))
}

fn parse_reset_in(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    json_number(
        value,
        &[
            "resetInSec",
            "resetInSeconds",
            "resetSeconds",
            "reset_in_sec",
            "resetsInSec",
        ],
    )
    .map(|seconds| now + Duration::seconds(seconds.max(0.0) as i64))
}

fn parse_date_value(value: &Value, _now: DateTime<Utc>) -> Option<DateTime<Utc>> {
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

fn epoch_to_date(value: f64) -> Option<DateTime<Utc>> {
    if value > 1_000_000_000_000.0 {
        Utc.timestamp_millis_opt(value as i64).single()
    } else if value > 1_000_000_000.0 {
        Utc.timestamp_opt(value as i64, 0).single()
    } else {
        None
    }
}

fn find_email(value: &Value) -> Option<String> {
    json_string(value, &["email", "emailAddress", "email_address"]).or_else(|| match value {
        Value::Object(object) => object.values().find_map(find_email),
        Value::Array(array) => array.iter().find_map(find_email),
        _ => None,
    })
}

fn has_key(value: &Value, names: &[&str]) -> bool {
    names.iter().any(|name| value.get(*name).is_some())
}

fn window_metric(
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

fn percent(used: f64, limit: f64) -> f64 {
    if !used.is_finite() || !limit.is_finite() || limit <= 0.0 {
        0.0
    } else {
        (used.max(0.0) / limit * 100.0).clamp(0.0, 100.0)
    }
}

fn web_headers(base_url: &Url, path: &str) -> Vec<(String, String)> {
    vec![
        ("Origin".to_owned(), base_url.origin().ascii_serialization()),
        ("Referer".to_owned(), format!("{}{}", base_url, path)),
        (
            "Accept".to_owned(),
            "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8".to_owned(),
        ),
    ]
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
mod tests {
    use super::*;

    #[test]
    fn api_direct_percent_one_is_one_percent_not_one_hundred() {
        let value = json!({"usagePercent": 1.0, "resetInSec": 60});
        let parsed =
            parse_window(&value, UsageWindowKind::Primary, "Rolling", true, false).unwrap();
        assert_eq!(parsed.window.used_percent, 1.0);
    }

    #[test]
    fn dashboard_fraction_is_converted_but_console_micro_cents_are_amounts() {
        let dashboard = json!({"usagePercent": 0.43, "resetInSec": 60});
        let parsed = parse_window(
            &dashboard,
            UsageWindowKind::Primary,
            "Rolling",
            false,
            false,
        )
        .unwrap();
        assert_eq!(parsed.window.used_percent, 43.0);

        let console = json!({"usedMicroCents": 6_000_000.0, "limitMicroCents": 12_000_000.0});
        let parsed =
            parse_window(&console, UsageWindowKind::Primary, "Rolling", false, true).unwrap();
        assert_eq!(parsed.window.used_percent, 50.0);
        assert_eq!(parsed.used_amount, Some(0.06));
        assert_eq!(parsed.limit_amount, Some(0.12));
    }

    #[test]
    fn workspace_ids_are_normalized_from_dashboard_urls() {
        assert_eq!(
            normalize_workspace_id("https://opencode.ai/workspace/wrk_123/go"),
            Some("wrk_123".to_owned())
        );
        assert_eq!(
            normalize_workspace_id("org_123"),
            Some("org_123".to_owned())
        );
    }

    #[test]
    fn console_workspace_discovery_ignores_nested_and_unrecognized_ids() {
        let orgs = json!([
            {"id": "user_123", "workspace": {"id": "wrk_nested"}},
            {"id": "org_selected"},
            {"id": "wrk_later"}
        ]);
        assert_eq!(
            find_console_workspace_id(&orgs),
            Some("org_selected".to_owned())
        );
    }

    #[test]
    fn console_status_diagnostic_reports_schema_without_response_values() {
        let root = json!({"access": {"meters": {"weekly": {"used": 123}}}});
        let message = console_status_shape_error(&root);
        assert!(message.contains("fiveHour"));
        assert!(message.contains("weekly"));
        assert!(!message.contains("123"));
        assert_eq!(
            console_status_shape_error(&json!({"access": null})),
            "the signed-in account has no active OpenCode Go subscription"
        );
    }

    #[test]
    fn console_status_uses_access_meters() {
        let root = json!({
            "access": {"meters": {
                "fiveHour": {"usedMicroCents": 6_000_000, "limitMicroCents": 12_000_000, "resetInSec": 300},
                "week": {"usedMicroCents": 9_000_000, "limitMicroCents": 30_000_000, "resetInSec": 900}
            }}
        });
        let account = AccountRecord::create(
            "go",
            "go@example.com",
            None,
            OPENCODE_GO,
            Some("wrk_123".to_owned()),
        )
        .unwrap();
        let result = parse_console_snapshot(&root, &account, "wrk_123").unwrap();
        let snapshot = result.snapshot.unwrap();
        assert_eq!(snapshot.primary.unwrap().used_percent, 50.0);
        assert_eq!(snapshot.secondary.unwrap().used_percent, 30.0);
    }

    #[test]
    fn console_status_uses_access_end_as_monthly_reset_fallback() {
        let renews_at = (Utc::now() + Duration::days(30)).to_rfc3339();
        let root = json!({
            "access": {
                "endsAt": renews_at,
                "meters": {
                    "fiveHour": {"usedMicroCents": 6_000_000, "limitMicroCents": 12_000_000},
                    "month": {"usedMicroCents": 30_000_000, "limitMicroCents": 100_000_000}
                }
            }
        });
        let account = AccountRecord::create(
            "go",
            "go@example.com",
            None,
            OPENCODE_GO,
            Some("wrk_123".to_owned()),
        )
        .unwrap();

        let snapshot = parse_console_snapshot(&root, &account, "wrk_123")
            .unwrap()
            .snapshot
            .unwrap();
        let monthly = snapshot
            .additional_windows
            .iter()
            .find(|window| window.key == "monthly")
            .unwrap();
        let expected = parse_date_value(&json!(renews_at), Utc::now()).unwrap();
        assert_eq!(monthly.window.reset_at_utc, Some(expected));
        assert!(monthly.window.limit_window_seconds > 0);
        assert!(snapshot.metrics.iter().any(|metric| {
            metric.key == "subscription-renewal" && metric.reset_at_utc == Some(expected)
        }));
    }

    #[test]
    fn console_status_prefers_month_meter_reset_over_access_end() {
        let month_reset = (Utc::now() + Duration::days(5)).to_rfc3339();
        let renews_at = (Utc::now() + Duration::days(10)).to_rfc3339();
        let root = json!({
            "access": {
                "endsAt": renews_at,
                "meters": {
                    "fiveHour": {"usedMicroCents": 6_000_000, "limitMicroCents": 12_000_000},
                    "month": {
                        "usedMicroCents": 30_000_000,
                        "limitMicroCents": 100_000_000,
                        "resetsAt": month_reset
                    }
                }
            }
        });
        let account = AccountRecord::create(
            "go",
            "go@example.com",
            None,
            OPENCODE_GO,
            Some("wrk_123".to_owned()),
        )
        .unwrap();

        let snapshot = parse_console_snapshot(&root, &account, "wrk_123")
            .unwrap()
            .snapshot
            .unwrap();
        let monthly = snapshot
            .additional_windows
            .iter()
            .find(|window| window.key == "monthly")
            .unwrap();
        assert_eq!(
            monthly.window.reset_at_utc,
            parse_date_value(&json!(month_reset), Utc::now())
        );
    }

    #[test]
    fn legacy_billing_balance_is_scaled_from_provider_units() {
        let response = json!({"customerID": "cus_test", "balance": 125_000_000});
        assert_eq!(find_legacy_billing_balance(&response), Some(1.25));
    }

    #[test]
    fn console_billing_uses_signed_balance_and_not_available_credits() {
        let response = json!({
            "balanceMicroCents": "125000000",
            "availableMicroCents": "999000000"
        });
        assert_eq!(find_console_billing_balance(&response), Some(1.25));

        let negative_balance = json!({"balanceMicroCents": -75_000_000});
        assert_eq!(find_console_billing_balance(&negative_balance), Some(-0.75));

        let absent_balance = json!({
            "balanceMicroCents": null,
            "availableMicroCents": "999000000"
        });
        assert_eq!(find_console_billing_balance(&absent_balance), None);
    }
}
