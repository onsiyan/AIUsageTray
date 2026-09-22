use crate::{
    accounts::{ANTIGRAVITY, AccountRecord, VerifiedIdentity},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError, OAuthProviderDefinition},
    providers::shared::{
        bearer_headers, invalid_payload, json_number, json_string, map_antigravity_http_error,
        missing_auth,
    },
    transport::{
        ReqwestUsageHttpTransport, TransportError, UsageHttpRequest, UsageHttpResponse,
        UsageHttpTransport,
    },
    usage::{
        AdditionalRateLimitWindow, RateLimitWindow, UsageAdapter, UsageMetric, UsageProbeResult,
        UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub fn oauth_definition() -> OAuthProviderDefinition {
    const DEFAULT_CLIENT_ID: &str =
        "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
    const DEFAULT_CLIENT_SECRET: &str = "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";

    let configured_client_id = std::env::var("ANTIGRAVITY_OAUTH_CLIENT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let configured_client_secret = std::env::var("ANTIGRAVITY_OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let (client_id, client_secret) = match (configured_client_id, configured_client_secret) {
        (Some(client_id), Some(client_secret)) => (client_id, client_secret),
        _ => (
            DEFAULT_CLIENT_ID.to_owned(),
            DEFAULT_CLIENT_SECRET.to_owned(),
        ),
    };
    let mut definition = OAuthProviderDefinition::new(
        ANTIGRAVITY,
        Url::parse("https://accounts.google.com/o/oauth2/v2/auth").expect("static OAuth URL"),
        Url::parse("https://oauth2.googleapis.com/token").expect("static OAuth URL"),
        Url::parse("http://127.0.0.1:0/callback").expect("static callback URL"),
        &client_id,
        Some(&client_secret),
        [
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/userinfo.email",
        ],
    )
    .expect("static Antigravity OAuth definition");
    definition.authorization_parameters = BTreeMap::from([
        ("access_type".to_owned(), "offline".to_owned()),
        ("prompt".to_owned(), "select_account consent".to_owned()),
    ]);
    definition.user_info_endpoint = Some(
        Url::parse("https://www.googleapis.com/oauth2/v2/userinfo").expect("static user-info URL"),
    );
    definition
}

pub struct AntigravityUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    local_transport: Option<Arc<dyn UsageHttpTransport>>,
    base_urls: Vec<Url>,
    user_agent: String,
}

impl AntigravityUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Self::with_local_probe(transport, auth, true)
    }

    /// Constructs the adapter without touching a running local provider.
    ///
    /// This is useful for deterministic contract tests and for callers that
    /// explicitly want the OAuth/API path only.
    pub fn new_without_local_probe(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Self::with_local_probe(transport, auth, false)
    }

    fn with_local_probe(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        enable_local_probe: bool,
    ) -> Result<Self, TransportError> {
        let base_urls = REMOTE_BASE_URLS
            .iter()
            .map(|value| {
                Url::parse(value).map_err(|error| TransportError::InvalidUrl(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let local_transport = if enable_local_probe {
            Some(Arc::new(ReqwestUsageHttpTransport::new_loopback(
                Duration::from_secs(12),
            )?) as Arc<dyn UsageHttpTransport>)
        } else {
            None
        };
        Ok(Self {
            transport,
            auth,
            local_transport,
            base_urls,
            user_agent: "antigravity".to_owned(),
        })
    }

    async fn post_to(
        &self,
        base_url: &Url,
        operation: &str,
        body: Value,
        material: &AccountAuthMaterial,
    ) -> Result<UsageHttpResponse, TransportError> {
        let url = base_url
            .join(&format!("/{}", operation.trim_start_matches('/')))
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let mut headers = bearer_headers(material, &self.user_agent);
        headers.insert("Content-Type".to_owned(), "application/json".to_owned());
        let body = serde_json::to_string(&body)
            .map_err(|error| TransportError::Serialization(error.to_string()))?;
        self.transport
            .send(UsageHttpRequest {
                method: Method::POST,
                url,
                headers,
                body: Some(body),
            })
            .await
    }

    async fn post_remote(
        &self,
        operation: &str,
        body: Value,
        material: &AccountAuthMaterial,
    ) -> Result<UsageHttpResponse, TransportError> {
        let mut last_response = None;
        let mut last_error = None;
        for base_url in &self.base_urls {
            match self
                .post_to(base_url, operation, body.clone(), material)
                .await
            {
                Ok(response)
                    if response.is_success()
                        || !is_retryable_remote_status(response.status_code) =>
                {
                    return Ok(response);
                }
                Ok(response) => {
                    last_response = Some(response);
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }
        if let Some(response) = last_response {
            Ok(response)
        } else {
            Err(last_error.unwrap_or_else(|| {
                TransportError::InvalidUrl("Antigravity endpoint list is empty".to_owned())
            }))
        }
    }

    async fn probe_local(&self, account: &AccountRecord) -> Option<UsageProbeResult> {
        let transport = self.local_transport.as_ref()?;
        let endpoints = discover_local_endpoints();
        if endpoints.is_empty() {
            return None;
        }

        let mut best_result = None;
        for endpoint in endpoints {
            let summary_groups = local_post(
                transport,
                &endpoint,
                LOCAL_QUOTA_SUMMARY_PATH,
                json!({ "forceRefresh": true }),
            )
            .await;
            let summary_groups = summary_groups
                .filter(UsageHttpResponse::is_success)
                .and_then(|response| serde_json::from_str::<Value>(&response.body).ok())
                .map(|root| parse_quota_summary(&root))
                .filter(|groups| has_usable_quota_summary(groups));

            // CodexBar treats the quota summary as the richest local source,
            // but obtains identity from GetUserStatus before accepting it.
            // That account check is essential when several Google accounts
            // are registered in the monitor: a local language server is not
            // account-scoped by the request itself.
            let status_root = local_post(
                transport,
                &endpoint,
                LOCAL_USER_STATUS_PATH,
                local_request_body(),
            )
            .await
            .filter(UsageHttpResponse::is_success)
            .and_then(|response| serde_json::from_str::<Value>(&response.body).ok());
            let email = status_root.as_ref().and_then(find_local_email);
            let plan_type = status_root.as_ref().and_then(find_local_plan_type);
            let status_models = status_root
                .as_ref()
                .map(parse_local_model_quotas)
                .unwrap_or_default();

            if let Some(groups) = summary_groups
                && local_identity_matches(account, email.as_deref())
            {
                let score = local_snapshot_score(
                    Some(&groups),
                    &status_models,
                    email.as_deref(),
                    plan_type.as_deref(),
                );
                let snapshot = snapshot_from_quota_summary(
                    account,
                    &groups,
                    &status_models,
                    email.clone(),
                    plan_type.clone(),
                    "local",
                );
                keep_best_candidate(
                    &mut best_result,
                    score,
                    UsageProbeResult::success(
                        snapshot,
                        Some(VerifiedIdentity {
                            email,
                            provider_account_id: None,
                            plan_type,
                        }),
                    ),
                );
                continue;
            }

            // IDE language servers commonly return 404 for the summary.  The
            // proven fallback order is GetUserStatus, then
            // GetCommandModelConfigs; neither path is allowed to win without
            // a matching account identity.
            if local_identity_matches(account, email.as_deref()) {
                if !status_models.is_empty() {
                    let score = local_snapshot_score(
                        None,
                        &status_models,
                        email.as_deref(),
                        plan_type.as_deref(),
                    );
                    let snapshot = snapshot_from_model_quotas(
                        account,
                        &status_models,
                        email.clone(),
                        plan_type.clone(),
                        "local-legacy",
                        "authoritative",
                    );
                    keep_best_candidate(
                        &mut best_result,
                        score,
                        UsageProbeResult::success(
                            snapshot,
                            Some(VerifiedIdentity {
                                email,
                                provider_account_id: None,
                                plan_type,
                            }),
                        ),
                    );
                    continue;
                }

                let command_models = local_post(
                    transport,
                    &endpoint,
                    LOCAL_COMMAND_MODEL_CONFIGS_PATH,
                    local_request_body(),
                )
                .await
                .filter(UsageHttpResponse::is_success)
                .and_then(|response| serde_json::from_str::<Value>(&response.body).ok())
                .map(|root| parse_local_model_quotas(&root))
                .unwrap_or_default();
                if !command_models.is_empty() {
                    let score = local_snapshot_score(
                        None,
                        &command_models,
                        email.as_deref(),
                        plan_type.as_deref(),
                    );
                    let snapshot = snapshot_from_model_quotas(
                        account,
                        &command_models,
                        email.clone(),
                        plan_type.clone(),
                        "local-command-models",
                        "authoritative",
                    );
                    keep_best_candidate(
                        &mut best_result,
                        score,
                        UsageProbeResult::success(
                            snapshot,
                            Some(VerifiedIdentity {
                                email,
                                provider_account_id: None,
                                plan_type,
                            }),
                        ),
                    );
                }
            }
        }

        best_result.map(|(_, result)| result)
    }

    async fn onboard_remote(
        &self,
        assist_root: &Value,
        material: &AccountAuthMaterial,
    ) -> Option<String> {
        let tier_id = find_onboard_tier(assist_root)?;
        let body = json!({
            "tierId": tier_id,
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI"
            }
        });
        let response = self
            .post_remote("v1internal:onboardUser", body, material)
            .await
            .ok()?;
        if response.is_success() {
            if let Ok(root) = serde_json::from_str::<Value>(&response.body)
                && let Some(project) = find_project_id(&root)
            {
                return Some(project);
            }
        }

        // Onboarding is eventually consistent. Keep the retry bounded and
        // refresh the same account-scoped loadCodeAssist response, as the
        // reference implementation does, rather than inventing a project id.
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let refreshed = self
                .post_remote(
                    "v1internal:loadCodeAssist",
                    json!({
                        "metadata": {
                            "ideType": "ANTIGRAVITY",
                            "platform": "PLATFORM_UNSPECIFIED",
                            "pluginType": "GEMINI"
                        }
                    }),
                    material,
                )
                .await
                .ok()?;
            if refreshed.is_success()
                && let Ok(root) = serde_json::from_str::<Value>(&refreshed.body)
                && let Some(project) = find_project_id(&root)
            {
                return Some(project);
            }
        }
        None
    }
}

#[async_trait]
impl UsageAdapter for AntigravityUsageAdapter {
    fn adapter_id(&self) -> &str {
        ANTIGRAVITY
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        if let Some(result) = self.probe_local(account).await {
            return Ok(result);
        }

        let material = match self.auth.get(account).await {
            Ok(Some(material)) => material,
            Ok(None) => return Ok(missing_auth("Antigravity")),
            Err(AuthError::ReauthenticationRequired(_)) => return Ok(missing_auth("Antigravity")),
            Err(error) => {
                return Ok(invalid_payload(
                    "Antigravity",
                    format!("credential provider failed: {error}"),
                ));
            }
        };
        if !material.has_bearer_token() {
            return Ok(missing_auth("Antigravity"));
        }

        let assist = self
            .post_remote(
                "v1internal:loadCodeAssist",
                json!({
                    "metadata": {
                        "ideType": "ANTIGRAVITY",
                        "platform": "PLATFORM_UNSPECIFIED",
                        "pluginType": "GEMINI"
                    }
                }),
                &material,
            )
            .await?;
        if !assist.is_success() {
            return Ok(map_antigravity_http_error(&assist, "loadCodeAssist"));
        }
        let assist_root: Value = match serde_json::from_str(&assist.body) {
            Ok(value) => value,
            Err(error) => return Ok(invalid_payload("Antigravity", error.to_string())),
        };
        let mut project_id = find_project_id(&assist_root).or_else(|| account.workspace_id.clone());
        if project_id.is_none() {
            project_id = self.onboard_remote(&assist_root, &material).await;
        }

        let models = self
            .post_remote(
                "v1internal:fetchAvailableModels",
                project_id
                    .as_deref()
                    .map(|project| json!({ "project": project }))
                    .unwrap_or_else(|| json!({})),
                &material,
            )
            .await?;
        let mut remote_quota_verified = false;
        let mut quotas = if models.is_success() {
            let models_root: Value = match serde_json::from_str(&models.body) {
                Ok(value) => value,
                Err(_) => Value::Null,
            };
            parse_model_quotas(&models_root)
        } else {
            Vec::new()
        };

        // fetchAvailableModels is sometimes an availability catalogue that
        // reports every model at 100%.  CodexBar verifies that case with the
        // authoritative retrieveUserQuota RPC; otherwise the UI would show a
        // plausible-looking but unverified quota.  A permission failure keeps
        // the catalogue as a degraded availability snapshot.
        if models.is_success() && should_verify_remote_quotas(&quotas) {
            if let Ok(response) = self
                .post_remote(
                    "v1internal:retrieveUserQuota",
                    project_id
                        .as_deref()
                        .map(|project| json!({ "project": project }))
                        .unwrap_or_else(|| json!({})),
                    &material,
                )
                .await
                && response.is_success()
                && let Ok(root) = serde_json::from_str::<Value>(&response.body)
            {
                let verified = parse_remote_quota_buckets(&root);
                if has_usable_remote_quotas(&verified) {
                    quotas = merge_verified_quotas(&quotas, &verified);
                    remote_quota_verified = true;
                }
            }
        } else if models.status_code == 403 {
            if let Ok(response) = self
                .post_remote(
                    "v1internal:retrieveUserQuota",
                    project_id
                        .as_deref()
                        .map(|project| json!({ "project": project }))
                        .unwrap_or_else(|| json!({})),
                    &material,
                )
                .await
                && response.is_success()
                && let Ok(root) = serde_json::from_str::<Value>(&response.body)
            {
                let verified = parse_remote_quota_buckets(&root);
                if has_usable_remote_quotas(&verified) {
                    quotas = verified;
                    remote_quota_verified = true;
                }
            }
        }
        let quota_summary = if remote_quota_verified {
            // retrieveUserQuota is the account-scoped authoritative response;
            // do not let a model-shaped remote summary overwrite it.
            Vec::new()
        } else {
            self.post_remote(
                "v1internal:retrieveUserQuotaSummary",
                project_id
                    .as_deref()
                    .map(|project| json!({ "project": project }))
                    .unwrap_or_else(|| json!({})),
                &material,
            )
            .await
            .ok()
            .filter(UsageHttpResponse::is_success)
            .and_then(|response| serde_json::from_str::<Value>(&response.body).ok())
            .map(|root| parse_quota_summary(&root))
            .filter(|groups| has_usable_quota_summary(groups))
            .unwrap_or_default()
        };

        if quotas.is_empty() && quota_summary.is_empty() {
            if !models.is_success() {
                return Ok(map_antigravity_http_error(&models, "fetchAvailableModels"));
            }
            return Ok(invalid_payload(
                "Antigravity",
                "no quota summary or model quotas were returned",
            ));
        }

        let effective_quotas = if quota_summary.is_empty() {
            quotas
        } else {
            quotas
                .iter()
                .map(|quota| apply_summary_quota(quota, &quota_summary))
                .collect()
        };
        quotas = effective_quotas;
        let plan_type = find_plan_type(&assist_root);
        let snapshot = if quota_summary.is_empty() {
            snapshot_from_model_quotas(
                account,
                &quotas,
                Some(account.email.clone()),
                plan_type.clone(),
                if remote_quota_verified {
                    "api-verified-quota"
                } else {
                    "api-model-catalog"
                },
                if remote_quota_verified {
                    "authoritative"
                } else {
                    "degraded"
                },
            )
        } else {
            snapshot_from_quota_summary(
                account,
                &quota_summary,
                &quotas,
                Some(account.email.clone()),
                plan_type.clone(),
                "api",
            )
        };
        Ok(UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: Some(account.email.clone()),
                provider_account_id: project_id,
                plan_type,
            }),
        ))
    }
}

