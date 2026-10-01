//! claude.ai web-session source and its organization, account, and session-key handling.

use super::*;

impl ClaudeUsageAdapter {
    pub(super) async fn get(
        &self,
        path: &str,
        session_key: &str,
    ) -> Result<crate::transport::UsageHttpResponse, TransportError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        self.transport
            .send(UsageHttpRequest {
                method: Method::GET,
                url,
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("Cookie".to_owned(), format!("sessionKey={session_key}")),
                    ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
                ]
                .into_iter()
                .collect(),
                body: None,
            })
            .await
    }
}

impl ClaudeUsageAdapter {
    pub(super) async fn probe_web(
        &self,
        account: &AccountRecord,
        session_key: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        let unauthorized = crate::usage::UsageAdapterErrorCode::Unauthorized;
        let initial = self.probe_web_once(account, session_key).await?;
        if initial.error.as_ref().map(|error| error.code) != Some(unauthorized) {
            return Ok(initial);
        }
        let (Some(refresher), Some(store)) = (
            self.browser_session_refresher.as_ref(),
            self.auth_material_store.as_ref(),
        ) else {
            return Ok(initial);
        };
        let Ok(Some(replacement)) = refresher.reimport(account).await else {
            return Ok(initial);
        };
        let Some(replacement_key) = claude_session_key(&replacement) else {
            return Ok(initial);
        };
        if replacement_key == session_key {
            return Ok(initial);
        }

        let (identity, verified_session_key) = match fetch_web_identity_for_account(
            self.transport.as_ref(),
            &replacement_key,
            account.provider_account_id.as_deref(),
        )
        .await
        {
            Ok(identity) => identity,
            Err(_) => return Ok(initial),
        };
        let email_matches = identity
            .email
            .as_deref()
            .is_some_and(|email| email.trim().eq_ignore_ascii_case(&account.email));
        let organization_matches = account
            .provider_account_id
            .as_deref()
            .is_none_or(|expected| identity.provider_account_id.as_deref() == Some(expected));
        if !email_matches || !organization_matches {
            return Ok(account_mismatch(
                "Claude browser session belongs to another account",
            ));
        }

        // The imported cookie is not written until both the account email and
        // the account's selected organization have been verified.
        let replaced = store
            .replace_cookie_if_matches(account.id, "sessionKey", session_key, &verified_session_key)
            .await
            .unwrap_or(false);
        let persisted = replaced
            || store
                .get(account.id)
                .await
                .ok()
                .flatten()
                .and_then(|material| claude_session_key(&material))
                .as_deref()
                == Some(verified_session_key.as_str());
        let mut retried = self.probe_web_once(account, &verified_session_key).await?;
        if persisted {
            retried.session_token_was_refreshed = true;
        } else if !retried.session_token_was_refreshed
            && retried.succeeded()
            && let Some(snapshot) = retried.snapshot.as_mut()
            && !snapshot
                .source_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.source == "auth.session-key")
        {
            snapshot.source_diagnostics.push(UsageSourceDiagnostic {
                source: "auth.session-key".to_owned(),
                code: UsageAdapterErrorCode::Unknown,
                message: "Claude usage was fetched with the verified replacement session, but secure storage did not confirm saving it; a later refresh may need to re-import the browser session".to_owned(),
                http_status_code: None,
                retry_after_seconds: None,
            });
        }
        Ok(retried)
    }

    pub(super) async fn probe_web_once(
        &self,
        account: &AccountRecord,
        session_key: &str,
    ) -> Result<UsageProbeResult, TransportError> {
        let initial_session_key = session_key.to_owned();
        let mut session_key = initial_session_key.clone();
        let organizations = self.get("api/organizations", &session_key).await?;
        if !organizations.is_success() {
            return Ok(map_claude_http_error(&organizations, "Claude"));
        }
        update_session_key_from_response(&mut session_key, &organizations);
        let organization_id =
            select_organization(&organizations.body, account.provider_account_id.as_deref())
                .ok_or_else(|| {
                    TransportError::Serialization("Claude organization id was not found".to_owned())
                })?;
        let usage_path = format!("api/organizations/{organization_id}/usage");
        let usage = self.get(&usage_path, &session_key).await?;
        if !usage.is_success() {
            return Ok(map_claude_http_error(&usage, "Claude"));
        }
        update_session_key_from_response(&mut session_key, &usage);
        let root: Value = serde_json::from_str(&usage.body)
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
        let usage_primary = five_hour
            .clone()
            .or_else(|| Some(synthetic_session_window()))
            .map(|mut window| {
                window.kind = UsageWindowKind::Primary;
                window
            });
        let secondary = weekly.clone();
        let additional = parse_claude_extra_windows(&root, now);
        let (mut spend, mut credits) = parse_extra_usage(&root);
        if self.fetch_prepaid_credits {
            let overage_path = format!("api/organizations/{organization_id}/overage_spend_limit");
            if let Ok(response) = self.get(&overage_path, &session_key).await {
                update_session_key_from_response(&mut session_key, &response);
                if response.is_success() {
                    spend = parse_overage_spend(&response.body).or(spend);
                }
            }
            let credits_path = format!("api/organizations/{organization_id}/prepaid/credits");
            if let Ok(response) = self.get(&credits_path, &session_key).await {
                update_session_key_from_response(&mut session_key, &response);
                if response.is_success() {
                    credits = parse_prepaid_credits(&response.body).or(credits);
                }
            }
        }
        let primary = usage_primary.or_else(|| spend.as_ref().and_then(spend_limit_window));
        // A response can rotate sessionKey while still returning valid usage.
        // Fetch the account identity on that exceptional path even when
        // optional identity enrichment is disabled; persistence must never be
        // based only on an unverified usage payload.
        let should_fetch_identity =
            self.fetch_account_identity || session_key != initial_session_key;
        let account_info = if should_fetch_identity {
            match self.get("api/account", &session_key).await {
                Ok(response) if response.is_success() => {
                    update_session_key_from_response(&mut session_key, &response);
                    parse_claude_web_account(&response.body, &organization_id)
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(account_info) = account_info.as_ref()
            && !identity_matches(
                account,
                account_info.email.as_deref(),
                [Some(organization_id.as_str())],
            )
        {
            return Ok(account_mismatch(
                "Claude Web session belongs to another account",
            ));
        }
        let identity_verified_for_rotation = account_info
            .as_ref()
            .and_then(|info| info.email.as_deref())
            .is_some_and(|email| email.trim().eq_ignore_ascii_case(&account.email))
            && account
                .provider_account_id
                .as_deref()
                .is_none_or(|expected| expected == organization_id);
        let observed_email = account_info
            .as_ref()
            .and_then(|info| info.email.clone())
            .or_else(|| Some(account.email.clone()));
        let plan_type = account_info.and_then(|info| info.plan_type);
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
        if primary.is_none()
            && secondary.is_none()
            && additional.is_empty()
            && spend.is_none()
            && credits.is_none()
        {
            return Ok(invalid_payload("Claude", "no usage lanes were present"));
        }
        let mut source_diagnostics = Vec::new();
        let mut session_token_was_refreshed = false;
        if session_key != initial_session_key
            && let Some(store) = self.auth_material_store.as_ref()
        {
            if !identity_verified_for_rotation {
                source_diagnostics.push(UsageSourceDiagnostic {
                        source: "auth.session-key".to_owned(),
                        code: UsageAdapterErrorCode::Unknown,
                        message: "Claude returned a renewed Web session, but its account identity could not be verified; the saved cookie was left unchanged".to_owned(),
                        http_status_code: None,
                        retry_after_seconds: None,
                    });
            } else {
                match store
                        .replace_cookie_if_matches(
                            account.id,
                            "sessionKey",
                            &initial_session_key,
                            &session_key,
                        )
                        .await
                    {
                        Ok(replaced) => session_token_was_refreshed = replaced,
                        Err(_) => source_diagnostics.push(UsageSourceDiagnostic {
                            source: "auth.session-key".to_owned(),
                            code: UsageAdapterErrorCode::Unknown,
                            message: "Claude renewed its Web session but the updated cookie could not be saved securely".to_owned(),
                            http_status_code: None,
                            retry_after_seconds: None,
                        }),
                    }
            }
        }
        let primary_kind = primary_window_kind(primary.as_ref());
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: Some(organization_id.clone()),
            plan_type: plan_type.clone(),
            primary,
            primary_window_kind: primary_kind,
            primary_window_is_synthetic: five_hour.is_none(),
            secondary,
            additional_windows: additional,
            credits,
            credit_inventory: None,
            spend,
            observed_email: observed_email.clone(),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics,
            source_diagnostics,
            provider_id: CLAUDE.to_owned(),
            source: Some("browser".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };
        let mut result = UsageProbeResult::success(
            snapshot,
            Some(VerifiedIdentity {
                email: observed_email,
                provider_account_id: Some(organization_id),
                plan_type,
            }),
        );
        result.session_token_was_refreshed = session_token_was_refreshed;
        Ok(result)
    }
}

impl ClaudeUsageAdapter {
    pub(super) async fn merge_web_extras(
        &self,
        account: &AccountRecord,
        oauth: UsageProbeResult,
        session_key: Option<&str>,
    ) -> UsageProbeResult {
        let Some(session_key) = session_key else {
            return oauth;
        };
        let Some(mut snapshot) = oauth.snapshot.clone() else {
            return oauth;
        };
        let web = match self.probe_web(account, session_key).await {
            Ok(result) if result.succeeded() => result,
            Ok(_) | Err(_) => return oauth,
        };
        let session_token_was_refreshed =
            oauth.session_token_was_refreshed || web.session_token_was_refreshed;
        let Some(web_snapshot) = web.snapshot else {
            return oauth;
        };
        if snapshot.spend.is_none() {
            snapshot.spend = web_snapshot.spend;
        }
        if snapshot.credits.is_none() {
            snapshot.credits = web_snapshot.credits;
        }
        let mut existing_window_keys = snapshot
            .additional_windows
            .iter()
            .map(|window| window.key.clone())
            .collect::<std::collections::HashSet<_>>();
        for window in web_snapshot.additional_windows {
            if existing_window_keys.insert(window.key.clone()) {
                snapshot.additional_windows.push(window);
            }
        }
        let mut existing_metric_keys = snapshot
            .metrics
            .iter()
            .map(|metric| metric.key.clone())
            .collect::<std::collections::HashSet<_>>();
        for metric in web_snapshot.metrics {
            if existing_metric_keys.insert(metric.key.clone()) {
                snapshot.metrics.push(metric);
            }
        }
        for diagnostic in web_snapshot.source_diagnostics {
            if !snapshot
                .source_diagnostics
                .iter()
                .any(|existing| existing.source == diagnostic.source)
            {
                snapshot.source_diagnostics.push(diagnostic);
            }
        }
        let mut result = UsageProbeResult::success(snapshot, oauth.identity);
        result.session_token_was_refreshed = session_token_was_refreshed;
        result
    }
}

pub(super) fn select_organization(body: &str, requested_id: Option<&str>) -> Option<String> {
    let root: Value = serde_json::from_str(body).ok()?;
    let organizations = root
        .get("organizations")
        .or_else(|| root.as_array().map(|_| &root))?
        .as_array()?;
    let candidates = organizations
        .iter()
        .filter_map(|item| {
            let id = json_string(item, &["uuid", "id"])?;
            let capabilities = item.get("capabilities").and_then(Value::as_array);
            let (has_chat_capability, is_not_api_only) = if let Some(capabilities) = capabilities {
                let capabilities = capabilities
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_ascii_lowercase)
                    .collect::<Vec<_>>();
                let has_chat_capability = capabilities.iter().any(|value| value == "chat");
                let is_api_only =
                    !capabilities.is_empty() && capabilities.iter().all(|value| value == "api");
                (has_chat_capability, !is_api_only)
            } else {
                (
                    item.get("has_chat_capability")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    !item
                        .get("is_api_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
            };
            Some((id, has_chat_capability, is_not_api_only))
        })
        .collect::<Vec<_>>();
    if let Some(requested) = requested_id.map(str::trim).filter(|id| !id.is_empty()) {
        return candidates
            .iter()
            .find(|(id, _, _)| id == requested)
            .map(|(id, _, _)| id.clone());
    }

    candidates
        .iter()
        .find(|(_, has_chat, _)| *has_chat)
        .map(|(id, _, _)| id.clone())
        .or_else(|| {
            candidates
                .iter()
                .find(|(_, _, is_not_api_only)| *is_not_api_only)
                .map(|(id, _, _)| id.clone())
        })
        .or_else(|| candidates.first().map(|(id, _, _)| id.clone()))
}

/// Fetches the identity associated with a Claude Web `sessionKey` cookie.
///
/// This is the Web-session counterpart to [`fetch_oauth_identity`]. It is
/// used by the user-driven browser login bridge before an account record is
/// created, so the account can be bound to the organization that supplied the
/// cookie rather than to a placeholder email.
pub async fn fetch_web_identity(
    transport: &dyn UsageHttpTransport,
    session_key: &str,
) -> Result<VerifiedIdentity, TransportError> {
    let (identity, _) = fetch_web_identity_for_account(transport, session_key, None).await?;
    Ok(identity)
}

pub(super) async fn fetch_web_identity_for_account(
    transport: &dyn UsageHttpTransport,
    session_key: &str,
    expected_organization_id: Option<&str>,
) -> Result<(VerifiedIdentity, String), TransportError> {
    let mut session_key = session_key.trim().to_owned();
    if !session_key.starts_with("sk-ant-") || session_key.len() <= "sk-ant-".len() {
        return Err(TransportError::Serialization(
            "Claude Web session key is missing or invalid".to_owned(),
        ));
    }
    let base_url = Url::parse("https://claude.ai/")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let headers = |session_key: &str| {
        [
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Cookie".to_owned(), format!("sessionKey={session_key}")),
            ("User-Agent".to_owned(), "UsageMonitor/0.1".to_owned()),
        ]
        .into_iter()
        .collect()
    };

    let organizations_url = base_url
        .join("api/organizations")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let organizations = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: organizations_url,
            headers: headers(&session_key),
            body: None,
        })
        .await?;
    if !organizations.is_success() {
        return Err(TransportError::Serialization(format!(
            "Claude Web organizations returned HTTP {}",
            organizations.status_code
        )));
    }
    update_session_key_from_response(&mut session_key, &organizations);
    let organization_id = select_organization(&organizations.body, expected_organization_id)
        .or_else(|| select_organization(&organizations.body, None))
        .ok_or_else(|| {
            TransportError::Serialization(
                "Claude Web organizations did not contain an organization id".to_owned(),
            )
        })?;

    let account_url = base_url
        .join("api/account")
        .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
    let account = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: account_url,
            headers: headers(&session_key),
            body: None,
        })
        .await?;
    if !account.is_success() {
        return Err(TransportError::Serialization(format!(
            "Claude Web account returned HTTP {}",
            account.status_code
        )));
    }
    update_session_key_from_response(&mut session_key, &account);
    let account = parse_claude_web_account(&account.body, &organization_id).ok_or_else(|| {
        TransportError::Serialization(
            "Claude Web account did not contain an account identity".to_owned(),
        )
    })?;
    if account.email.is_none() {
        return Err(TransportError::Serialization(
            "Claude Web account did not contain an email address".to_owned(),
        ));
    }
    Ok((
        VerifiedIdentity {
            email: account.email,
            provider_account_id: Some(organization_id),
            plan_type: account.plan_type,
        },
        session_key,
    ))
}

