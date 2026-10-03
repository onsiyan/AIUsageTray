//! OAuth source: usage, profile identity and plan, limit windows, and reset grants.

use super::*;

impl ClaudeUsageAdapter {
    pub(super) async fn oauth_rate_limit_remaining(&self, access_token: &str) -> Option<i64> {
        let key = oauth_token_key(access_token);
        let now = Utc::now();
        let mut blocked = self.oauth_rate_limit_until.lock().await;
        let until = blocked.get(&key).copied()?;
        if until <= now {
            blocked.remove(&key);
            return None;
        }
        Some((until - now).num_seconds().max(1))
    }

    pub(super) async fn record_oauth_rate_limit(
        &self,
        access_token: &str,
        response: &crate::transport::UsageHttpResponse,
    ) {
        let retry_after = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse::<i64>().ok())
            .filter(|seconds| *seconds >= 0)
            .unwrap_or(300)
            // An untrusted header must not overflow chrono or block for days.
            .min(24 * 60 * 60);
        self.oauth_rate_limit_until.lock().await.insert(
            oauth_token_key(access_token),
            Utc::now() + chrono::Duration::seconds(retry_after),
        );
    }

    pub(super) async fn clear_oauth_rate_limit(&self, access_token: &str) {
        self.oauth_rate_limit_until
            .lock()
            .await
            .remove(&oauth_token_key(access_token));
    }
}