const LOCAL_QUOTA_SUMMARY_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";
const LOCAL_USER_STATUS_PATH: &str = "/exa.language_server_pb.LanguageServerService/GetUserStatus";
const LOCAL_COMMAND_MODEL_CONFIGS_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/GetCommandModelConfigs";
const ANTIGRAVITY_PROCESS_PATH_PATTERN: &str =
    r"(?i)[\\/](?:antigravity|antigravity-ide)(?:[\\/]|$)";
const REMOTE_BASE_URLS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com/",
    "https://daily-cloudcode-pa.googleapis.com/",
    "https://cloudcode-pa.googleapis.com/",
];

fn is_retryable_remote_status(status_code: u16) -> bool {
    matches!(status_code, 404 | 408 | 425 | 429) || (500..=599).contains(&status_code)
}

#[derive(Clone, Debug)]
struct LocalEndpoint {
    port: u16,
    csrf_token: String,
}

fn local_snapshot_score(
    summary_groups: Option<&[LocalQuotaSummaryGroup]>,
    model_quotas: &[Quota],
    observed_email: Option<&str>,
    plan_type: Option<&str>,
) -> usize {
    let mut score = if let Some(groups) = summary_groups {
        let bucket_count = groups
            .iter()
            .map(|group| group.buckets.len())
            .sum::<usize>();
        let known_bucket_count = groups
            .iter()
            .flat_map(|group| &group.buckets)
            .filter(|bucket| !bucket.disabled && bucket.remaining_fraction.is_some())
            .count();
        1_000usize
            .saturating_add(groups.len().saturating_mul(10))
            .saturating_add(bucket_count)
            .saturating_add(known_bucket_count.saturating_mul(20))
    } else {
        let known_model_count = model_quotas
            .iter()
            .filter(|quota| quota.remaining_fraction.is_some())
            .count();
        model_quotas
            .len()
            .saturating_add(known_model_count.saturating_mul(10))
    };
    if observed_email.is_some() {
        score = score.saturating_add(2);
    }
    if plan_type.is_some() {
        score = score.saturating_add(1);
    }
    score
}

