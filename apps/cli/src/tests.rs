use super::*;
use usage_monitor_core::{
    accounts::{ANTIGRAVITY, InMemoryAccountStore},
    auth::{InMemoryAuthMaterialStore, InMemoryOAuthCredentialStore, StoredOAuthCredential},
};

fn account(
    provider: &str,
    reference: &str,
    label: &str,
    email: &str,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
) -> AccountRecord {
    let mut account = AccountRecord::create(
        label,
        email,
        None,
        provider,
        workspace_id.map(str::to_owned),
    )
    .unwrap();
    account.account_ref = Some(reference.to_owned());
    account.workspace_name = workspace_name.map(str::to_owned);
    account
}

#[test]
fn stable_reference_selects_account_and_json_hides_internal_uuid() {
    let first = account(
        OPENAI,
        "ch1",
        "Work",
        "same@example.com",
        Some("ws-1"),
        None,
    );
    let mut second = account(
        CLAUDE,
        "cc1",
        "Work",
        "same@example.com",
        Some("ws-2"),
        None,
    );
    second.alias = Some("ch1".to_owned());
    let accounts = [first.clone(), second];

    let selected = resolve_account(&accounts, "CH1", &SelectorFilters::default()).unwrap();
    assert_eq!(selected.id, first.id);
    let value = account_value(selected);
    assert_eq!(value["account_ref"], "ch1");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["provider_id"], "openai");
    assert!(value.get("id").is_none());
    assert!(!value.to_string().contains(&first.id.to_string()));

    let filtered = resolve_account(
        &accounts,
        "ch1",
        &SelectorFilters {
            provider: Some("claude"),
            workspace: None,
        },
    )
    .unwrap_err();
    assert_eq!(filtered.code, "account_not_found");
}

#[test]
fn ambiguous_email_requires_provider_or_workspace_qualifier() {
    let first = account(
        OPENAI,
        "ch1",
        "Personal",
        "same@example.com",
        Some("ws-1"),
        None,
    );
    let second = account(
        OPENAI,
        "ch2",
        "Work",
        "same@example.com",
        Some("ws-2"),
        None,
    );
    let accounts = [first, second];

    let error =
        resolve_account(&accounts, "same@example.com", &SelectorFilters::default()).unwrap_err();
    assert_eq!(error.code, "ambiguous_account");
    assert_eq!(error.candidates, ["ch1", "ch2"]);

    let selected = resolve_account(
        &accounts,
        "same@example.com",
        &SelectorFilters {
            provider: Some("codex"),
            workspace: Some("ws-2"),
        },
    )
    .unwrap();
    assert_eq!(selected.account_ref.as_deref(), Some("ch2"));
}

#[test]
fn command_line_accepts_json_after_subcommands_and_refresh_all() {
    let parsed =
        Cli::try_parse_from(["usage-monitor-cli", "usage", "refresh", "--all", "--json"]).unwrap();
    assert!(parsed.json);
    assert!(matches!(
        parsed.command,
        Command::Usage {
            command: UsageCommand::Refresh { all: true, .. }
        }
    ));
}

#[test]
fn account_remove_accepts_stable_reference_confirmation_and_delete_alias() {
    for verb in ["remove", "delete"] {
        let parsed = Cli::try_parse_from([
            "usage-monitor-cli",
            "account",
            verb,
            "ch2",
            "--yes",
            "--provider",
            "codex",
            "--workspace",
            "Work",
            "--json",
        ])
        .unwrap();
        assert!(parsed.json);
        let Command::Account {
            command: AccountCommand::Remove(arguments),
        } = parsed.command
        else {
            panic!("expected account remove command");
        };
        assert_eq!(arguments.selection.selector, "ch2");
        assert_eq!(arguments.selection.provider.as_deref(), Some("codex"));
        assert_eq!(arguments.selection.workspace.as_deref(), Some("Work"));
        assert!(arguments.yes);
    }
}

#[test]
fn account_remove_requires_explicit_confirmation_without_an_interactive_terminal() {
    assert_eq!(
        removal_confirmation_mode(true, true, false).unwrap(),
        RemovalConfirmationMode::Confirmed
    );
    assert_eq!(
        removal_confirmation_mode(false, false, true).unwrap(),
        RemovalConfirmationMode::Prompt
    );
    for (json_output, interactive) in [(true, true), (false, false)] {
        let error = removal_confirmation_mode(false, json_output, interactive).unwrap_err();
        assert_eq!(error.code, "confirmation_required");
        assert_eq!(error.exit_code, EXIT_OPERATION_NOT_CONFIRMED);
    }
}