impl ClaudeUsageAdapter {
    pub(super) async fn get_oauth(
        &self,
        access_token: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let mut url = self
            .oauth_base_url
            .join("api/oauth/usage")
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        // `cedar_ember=1` adds the usage-limit reset grants. The server only
        // returns them to a recent Claude Code CLI client, so identify as one.
        url.query_pairs_mut().append_pair("cedar_ember", "1");
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("Authorization".to_owned(), format!("Bearer {access_token}")),
                    ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
                    (
                        "User-Agent".to_owned(),
                        format!("claude-cli/{CLAUDE_CODE_CLIENT_VERSION} (external, cli)"),
                    ),
                    ("x-app".to_owned(), "cli".to_owned()),
                    (
                        "anthropic-client-platform".to_owned(),
                        "claude_code_cli".to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }

    pub(super) async fn get_oauth_profile(
        &self,
        access_token: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let url = self
            .oauth_base_url
            .join("api/oauth/profile")
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("Authorization".to_owned(), format!("Bearer {access_token}")),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }
}

impl ClaudeUsageAdapter {
    pub(super) async fn probe_oauth(
        &self,
        account: &AccountRecord,
        access_token: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        if let Some(retry_after_seconds) = self.oauth_rate_limit_remaining(access_token).await {
            return Ok(UsageProbeResult::failure(crate::usage::UsageAdapterError {
                code: crate::usage::UsageAdapterErrorCode::RateLimited,
                message: "Claude OAuth usage endpoint is temporarily rate-limited".to_owned(),
                http_status_code: Some(429),
                retry_after_seconds: Some(retry_after_seconds as u64),
            }));
        }
        let usage_request = async {
            let response = self
                .get_oauth(access_token)
                .await
                .map_err(transport_failure)?;
            if !response.is_success() {
                if response.status_code == 429 {
                    self.record_oauth_rate_limit(access_token, &response).await;
                }
                return Err(map_claude_http_error(&response, "Claude OAuth"));
            }
            Ok(response)
        };
        let profile_request = async {
            let profile = if self.fetch_account_identity {
                self.get_oauth_profile(access_token)
                    .await
                    .ok()
                    .filter(|response| response.is_success())
                    .and_then(|response| parse_claude_profile(&response.body))
            } else {
                None
            };
            Ok::<_, UsageProbeResult>(profile)
        };
        // Both endpoints use the same account token and are independent. Keep
        // identity verification, but overlap its round trip with usage.
        // A rejected usage request cancels profile work immediately instead
        // of delaying the authentication/rate-limit error behind that request.
        let (response, profile) = match tokio::try_join!(usage_request, profile_request) {
            Ok(results) => results,
            Err(failure) => return Ok(failure),
        };
        self.clear_oauth_rate_limit(access_token).await;
        let root: Value = serde_json::from_str(&response.body)
            .map_err(|error| TransportError::Serialization(error.to_string()))?;
        let now = Utc::now();
        let five_hour = parse_window(
            root.get("five_hour"),
            UsageWindowKind::Primary,
            "Session",
            now,
        );
        let weekly = parse_window(
            root.get("seven_day"),
            UsageWindowKind::Secondary,
            "Weekly",
            now,
        );
        // Promote the weekly lane to the primary lane when Claude has no live
        // five-hour session window (enterprise/credit accounts). The weekly
        // lane is retained independently below, exactly as the source API
        // exposes it.
        let usage_primary = five_hour
            .clone()
            .or_else(|| weekly.clone())
            .or_else(|| {
                parse_window(
                    root.get("seven_day_oauth_apps"),
                    UsageWindowKind::Primary,
                    "OAuth apps weekly",
                    now,
                )
            })
            .or_else(|| {
                parse_window(
                    root.get("seven_day_sonnet"),
                    UsageWindowKind::Primary,
                    "Sonnet weekly",
                    now,
                )
            })
            .or_else(|| {
                parse_window(
                    root.get("seven_day_opus"),
                    UsageWindowKind::Primary,
                    "Opus weekly",
                    now,
                )
            })
            .map(|mut window| {
                window.kind = UsageWindowKind::Primary;
                window
            });
        // The weekly lane is independent from the primary fallback. If the
        // API omits five_hour, the reference still exposes seven_day as both
        // the selected primary and the explicit weekly lane.
        let secondary = weekly.clone();
        let additional = parse_claude_extra_windows(&root, now);

        let (spend, credits) = parse_extra_usage(&root);
        let primary = usage_primary.or_else(|| spend.as_ref().and_then(spend_limit_window));
        if primary.is_none() && secondary.is_none() && additional.is_empty() && spend.is_none() {
            return Ok(invalid_payload(
                "Claude OAuth",
                "no usage lanes were present",
            ));
        }
        let mut metrics = Vec::new();
        let primary_metric_key = if primary
            .as_ref()
            .is_some_and(|window| window.name == "Spend limit")
        {
            "spend_limit"
        } else {
            "session"
        };
        add_metric(&mut metrics, primary_metric_key, primary.as_ref());
        add_metric(&mut metrics, "weekly", secondary.as_ref());
        for item in &additional {
            add_metric(&mut metrics, &item.key, Some(&item.window));
        }
        if let Some(profile) = profile.as_ref()
            && !identity_matches(
                account,
                profile.email.as_deref(),
                [
                    profile.organization_id.as_deref(),
                    profile.account_id.as_deref(),
                ],
            )
        {
            return Ok(account_mismatch(
                "Claude OAuth profile belongs to another account",
            ));
        }
        let response_account_id = profile
            .as_ref()
            .and_then(|profile| {
                profile
                    .organization_id
                    .clone()
                    .or_else(|| profile.account_id.clone())
            })
            .or_else(|| account.provider_account_id.clone());
        let observed_email = profile
            .as_ref()
            .and_then(|profile| profile.email.clone())
            .or_else(|| Some(account.email.clone()));
        // The usage payload rarely names the subscription; the profile does.
        let plan_type = claude_oauth_plan_type(&root).or_else(|| {
            profile
                .as_ref()
                .and_then(|profile| profile.plan_type.clone())
        });
        let primary_kind = primary_window_kind(primary.as_ref());
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: response_account_id.clone(),
            plan_type: plan_type.clone(),
            primary,
            primary_window_kind: primary_kind,
            primary_window_is_synthetic: false,
            secondary,
            additional_windows: additional,
            credits,
            credit_inventory: parse_claude_reset_grants(&root, now),
            spend,
            observed_email: observed_email.clone(),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics: Vec::new(),
            provider_id: CLAUDE.to_owned(),
            source: Some("oauth".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        Ok(UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: observed_email,
                provider_account_id: response_account_id,
                plan_type,
            }),
        ))
    }
}