fn keep_best_candidate<T>(best: &mut Option<(usize, T)>, score: usize, candidate: T) {
    if best
        .as_ref()
        .map_or(true, |(best_score, _)| score > *best_score)
    {
        *best = Some((score, candidate));
    }
}

#[derive(Clone, Debug)]
struct LocalQuotaSummaryGroup {
    buckets: Vec<LocalQuotaSummaryBucket>,
}

#[derive(Clone, Debug)]
struct LocalQuotaSummaryBucket {
    bucket_id: String,
    display_name: String,
    window: Option<String>,
    remaining_fraction: Option<f64>,
    reset_at_utc: Option<DateTime<Utc>>,
    reset_description: Option<String>,
    disabled: bool,
    group_name: String,
}

async fn local_post(
    transport: &Arc<dyn UsageHttpTransport>,
    endpoint: &LocalEndpoint,
    path: &str,
    body: Value,
) -> Option<UsageHttpResponse> {
    let url = Url::parse(&format!("https://127.0.0.1:{}{path}", endpoint.port)).ok()?;
    let body = serde_json::to_string(&body).ok()?;
    let mut headers = BTreeMap::from([
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Connect-Protocol-Version".to_owned(), "1".to_owned()),
    ]);
    if !endpoint.csrf_token.is_empty() {
        headers.insert(
            "X-Codeium-Csrf-Token".to_owned(),
            endpoint.csrf_token.clone(),
        );
    }
    transport
        .send(UsageHttpRequest {
            method: Method::POST,
            url,
            headers,
            body: Some(body),
        })
        .await
        .ok()
}

fn local_request_body() -> Value {
    json!({
        "metadata": {
            "ideName": "antigravity",
            "extensionName": "antigravity",
            "ideVersion": "unknown",
            "locale": "en"
        }
    })
}

