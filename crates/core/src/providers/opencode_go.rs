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

async fn send_with_guarded_redirects(
    transport: &dyn UsageHttpTransport,
    mut request: UsageHttpRequest,
) -> Result<UsageHttpResponse, TransportError> {
    let mut redirects_followed = 0;
    loop {
        let response = transport.send(request.clone()).await?;
        if !matches!(response.status_code, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        if redirects_followed >= MAX_OPEN_CODE_REDIRECTS {
            return Ok(response);
        }
        let Some(location) = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
            .map(|(_, value)| value)
        else {
            return Ok(response);
        };
        let destination = request
            .url
            .join(location)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        if !is_allowed_redirect_target(&request.url, &destination) {
            return Ok(response);
        }
        let Some(next_request) =
            request_after_redirect(&request, response.status_code, destination)
        else {
            return Ok(response);
        };
        request = next_request;
        redirects_followed += 1;
    }
}

fn is_allowed_redirect_target(source: &Url, destination: &Url) -> bool {
    source.scheme() == "https"
        && destination.scheme() == "https"
        && source.origin() == destination.origin()
        && destination.username().is_empty()
        && destination.password().is_none()
}

fn request_after_redirect(
    request: &UsageHttpRequest,
    status_code: u16,
    destination: Url,
) -> Option<UsageHttpRequest> {
    let mut redirected = request.clone();
    redirected.url = destination;
    let rewrite_to_get = match status_code {
        301 | 302 if request.method == Method::POST => true,
        301 | 302 => matches!(request.method, Method::GET | Method::HEAD),
        303 => request.method != Method::HEAD,
        307 | 308 => return Some(redirected),
        _ => false,
    };
    if !rewrite_to_get && !matches!(request.method, Method::GET | Method::HEAD) {
        return None;
    }
    if rewrite_to_get {
        redirected.method = Method::GET;
        redirected.body = None;
        redirected.headers.retain(|name, _| {
            !name.eq_ignore_ascii_case("content-type")
                && !name.eq_ignore_ascii_case("content-length")
                && !name.eq_ignore_ascii_case("transfer-encoding")
        });
    }
    Some(redirected)
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

    fn spawn_console_billing(
        &self,
        material: &AccountAuthMaterial,
        workspace: &str,
    ) -> Result<ConsoleBillingTask, TransportError> {
        let request = self.build_request(
            CONSOLE_BILLING_PATH,
            material,
            [("x-org-id".to_owned(), workspace.to_owned())],
        )?;
        let transport = Arc::clone(&self.transport);
        Ok(tokio::spawn(async move {
            send_with_guarded_redirects(transport.as_ref(), request).await
        }))
    }

    async fn enrich_console_balance_if_ready(
        &self,
        result: &mut UsageProbeResult,
        task: &mut Option<ConsoleBillingTask>,
    ) {
        if task.is_none() {
            return;
        }
        match fetch_console_balance(task, CONSOLE_BILLING_OPTIONAL_JOIN_TIMEOUT).await {
            Ok((balance, root)) => enrich_balance(result, Some(balance), &root),
            Err(item) => add_snapshot_diagnostic(result, item),
        }
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
        send_with_guarded_redirects(
            self.transport.as_ref(),
            UsageHttpRequest {
                method,
                url,
                headers: request_headers,
                body,
            },
        )
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
        if material.cookie_header().is_none() && !material.has_bearer_token() {
            return Ok(missing_auth("OpenCode Go web"));
        }

        let mut diagnostics = Vec::new();
        let workspace = if let Some(workspace) = account
            .workspace_id
            .as_deref()
            .and_then(normalize_workspace_id)
        {
            Some(workspace)
        } else {
            // `OPENCODE_GO_WORKSPACE_ID` is process-wide, so it is only used
            // during discovery to choose among this account's own workspaces;
            // applying it directly would point every account at one workspace.
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

        let mut billing_task = if let Some(workspace) = workspace.as_deref() {
            match self.spawn_console_billing(material, workspace) {
                Ok(task) => Some(task),
                Err(error) => {
                    diagnostics.push(transport_diagnostic("web.console.billing", &error));
                    None
                }
            }
        } else {
            None
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
                            if billing_task.is_some() {
                                match fetch_console_balance(
                                    &mut billing_task,
                                    CONSOLE_BILLING_REQUIRED_TIMEOUT,
                                )
                                .await
                                {
                                    Ok((balance, _)) => {
                                        let mut result = balance_only_snapshot(
                                            account,
                                            workspace,
                                            balance,
                                            "web-console",
                                        );
                                        if let Some(snapshot) = result.snapshot.as_mut() {
                                            snapshot.source_diagnostics.append(&mut diagnostics);
                                        }
                                        return Ok(result);
                                    }
                                    Err(item) => diagnostics.push(item),
                                }
                            }
                        } else if let Some(mut result) =
                            parse_console_snapshot(&root, account, workspace)
                        {
                            if let Some(snapshot) = result.snapshot.as_mut() {
                                snapshot.source_diagnostics.append(&mut diagnostics);
                            }
                            self.enrich_console_balance_if_ready(&mut result, &mut billing_task)
                                .await;
                            if result.succeeded() {
                                return Ok(result);
                            }
                        } else {
                            let missing_usage_fields = console_status_missing_usage_fields(&root);
                            diagnostics.push(diagnostic(
                                "web.console.status",
                                UsageAdapterErrorCode::InvalidPayload,
                                console_status_shape_error(&root),
                                Some(response.status_code),
                            ));
                            if missing_usage_fields && billing_task.is_some() {
                                match fetch_console_balance(
                                    &mut billing_task,
                                    CONSOLE_BILLING_REQUIRED_TIMEOUT,
                                )
                                .await
                                {
                                    Ok((balance, _)) => {
                                        let mut result = balance_only_snapshot(
                                            account,
                                            workspace,
                                            balance,
                                            "web-console",
                                        );
                                        if let Some(snapshot) = result.snapshot.as_mut() {
                                            snapshot.source_diagnostics.append(&mut diagnostics);
                                        }
                                        return Ok(result);
                                    }
                                    Err(item) => diagnostics.push(item),
                                }
                            }
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
                                self.enrich_console_balance_if_ready(
                                    &mut result,
                                    &mut billing_task,
                                )
                                .await;
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

            if let Some(task) = billing_task.take() {
                task.abort();
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
                    if let Ok(root) = parse_json_document(&response.body)
                        && let Some(balance) =
                            find_legacy_billing_balance(&root).or_else(|| find_balance(&root))
                    {
                        let mut result =
                            balance_only_snapshot(account, workspace, balance, "web-legacy");
                        if let Some(snapshot) = result.snapshot.as_mut() {
                            snapshot.source_diagnostics = diagnostics;
                        }
                        return Ok(result);
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

    async fn discover_workspace_id(
        &self,
        material: &AccountAuthMaterial,
    ) -> Result<Option<String>, TransportError> {
        let response = self.request(CONSOLE_ORGS_PATH, material, []).await?;
        if response.is_success()
            && let Ok(root) = parse_json_document(&response.body)
        {
            let preferred = env::var("OPENCODE_GO_WORKSPACE_ID")
                .ok()
                .and_then(|value| normalize_workspace_id(&value));
            if let Some(workspace) = select_console_workspace_id(&root, preferred.as_deref()) {
                return Ok(Some(workspace));
            }
        }

        let legacy = self
            .request_server(WORKSPACES_SERVER_ID, None, material, self.base_url.as_ref())
            .await?;
        if legacy.is_success() {
            if let Ok(root) = parse_json_document(&legacy.body)
                && let Some(workspace) = find_workspace_id(&root)
            {
                return Ok(Some(workspace));
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
                self.base_url.as_ref(),
                Method::POST,
            )
            .await?;
        if legacy_post.is_success() {
            if let Ok(root) = parse_json_document(&legacy_post.body)
                && let Some(workspace) = find_workspace_id(&root)
            {
                return Ok(Some(workspace));
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
    match (monthly.as_mut(), renews_at) {
        (Some(monthly), Some(renews_at)) if monthly.window.reset_at_utc.is_none() => {
            monthly.window.reset_at_utc = Some(renews_at);
            monthly.window.limit_window_seconds = fixed_window_seconds(UsageWindowKind::Additional);
        }
        _ => {}
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
            limit_window_seconds: fixed_window_seconds(kind),
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
            reset_at_utc: seconds.and_then(|seconds| checked_after(now, seconds as f64)),
            limit_window_seconds: fixed_window_seconds(kind),
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

fn find_window(value: &Value, role: WindowRole) -> Option<&Value> {
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
/// Picks the preferred workspace when it is one of this account's own
/// workspaces, otherwise the first valid workspace row.
fn select_console_workspace_id(value: &Value, preferred: Option<&str>) -> Option<String> {
    let workspaces = value
        .as_array()?
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?;
            is_console_workspace_id(id).then(|| id.to_owned())
        })
        .collect::<Vec<_>>();
    preferred
        .and_then(|preferred| workspaces.iter().find(|id| id.as_str() == preferred))
        .or_else(|| workspaces.first())
        .cloned()
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

fn console_status_missing_usage_fields(root: &Value) -> bool {
    let Some(access) = root.get("access").filter(|value| value.is_object()) else {
        return false;
    };
    let access_meters = access.get("meters");
    if access_meters.is_some_and(|meters| !meters.is_object()) {
        return false;
    }
    let root_meters = root.get("meters");
    if access_meters.is_none() && root_meters.is_some_and(|meters| !meters.is_object()) {
        return false;
    }
    let meters = access_meters.or(root_meters).unwrap_or(root);
    if !meters.is_object() {
        return false;
    }

    first_named(meters, &["fiveHour", "five_hour", "rolling", "session"]).is_none()
        && find_window(root, WindowRole::Rolling).is_none()
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

fn find_console_billing_balance(value: &Value) -> Option<f64> {
    json_number(value, &["balanceMicroCents", "balance_micro_cents"])
        .map(|micro_cents| micro_cents / MICRO_CENTS_PER_USD)
}

async fn fetch_console_balance(
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

fn find_legacy_billing_balance(value: &Value) -> Option<f64> {
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
fn checked_after(now: DateTime<Utc>, seconds: f64) -> Option<DateTime<Utc>> {
    if !seconds.is_finite() {
        return None;
    }
    Duration::try_seconds(seconds.max(0.0) as i64).and_then(|delay| now.checked_add_signed(delay))
}

/// `limit_window_seconds` is the fixed length of the quota window, never the
/// time remaining until reset (that is derived from `reset_at_utc`). OpenCode
/// Go uses a rolling 5-hour, a weekly, and a monthly window.
fn fixed_window_seconds(kind: UsageWindowKind) -> i64 {
    match kind {
        UsageWindowKind::Primary => 5 * 60 * 60,
        UsageWindowKind::Secondary => 7 * 24 * 60 * 60,
        UsageWindowKind::Additional => 30 * 24 * 60 * 60,
    }
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
    use std::{collections::VecDeque, sync::Mutex};
    use tokio::sync::Notify;

    struct StaticAuthMaterialProvider(AccountAuthMaterial);

    #[async_trait]
    impl AccountAuthMaterialProvider for StaticAuthMaterialProvider {
        async fn get(
            &self,
            _account: &AccountRecord,
        ) -> Result<Option<AccountAuthMaterial>, AuthError> {
            Ok(Some(self.0.clone()))
        }
    }

    struct ConsoleBillingTransport {
        status_body: String,
        billing_body: String,
        billing_delay: StdDuration,
        wait_for_billing_before_status: bool,
        billing_started: Notify,
        requests: Mutex<Vec<UsageHttpRequest>>,
    }

    #[async_trait]
    impl UsageHttpTransport for ConsoleBillingTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            let path = request.url.path().to_owned();
            self.requests.lock().unwrap().push(request);

            if path.ends_with(CONSOLE_BILLING_PATH) {
                self.billing_started.notify_one();
                tokio::time::sleep(self.billing_delay).await;
                return Ok(json_response(200, &self.billing_body));
            }
            if path.ends_with(CONSOLE_STATUS_PATH) {
                if self.wait_for_billing_before_status {
                    tokio::time::timeout(
                        StdDuration::from_secs(1),
                        self.billing_started.notified(),
                    )
                    .await
                    .expect("billing request should start before status completes");
                }
                return Ok(json_response(200, &self.status_body));
            }

            Err(TransportError::InvalidUrl(format!(
                "unexpected test request path: {path}"
            )))
        }
    }

    fn json_response(status_code: u16, body: &str) -> UsageHttpResponse {
        UsageHttpResponse {
            status_code,
            body: body.to_owned(),
            headers: Default::default(),
        }
    }

    fn console_probe_adapter(transport: Arc<dyn UsageHttpTransport>) -> OpenCodeGoUsageAdapter {
        OpenCodeGoUsageAdapter::new(
            transport,
            Arc::new(StaticAuthMaterialProvider(
                AccountAuthMaterial::from_cookie_header("session=test-session", None),
            )),
        )
        .unwrap()
        .with_source_mode(OpenCodeGoSourceMode::Web)
    }

    fn console_probe_account() -> AccountRecord {
        AccountRecord::create(
            "go",
            "go@example.com",
            None,
            OPENCODE_GO,
            Some("wrk_123".to_owned()),
        )
        .unwrap()
    }

    fn active_go_status() -> String {
        json!({
            "access": {"meters": {
                "fiveHour": {"usedMicroCents": 1_000_000, "limitMicroCents": 10_000_000},
                "week": {"usedMicroCents": 3_000_000, "limitMicroCents": 10_000_000}
            }}
        })
        .to_string()
    }

    #[tokio::test]
    async fn console_balance_enrichment_is_parallel_and_bounded() {
        let transport = Arc::new(ConsoleBillingTransport {
            status_body: active_go_status(),
            billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
            billing_delay: StdDuration::from_millis(500),
            wait_for_billing_before_status: true,
            billing_started: Notify::new(),
            requests: Mutex::new(Vec::new()),
        });
        let adapter = console_probe_adapter(transport.clone());

        let result = adapter.probe(&console_probe_account()).await.unwrap();
        let snapshot = result.snapshot.unwrap();

        assert!(snapshot.primary.is_some());
        assert!(snapshot.credits.is_none());
        assert!(snapshot.source_diagnostics.iter().any(|item| {
            item.source == "web.console.billing" && item.message.contains("wait bound")
        }));
        let paths = transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path.ends_with(CONSOLE_STATUS_PATH)));
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(CONSOLE_BILLING_PATH))
        );
        let requests = transport.requests.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(
                request.headers.get("Cookie").map(String::as_str),
                Some("session=test-session")
            );
            assert_eq!(
                request.headers.get("x-org-id").map(String::as_str),
                Some("wrk_123")
            );
        }
    }

    #[tokio::test]
    async fn console_usage_accepts_an_account_scoped_oauth_bearer_without_cookies() {
        let transport = Arc::new(ConsoleBillingTransport {
            status_body: active_go_status(),
            billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
            billing_delay: StdDuration::from_millis(1),
            wait_for_billing_before_status: false,
            billing_started: Notify::new(),
            requests: Mutex::new(Vec::new()),
        });
        let adapter = OpenCodeGoUsageAdapter::new(
            transport.clone(),
            Arc::new(StaticAuthMaterialProvider(AccountAuthMaterial {
                bearer_token: Some("opencode-access-token".to_owned()),
                oauth_refresh_token: Some("opencode-refresh-token".to_owned()),
                ..AccountAuthMaterial::default()
            })),
        )
        .unwrap()
        .with_source_mode(OpenCodeGoSourceMode::Automatic);

        let result = adapter.probe(&console_probe_account()).await.unwrap();

        assert!(result.succeeded(), "{result:?}");
        assert!(result.snapshot.unwrap().primary.is_some());
        let requests = transport.requests.lock().unwrap();
        assert!(requests.iter().any(|request| {
            request.url.path().ends_with(CONSOLE_STATUS_PATH)
                && request.headers.get("Authorization").map(String::as_str)
                    == Some("Bearer opencode-access-token")
                && !request.headers.contains_key("Cookie")
        }));
    }

    #[tokio::test]
    async fn console_balance_is_required_when_account_has_no_go_subscription() {
        let transport = Arc::new(ConsoleBillingTransport {
            status_body: json!({"access": null}).to_string(),
            billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
            billing_delay: StdDuration::ZERO,
            wait_for_billing_before_status: true,
            billing_started: Notify::new(),
            requests: Mutex::new(Vec::new()),
        });
        let adapter = console_probe_adapter(transport);

        let snapshot = adapter
            .probe(&console_probe_account())
            .await
            .unwrap()
            .snapshot
            .unwrap();

        assert_eq!(snapshot.source.as_deref(), Some("web-console"));
        assert_eq!(
            snapshot
                .credits
                .as_ref()
                .and_then(|credits| credits.balance),
            Some(1.25)
        );
        assert!(snapshot.primary.is_none());
        assert!(snapshot.source_diagnostics.iter().any(|item| {
            item.source == "web.console.status"
                && item.code == UsageAdapterErrorCode::NoSubscription
        }));
    }

    #[tokio::test]
    async fn console_balance_is_required_when_subscription_usage_fields_are_missing() {
        let transport = Arc::new(ConsoleBillingTransport {
            status_body: json!({"access": {"meters": {"week": {"usagePercent": 35.0}}}})
                .to_string(),
            billing_body: json!({"balanceMicroCents": 125_000_000}).to_string(),
            billing_delay: StdDuration::ZERO,
            wait_for_billing_before_status: true,
            billing_started: Notify::new(),
            requests: Mutex::new(Vec::new()),
        });
        let adapter = console_probe_adapter(transport);

        let snapshot = adapter
            .probe(&console_probe_account())
            .await
            .unwrap()
            .snapshot
            .unwrap();

        assert_eq!(snapshot.source.as_deref(), Some("web-console"));
        assert_eq!(
            snapshot
                .credits
                .as_ref()
                .and_then(|credits| credits.balance),
            Some(1.25)
        );
        assert!(snapshot.primary.is_none());
        assert!(snapshot.source_diagnostics.iter().any(|item| {
            item.source == "web.console.status"
                && item.code == UsageAdapterErrorCode::InvalidPayload
        }));
    }

    struct RedirectSequenceTransport {
        responses: Mutex<VecDeque<UsageHttpResponse>>,
        requests: Mutex<Vec<UsageHttpRequest>>,
    }

    #[async_trait]
    impl UsageHttpTransport for RedirectSequenceTransport {
        async fn send(
            &self,
            request: UsageHttpRequest,
        ) -> Result<UsageHttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("test response should be queued"))
        }
    }

    fn redirect_response(status_code: u16, location: &str) -> UsageHttpResponse {
        UsageHttpResponse {
            status_code,
            body: String::new(),
            headers: [("Location".to_owned(), location.to_owned())].into(),
        }
    }

    fn success_response() -> UsageHttpResponse {
        UsageHttpResponse {
            status_code: 200,
            body: "ok".to_owned(),
            headers: Default::default(),
        }
    }

    #[test]
    fn api_direct_percent_one_is_one_percent_not_one_hundred() {
        let value = json!({"usagePercent": 1.0, "resetInSec": 60});
        let parsed =
            parse_window(&value, UsageWindowKind::Primary, "Rolling", true, false).unwrap();
        assert_eq!(parsed.window.used_percent, 1.0);
    }

    #[test]
    fn api_usage_renewal_precedes_root_and_root_remains_a_fallback() {
        let nested_renewal = (Utc::now() + Duration::days(5)).to_rfc3339();
        let root_renewal = (Utc::now() + Duration::days(10)).to_rfc3339();
        let nested_root = json!({
            "renewAt": root_renewal,
            "usage": {
                "renewAt": nested_renewal,
                "rolling": {"usagePercent": 45.0}
            }
        });
        let root_fallback = json!({
            "renewAt": root_renewal,
            "usage": {"rolling": {"usagePercent": 45.0}}
        });
        let account = AccountRecord::create(
            "go",
            "go@example.com",
            None,
            OPENCODE_GO,
            Some("wrk_123".to_owned()),
        )
        .unwrap();
        let renewal_for = |root: &Value| {
            parse_api_snapshot(root, &account)
                .snapshot
                .unwrap()
                .metrics
                .into_iter()
                .find(|metric| metric.key == "subscription-renewal")
                .and_then(|metric| metric.reset_at_utc)
        };

        assert_eq!(
            renewal_for(&nested_root),
            parse_date_value(&json!(nested_renewal), Utc::now())
        );
        assert_eq!(
            renewal_for(&root_fallback),
            parse_date_value(&json!(root_renewal), Utc::now())
        );
    }

    #[test]
    fn redirect_policy_requires_https_and_the_same_origin() {
        let source = Url::parse("https://opencode.ai/console/api/orgs").unwrap();
        assert!(is_allowed_redirect_target(
            &source,
            &Url::parse("https://opencode.ai/console/api/orgs-v2").unwrap()
        ));
        assert!(!is_allowed_redirect_target(
            &source,
            &Url::parse("http://opencode.ai/console/api/orgs").unwrap()
        ));
        assert!(!is_allowed_redirect_target(
            &source,
            &Url::parse("https://opencode.ai:8443/console/api/orgs").unwrap()
        ));
        assert!(!is_allowed_redirect_target(
            &source,
            &Url::parse("https://user@opencode.ai/console/api/orgs").unwrap()
        ));
    }

    #[tokio::test]
    async fn same_origin_https_redirect_is_followed_with_session_headers() {
        let transport = RedirectSequenceTransport {
            responses: Mutex::new(VecDeque::from([
                redirect_response(302, "/console/api/orgs-v2"),
                success_response(),
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let request = UsageHttpRequest {
            method: Method::GET,
            url: Url::parse("https://opencode.ai/console/api/orgs").unwrap(),
            headers: [("Cookie".to_owned(), "session=secret".to_owned())].into(),
            body: None,
        };

        let response = send_with_guarded_redirects(&transport, request)
            .await
            .unwrap();
        let requests = transport.requests.lock().unwrap();
        assert_eq!(response.status_code, 200);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].url.path(), "/console/api/orgs-v2");
        assert_eq!(requests[1].headers.get("Cookie").unwrap(), "session=secret");
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_not_followed_with_account_credentials() {
        let transport = RedirectSequenceTransport {
            responses: Mutex::new(VecDeque::from([
                redirect_response(302, "https://attacker.example/collect"),
                success_response(),
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let request = UsageHttpRequest {
            method: Method::GET,
            url: Url::parse("https://opencode.ai/console/api/orgs").unwrap(),
            headers: [("Cookie".to_owned(), "session=secret".to_owned())].into(),
            body: None,
        };

        let response = send_with_guarded_redirects(&transport, request)
            .await
            .unwrap();
        assert_eq!(response.status_code, 302);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn post_redirect_rewrites_to_get_and_drops_body_headers() {
        let transport = RedirectSequenceTransport {
            responses: Mutex::new(VecDeque::from([
                redirect_response(303, "/server-v2"),
                success_response(),
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let request = UsageHttpRequest {
            method: Method::POST,
            url: Url::parse("https://opencode.ai/_server").unwrap(),
            headers: [
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), "Bearer secret".to_owned()),
            ]
            .into(),
            body: Some("[]".to_owned()),
        };

        let response = send_with_guarded_redirects(&transport, request)
            .await
            .unwrap();
        let requests = transport.requests.lock().unwrap();
        assert_eq!(response.status_code, 200);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, Method::GET);
        assert!(requests[1].body.is_none());
        assert!(
            !requests[1]
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("content-type"))
        );
        assert_eq!(
            requests[1].headers.get("Authorization").unwrap(),
            "Bearer secret"
        );
    }

    #[tokio::test]
    async fn post_307_redirect_preserves_method_and_body() {
        let transport = RedirectSequenceTransport {
            responses: Mutex::new(VecDeque::from([
                redirect_response(307, "/server-preserved"),
                success_response(),
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let request = UsageHttpRequest {
            method: Method::POST,
            url: Url::parse("https://opencode.ai/_server").unwrap(),
            headers: [("Content-Type".to_owned(), "application/json".to_owned())].into(),
            body: Some("[]".to_owned()),
        };

        let response = send_with_guarded_redirects(&transport, request)
            .await
            .unwrap();
        let requests = transport.requests.lock().unwrap();
        assert_eq!(response.status_code, 200);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, Method::POST);
        assert_eq!(requests[1].body.as_deref(), Some("[]"));
    }

    #[tokio::test]
    async fn redirect_chain_is_bounded() {
        let transport = RedirectSequenceTransport {
            responses: Mutex::new(
                (0..=MAX_OPEN_CODE_REDIRECTS)
                    .map(|_| redirect_response(302, "/loop"))
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        };
        let request = UsageHttpRequest {
            method: Method::GET,
            url: Url::parse("https://opencode.ai/loop").unwrap(),
            headers: Default::default(),
            body: None,
        };

        let response = send_with_guarded_redirects(&transport, request)
            .await
            .unwrap();
        assert_eq!(response.status_code, 302);
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            MAX_OPEN_CODE_REDIRECTS + 1
        );
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
    fn relative_reset_parser_accepts_codexbar_field_aliases() {
        for key in ["reset_sec", "resetsInSeconds", "resetIn", "resetSec"] {
            let mut value = serde_json::Map::new();
            value.insert("usagePercent".to_owned(), json!(25.0));
            value.insert(key.to_owned(), json!(3600));

            let parsed = parse_window(
                &Value::Object(value),
                UsageWindowKind::Primary,
                "Rolling",
                true,
                false,
            )
            .unwrap();
            assert!(parsed.window.reset_at_utc.is_some(), "missing {key}");
            assert_eq!(
                parsed.window.limit_window_seconds,
                5 * 60 * 60,
                "the window length must not be a countdown for {key}"
            );
        }
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
            select_console_workspace_id(&orgs, None),
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

    #[test]
    fn workspace_preference_only_selects_among_the_accounts_own_workspaces() {
        let orgs = json!([{ "id": "wrk_first" }, { "id": "wrk_second" }]);
        assert_eq!(
            select_console_workspace_id(&orgs, Some("wrk_second")).as_deref(),
            Some("wrk_second")
        );
        assert_eq!(
            select_console_workspace_id(&orgs, Some("wrk_someone_else")).as_deref(),
            Some("wrk_first")
        );
    }
}