#[derive(Debug, Clone)]
pub(super) struct ClaudeProfile {
    pub(super) account_id: Option<String>,
    pub(super) organization_id: Option<String>,
    pub(super) email: Option<String>,
    pub(super) plan_type: Option<String>,
}

/// Fetches the identity associated with a Claude Code OAuth access token.
///
/// This is intentionally separate from [`ClaudeUsageAdapter::probe`]: a host
/// needs the verified identity before it can create the durable account record
/// that will be used by the normal refresh runtime. The endpoint and headers
/// are the same ones used by the OAuth usage adapter; no browser cookies or
/// WebView state are involved.
pub async fn fetch_oauth_identity(
    transport: &dyn UsageHttpTransport,
    access_token: &str,
) -> Result<VerifiedIdentity, TransportError> {
    let url = Url::parse("https://api.anthropic.com/api/oauth/profile")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let response = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url,
            headers: [
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), format!("Bearer {access_token}")),
                ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
                ("User-Agent".to_owned(), "claude-code/2.1.0".to_owned()),
            ]
            .into_iter()
            .collect(),
            body: None,
        })
        .await?;
    if !response.is_success() {
        return Err(TransportError::Serialization(format!(
            "Claude OAuth profile returned HTTP {}",
            response.status_code
        )));
    }
    let profile = parse_claude_profile(&response.body).ok_or_else(|| {
        TransportError::Serialization(
            "Claude OAuth profile did not contain an account identity".to_owned(),
        )
    })?;
    if profile.email.is_none() && profile.account_id.is_none() && profile.organization_id.is_none()
    {
        return Err(TransportError::Serialization(
            "Claude OAuth profile did not contain an account identity".to_owned(),
        ));
    }
    Ok(VerifiedIdentity {
        email: profile.email,
        provider_account_id: profile.organization_id.or(profile.account_id),
        plan_type: profile.plan_type,
    })
}

pub(super) fn parse_claude_profile(body: &str) -> Option<ClaudeProfile> {
    let root: Value = serde_json::from_str(body).ok()?;
    let account = root.get("account");
    let organization = root.get("organization");
    Some(ClaudeProfile {
        account_id: account
            .and_then(|value| json_string(value, &["uuid", "id"]))
            .or_else(|| json_string(&root, &["accountUuid", "account_uuid"])),
        organization_id: organization
            .and_then(|value| json_string(value, &["uuid", "id"]))
            .or_else(|| json_string(&root, &["organizationUuid", "organization_uuid"])),
        email: account
            .and_then(|value| json_string(value, &["emailAddress", "email_address", "email"]))
            .or_else(|| json_string(&root, &["emailAddress", "email_address", "email"])),
        plan_type: claude_profile_plan_type(account, organization),
    })
}