#[derive(Debug, Clone)]
pub(super) struct ClaudeWebAccount {
    pub(super) email: Option<String>,
    pub(super) plan_type: Option<String>,
}

pub(super) fn parse_claude_web_account(
    body: &str,
    organization_id: &str,
) -> Option<ClaudeWebAccount> {
    let root: Value = serde_json::from_str(body).ok()?;
    let email = json_string(&root, &["email_address", "emailAddress", "email"]);
    let membership = root
        .get("memberships")
        .and_then(Value::as_array)
        .and_then(|memberships| {
            memberships.iter().find(|membership| {
                membership
                    .get("organization")
                    .and_then(|organization| json_string(organization, &["uuid", "id"]))
                    .is_some_and(|id| id == organization_id)
            })
        })
        .or_else(|| root.get("memberships").and_then(Value::as_array)?.first());
    let organization = membership.and_then(|value| value.get("organization"));
    let rate_limit_tier =
        organization.and_then(|value| json_string(value, &["rate_limit_tier", "rateLimitTier"]));
    let billing_type =
        organization.and_then(|value| json_string(value, &["billing_type", "billingType"]));
    let seat_tier = membership.and_then(|value| json_string(value, &["seat_tier", "seatTier"]));
    Some(ClaudeWebAccount {
        email,
        plan_type: claude_plan_label(
            None,
            rate_limit_tier.as_deref(),
            billing_type.as_deref(),
            seat_tier.as_deref(),
        ),
    })
}

