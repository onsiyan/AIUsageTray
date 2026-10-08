//! OpenCode console (web) source: guarded redirects, console requests, workspace discovery, and balance.

use super::*;

pub(super) async fn send_with_guarded_redirects(
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

pub(super) fn is_allowed_redirect_target(source: &Url, destination: &Url) -> bool {
    source.scheme() == "https"
        && destination.scheme() == "https"
        && source.origin() == destination.origin()
        && destination.username().is_empty()
        && destination.password().is_none()
}

pub(super) fn request_after_redirect(
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
    pub(super) fn spawn_console_billing(
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

    pub(super) async fn enrich_console_balance_if_ready(
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

    pub(super) async fn request_server(
        &self,
        server_id: &str,
        args: Option<&str>,
        material: &AccountAuthMaterial,
        referer: &str,
    ) -> Result<UsageHttpResponse, TransportError> {
        self.request_server_method(server_id, args, material, referer, Method::GET)
            .await
    }

    pub(super) async fn request_server_method(
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
}

impl OpenCodeGoUsageAdapter {
    pub(super) async fn probe_web(
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

    pub(super) async fn fetch_console_status(
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

    pub(super) async fn discover_workspace_id(
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
}

pub(super) fn parse_console_snapshot(
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

pub(super) fn parse_web_page(
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

pub(super) fn find_workspace_id(value: &Value) -> Option<String> {
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

// The Console endpoint returns workspace/org rows. Only a row's own `id`
// with a recognized Console prefix is valid.
// Recursively accepting arbitrary `id` fields can select a user or nested
// resource ID and make the subsequent usage request target the wrong scope.
/// Picks the preferred workspace when it is one of this account's own
/// workspaces, otherwise the first valid workspace row.
pub(super) fn select_console_workspace_id(
    value: &Value,
    preferred: Option<&str>,
) -> Option<String> {
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

pub(super) fn is_console_workspace_id(value: &str) -> bool {
    ["wrk_", "org_"].iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    })
}

pub(super) fn console_status_missing_usage_fields(root: &Value) -> bool {
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

pub(super) fn console_status_shape_error(root: &Value) -> String {
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

pub(super) fn object_field_names(value: &Value) -> String {
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

pub(super) fn json_shape_summary(value: &Value) -> String {
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

pub(super) fn find_workspace_in_text(body: &str) -> Option<String> {
    Regex::new(r#"(?i)(?:workspace(?:Id|_id)?|org(?:Id|_id)?)\s*["']?\s*[:=]\s*["']([^"']+)["']"#)
        .ok()?
        .captures(body)
        .and_then(|captures| captures.get(1))
        .and_then(|value| normalize_workspace_id(value.as_str()))
}

pub(super) fn normalize_workspace_id(value: &str) -> Option<String> {
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

pub(super) fn web_headers(base_url: &Url, path: &str) -> Vec<(String, String)> {
    vec![
        ("Origin".to_owned(), base_url.origin().ascii_serialization()),
        ("Referer".to_owned(), format!("{}{}", base_url, path)),
        (
            "Accept".to_owned(),
            "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8".to_owned(),
        ),
    ]
}