#[tokio::test]
async fn local_account_removal_clears_account_and_both_credential_stores() {
    let account_store = InMemoryAccountStore::default();
    let oauth_store = InMemoryOAuthCredentialStore::default();
    let auth_store = InMemoryAuthMaterialStore::default();
    let account = AccountRecord::create("Work", "work@example.com", None, OPENAI, None).unwrap();
    account_store.upsert(&account).await.unwrap();
    oauth_store
        .save(
            account.id,
            &StoredOAuthCredential {
                provider_id: OPENAI.to_owned(),
                refresh_token: "test-refresh-token".to_owned(),
                client_id: None,
                client_secret: None,
                id_token: None,
                provider_account_id: None,
                workspace_id: None,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    auth_store
        .save(
            account.id,
            &AccountAuthMaterial {
                bearer_token: Some("test-access-token".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    remove_local_account_data(&account, &account_store, &oauth_store, &auth_store)
        .await
        .unwrap();

    assert!(account_store.get(account.id).await.unwrap().is_none());
    assert!(oauth_store.get(account.id).await.unwrap().is_none());
    assert!(auth_store.get(account.id).await.unwrap().is_none());
}

#[test]
fn account_add_accepts_supported_provider_names_and_json_mode() {
    let parsed = Cli::try_parse_from([
        "usage-monitor-cli",
        "account",
        "add",
        "openrouter",
        "--credentials-stdin",
        "--alias",
        "Research key",
        "--json",
    ])
    .unwrap();
    assert!(parsed.json);
    let Command::Account {
        command: AccountCommand::Add(arguments),
    } = parsed.command
    else {
        panic!("expected account add command");
    };
    assert_eq!(
        AccountAddProvider::parse(&arguments.provider),
        Some(AccountAddProvider::OpenRouter)
    );
    assert_eq!(arguments.alias.as_deref(), Some("Research key"));
    assert!(arguments.credentials_stdin);
    assert!(account_add_uses_stdin(
        AccountAddProvider::OpenRouter,
        &arguments
    ));
}

#[test]
fn account_add_routes_each_provider_to_its_existing_login_flow() {
    let cases = [
        ("codex", AccountAddProvider::Codex),
        ("claude", AccountAddProvider::Claude),
        ("openrouter", AccountAddProvider::OpenRouter),
        ("opencode-go", AccountAddProvider::OpenCodeGo),
        ("antigravity", AccountAddProvider::Antigravity),
    ];
    let arguments = AccountAddArgs {
        provider: String::new(),
        alias: None,
        api_key_stdin: false,
        credentials_stdin: false,
    };

    for (name, expected_provider) in cases {
        let provider = AccountAddProvider::parse(name).unwrap();
        assert_eq!(provider, expected_provider);
        // The sign-in helper picks the provider flow from its first argument.
        let built = build_account_add_arguments(provider, Path::new("accounts.db"), &arguments);
        assert_eq!(built[0].to_string_lossy(), provider.login_command());
        assert_eq!(provider.login_command(), name);
    }
    assert_eq!(
        AccountAddProvider::parse("openai"),
        Some(AccountAddProvider::Codex)
    );
    assert_eq!(
        AccountAddProvider::parse("opencodego"),
        Some(AccountAddProvider::OpenCodeGo)
    );
    assert!(AccountAddProvider::parse("unknown").is_none());
}

#[test]
fn claude_account_add_uses_only_the_provider_owned_oauth_login() {
    let arguments = AccountAddArgs {
        provider: "claude".to_owned(),
        alias: Some("Work Claude".to_owned()),
        api_key_stdin: false,
        credentials_stdin: false,
    };
    let built = build_account_add_arguments(
        AccountAddProvider::Claude,
        Path::new("accounts.db"),
        &arguments,
    );
    let built = built
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert!(
        built
            .windows(2)
            .any(|pair| pair == ["--database", "accounts.db"])
    );
    assert!(
        built
            .windows(2)
            .any(|pair| pair == ["--label", "Work Claude"])
    );
    assert!(!built.iter().any(|value| value == "--browser"));
    assert!(!built.iter().any(|value| value == "--profile"));
    assert!(!built.iter().any(|value| value == "--login"));
    assert!(!built.iter().any(|value| value == "--probe-existing"));
}

#[test]
fn openrouter_add_passes_secrets_only_through_stdin_not_arguments() {
    let arguments = AccountAddArgs {
        provider: "openrouter".to_owned(),
        alias: Some("Test key".to_owned()),
        api_key_stdin: true,
        credentials_stdin: false,
    };
    let built = build_account_add_arguments(
        AccountAddProvider::OpenRouter,
        Path::new("accounts.db"),
        &arguments,
    );
    let built = built
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert!(
        built
            .windows(2)
            .any(|pair| pair == ["--database", "accounts.db"])
    );
    assert!(built.windows(2).any(|pair| pair == ["--label", "Test key"]));
    assert!(built.contains(&"--api-key-stdin".to_owned()));
    assert!(built.contains(&"--new".to_owned()));
    assert!(!built.iter().any(|value| value.contains("sk-or-")));
}

#[test]
fn openrouter_add_rejects_missing_or_blank_environment_keys() {
    let arguments = AccountAddArgs {
        provider: "openrouter".to_owned(),
        alias: None,
        api_key_stdin: false,
        credentials_stdin: false,
    };

    assert!(!has_openrouter_key_source(&arguments, None));
    assert!(!has_openrouter_key_source(&arguments, Some("  \t")));
    assert!(has_openrouter_key_source(&arguments, Some("sk-or-test")));
    assert!(has_openrouter_key_source(
        &AccountAddArgs {
            api_key_stdin: true,
            ..arguments
        },
        None
    ));
}

#[tokio::test]
async fn provider_reference_marker_is_captured_from_child_stdout() {
    use tokio::io::AsyncWriteExt;

    let (mut writer, reader) = tokio::io::duplex(128);
    writer
        .write_all(b"USAGE_MONITOR_ACCOUNT_REF=ch4\r\n")
        .await
        .unwrap();
    drop(writer);

    let references = forward_probe_stdout(reader, true).await.unwrap();
    assert_eq!(references, ["ch4"]);
}

#[test]
fn usage_get_json_includes_the_refreshed_snapshot_and_status() {
    let account = account(OPENAI, "ch1", "Personal", "user@example.com", None, None);
    let outcome = test_refresh_outcome(
        &account,
        RefreshStatus::Updated,
        Some(test_snapshot(&account, false)),
        None,
    );

    let value = usage_get_value(&account, &outcome);
    assert_eq!(value["refresh"]["status"], "updated");
    assert_eq!(value["snapshot"]["is_stale"], false);
    assert_eq!(usage_get_exit_code(outcome.status), 0);
}

#[test]
fn usage_get_json_keeps_last_good_snapshot_and_reports_refresh_failure() {
    let account = account(OPENAI, "ch1", "Personal", "user@example.com", None, None);
    let error = UsageAdapterError {
        code: usage_monitor_core::usage::UsageAdapterErrorCode::NetworkFailure,
        message: "provider is unavailable".to_owned(),
        http_status_code: None,
        retry_after_seconds: None,
    };
    let outcome = test_refresh_outcome(
        &account,
        RefreshStatus::RetainedStale,
        Some(test_snapshot(&account, true)),
        Some(error),
    );

    let value = usage_get_value(&account, &outcome);
    assert_eq!(value["refresh"]["status"], "retained_stale");
    assert_eq!(value["snapshot"]["is_stale"], true);
    assert_eq!(
        value["refresh"]["error"]["message"],
        "provider is unavailable"
    );
    assert_eq!(usage_get_exit_code(outcome.status), EXIT_REFRESH_FAILED);
}

fn test_refresh_outcome(
    account: &AccountRecord,
    status: RefreshStatus,
    snapshot: Option<UsageSnapshot>,
    error: Option<UsageAdapterError>,
) -> RefreshOutcome {
    RefreshOutcome {
        account_id: account.id,
        provider_id: account.provider_id.clone(),
        reason: RefreshReason::Manual,
        status,
        snapshot,
        identity: None,
        error,
        storage_error: None,
        completed_at_utc: Utc::now(),
    }
}

fn test_snapshot(account: &AccountRecord, is_stale: bool) -> UsageSnapshot {
    UsageSnapshot {
        account_id: account.id,
        observed_at_utc: Utc::now(),
        response_account_id: None,
        plan_type: Some("free".to_owned()),
        primary: Some(usage_monitor_core::usage::RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "Primary".to_owned(),
            used_percent: 25.0,
            reset_at_utc: None,
            limit_window_seconds: 3600,
        }),
        primary_window_kind: None,
        primary_window_is_synthetic: false,
        secondary: None,
        additional_windows: Vec::new(),
        credits: None,
        credit_inventory: None,
        spend: None,
        observed_email: Some(account.email.clone()),
        is_stale,
        stale_reason: is_stale.then(|| "temporary provider failure".to_owned()),
        stale_at_utc: is_stale.then(Utc::now),
        metrics: Vec::new(),
        source_diagnostics: Vec::new(),
        provider_id: account.provider_id.clone(),
        source: Some("test".to_owned()),
        data_confidence: "authoritative".to_owned(),
    }
}

#[test]
fn refresh_modes_do_not_fall_back_to_claude_web_or_global_cli() {
    let claude = account(CLAUDE, "cc1", "Claude", "claude@example.com", None, None);
    let config = provider_config_for(&claude, None);
    assert_eq!(config.claude_source_mode, ClaudeSourceMode::OAuth);
    let web_material = AccountAuthMaterial::from_cookie_header("sessionKey=web-session", None);
    assert_eq!(
        provider_config_for(&claude, Some(&web_material)).claude_source_mode,
        ClaudeSourceMode::OAuth
    );

    let opencode = account(
        OPENCODE_GO,
        "oc1",
        "OpenCode",
        "opencode@example.com",
        None,
        None,
    );
    assert_eq!(
        provider_config_for(&opencode, None).opencode_go_source_mode,
        OpenCodeGoSourceMode::Api
    );
    let browser_material = AccountAuthMaterial::from_cookie_header("auth=browser-session", None);
    assert_eq!(
        provider_config_for(&opencode, Some(&browser_material)).opencode_go_source_mode,
        OpenCodeGoSourceMode::Web
    );
    let console_oauth_material = AccountAuthMaterial {
        bearer_token: Some("console-access-token".to_owned()),
        oauth_access_token: Some("console-access-token".to_owned()),
        oauth_refresh_token: Some("console-refresh-token".to_owned()),
        ..AccountAuthMaterial::default()
    };
    assert_eq!(
        provider_config_for(&opencode, Some(&console_oauth_material)).opencode_go_source_mode,
        OpenCodeGoSourceMode::Web
    );
    let unrelated_cookie = AccountAuthMaterial::from_cookie_header("unrelated=value", None);
    assert_eq!(
        provider_config_for(&opencode, Some(&unrelated_cookie)).opencode_go_source_mode,
        OpenCodeGoSourceMode::Api
    );

    let antigravity = account(
        ANTIGRAVITY,
        "ag1",
        "Google",
        "google@example.com",
        None,
        None,
    );
    assert!(!provider_config_for(&antigravity, None).enable_antigravity_local_probe);
}

struct ModeMarker(&'static str, &'static str);

#[async_trait::async_trait]
impl UsageAdapter for ModeMarker {
    fn adapter_id(&self) -> &str {
        self.0
    }

    async fn probe(&self, _account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        Ok(UsageProbeResult::failure(UsageAdapterError {
            code: usage_monitor_core::usage::UsageAdapterErrorCode::Unknown,
            message: self.1.to_owned(),
            http_status_code: None,
            retry_after_seconds: None,
        }))
    }
}

#[tokio::test]
async fn scheduler_uses_each_accounts_own_source_mode() {
    let dispatcher = |provider_id: &'static str| PerAccountSourceAdapter {
        provider_id: provider_id.to_owned(),
        auth_store: Arc::new(WindowsCredentialManagerAuthMaterialStore),
        variants: SOURCE_VARIANTS
            .iter()
            .map(|modes| {
                let marker: &'static str = match modes {
                    (ClaudeSourceMode::OAuth, OpenCodeGoSourceMode::Web) => "oauth/web",
                    (ClaudeSourceMode::OAuth, _) => "oauth/api",
                    (_, OpenCodeGoSourceMode::Web) => "admin/web",
                    _ => "admin/api",
                };
                (
                    *modes,
                    Arc::new(ModeMarker(provider_id, marker)) as Arc<dyn UsageAdapter>,
                )
            })
            .collect(),
    };
    // A fresh account id has no saved material: Claude fails closed to
    // OAuth and OpenCode Go uses the strict API source, as in `refresh`.
    let claude = account(CLAUDE, "cc1", "Claude", "c@example.com", None, None);
    let result = dispatcher(CLAUDE).probe(&claude).await.unwrap();
    assert!(result.error.unwrap().message.starts_with("oauth/"));
    let opencode = account(OPENCODE_GO, "oc1", "Go", "g@example.com", None, None);
    let result = dispatcher(OPENCODE_GO).probe(&opencode).await.unwrap();
    assert!(result.error.unwrap().message.ends_with("/api"));
}