pub(super) fn claude_session_key(material: &crate::auth::AccountAuthMaterial) -> Option<String> {
    material
        .cookies
        .iter()
        .find(|cookie| cookie.name.eq_ignore_ascii_case("sessionKey"))
        .map(|cookie| cookie.value.trim().to_owned())
        .filter(|value| value.starts_with("sk-ant-") && value.len() > "sk-ant-".len())
}

pub(super) fn update_session_key_from_response(
    session_key: &mut String,
    response: &crate::transport::UsageHttpResponse,
) {
    if let Some(renewed) = rotated_claude_session_key(response) {
        *session_key = renewed;
    }
}

pub(super) fn rotated_claude_session_key(
    response: &crate::transport::UsageHttpResponse,
) -> Option<String> {
    if response.status_code != 200 {
        return None;
    }
    let set_cookie = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, value)| value)?;

    let mut latest_session_key = None;
    for header_line in set_cookie.lines() {
        let bytes = header_line.as_bytes();
        let cookie_name = b"sessionKey=";
        if bytes.len() < cookie_name.len() {
            continue;
        }
        for start in 0..=bytes.len() - cookie_name.len() {
            if !bytes[start..start + cookie_name.len()].eq_ignore_ascii_case(cookie_name) {
                continue;
            }
            let boundary = header_line[..start].trim_end();
            if !boundary.is_empty() && !boundary.ends_with(',') {
                continue;
            }
            let value_start = start + cookie_name.len();
            let value_end = header_line[value_start..]
                .find([';', ',', '\r', '\n'])
                .map_or(header_line.len(), |offset| value_start + offset);
            let candidate = header_line[value_start..value_end].trim();
            if candidate.starts_with("sk-ant-") && !candidate.chars().any(char::is_whitespace) {
                latest_session_key = Some(candidate.to_owned());
            }
        }
    }
    latest_session_key
}
