use crate::{
    accounts::{ANTIGRAVITY, AccountId, AccountRecord},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError, OAuthProviderDefinition},
    providers::shared::{
        bearer_headers, invalid_payload, json_number, json_string, map_antigravity_http_error,
        missing_auth,
    },
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, RateLimitWindow, UsageAdapter, UsageMetric, UsageProbeResult,
        UsageSnapshot, UsageSourceDiagnostic, UsageWindowKind,
    },
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use url::Url;
mod quotas;
mod remote;
mod summary;

use quotas::*;
use remote::*;
use summary::*;

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
    base_urls: Vec<Url>,
    user_agent: String,
}

impl AntigravityUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        let base_urls = REMOTE_BASE_URLS
            .iter()
            .map(|value| {
                Url::parse(value).map_err(|error| TransportError::InvalidUrl(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            transport,
            auth,
            base_urls,
            user_agent: "antigravity".to_owned(),
        })
    }
}

#[async_trait]
impl UsageAdapter for AntigravityUsageAdapter {
    fn adapter_id(&self) -> &str {
        ANTIGRAVITY
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
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

        let code_assist = match cached_code_assist(account.id) {
            Some(info) => info,
            None => {
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
                let mut project_id =
                    find_project_id(&assist_root).or_else(|| account.workspace_id.clone());
                if project_id.is_none() {
                    project_id = self.onboard_remote(&assist_root, &material).await;
                }
                let info = CodeAssistInfo {
                    fetched_at: Instant::now(),
                    project_id,
                    plan_type: find_plan_type(&assist_root),
                };
                // Only a resolved project is worth reusing; without one the
                // next refresh should ask (and onboard) again.
                if info.project_id.is_some() {
                    store_code_assist(account.id, Some(info.clone()));
                }
                info
            }
        };
        let project_id = code_assist.project_id.clone();

        let project_body = project_id
            .as_deref()
            .map(|project| json!({ "project": project }))
            .unwrap_or_else(|| json!({}));
        // The model catalogue and the grouped quota summary are independent
        // calls; fetch them together rather than one after the other.
        let (models, quota_summary_response) = tokio::join!(
            self.post_remote(
                "v1internal:fetchAvailableModels",
                project_body.clone(),
                &material
            ),
            self.post_remote_best_effort(
                "v1internal:retrieveUserQuotaSummary",
                project_body,
                &material,
            ),
        );
        let models = models?;
        if !models.is_success() {
            // A rejected project (or token) may mean the cached project is no
            // longer valid; resolve it afresh next time.
            store_code_assist(account.id, None);
        }
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
        } else if models.status_code == 403
            && let Ok(response) = self
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
        // Fetch grouped or model-shaped quota windows as a separate,
        // best-effort enrichment even when retrieveUserQuota succeeded. The
        // latter remains the fallback when the summary has no valid quota
        // values. Do not require fixed group names: Google can return either
        // shared pools or per-model groups for the same account.
        let mut quota_summary_diagnostics = Vec::new();
        let quota_summary = match quota_summary_response {
            Ok(response) if !response.is_success() => {
                quota_summary_diagnostics.push(quota_summary_response_diagnostic(&response));
                Vec::new()
            }
            Ok(response) => match serde_json::from_str::<Value>(&response.body) {
                Ok(root) => {
                    let groups = parse_quota_summary(&root);
                    if has_usable_quota_summary(&groups) {
                        groups
                    } else {
                        quota_summary_diagnostics
                            .push(quota_summary_empty_diagnostic(response.status_code));
                        Vec::new()
                    }
                }
                Err(_) => {
                    quota_summary_diagnostics
                        .push(quota_summary_payload_diagnostic(response.status_code));
                    Vec::new()
                }
            },
            Err(error) => {
                quota_summary_diagnostics.push(quota_summary_transport_diagnostic(&error));
                Vec::new()
            }
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
        let plan_type = code_assist.plan_type.clone();
        let mut snapshot = if quota_summary.is_empty() {
            snapshot_from_model_quotas(
                account,
                &quotas,
                None,
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
                None,
                plan_type.clone(),
                "api",
            )
        };
        snapshot
            .source_diagnostics
            .extend(quota_summary_diagnostics);
        // The remote quota RPCs identify their project, not the Google user.
        // Keep that response context on the snapshot and leave the OAuth
        // identity captured during account linking untouched.
        snapshot.response_account_id = project_id;
        Ok(UsageProbeResult::success(snapshot, None))
    }
}

const REMOTE_BASE_URLS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com/",
    "https://daily-cloudcode-pa.googleapis.com/",
    "https://cloudcode-pa.googleapis.com/",
];
// Antigravity Manager uses its own current release version for this native
// quota-summary client identity, not the locally installed IDE's file version.
const QUOTA_SUMMARY_USER_AGENT: &str = "vscode/1.X.X (Antigravity/4.7.14-beta)";

#[cfg(test)]
mod tests;