/// The OAuth profile names the subscription in
/// `organization.organization_type` (for example `claude_pro`, `claude_max`),
/// with the Max multiplier in `rate_limit_tier` and the Team seat in
/// `seat_tier`. `account.has_claude_max`/`has_claude_pro` are the fallback.
pub(super) fn claude_profile_plan_type(
    account: Option<&Value>,
    organization: Option<&Value>,
) -> Option<String> {
    let field = |names: &[&str]| organization.and_then(|value| json_string(value, names));
    let organization_type = field(&["organization_type", "organizationType"]);
    let rate_limit_tier = field(&["rate_limit_tier", "rateLimitTier"]);
    let seat_tier = field(&["seat_tier", "seatTier"]);
    claude_plan_label(
        organization_type.as_deref(),
        rate_limit_tier.as_deref(),
        None,
        seat_tier.as_deref(),
    )
    .or_else(|| {
        let flag = |name: &str| {
            account
                .and_then(|value| value.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if flag("has_claude_max") {
            Some(claude_plan_label(
                Some("max"),
                rate_limit_tier.as_deref(),
                None,
                None,
            )?)
        } else if flag("has_claude_pro") {
            Some("Claude Pro".to_owned())
        } else {
            None
        }
    })
}

pub(super) fn claude_oauth_plan_type(root: &Value) -> Option<String> {
    let subscription = json_string(root, &["subscriptionType", "subscription_type"]);
    let rate_limit_tier = json_string(root, &["rate_limit_tier", "rateLimitTier"]);
    claude_plan_label(
        subscription.as_deref(),
        rate_limit_tier.as_deref(),
        None,
        None,
    )
    .or_else(|| json_string(root, &["plan"]))
}

pub(super) fn claude_plan_label(
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    billing_type: Option<&str>,
    seat_tier: Option<&str>,
) -> Option<String> {
    let combined = [subscription_type, rate_limit_tier, billing_type]
        .into_iter()
        .flatten()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if combined.contains("max") {
        let multiplier = rate_limit_tier.and_then(max_usage_multiplier);
        return Some(match multiplier {
            Some(multiplier) => format!("Claude Max {multiplier}"),
            None => "Claude Max".to_owned(),
        });
    }

    if combined.contains("pro") {
        Some("Claude Pro".to_owned())
    } else if combined.contains("team") {
        match seat_tier.map(|value| value.to_ascii_lowercase()).as_deref() {
            Some("team_standard") => Some("Claude Team Standard".to_owned()),
            Some("team_tier_1") => Some("Claude Team Premium".to_owned()),
            _ => Some("Claude Team".to_owned()),
        }
    } else if combined.contains("enterprise") {
        Some("Claude Enterprise".to_owned())
    } else if combined.contains("ultra") {
        Some("Claude Ultra".to_owned())
    } else {
        None
    }
}

pub(super) fn max_usage_multiplier(rate_limit_tier: &str) -> Option<String> {
    let words = rate_limit_tier
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let max_index = words.iter().position(|word| *word == "max")?;
    let multiplier = words.get(max_index + 1)?.to_owned();
    (multiplier.ends_with('x') && multiplier[..multiplier.len() - 1].parse::<u32>().is_ok())
        .then_some(multiplier)
}

pub(super) fn oauth_token_key(access_token: &str) -> String {
    let digest = Sha256::digest(access_token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn normalize_claude_oauth_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let token = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();
    token
        .to_ascii_lowercase()
        .starts_with("sk-ant-oat")
        .then_some(token.to_owned())
}

pub(super) struct OAuthLimitWindow {
    pub(super) key: String,
    pub(super) name: String,
    pub(super) window: RateLimitWindow,
}

pub(super) fn parse_claude_extra_windows(
    root: &Value,
    now: chrono::DateTime<Utc>,
) -> Vec<AdditionalRateLimitWindow> {
    let mut windows = Vec::new();
    for (key, name) in [
        ("seven_day_sonnet", "Sonnet weekly"),
        ("seven_day_opus", "Opus weekly"),
        ("seven_day_oauth_apps", "OAuth apps weekly"),
        ("iguana_necktie", "Additional weekly"),
    ] {
        if let Some(window) = parse_window(root.get(key), UsageWindowKind::Additional, name, now) {
            windows.push(AdditionalRateLimitWindow {
                key: key.to_owned(),
                name: name.to_owned(),
                window,
            });
        }
    }

    let routine_keys = [
        "seven_day_routines",
        "seven_day_claude_routines",
        "claude_routines",
        "routines",
        "routine",
        "seven_day_cowork",
        "cowork",
    ];
    if let Some((key, window)) = routine_keys.iter().find_map(|key| {
        parse_window(
            root.get(*key),
            UsageWindowKind::Additional,
            "Daily Routines",
            now,
        )
        .map(|window| (*key, window))
    }) {
        windows.push(AdditionalRateLimitWindow {
            key: key.to_owned(),
            name: "Daily Routines".to_owned(),
            window,
        });
    }

    for limit in parse_oauth_limits(root.get("limits"), now) {
        if !windows.iter().any(|existing| existing.key == limit.key) {
            windows.push(AdditionalRateLimitWindow {
                key: limit.key,
                name: limit.name,
                window: limit.window,
            });
        }
    }
    windows
}

pub(super) fn parse_oauth_limits(
    value: Option<&Value>,
    now: chrono::DateTime<Utc>,
) -> Vec<OAuthLimitWindow> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            // Deliberately do not filter `is_active`: observed
            // enforceable scoped limits can report false. The stable shape is
            // group=weekly + kind=weekly_scoped.
            if !json_string(entry, &["group"])
                .is_some_and(|group| group.eq_ignore_ascii_case("weekly"))
                || !json_string(entry, &["kind"])
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("weekly_scoped"))
            {
                return None;
            }
            let percent = json_number(entry, &["percent", "utilization", "used_percent"])?;
            let reset_at = reset_at(entry, now);
            let model = entry
                .get("scope")
                .and_then(|scope| scope.get("model"))
                .and_then(Value::as_object)?;
            let model_name = json_string(
                &Value::Object(model.clone()),
                &["display_name", "displayName"],
            )?;
            let model_id = json_string(&Value::Object(model.clone()), &["id"]);
            if is_all_models_scope(model_id.as_deref(), &model_name) {
                return None;
            }
            let identity = model_id.as_deref().unwrap_or(&model_name);
            let slug = slugify(identity);
            if slug.is_empty() {
                return None;
            }
            let window_name = model_name.clone();
            Some(OAuthLimitWindow {
                key: format!("claude-weekly-scoped-{slug}"),
                name: model_name,
                window: RateLimitWindow {
                    kind: UsageWindowKind::Additional,
                    name: window_name,
                    used_percent: normalize_percent(percent),
                    reset_at_utc: reset_at,
                    limit_window_seconds: 7 * 24 * 60 * 60,
                },
            })
        })
        .collect()
}

