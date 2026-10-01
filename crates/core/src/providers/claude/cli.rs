//! Claude Code CLI fallback: running `claude /usage`, its cooldown, and result cache.

use super::*;

#[derive(Debug, Clone)]
pub(super) struct CachedCliResult {
    pub(super) recorded_at: chrono::DateTime<Utc>,
    pub(super) result: UsageProbeResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CliCacheKey {
    pub(super) account_id: AccountId,
    pub(super) binary: String,
    pub(super) account_scope: String,
    pub(super) use_web_extras: bool,
    pub(super) include_prepaid_balance: bool,
}

impl ClaudeUsageAdapter {
    pub(super) async fn probe_cli(
        &self,
        account: &AccountRecord,
        options: ClaudeCliProbeOptions,
        session_key: Option<&str>,
    ) -> UsageProbeResult {
        let environment = std::env::vars().collect::<HashMap<_, _>>();
        let binary = match claude_cli::resolve_binary(&environment) {
            Some(binary) => binary,
            None => {
                return map_claude_cli_error(ClaudeCliError::NotInstalled);
            }
        };
        let binary_label = binary.to_string_lossy().to_ascii_lowercase();
        let cache_key = CliCacheKey {
            account_id: account.id,
            binary: binary_label.clone(),
            account_scope: account
                .provider_account_id
                .clone()
                .unwrap_or_else(|| account.email.to_ascii_lowercase()),
            // The Rust adapter currently enriches OAuth directly with Web;
            // CLI web extras stay an explicit future key rather than being
            // silently conflated with this cache entry.
            use_web_extras: self.fetch_web_extras,
            include_prepaid_balance: self.fetch_prepaid_credits,
        };
        if options.use_background_cache
            && let Some(cached) = self.cli_cached(&cache_key).await
        {
            return cached;
        }
        if !options.user_initiated {
            if let Some(retry_after_seconds) = self
                .cli_rate_limit_remaining(&binary_label, &environment)
                .await
            {
                return UsageProbeResult::failure(crate::usage::UsageAdapterError {
                    code: crate::usage::UsageAdapterErrorCode::RateLimited,
                    message: "Claude CLI usage endpoint is temporarily rate-limited".to_owned(),
                    http_status_code: None,
                    retry_after_seconds: Some(retry_after_seconds as u64),
                });
            }
            if matches!(
                claude_cli::auth_status(&environment, std::time::Duration::from_secs(5)).await,
                Ok(claude_cli::ClaudeCliAuthStatus::LoggedOut)
            ) {
                return map_claude_cli_error(ClaudeCliError::NotLoggedIn);
            }
        }
        let result = claude_cli::probe(&environment, options).await;
        let result = match result {
            Err(ClaudeCliError::TimedOut) | Err(ClaudeCliError::Parse(_))
                if options.retry_timeout > options.timeout =>
            {
                let mut retry_options = options;
                retry_options.timeout = options.retry_timeout;
                retry_options.retry_timeout = options.retry_timeout;
                claude_cli::probe(&environment, retry_options).await
            }
            result => result,
        };
        let usage = match result {
            Ok(usage) => {
                self.cli_rate_limit_clear(&binary_label, &environment).await;
                usage
            }
            Err(error) => {
                if matches!(error, ClaudeCliError::RateLimited) {
                    self.cli_rate_limit_record(&binary_label, &environment)
                        .await;
                }
                return map_claude_cli_error(error);
            }
        };
        // The CLI reads the machine's global Claude Code login, which is not
        // bound to this account. As an automatic fallback it may only report
        // usage when it proves it is signed in as this same account;
        // otherwise another account's usage would be published here.
        if let Some(rejection) =
            unverified_cli_fallback(self.source_mode, account, usage.observed_email.as_deref())
        {
            return rejection;
        }
        if self.fetch_account_identity && !email_matches(account, usage.observed_email.as_deref()) {
            return account_mismatch("Claude CLI session belongs to another account");
        }
        let now = Utc::now();
        let mut metrics = Vec::new();
        add_metric(&mut metrics, "session", Some(&usage.primary));
        add_metric(&mut metrics, "weekly", usage.secondary.as_ref());
        for item in &usage.additional_windows {
            add_metric(&mut metrics, &item.key, Some(&item.window));
        }
        let observed_email = usage
            .observed_email
            .clone()
            .or_else(|| Some(account.email.clone()));
        // The CLI status panel exposes a display organization, not a stable
        // provider UUID. Do not persist that label as an account id.
        let response_account_id = account.provider_account_id.clone();
        let plan_type = usage.plan_type.clone();
        let result = UsageProbeResult::success(
            UsageSnapshot {
                account_id: account.id,
                observed_at_utc: now,
                response_account_id: response_account_id.clone(),
                plan_type: plan_type.clone(),
                primary: Some(usage.primary),
                primary_window_kind: Some(UsagePrimaryWindowKind::Session),
                primary_window_is_synthetic: false,
                secondary: usage.secondary,
                additional_windows: usage.additional_windows,
                credits: None,
                credit_inventory: None,
                spend: None,
                observed_email: observed_email.clone(),
                is_stale: false,
                stale_reason: None,
                stale_at_utc: None,
                metrics,
                source_diagnostics: Vec::new(),
                provider_id: CLAUDE.to_owned(),
                source: Some("cli".to_owned()),
                data_confidence: "authoritative".to_owned(),
            },
            Some(VerifiedIdentity {
                email: observed_email,
                provider_account_id: response_account_id,
                plan_type,
            }),
        );
        let result = if self.runtime == ClaudeRuntime::App && self.fetch_web_extras {
            self.merge_web_extras(account, result, session_key).await
        } else {
            result
        };
        if options.use_background_cache {
            self.cli_cache_store(cache_key, &result).await;
        }
        result
    }

