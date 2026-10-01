//! Local language-server source: endpoint discovery, requests, and identity checks.

use super::*;

impl AntigravityUsageAdapter {
    pub(super) async fn probe_local(&self, account: &AccountRecord) -> Option<UsageProbeResult> {
        let transport = self.local_transport.as_ref()?;
        // Process discovery runs PowerShell synchronously; keep it off the
        // async worker threads shared with other provider refreshes and the UI.
        let endpoints = tokio::task::spawn_blocking(cached_local_endpoints)
            .await
            .unwrap_or_default();
        if endpoints.is_empty() {
            return None;
        }

        let mut best_result = None;
        for endpoint in endpoints {
            // CodexBar treats the quota summary as the richest local source,
            // but obtains identity from GetUserStatus before accepting it.
            // That account check is essential when several Google accounts
            // are registered in the monitor: a local language server is not
            // account-scoped by the request itself. Identify first, so the
            // slow forced summary refresh only runs for the signed-in account.
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
            if !local_identity_matches(account, email.as_deref()) {
                continue;
            }
            let plan_type = status_root.as_ref().and_then(find_local_plan_type);
            let status_models = status_root
                .as_ref()
                .map(parse_local_model_quotas)
                .unwrap_or_default();

            let summary_groups = local_post(
                transport,
                &endpoint,
                LOCAL_QUOTA_SUMMARY_PATH,
                json!({ "forceRefresh": true }),
            )
            .await
            .filter(UsageHttpResponse::is_success)
            .and_then(|response| serde_json::from_str::<Value>(&response.body).ok())
            .map(|root| parse_quota_summary(&root))
            .filter(|groups| has_usable_quota_summary(groups));

            if let Some(groups) = summary_groups {
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
            // a matching account identity (checked above).
            {
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
}

#[derive(Clone, Debug)]
pub(super) struct LocalEndpoint {
    pub(super) port: u16,
    pub(super) csrf_token: String,
}

pub(super) fn local_snapshot_score(
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

pub(super) fn keep_best_candidate<T>(best: &mut Option<(usize, T)>, score: usize, candidate: T) {
    if best
        .as_ref()
        .is_none_or(|(best_score, _)| score > *best_score)
    {
        *best = Some((score, candidate));
    }
}

pub(super) async fn local_post(
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

pub(super) fn local_request_body() -> Value {
    json!({
        "metadata": {
            "ideName": "antigravity",
            "extensionName": "antigravity",
            "ideVersion": "unknown",
            "locale": "en"
        }
    })
}

/// Process discovery launches PowerShell and scans every process. A refresh
/// of several Antigravity accounts would otherwise repeat that scan once per
/// account, so share one result for a short period.
pub(super) fn cached_local_endpoints() -> Vec<LocalEndpoint> {
    const TTL: Duration = Duration::from_secs(30);
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<LocalEndpoint>)>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((discovered_at, endpoints)) = cache.as_ref()
        && discovered_at.elapsed() < TTL
    {
        return endpoints.clone();
    }
    let endpoints = discover_local_endpoints();
    *cache = Some((std::time::Instant::now(), endpoints.clone()));
    endpoints
}

#[cfg(windows)]
pub(super) fn discover_local_endpoints() -> Vec<LocalEndpoint> {
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

    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    crate::providers::shared::hide_console_window(&mut command);
    let output = match command.output() {
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
pub(super) fn discover_local_endpoints() -> Vec<LocalEndpoint> {
    Vec::new()
}

pub(super) fn parse_local_model_quotas(root: &Value) -> Vec<Quota> {
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

pub(super) fn find_local_email(root: &Value) -> Option<String> {
    let status = root.get("userStatus").or_else(|| {
        root.get("response")
            .and_then(|value| value.get("userStatus"))
    })?;
    json_string(status, &["email"])
}

pub(super) fn find_local_plan_type(root: &Value) -> Option<String> {
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

pub(super) fn local_identity_matches(
    account: &AccountRecord,
    observed_email: Option<&str>,
) -> bool {
    let expected = account.email.trim();
    observed_email.is_some_and(|observed| {
        !expected.is_empty()
            && !observed.trim().is_empty()
            && expected.eq_ignore_ascii_case(observed.trim())
    })
}
