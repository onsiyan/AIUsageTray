//! Cloud Code (remote) source: endpoint fallback, onboarding, project and tier lookup, and its cache.

use super::*;

/// How long an account's `loadCodeAssist` answer is reused. It only yields
/// the Cloud Code project and the subscription tier, which rarely change,
/// while the call costs about a second on every refresh.
pub(super) const CODE_ASSIST_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub(super) struct CodeAssistInfo {
    pub(super) fetched_at: Instant,
    pub(super) project_id: Option<String>,
    pub(super) plan_type: Option<String>,
}

pub(super) static CODE_ASSIST_CACHE: LazyLock<Mutex<HashMap<AccountId, CodeAssistInfo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn cached_code_assist(account_id: AccountId) -> Option<CodeAssistInfo> {
    let cache = CODE_ASSIST_CACHE.lock().ok()?;
    cache
        .get(&account_id)
        .filter(|info| info.fetched_at.elapsed() < CODE_ASSIST_CACHE_TTL)
        .cloned()
}

/// The endpoint that last answered each operation. It is tried first next
/// time, so one that stalls is waited on once, not on every refresh. Shared
/// by every adapter, since a refresh builds its own.
pub(super) type EndpointMemory = Arc<Mutex<HashMap<String, String>>>;
pub(super) static ANSWERING_ENDPOINT: LazyLock<EndpointMemory> = LazyLock::new(Default::default);

impl AntigravityUsageAdapter {
    /// The endpoints in the order to try them for `operation`.
    fn endpoints_for(&self, operation: &str) -> Vec<&Url> {
        let answering = self
            .answering
            .lock()
            .ok()
            .and_then(|endpoints| endpoints.get(operation).cloned());
        let mut endpoints = self.base_urls.iter().collect::<Vec<_>>();
        if let Some(index) = answering
            .and_then(|answering| endpoints.iter().position(|url| url.as_str() == answering))
        {
            let answering = endpoints.remove(index);
            endpoints.insert(0, answering);
        }
        endpoints
    }

    fn note_answering_endpoint(&self, operation: &str, base_url: &Url) {
        if let Ok(mut endpoints) = self.answering.lock() {
            endpoints.insert(operation.to_owned(), base_url.as_str().to_owned());
        }
    }
}

pub(super) fn store_code_assist(account_id: AccountId, info: Option<CodeAssistInfo>) {
    if let Ok(mut cache) = CODE_ASSIST_CACHE.lock() {
        match info {
            Some(info) => cache.insert(account_id, info),
            None => cache.remove(&account_id),
        };
    }
}

impl AntigravityUsageAdapter {
    pub(super) async fn post_to(
        &self,
        base_url: &Url,
        operation: &str,
        body: Value,
        material: &AccountAuthMaterial,
    ) -> Result<UsageHttpResponse, TransportError> {
        self.post_to_with_user_agent(base_url, operation, body, material, &self.user_agent)
            .await
    }

    pub(super) async fn post_to_with_user_agent(
        &self,
        base_url: &Url,
        operation: &str,
        body: Value,
        material: &AccountAuthMaterial,
        user_agent: &str,
    ) -> Result<UsageHttpResponse, TransportError> {
        let url = base_url
            .join(&format!("/{}", operation.trim_start_matches('/')))
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        let mut headers = bearer_headers(material, user_agent);
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

    pub(super) async fn post_remote(
        &self,
        operation: &str,
        body: Value,
        material: &AccountAuthMaterial,
    ) -> Result<UsageHttpResponse, TransportError> {
        let mut last_response = None;
        let mut last_error = None;
        for base_url in self.endpoints_for(operation) {
            match self
                .post_to(base_url, operation, body.clone(), material)
                .await
            {
                Ok(response)
                    if response.is_success()
                        || !is_retryable_remote_status(response.status_code) =>
                {
                    if response.is_success() {
                        self.note_answering_endpoint(operation, base_url);
                    }
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

    pub(super) async fn post_remote_best_effort(
        &self,
        operation: &str,
        body: Value,
        material: &AccountAuthMaterial,
    ) -> Result<UsageHttpResponse, TransportError> {
        let mut last_response = None;
        let mut last_error = None;
        for base_url in self.endpoints_for(operation) {
            match self
                .post_to_with_user_agent(
                    base_url,
                    operation,
                    body.clone(),
                    material,
                    QUOTA_SUMMARY_USER_AGENT,
                )
                .await
            {
                Ok(response) if response.is_success() => {
                    self.note_answering_endpoint(operation, base_url);
                    return Ok(response);
                }
                Ok(response) => last_response = Some(response),
                Err(error) => last_error = Some(error),
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
}

impl AntigravityUsageAdapter {
    pub(super) async fn onboard_remote(
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
        if response.is_success()
            && let Ok(root) = serde_json::from_str::<Value>(&response.body)
            && let Some(project) = find_project_id(&root)
        {
            return Some(project);
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

pub(super) fn is_retryable_remote_status(status_code: u16) -> bool {
    matches!(status_code, 404 | 408 | 425 | 429) || (500..=599).contains(&status_code)
}

pub(super) fn find_project_id(root: &Value) -> Option<String> {
    let project = root
        .get("cloudaicompanionProject")
        .or_else(|| root.get("response")?.get("cloudaicompanionProject"))?;
    if let Some(value) = project.as_str() {
        return Some(value.to_owned());
    }
    json_string(project, &["id", "projectId", "project_id"])
}

pub(super) fn find_onboard_tier(root: &Value) -> Option<String> {
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

pub(super) fn find_plan_type(root: &Value) -> Option<String> {
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
pub(super) fn tier_label(value: &Value) -> Option<String> {
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