#[cfg(windows)]
fn discover_local_endpoints() -> Vec<LocalEndpoint> {
    // Keep discovery constrained to the running language_server process. The
    // command returns only the PID-derived ports and CSRF value; the command
    // line itself is never returned or logged because it contains credentials.
    // A generic --app_data_dir flag is not sufficient proof that another
    // product's language_server belongs to Antigravity.
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$antigravityPathPattern = '__ANTIGRAVITY_PROCESS_PATH_PATTERN__'
$rows = @(
  Get-CimInstance Win32_Process |
    Where-Object {
      $executablePath = [string]$_.ExecutablePath
      $commandLine = [string]$_.CommandLine
      $_.Name -ieq 'language_server.exe' -and
      ($executablePath -match $antigravityPathPattern -or
       $commandLine -match $antigravityPathPattern)
    } |
    ForEach-Object {
      $command = [string]$_.CommandLine
      $match = [regex]::Match($command, '--csrf_token(?:=|\s+)(?:"([^"]+)"|(\S+))')
      if (-not $match.Success) { return }
      $csrf = if ($match.Groups[1].Success) { $match.Groups[1].Value } else { $match.Groups[2].Value }
      $ports = @(
        Get-NetTCPConnection -State Listen -OwningProcess $_.ProcessId -ErrorAction SilentlyContinue |
          Select-Object -ExpandProperty LocalPort -Unique |
          ForEach-Object { [int]$_ }
      )
      [pscustomobject]@{
        pid = [int]$_.ProcessId
        csrfToken = $csrf
        ports = @($ports)
      }
    }
)
ConvertTo-Json -Compress -Depth 4 -InputObject @($rows)
"#
    .replace(
        "__ANTIGRAVITY_PROCESS_PATH_PATTERN__",
        ANTIGRAVITY_PROCESS_PATH_PATTERN,
    );

    let output = match Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let root: Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let rows = match root {
        Value::Array(rows) => rows,
        value => vec![value],
    };
    let mut endpoints = Vec::new();
    for row in rows {
        let Some(csrf_token) = json_string(&row, &["csrfToken", "csrf_token"]) else {
            continue;
        };
        let ports = row
            .get("ports")
            .map(|value| match value {
                Value::Array(values) => values
                    .iter()
                    .filter_map(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
                    .filter_map(|value| u16::try_from(value).ok())
                    .collect::<Vec<_>>(),
                Value::Number(value) => value
                    .as_u64()
                    .and_then(|value| u16::try_from(value).ok())
                    .into_iter()
                    .collect(),
                Value::String(value) => value.parse::<u16>().ok().into_iter().collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        for port in ports {
            if !endpoints
                .iter()
                .any(|endpoint: &LocalEndpoint| endpoint.port == port)
            {
                endpoints.push(LocalEndpoint {
                    port,
                    csrf_token: csrf_token.clone(),
                });
            }
        }
    }
    endpoints
}

#[cfg(not(windows))]
fn discover_local_endpoints() -> Vec<LocalEndpoint> {
    Vec::new()
}

fn parse_quota_summary(root: &Value) -> Vec<LocalQuotaSummaryGroup> {
    let candidates = [root.get("response"), root.get("summary"), Some(root)];
    for candidate in candidates.into_iter().flatten() {
        let payload = candidate.get("quotaSummary").unwrap_or(candidate);
        let Some(groups) = payload.get("groups").and_then(Value::as_array) else {
            continue;
        };
        let parsed = groups
            .iter()
            .filter_map(parse_quota_summary_group)
            .collect::<Vec<_>>();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    Vec::new()
}

fn has_usable_quota_summary(groups: &[LocalQuotaSummaryGroup]) -> bool {
    groups.iter().any(|group| {
        group
            .buckets
            .iter()
            .any(|bucket| !bucket.disabled && bucket.remaining_fraction.is_some())
    })
}

fn parse_quota_summary_group(value: &Value) -> Option<LocalQuotaSummaryGroup> {
    let display_name =
        json_string(value, &["displayName", "display_name", "name"]).unwrap_or_default();
    let buckets = value
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bucket| parse_quota_summary_bucket(bucket, &display_name))
        .collect::<Vec<_>>();
    (!buckets.is_empty()).then_some(LocalQuotaSummaryGroup { buckets })
}

fn parse_quota_summary_bucket(value: &Value, group_name: &str) -> Option<LocalQuotaSummaryBucket> {
    let display_name =
        json_string(value, &["displayName", "display_name", "name"]).unwrap_or_default();
    let bucket_id = json_string(value, &["bucketId", "bucket_id", "id"])
        .or_else(|| (!display_name.is_empty()).then(|| display_name.clone()))?;
    let remaining_fraction = json_number(value, &["remainingFraction", "remaining_fraction"])
        .or_else(|| value.get("remaining").and_then(parse_remaining_fraction));
    Some(LocalQuotaSummaryBucket {
        bucket_id,
        display_name,
        window: json_string(value, &["window"]),
        remaining_fraction,
        reset_at_utc: parse_date_value(value.get("resetTime").or_else(|| value.get("reset_time"))),
        reset_description: json_string(value, &["description"]),
        disabled: crate::providers::shared::json_bool(value, &["disabled"]).unwrap_or(false),
        group_name: group_name.to_owned(),
    })
}

fn parse_remaining_fraction(value: &Value) -> Option<f64> {
    if let Some(value) = json_number(value, &["remainingFraction", "remaining_fraction"]) {
        return Some(value);
    }
    let is_remaining_fraction = json_string(value, &["case"])
        .is_some_and(|case| case.eq_ignore_ascii_case("remainingFraction"));
    is_remaining_fraction.then(|| json_number(value, &["value"]))?
}

fn parse_date_value(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Ok(seconds) = text.parse::<f64>() {
            return timestamp_to_utc(seconds);
        }
    }
    value.as_f64().and_then(timestamp_to_utc)
}

fn timestamp_to_utc(value: f64) -> Option<DateTime<Utc>> {
    if !value.is_finite() {
        return None;
    }
    if value.abs() >= 100_000_000_000.0 {
        DateTime::from_timestamp_millis(value as i64)
    } else {
        DateTime::from_timestamp(value as i64, 0)
    }
}

fn parse_local_model_quotas(root: &Value) -> Vec<Quota> {
    let status = root.get("userStatus").or_else(|| {
        root.get("response")
            .and_then(|value| value.get("userStatus"))
    });
    let configs = status
        .and_then(|status| status.get("cascadeModelConfigData"))
        .or_else(|| status.and_then(|status| status.get("cascade_model_config_data")))
        .and_then(|data| {
            data.get("clientModelConfigs")
                .or_else(|| data.get("client_model_configs"))
        })
        .and_then(Value::as_array)
        .or_else(|| {
            root.get("clientModelConfigs")
                .or_else(|| root.get("client_model_configs"))
                .and_then(Value::as_array)
        })
        .or_else(|| {
            root.get("response")
                .and_then(|response| {
                    response
                        .get("clientModelConfigs")
                        .or_else(|| response.get("client_model_configs"))
                })
                .and_then(Value::as_array)
        });
    configs
        .into_iter()
        .flatten()
        .filter_map(|config| {
            let quota_info = config
                .get("quotaInfo")
                .or_else(|| config.get("quota_info"))?;
            let model = config
                .get("modelOrAlias")
                .or_else(|| config.get("model_or_alias"))
                .and_then(|value| json_string(value, &["model", "id"]))
                .or_else(|| json_string(config, &["model", "modelId", "model_id"]))?;
            let label = json_string(config, &["label", "displayName", "display_name"])
                .unwrap_or_else(|| model.clone());
            let remaining = json_number(quota_info, &["remainingFraction", "remaining_fraction"]);
            let reset = parse_date_value(
                quota_info
                    .get("resetTime")
                    .or_else(|| quota_info.get("reset_time")),
            );
            Some(to_quota(&model, label, remaining, reset))
        })
        .collect()
}

fn should_verify_remote_quotas(quotas: &[Quota]) -> bool {
    !quotas.is_empty()
        && quotas.iter().all(|quota| {
            quota
                .remaining_fraction
                .is_some_and(|remaining| remaining >= 0.999)
        })
}

fn has_usable_remote_quotas(quotas: &[Quota]) -> bool {
    quotas
        .iter()
        .any(|quota| quota.remaining_fraction.is_some())
}

fn parse_remote_quota_buckets(root: &Value) -> Vec<Quota> {
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

fn merge_verified_quotas(catalog: &[Quota], verified: &[Quota]) -> Vec<Quota> {
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

fn find_local_email(root: &Value) -> Option<String> {
    let status = root.get("userStatus").or_else(|| {
        root.get("response")
            .and_then(|value| value.get("userStatus"))
    })?;
    json_string(status, &["email"])
}

fn find_local_plan_type(root: &Value) -> Option<String> {
    let status = root.get("userStatus").or_else(|| {
        root.get("response")
            .and_then(|value| value.get("userStatus"))
    })?;
    status
        .get("userTier")
        .or_else(|| status.get("user_tier"))
        .and_then(tier_label)
        .or_else(|| {
            status
                .get("planStatus")
                .or_else(|| status.get("plan_status"))
                .and_then(|plan| plan.get("planInfo").or_else(|| plan.get("plan_info")))
                .and_then(|info| {
                    json_string(
                        info,
                        &[
                            "planDisplayName",
                            "displayName",
                            "productName",
                            "planName",
                            "planShortName",
                            "planType",
                            "name",
                            "id",
                        ],
                    )
                })
        })
}

#[derive(Clone, Copy)]
enum LocalBucketKind {
    Session,
    Weekly,
    Other,
}

fn local_bucket_kind(bucket: &LocalQuotaSummaryBucket) -> LocalBucketKind {
    let values = [
        bucket.bucket_id.as_str(),
        bucket.display_name.as_str(),
        bucket.window.as_deref().unwrap_or_default(),
    ];
    for value in values {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        let tokens = normalized
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        let is_session = tokens.iter().any(|token| {
            matches!(
                *token,
                "session" | "5h" | "5hr" | "5hrs" | "5hour" | "5hours"
            )
        }) || tokens
            .windows(2)
            .any(|pair| pair[0] == "five" && pair[1] == "hour");
        if is_session {
            return LocalBucketKind::Session;
        }
        if tokens
            .iter()
            .any(|token| matches!(*token, "week" | "weekly"))
        {
            return LocalBucketKind::Weekly;
        }
    }
    LocalBucketKind::Other
}

fn local_group_title(group_name: &str) -> String {
    let lower = group_name.trim().to_ascii_lowercase();
    if lower.contains("gemini") {
        "Gemini".to_owned()
    } else if lower.contains("claude") || lower.contains("gpt") {
        "Claude/GPT".to_owned()
    } else if group_name.trim().is_empty() {
        "Quota".to_owned()
    } else {
        group_name.trim().to_owned()
    }
}

fn local_bucket_title(bucket: &LocalQuotaSummaryBucket) -> String {
    match local_bucket_kind(bucket) {
        LocalBucketKind::Session => "5-hour".to_owned(),
        LocalBucketKind::Weekly => "weekly".to_owned(),
        LocalBucketKind::Other => bucket.display_name.clone(),
    }
}

fn local_window_minutes(bucket: &LocalQuotaSummaryBucket) -> Option<i64> {
    match local_bucket_kind(bucket) {
        LocalBucketKind::Session => Some(300),
        LocalBucketKind::Weekly => Some(10_080),
        LocalBucketKind::Other => None,
    }
}

fn local_bucket_window(
    bucket: &LocalQuotaSummaryBucket,
    kind: UsageWindowKind,
    name: String,
) -> RateLimitWindow {
    let used_percent = bucket
        .remaining_fraction
        .map(|remaining| ((1.0 - remaining) * 100.0).clamp(0.0, 100.0))
        .unwrap_or(0.0);
    RateLimitWindow {
        kind,
        name,
        used_percent,
        reset_at_utc: bucket.reset_at_utc,
        limit_window_seconds: local_window_seconds(bucket),
    }
}

fn local_window_seconds(bucket: &LocalQuotaSummaryBucket) -> i64 {
    match local_bucket_kind(bucket) {
        LocalBucketKind::Session => 5 * 60 * 60,
        LocalBucketKind::Weekly => 7 * 24 * 60 * 60,
        LocalBucketKind::Other => 0,
    }
}

fn local_bucket_metric(bucket: &LocalQuotaSummaryBucket) -> UsageMetric {
    let group_title = local_group_title(&bucket.group_name);
    let bucket_title = local_bucket_title(bucket);
    let usage_known = !bucket.disabled && bucket.remaining_fraction.is_some();
    let used_percent = usage_known
        .then(|| bucket.remaining_fraction)
        .flatten()
        .map(|remaining| ((1.0 - remaining) * 100.0).clamp(0.0, 100.0));
    let mut metadata = HashMap::from([
        ("source".to_owned(), "local-quota-summary".to_owned()),
        ("group".to_owned(), group_title.clone()),
        ("raw_group".to_owned(), bucket.group_name.clone()),
        ("bucket_id".to_owned(), bucket.bucket_id.clone()),
        ("raw_bucket".to_owned(), bucket.display_name.clone()),
        ("usage_known".to_owned(), usage_known.to_string()),
    ]);
    if let Some(remaining) = bucket.remaining_fraction {
        metadata.insert("remaining_fraction".to_owned(), remaining.to_string());
    }
    if let Some(minutes) = local_window_minutes(bucket) {
        metadata.insert("window_minutes".to_owned(), minutes.to_string());
    }
    let window_seconds = local_window_seconds(bucket);
    if window_seconds > 0 {
        metadata.insert("window_seconds".to_owned(), window_seconds.to_string());
    }
    if let Some(description) = bucket.reset_description.as_deref() {
        metadata.insert("reset_description".to_owned(), description.to_owned());
    }
    UsageMetric {
        key: format!("antigravity-quota-summary-{}", bucket.bucket_id),
        name: format!("{group_title} {bucket_title}"),
        used_percent,
        used_amount: used_percent,
        limit_amount: usage_known.then_some(100.0),
        remaining_amount: usage_known
            .then(|| bucket.remaining_fraction)
            .flatten()
            .map(|value| value * 100.0),
        unit: usage_known.then(|| "percent".to_owned()),
        reset_at_utc: bucket.reset_at_utc,
        reset_label: bucket.reset_description.clone(),
        metadata,
    }
}

fn quota_metric(quota: &Quota) -> UsageMetric {
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

fn snapshot_from_quota_summary(
    account: &AccountRecord,
    groups: &[LocalQuotaSummaryGroup],
    models: &[Quota],
    observed_email: Option<String>,
    plan_type: Option<String>,
    source: &str,
) -> UsageSnapshot {
    let buckets = groups
        .iter()
        .flat_map(|group| group.buckets.iter())
        .collect::<Vec<_>>();
    let all_windows = buckets
        .iter()
        .filter(|bucket| !bucket.disabled && bucket.remaining_fraction.is_some())
        .map(|bucket| {
            let group_title = local_group_title(&bucket.group_name);
            let bucket_title = local_bucket_title(bucket);
            AdditionalRateLimitWindow {
                key: format!("antigravity-quota-summary-{}", bucket.bucket_id),
                name: format!("{group_title} {bucket_title}"),
                window: local_bucket_window(
                    bucket,
                    UsageWindowKind::Additional,
                    format!("{group_title} {bucket_title}"),
                ),
            }
        })
        .collect::<Vec<_>>();

    let representative = |family: &str, kind: UsageWindowKind| {
        buckets
            .iter()
            .filter(|bucket| {
                !bucket.disabled
                    && bucket.remaining_fraction.is_some()
                    && local_group_title(&bucket.group_name)
                        .to_ascii_lowercase()
                        .contains(family)
            })
            .max_by(|left, right| {
                let left_used = left
                    .remaining_fraction
                    .map(|value| 1.0 - value)
                    .unwrap_or_default();
                let right_used = right
                    .remaining_fraction
                    .map(|value| 1.0 - value)
                    .unwrap_or_default();
                left_used.total_cmp(&right_used)
            })
            .map(|bucket| {
                let group_title = local_group_title(&bucket.group_name);
                let bucket_title = local_bucket_title(bucket);
                local_bucket_window(bucket, kind, format!("{group_title} {bucket_title}"))
            })
    };
    let primary = representative("gemini", UsageWindowKind::Primary).or_else(|| {
        buckets
            .iter()
            .find(|bucket| !bucket.disabled && bucket.remaining_fraction.is_some())
            .map(|bucket| {
                let group_title = local_group_title(&bucket.group_name);
                let bucket_title = local_bucket_title(bucket);
                local_bucket_window(
                    bucket,
                    UsageWindowKind::Primary,
                    format!("{group_title} {bucket_title}"),
                )
            })
    });
    let secondary = representative("claude/gpt", UsageWindowKind::Secondary);

    let effective_models = models
        .iter()
        .map(|quota| apply_summary_quota(quota, groups))
        .collect::<Vec<_>>();
    let mut metrics = buckets
        .iter()
        .map(|bucket| local_bucket_metric(bucket))
        .collect::<Vec<_>>();
    metrics.extend(effective_models.iter().map(quota_metric));

    UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: account.workspace_id.clone(),
        plan_type,
        primary,
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary,
        additional_windows: all_windows,
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
        data_confidence: "authoritative".to_owned(),
    }
}

fn snapshot_from_model_quotas(
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
    let metrics = ordered.iter().map(quota_metric).collect::<Vec<_>>();

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

fn local_identity_matches(account: &AccountRecord, observed_email: Option<&str>) -> bool {
    let expected = account.email.trim();
    observed_email.is_some_and(|observed| {
        !expected.is_empty()
            && !observed.trim().is_empty()
            && expected.eq_ignore_ascii_case(observed.trim())
    })
}

#[derive(Clone)]
struct Quota {
    key: String,
    name: String,
    remaining_fraction: Option<f64>,
    used_percent: f64,
    window: RateLimitWindow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AntigravityQuotaPool {
    Gemini,
    ClaudeGpt,
}

fn model_is_summary_eligible(quota: &Quota) -> bool {
    let model_id = quota.key.to_ascii_lowercase();
    let label = quota.name.to_ascii_lowercase();
    !model_id.starts_with("tab_")
        && ![&model_id, &label].iter().any(|text| {
            text.contains("lite") || text.contains("autocomplete") || text.contains("image")
        })
}

fn model_quota_pool(quota: &Quota) -> Option<AntigravityQuotaPool> {
    if !model_is_summary_eligible(quota) {
        return None;
    }
    model_quota_family(quota)
}

fn model_quota_family(quota: &Quota) -> Option<AntigravityQuotaPool> {
    let model = format!("{} {}", quota.key, quota.name).to_ascii_lowercase();
    if model.contains("claude") || model.contains("gpt") || model.contains("openai") {
        Some(AntigravityQuotaPool::ClaudeGpt)
    } else if model.contains("gemini") && (model.contains("pro") || model.contains("flash")) {
        Some(AntigravityQuotaPool::Gemini)
    } else {
        None
    }
}

fn model_quota_mirror_pool(quota: &Quota) -> Option<AntigravityQuotaPool> {
    model_quota_family(quota).or_else(|| {
        let model_id = quota.key.to_ascii_lowercase();
        let label = quota.name.to_ascii_lowercase();
        (model_id.starts_with("tab_")
            || model_id.contains("autocomplete")
            || label.contains("autocomplete"))
        .then_some(AntigravityQuotaPool::Gemini)
    })
}

fn known_remaining_fraction(quota: &Quota) -> Option<f64> {
    quota
        .remaining_fraction
        .filter(|fraction| fraction.is_finite())
        .map(|fraction| fraction.clamp(0.0, 1.0))
}

fn compare_quota_representatives(left: &Quota, right: &Quota) -> std::cmp::Ordering {
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

fn model_quota_representative_index(models: &[Quota], pool: AntigravityQuotaPool) -> Option<usize> {
    models
        .iter()
        .enumerate()
        .filter(|(_, quota)| {
            model_quota_pool(quota) == Some(pool) && known_remaining_fraction(quota).is_some()
        })
        .min_by(|(_, left), (_, right)| compare_quota_representatives(left, right))
        .map(|(index, _)| index)
}

fn local_unknown_model_representative_index(models: &[Quota]) -> Option<usize> {
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

fn remote_quota_mirrors_pool(quota: &Quota, pool_quota: &Quota) -> bool {
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

fn should_show_model_quota_window(
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

fn apply_summary_quota(quota: &Quota, groups: &[LocalQuotaSummaryGroup]) -> Quota {
    let Some(bucket) = select_summary_bucket(quota, groups) else {
        return quota.clone();
    };
    let Some(remaining_fraction) = bucket.remaining_fraction else {
        return quota.clone();
    };
    let used_percent = ((1.0 - remaining_fraction) * 100.0).clamp(0.0, 100.0);
    let mut next = quota.clone();
    next.remaining_fraction = Some(remaining_fraction);
    next.used_percent = used_percent;
    next.window.used_percent = used_percent;
    next.window.reset_at_utc = bucket.reset_at_utc;
    next.window.limit_window_seconds = local_window_seconds(bucket);
    next
}

fn select_summary_bucket<'a>(
    quota: &Quota,
    groups: &'a [LocalQuotaSummaryGroup],
) -> Option<&'a LocalQuotaSummaryBucket> {
    let model_lower = format!("{} {}", quota.key, quota.name).to_ascii_lowercase();
    let is_third_party = model_lower.contains("claude") || model_lower.contains("gpt");
    let mut session = None;
    let mut weekly = None;

    for group in groups {
        let group_lower = group
            .buckets
            .first()
            .map(|bucket| bucket.group_name.to_ascii_lowercase())
            .unwrap_or_default();
        let group_is_third_party = group_lower.contains("claude")
            || group_lower.contains("gpt")
            || group_lower.contains("3p");
        if group_is_third_party != is_third_party {
            continue;
        }
        for bucket in &group.buckets {
            if bucket.disabled || bucket.remaining_fraction.is_none() {
                continue;
            }
            match local_bucket_kind(bucket) {
                LocalBucketKind::Session => {
                    session = more_constrained_bucket(session, Some(bucket));
                }
                LocalBucketKind::Weekly => {
                    weekly = more_constrained_bucket(weekly, Some(bucket));
                }
                LocalBucketKind::Other => {}
            }
        }
    }

    match (session, weekly) {
        (Some(_session), Some(weekly)) if weekly.remaining_fraction <= Some(0.001) => Some(weekly),
        (Some(session), Some(weekly)) => {
            if session.remaining_fraction <= weekly.remaining_fraction {
                Some(session)
            } else {
                Some(weekly)
            }
        }
        (Some(session), None) => Some(session),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    }
}

fn more_constrained_bucket<'a>(
    current: Option<&'a LocalQuotaSummaryBucket>,
    candidate: Option<&'a LocalQuotaSummaryBucket>,
) -> Option<&'a LocalQuotaSummaryBucket> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => {
            if candidate.remaining_fraction < current.remaining_fraction {
                Some(candidate)
            } else {
                Some(current)
            }
        }
        (None, candidate) => candidate,
        (current, None) => current,
    }
}

fn parse_model_quotas(root: &Value) -> Vec<Quota> {
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

fn to_quota(
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

fn parse_date(value: &Value, names: &[&str]) -> Option<DateTime<Utc>> {
    names
        .iter()
        .find_map(|name| parse_date_value(value.get(*name)))
}

fn find_project_id(root: &Value) -> Option<String> {
    let project = root
        .get("cloudaicompanionProject")
        .or_else(|| root.get("response")?.get("cloudaicompanionProject"))?;
    if let Some(value) = project.as_str() {
        return Some(value.to_owned());
    }
    json_string(project, &["id", "projectId", "project_id"])
}

fn find_onboard_tier(root: &Value) -> Option<String> {
    let payload = root.get("response").unwrap_or(root);
    let allowed_tiers = payload
        .get("allowedTiers")
        .or_else(|| payload.get("allowed_tiers"))
        .and_then(Value::as_array);
    allowed_tiers
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|tier| {
                    tier.get("isDefault")
                        .or_else(|| tier.get("is_default"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        && json_string(tier, &["id", "tierId", "tier_id"]).is_some()
                })
                .or_else(|| {
                    tiers
                        .iter()
                        .find(|tier| json_string(tier, &["id", "tierId", "tier_id"]).is_some())
                })
        })
        .and_then(|tier| json_string(tier, &["id", "tierId", "tier_id"]))
        .or_else(|| {
            payload
                .get("paidTier")
                .or_else(|| payload.get("paid_tier"))
                .and_then(|tier| json_string(tier, &["id", "tierId", "tier_id"]))
        })
        .or_else(|| {
            payload
                .get("currentTier")
                .or_else(|| payload.get("current_tier"))
                .and_then(|tier| json_string(tier, &["id", "tierId", "tier_id"]))
        })
}

fn find_plan_type(root: &Value) -> Option<String> {
    // `loadCodeAssist` has two generations of tier fields.  The newer
    // `paidTier` is the authoritative Google subscription (for example
    // `Google AI Pro` / `Google AI Ultra`), while `planInfo` is the legacy
    // Windsurf/Codeium-compatible field and may only say `Pro` for every
    // paid tier.  Keep the old fields as fallbacks for older responses.
    let payloads = [root.get("response"), Some(root)];
    for payload in payloads.into_iter().flatten() {
        if let Some(label) = payload
            .get("paidTier")
            .or_else(|| payload.get("paid_tier"))
            .and_then(tier_label)
        {
            return Some(label);
        }
        if let Some(label) = payload
            .get("currentTier")
            .or_else(|| payload.get("current_tier"))
            .and_then(tier_label)
        {
            return Some(label);
        }
        if let Some(label) = payload
            .get("planInfo")
            .or_else(|| payload.get("plan_info"))
            .and_then(|value| {
                json_string(
                    value,
                    &[
                        "planDisplayName",
                        "displayName",
                        "productName",
                        "planName",
                        "planShortName",
                        "planType",
                        "plan_type",
                        "name",
                        "id",
                    ],
                )
            })
        {
            return Some(label);
        }
        if let Some(label) = payload
            .get("allowedTiers")
            .or_else(|| payload.get("allowed_tiers"))
            .and_then(Value::as_array)
            .and_then(|tiers| {
                tiers.iter().find(|tier| {
                    json_string(tier, &["id", "tierId", "tier_id"])
                        .is_some_and(|id| id.eq_ignore_ascii_case("free-tier"))
                        || tier
                            .get("isDefault")
                            .or_else(|| tier.get("is_default"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                })
            })
            .and_then(tier_label)
        {
            return Some(label);
        }
    }
    None
}

/// Return a human-readable tier label without losing the machine-readable id
/// when a response omits `name`.  Some versions return the tier itself as a
/// string, while newer responses return `{ id, name, ... }`.
fn tier_label(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            json_string(
                value,
                &[
                    "name",
                    "displayName",
                    "display_name",
                    "productName",
                    "planName",
                    "plan_name",
                    "id",
                    "tierId",
                    "tier_id",
                    "slug",
                ],
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_quota_summary_preserves_families_and_cadence_windows() {
        let root = json!({
            "response": {
                "groups": [
                    {
                        "displayName": "Gemini Models",
                        "buckets": [
                            {
                                "bucketId": "gemini-weekly",
                                "displayName": "Weekly Limit Remaining",
                                "remainingFraction": 0.43,
                                "resetTime": "2030-01-08T00:00:00Z"
                            },
                            {
                                "bucketId": "gemini-5h",
                                "displayName": "Five Hour Limit Remaining",
                                "remainingFraction": 0.97,
                                "resetTime": "2030-01-01T05:00:00Z"
                            }
                        ]
                    },
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {
                                "bucketId": "3p-weekly",
                                "displayName": "Weekly Limit Remaining",
                                "remainingFraction": 0.57
                            },
                            {
                                "bucketId": "3p-5h",
                                "displayName": "Five Hour Limit Remaining",
                                "remainingFraction": 1.0
                            }
                        ]
                    }
                ]
            }
        });

        let groups = parse_quota_summary(&root);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].buckets.len(), 2);
        assert!(matches!(
            local_bucket_kind(&groups[0].buckets[0]),
            LocalBucketKind::Weekly
        ));
        assert!(matches!(
            local_bucket_kind(&groups[0].buckets[1]),
            LocalBucketKind::Session
        ));

        let account =
            AccountRecord::create("local", "local@example.com", None, ANTIGRAVITY, None).unwrap();
        let models = vec![to_quota(
            "MODEL_PLACEHOLDER_M1",
            "Gemini Pro".to_owned(),
            Some(0.75),
            None,
        )];
        let snapshot = snapshot_from_quota_summary(
            &account,
            &groups,
            &models,
            Some("local@example.com".to_owned()),
            Some("Google AI Pro".to_owned()),
            "local",
        );

        assert_eq!(snapshot.metrics.len(), 5);
        assert_eq!(snapshot.additional_windows.len(), 4);
        assert_eq!(snapshot.primary.as_ref().unwrap().name, "Gemini weekly");
        assert_eq!(
            snapshot.secondary.as_ref().unwrap().name,
            "Claude/GPT weekly"
        );
        assert!((snapshot.metrics[0].used_percent.unwrap() - 57.0).abs() < 1e-9);
        assert!((snapshot.metrics[1].used_percent.unwrap() - 3.0).abs() < 1e-9);
        assert!((snapshot.metrics[2].used_percent.unwrap() - 43.0).abs() < 1e-9);
        assert!((snapshot.metrics[3].used_percent.unwrap() - 0.0).abs() < 1e-9);
        assert_eq!(snapshot.metrics[4].name, "Gemini Pro");
        assert!((snapshot.metrics[4].used_percent.unwrap() - 57.0).abs() < 1e-9);
        assert_eq!(
            snapshot.additional_windows[0].window.limit_window_seconds,
            604_800
        );
        assert_eq!(
            snapshot.additional_windows[1].window.limit_window_seconds,
            18_000
        );
        assert_eq!(snapshot.source.as_deref(), Some("local"));
    }

    #[test]
    fn summary_bucket_window_uses_absolute_reset_and_fixed_duration() {
        let root = json!({
            "groups": [{
                "displayName": "Gemini Models",
                "buckets": [{
                    "bucketId": "gemini-5h",
                    "window": "FIVE_HOUR",
                    "remainingFraction": 0.8,
                    "resetTime": 1_704_067_200_000_i64
                }]
            }]
        });
        let groups = parse_quota_summary(&root);
        let bucket = &groups[0].buckets[0];
        assert_eq!(
            bucket.reset_at_utc,
            Some(DateTime::from_timestamp(1_704_067_200, 0).unwrap())
        );
        let window =
            local_bucket_window(bucket, UsageWindowKind::Primary, "Gemini 5-hour".to_owned());
        assert_eq!(window.limit_window_seconds, 18_000);
        assert_eq!(
            window.seconds_until_reset(DateTime::from_timestamp(1_704_063_600, 0).unwrap()),
            Some(3_600)
        );
    }

    #[test]
    fn unknown_or_disabled_summary_buckets_keep_reset_context_without_fake_windows() {
        let root = json!({
            "groups": [{
                "displayName": "Gemini Models",
                "buckets": [
                    {
                        "bucketId": "gemini-5h-limit",
                        "displayName": "5h limit",
                        "remainingFraction": 0.8,
                        "resetTime": "2030-01-01T05:00:00Z"
                    },
                    {
                        "bucketId": "gemini-weekly",
                        "displayName": "Weekly Limit",
                        "remainingFraction": 0.25,
                        "disabled": true,
                        "resetTime": "2030-01-08T00:00:00Z"
                    },
                    {
                        "bucketId": "3p-5h",
                        "displayName": "Five Hour Limit",
                        "resetTime": "2030-01-01T05:00:00Z"
                    }
                ]
            }]
        });
        let groups = parse_quota_summary(&root);
        let account =
            AccountRecord::create("local", "local@example.com", None, ANTIGRAVITY, None).unwrap();

        let snapshot = snapshot_from_quota_summary(
            &account,
            &groups,
            &[],
            Some("local@example.com".to_owned()),
            None,
            "local",
        );

        assert_eq!(
            snapshot.primary.as_ref().unwrap().limit_window_seconds,
            18_000
        );
        assert_eq!(snapshot.additional_windows.len(), 1);
        for bucket_id in ["gemini-weekly", "3p-5h"] {
            let metric = snapshot
                .metrics
                .iter()
                .find(|metric| {
                    metric.metadata.get("bucket_id").map(String::as_str) == Some(bucket_id)
                })
                .unwrap();
            assert_eq!(metric.used_percent, None);
            assert_eq!(metric.remaining_amount, None);
            assert_eq!(metric.unit, None);
            assert_eq!(
                metric.metadata.get("usage_known").map(String::as_str),
                Some("false")
            );
            assert!(metric.reset_at_utc.is_some());
        }
    }

    #[test]
    fn weekly_zero_bucket_wins_over_five_hour_bucket() {
        let root = json!({
            "groups": [{
                "displayName": "Claude and GPT models",
                "buckets": [
                    {"bucketId": "3p-5h", "remainingFraction": 0.01},
                    {"bucketId": "3p-weekly", "remainingFraction": 0.0}
                ]
            }]
        });
        let groups = parse_quota_summary(&root);
        let model = to_quota("claude-sonnet", "Claude Sonnet".to_owned(), Some(1.0), None);
        let effective = apply_summary_quota(&model, &groups);
        assert_eq!(effective.remaining_fraction, Some(0.0));
        assert_eq!(effective.used_percent, 100.0);
        assert_eq!(effective.window.limit_window_seconds, 604_800);
    }

    #[test]
    fn subscription_name_prefers_paid_google_tier_over_legacy_plan_info() {
        let root = json!({
            "cloudaicompanionProject": "project-1",
            "currentTier": {"id": "free-tier", "name": "Antigravity Starter Quota"},
            "paidTier": {"id": "g1-ultra-tier", "name": "Google AI Ultra"},
            "planInfo": {"planName": "Pro", "planType": "PAID"}
        });

        assert_eq!(find_plan_type(&root).as_deref(), Some("Google AI Ultra"));
    }

    #[test]
    fn subscription_name_falls_back_to_current_tier_and_allowed_free_tier() {
        let current = json!({
            "currentTier": {"id": "g1-pro-tier", "name": "Google AI Pro"}
        });
        assert_eq!(find_plan_type(&current).as_deref(), Some("Google AI Pro"));

        let free = json!({
            "allowedTiers": [
                {"id": "free-tier", "name": "Antigravity Starter Quota", "isDefault": true}
            ]
        });
        assert_eq!(
            find_plan_type(&free).as_deref(),
            Some("Antigravity Starter Quota")
        );
    }

    #[test]
    fn local_user_tier_name_beats_legacy_plan_name() {
        let root = json!({
            "userStatus": {
                "userTier": {"id": "g1-pro-tier", "name": "Google AI Pro"},
                "planStatus": {"planInfo": {"planName": "Pro"}}
            }
        });

        assert_eq!(
            find_local_plan_type(&root).as_deref(),
            Some("Google AI Pro")
        );
    }

    #[test]
    fn local_model_fallback_accepts_command_config_envelope() {
        let root = json!({
            "response": {
                "clientModelConfigs": [{
                    "modelOrAlias": {"model": "gemini-pro"},
                    "label": "Gemini Pro",
                    "quotaInfo": {
                        "remainingFraction": 0.62,
                        "resetTime": "2030-01-01T05:00:00Z"
                    }
                }]
            }
        });
        let quotas = parse_local_model_quotas(&root);
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].key, "gemini-pro");
        assert_eq!(quotas[0].used_percent, 38.0);
    }

    #[test]
    fn model_quota_snapshot_uses_constrained_gemini_and_claude_gpt_representatives() {
        let account =
            AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
        let quotas = vec![
            to_quota(
                "gemini-pro",
                "Gemini Pro".to_owned(),
                Some(0.4),
                Some(
                    DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
                        .unwrap()
                        .into(),
                ),
            ),
            to_quota(
                "gemini-flash",
                "Gemini Flash".to_owned(),
                Some(0.2),
                Some(
                    DateTime::parse_from_rfc3339("2030-01-01T06:00:00Z")
                        .unwrap()
                        .into(),
                ),
            ),
            to_quota("claude-sonnet", "Claude Sonnet".to_owned(), Some(0.6), None),
            to_quota("gpt-oss-120b", "GPT-OSS 120B".to_owned(), Some(0.3), None),
            to_quota("gemini-image", "Gemini Image".to_owned(), Some(0.0), None),
            to_quota(
                "tab_gemini_autocomplete",
                "Gemini autocomplete".to_owned(),
                Some(0.0),
                None,
            ),
        ];

        let snapshot = snapshot_from_model_quotas(
            &account,
            &quotas,
            Some(account.email.clone()),
            None,
            "local-legacy",
            "authoritative",
        );

        let primary = snapshot.primary.as_ref().unwrap();
        assert_eq!(primary.name, "Gemini Flash");
        assert_eq!(primary.used_percent, 80.0);
        assert_eq!(primary.reset_at_utc, quotas[1].window.reset_at_utc);
        let secondary = snapshot.secondary.as_ref().unwrap();
        assert_eq!(secondary.name, "GPT-OSS 120B");
        assert_eq!(secondary.used_percent, 70.0);
        let mut additional_keys = snapshot
            .additional_windows
            .iter()
            .map(|window| window.key.as_str())
            .collect::<Vec<_>>();
        additional_keys.sort_unstable();
        assert_eq!(additional_keys, ["gemini-image", "tab_gemini_autocomplete"]);
        assert_eq!(snapshot.metrics.len(), quotas.len());
    }

    #[test]
    fn remote_model_windows_hide_only_exact_pool_mirrors() {
        let account =
            AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
        let reset = Some(
            DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
                .unwrap()
                .into(),
        );
        let quotas = vec![
            to_quota("gemini-flash", "Gemini Flash".to_owned(), Some(0.7), reset),
            to_quota("gpt-oss-120b", "GPT-OSS 120B".to_owned(), Some(0.8), reset),
            to_quota(
                "gemini-3.7-flash-image",
                "Gemini 3.7 Flash Image".to_owned(),
                Some(0.7),
                reset,
            ),
            to_quota(
                "gemini-3.7-flash-image-no-reset",
                "Gemini 3.7 Flash Image No Reset".to_owned(),
                Some(0.7),
                None,
            ),
            to_quota(
                "gemini-3.7-flash-image-different",
                "Gemini 3.7 Flash Image Different".to_owned(),
                Some(0.6),
                reset,
            ),
            to_quota(
                "gemini-3.7-flash-image-reset-only",
                "Gemini 3.7 Flash Image Reset Only".to_owned(),
                None,
                reset,
            ),
            to_quota(
                "gemini-3.7-flash-image-full",
                "Gemini 3.7 Flash Image Full".to_owned(),
                Some(1.0),
                reset,
            ),
            to_quota(
                "claude-sonnet-image",
                "Claude Sonnet Image".to_owned(),
                Some(0.8),
                reset,
            ),
            to_quota(
                "claude-sonnet-image-different",
                "Claude Sonnet Image Different".to_owned(),
                Some(0.6),
                reset,
            ),
            to_quota(
                "tab_gemini_autocomplete",
                "Gemini autocomplete".to_owned(),
                Some(0.7),
                reset,
            ),
        ];

        let local = snapshot_from_model_quotas(
            &account,
            &quotas,
            Some(account.email.clone()),
            None,
            "local-legacy",
            "authoritative",
        );
        let remote = snapshot_from_model_quotas(
            &account,
            &quotas,
            Some(account.email.clone()),
            None,
            "api-model-catalog",
            "authoritative",
        );

        let mut local_keys = local
            .additional_windows
            .iter()
            .map(|window| window.key.as_str())
            .collect::<Vec<_>>();
        local_keys.sort_unstable();
        assert_eq!(
            local_keys,
            [
                "claude-sonnet-image",
                "claude-sonnet-image-different",
                "gemini-3.7-flash-image",
                "gemini-3.7-flash-image-different",
                "gemini-3.7-flash-image-no-reset",
                "gemini-3.7-flash-image-reset-only",
                "tab_gemini_autocomplete",
            ]
        );

        let mut remote_keys = remote
            .additional_windows
            .iter()
            .map(|window| window.key.as_str())
            .collect::<Vec<_>>();
        remote_keys.sort_unstable();
        assert_eq!(
            remote_keys,
            [
                "claude-sonnet-image-different",
                "gemini-3.7-flash-image-different",
                "gemini-3.7-flash-image-no-reset",
                "gemini-3.7-flash-image-reset-only",
            ]
        );
        assert_eq!(local.metrics.len(), quotas.len());
        assert_eq!(remote.metrics.len(), quotas.len());
    }

    #[test]
    fn unknown_model_fallback_is_local_only() {
        let account =
            AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
        let quotas = vec![to_quota(
            "experimental-text-model",
            "Experimental text model".to_owned(),
            Some(0.7),
            None,
        )];

        let local = snapshot_from_model_quotas(
            &account,
            &quotas,
            Some(account.email.clone()),
            None,
            "local-legacy",
            "authoritative",
        );
        let remote = snapshot_from_model_quotas(
            &account,
            &quotas,
            Some(account.email.clone()),
            None,
            "api-model-catalog",
            "degraded",
        );

        assert_eq!(
            local.primary.as_ref().map(|window| window.name.as_str()),
            Some("Experimental text model")
        );
        assert!(remote.primary.is_none());
        assert_eq!(remote.metrics.len(), 1);
    }

    #[test]
    fn local_identity_matching_is_required_for_account_scoped_usage() {
        let account =
            AccountRecord::create("one", "one@example.com", None, ANTIGRAVITY, None).unwrap();
        assert!(local_identity_matches(&account, Some("ONE@example.com")));
        assert!(!local_identity_matches(&account, Some("two@example.com")));
        assert!(!local_identity_matches(&account, None));
    }

    #[test]
    fn local_process_filter_requires_an_antigravity_path_segment() {
        let pattern = regex::Regex::new(ANTIGRAVITY_PROCESS_PATH_PATTERN).unwrap();

        assert!(pattern.is_match(
            r"C:\Users\user\AppData\Local\Programs\antigravity\resources\bin\language_server.exe"
        ));
        assert!(pattern.is_match(r"--app_data_dir=C:\Users\user\AppData\Roaming\Antigravity-IDE"));
        assert!(!pattern.is_match(
            r"C:\Program Files\OtherEditor\resources\bin\language_server.exe --app_data_dir C:\Users\user\OtherEditor"
        ));
        assert!(
            !pattern.is_match(r"C:\Program Files\notantigravity\resources\bin\language_server.exe")
        );
    }

    #[test]
    fn local_snapshot_ranking_prefers_complete_quota_summaries() {
        let complete = parse_quota_summary(&json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {"bucketId": "gemini-5h", "remainingFraction": 0.92},
                        {"bucketId": "gemini-weekly", "remainingFraction": 0.71}
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {"bucketId": "3p-5h", "remainingFraction": 0.83},
                        {"bucketId": "3p-weekly", "remainingFraction": 0.64}
                    ]
                }
            ]
        }));
        let sparse = parse_quota_summary(&json!({
            "groups": [{
                "displayName": "Gemini Models",
                "buckets": [{"bucketId": "gemini-5h", "remainingFraction": 0.92}]
            }]
        }));
        let model_fallback = vec![to_quota(
            "gemini-pro",
            "Gemini Pro".to_owned(),
            Some(0.92),
            None,
        )];

        let complete_score = local_snapshot_score(
            Some(&complete),
            &[],
            Some("one@example.com"),
            Some("Google AI Pro"),
        );
        let sparse_score = local_snapshot_score(Some(&sparse), &[], Some("one@example.com"), None);
        let fallback_score = local_snapshot_score(
            None,
            &model_fallback,
            Some("one@example.com"),
            Some("Google AI Pro"),
        );

        assert!(complete_score > sparse_score);
        assert!(sparse_score > fallback_score);
    }

    #[test]
    fn local_probe_candidate_selection_keeps_the_highest_score() {
        let mut best = None;
        keep_best_candidate(&mut best, 14, "model-fallback");
        keep_best_candidate(&mut best, 1_107, "complete-summary");
        keep_best_candidate(&mut best, 1_033, "sparse-summary");

        assert_eq!(best, Some((1_107, "complete-summary")));
    }

    #[test]
    fn remote_quota_buckets_are_deduplicated_by_lowest_remaining_fraction() {
        let root = json!({
            "buckets": [
                {"modelId": "gemini-pro", "remainingFraction": 0.9},
                {"modelId": "gemini-pro", "remainingFraction": 0.4},
                {"modelId": "claude-sonnet", "remainingFraction": 0.8}
            ]
        });
        let quotas = parse_remote_quota_buckets(&root);
        assert_eq!(quotas.len(), 2);
        assert_eq!(
            quotas
                .iter()
                .find(|quota| quota.key == "gemini-pro")
                .and_then(|quota| quota.remaining_fraction),
            Some(0.4)
        );
        assert!(should_verify_remote_quotas(&[
            to_quota("one", "One".to_owned(), Some(1.0), None),
            to_quota("two", "Two".to_owned(), Some(0.999), None),
        ]));
    }

    #[test]
    fn onboarding_prefers_default_allowed_tier() {
        let root = json!({
            "allowedTiers": [
                {"id": "paid-tier", "isDefault": false},
                {"id": "free-tier", "isDefault": true}
            ],
            "paidTier": {"id": "paid-tier"}
        });
        assert_eq!(find_onboard_tier(&root).as_deref(), Some("free-tier"));
    }
}