    pub(super) async fn cli_rate_limit_remaining(
        &self,
        key: &str,
        environment: &HashMap<String, String>,
    ) -> Option<i64> {
        let now = Utc::now();
        let mut blocked = self.cli_rate_limit_until.lock().await;
        if let Some(until) = blocked.get(key).copied() {
            if until > now {
                return Some((until - now).num_seconds().max(1));
            }
            blocked.remove(key);
        }
        drop(blocked);
        let remaining = claude_cli::persisted_rate_limit_remaining(environment)?;
        self.cli_rate_limit_until
            .lock()
            .await
            .insert(key.to_owned(), now + Duration::seconds(remaining.max(1)));
        Some(remaining.max(1))
    }

    pub(super) async fn cli_rate_limit_record(
        &self,
        key: &str,
        environment: &HashMap<String, String>,
    ) {
        self.cli_rate_limit_until
            .lock()
            .await
            .insert(key.to_owned(), Utc::now() + Duration::minutes(5));
        claude_cli::record_persisted_rate_limit(environment, 5 * 60);
    }

    pub(super) async fn cli_rate_limit_clear(
        &self,
        key: &str,
        environment: &HashMap<String, String>,
    ) {
        self.cli_rate_limit_until.lock().await.remove(key);
        claude_cli::clear_persisted_rate_limit(environment);
    }

    pub(super) async fn cli_cached(&self, key: &CliCacheKey) -> Option<UsageProbeResult> {
        let now = Utc::now();
        let mut cache = self.cli_cache.lock().await;
        let entry = cache.get(key).cloned()?;
        let expired = (now - entry.recorded_at) >= Duration::minutes(15);
        let reset = entry.result.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .all_rate_windows()
                .any(|window| window.reset_at_utc.is_some_and(|reset| reset <= now))
        });
        if expired || reset {
            cache.remove(key);
            return None;
        }
        Some(entry.result)
    }

    pub(super) async fn cli_cache_store(&self, key: CliCacheKey, result: &UsageProbeResult) {
        if result.succeeded() {
            self.cli_cache.lock().await.insert(
                key,
                CachedCliResult {
                    recorded_at: Utc::now(),
                    result: result.clone(),
                },
            );
        }
    }
}

/// The CLI reads the machine's global Claude Code login. As an automatic
/// fallback it is accepted only when it reports this account's email.
pub(super) fn unverified_cli_fallback(
    source_mode: ClaudeSourceMode,
    account: &AccountRecord,
    cli_email: Option<&str>,
) -> Option<UsageProbeResult> {
    let verified = cli_email.is_some_and(|email| email_matches(account, Some(email)));
    (source_mode == ClaudeSourceMode::Automatic && !verified).then(|| {
        UsageProbeResult::failure(crate::usage::UsageAdapterError {
            code: UsageAdapterErrorCode::AuthenticationUnavailable,
            message: "Claude CLI is not verified as signed in to this account".to_owned(),
            http_status_code: None,
            retry_after_seconds: None,
        })
    })
}

pub(super) fn map_claude_cli_error(error: ClaudeCliError) -> UsageProbeResult {
    let (code, retry_after_seconds) = match error {
        ClaudeCliError::NotInstalled | ClaudeCliError::NotLoggedIn => (
            crate::usage::UsageAdapterErrorCode::AuthenticationUnavailable,
            None,
        ),
        ClaudeCliError::RateLimited => {
            (crate::usage::UsageAdapterErrorCode::RateLimited, Some(300))
        }
        ClaudeCliError::TimedOut | ClaudeCliError::ProcessExited | ClaudeCliError::Launch(_) => {
            (crate::usage::UsageAdapterErrorCode::TransientHttp, None)
        }
        ClaudeCliError::OutputTooLarge | ClaudeCliError::Parse(_) => {
            (crate::usage::UsageAdapterErrorCode::InvalidPayload, None)
        }
    };
    UsageProbeResult::failure(crate::usage::UsageAdapterError {
        code,
        message: error.to_string(),
        http_status_code: None,
        retry_after_seconds,
    })
}