pub(super) fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_matches('-').to_owned()
}

pub(super) fn is_all_models_scope(model_id: Option<&str>, model_name: &str) -> bool {
    let name = slugify(model_name);
    if name == "all-models" {
        return true;
    }
    model_id
        .map(slugify)
        .is_some_and(|id| id == "all-models" || id.ends_with("-all-models"))
}

/// Maps Claude's usage-limit reset grants (`cedar_ember`) to the shared reset
/// credit inventory. Each remaining reset of a grant becomes one credit, so a
/// grant with two resets left lists twice, as it does in Claude's own UI.
pub(super) fn parse_claude_reset_grants(
    root: &Value,
    now: chrono::DateTime<Utc>,
) -> Option<UsageCreditInventory> {
    let block = root.get("cedar_ember").filter(|block| block.is_object())?;
    let grants = block.get("grants").and_then(Value::as_array)?;
    let mut credits = Vec::new();
    let mut available_count = 0_u32;
    for grant in grants {
        let resets_left = grant
            .get("resets_left")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u8::MAX)) as u32;
        let expires_at_utc = claude_grant_time(grant, "ends_at");
        if resets_left == 0 || expires_at_utc.is_some_and(|expires_at| expires_at <= now) {
            continue;
        }
        let paused = grant.get("paused").and_then(Value::as_bool) == Some(true);
        let clears = grant
            .get("clears")
            .and_then(Value::as_array)
            .map(|clears| clears.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let (reset_type, title) = if clears.contains(&"seven_day") {
            ("full", "Full reset")
        } else if clears.contains(&"five_hour") {
            ("five_hour", "5-hour reset")
        } else {
            ("reset", "Usage-limit reset")
        };
        if !paused {
            available_count = available_count.saturating_add(resets_left);
        }
        for _ in 0..resets_left {
            credits.push(UsageCreditRecord {
                id: json_string(grant, &["id"]),
                reset_type: Some(reset_type.to_owned()),
                status: Some(if paused { "paused" } else { "available" }.to_owned()),
                granted_at_utc: claude_grant_time(grant, "starts_at"),
                expires_at_utc,
                redeem_started_at_utc: None,
                redeemed_at_utc: None,
                title: Some(title.to_owned()),
                description: json_string(grant, &["label"]),
            });
        }
    }
    Some(UsageCreditInventory {
        available_count,
        credits,
    })
}

pub(super) fn claude_grant_time(grant: &Value, key: &str) -> Option<chrono::DateTime<Utc>> {
    grant
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}
