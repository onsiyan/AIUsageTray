//! Add and exercise one OpenCode Go account on Windows.
//!
//! Login uses OpenCode's Console device-authorization flow in the user's
//! normal browser. The helper never receives a password, embeds a WebView,
//! reads browser cookies, or keeps a browser process alive. OAuth tokens are
//! account-scoped in Windows Credential Manager.

use chrono::{Duration as ChronoDuration, Utc};
use codex_usage_core::{
    accounts::{AccountId, AccountRecord, AccountStore, OPENCODE_GO},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        OAuthBrowserLauncher, StoredAuthMaterialProvider,
    },
    opencode_go_oauth::DEFAULT_OPENCODE_CONSOLE_CLIENT_ID,
    providers::{opencode_go::OpenCodeGoSourceMode, registry::ProviderRegistryConfig},
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{ReqwestUsageHttpTransport, UsageHttpRequest, UsageHttpTransport},
    usage::UsageSnapshotStore,
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore, WindowsDefaultBrowserLauncher,
};
use reqwest::Method;
use serde_json::{Value, json};
use std::{env, path::PathBuf, sync::Arc, time::Duration};
use tokio::time::{Instant, sleep};
use url::Url;

const OPENCODE_CONSOLE_BASE_URL: &str = "https://opencode.ai/";

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    resume_account: Option<AccountId>,
    timeout_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let database_path = arguments
        .database
        .unwrap_or_else(default_accounts_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite;
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);

    let account = if let Some(account_id) = arguments.resume_account {
        let account = account_store
            .get(account_id)
            .await?
            .ok_or("OpenCode Go account was not found in the selected database")?;
        if account.provider_id != OPENCODE_GO {
            return Err("the selected account is not an OpenCode Go account".into());
        }
        let material = secure_material_store
            .get(account.id)
            .await?
            .ok_or("the account has no saved OpenCode credentials in Credential Manager")?;
        if !material.has_bearer_token() {
            return Err("the account has no saved OpenCode access token".into());
        }
        println!("Reusing the saved OpenCode Go OAuth session (token withheld).");
        account
    } else {
        // Keep the authorized account and OAuth material even when the first
        // usage probe fails, so API authorization can be tested independently.
        let label = arguments
            .label
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("OpenCode Go account");
        let timeout = Duration::from_secs(arguments.timeout_seconds.max(1));
        let login = login_with_device_code(transport.as_ref(), timeout).await?;
        let account = AccountRecord::create(
            label,
            &login.email,
            Some(login.user_id),
            OPENCODE_GO,
            login.workspace_id.clone(),
        )?
        .with_workspace_name(login.workspace_name.as_deref());
        let account = account_store
            .upsert_or_get_by_provider_identity(&account)
            .await?;
        secure_material_store
            .save(account.id, &login.material)
            .await?;
        println!("OpenCode Console authorized as {}.", account.email);
        account
    };

    let account = account_store
        .get(account.id)
        .await?
        .ok_or("OpenCode Go account disappeared after its browser session was saved")?;
    announce_cli_account_reference(&account);

    // Account addition must validate the captured session against the same
    // web source that will be used by the eventual tray refresh.  Do not let
    // a local database or an ambient API key make a bad cookie look valid.
    let auth = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let runtime = UsageRuntime::from_dependencies_with_auth_store(
        account_store.clone(),
        snapshot_store,
        transport,
        auth,
        secure_material_store,
        ProviderRegistryConfig {
            opencode_go_source_mode: OpenCodeGoSourceMode::Web,
            ..ProviderRegistryConfig::default()
        },
        RefreshCoordinatorConfig {
            cadence: RefreshCadence::Manual,
            ..RefreshCoordinatorConfig::default()
        },
    )?;

    let outcome = runtime
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        let account_ref = account.account_ref.as_deref().unwrap_or("the account");
        if let Some(error) = outcome.error {
            return Err(format!(
                "OpenCode Go usage validation failed: {:?}: {}. The account and OAuth credentials were preserved; retry with `codex-usage usage refresh {account_ref}`.",
                error.code, error.message
            )
            .into());
        }
        return Err(format!(
            "OpenCode Go usage validation failed: {:?}. The account and OAuth credentials were preserved; retry with `codex-usage usage refresh {account_ref}`.",
            outcome.status
        )
        .into());
    }

    // Refresh persists verified email/provider identity.  The web adapter
    // also returns the selected workspace as response_account_id; pin it so
    // later refreshes cannot drift to another workspace in the same session.
    let mut persisted = account_store
        .get(account.id)
        .await?
        .ok_or("new OpenCode Go account disappeared from storage")?;
    if let Some(workspace) = outcome
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.response_account_id.as_deref())
        .filter(|value| value.starts_with("wrk_") || value.starts_with("org_"))
    {
        persisted = persisted.with_workspace_id(Some(workspace));
        account_store.upsert(&persisted).await?;
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!("Account: {} ({})", persisted.label, persisted.email);
    println!(
        "OK | source={} | plan={} | confidence={}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.data_confidence
    );
    if let Some(workspace) = persisted.workspace_id.as_deref() {
        println!("Workspace: {workspace}");
    }
    for window in snapshot.all_rate_windows() {
        println!(
            "  {}: {:.2}% remaining | reset {}",
            window.name,
            window.remaining_percent(),
            window
                .reset_at_utc
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    if let Some(credits) = snapshot.credits.as_ref() {
        println!(
            "  credits: {}",
            credits
                .balance
                .map(|value| format!("${value:.4}"))
                .unwrap_or_else(|| "unavailable".to_owned())
        );
    }
    if let Some(spend) = snapshot.spend.as_ref() {
        println!(
            "  monthly: used={} | limit={} | remaining={}",
            format_money(spend.monthly_usage),
            format_money(spend.monthly_limit),
            spend
                .remaining_percent()
                .map(|value| format!("{value:.2}%"))
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    for diagnostic in &snapshot.source_diagnostics {
        println!(
            "  diagnostic[{}]: {:?} | {}",
            diagnostic.source, diagnostic.code, diagnostic.message
        );
    }
    println!("Database: {}", database_path.display());
    println!("Credential storage: Windows Credential Manager");
    Ok(())
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut arguments = Arguments {
        timeout_seconds: 900,
        ..Arguments::default()
    };
    let mut values = env::args_os().skip(1);
    while let Some(argument) = values.next() {
        match argument.to_string_lossy().as_ref() {
            "--database" => {
                arguments.database = Some(PathBuf::from(
                    values.next().ok_or("--database requires a path")?,
                ));
            }
            "--label" => {
                arguments.label = Some(
                    values
                        .next()
                        .ok_or("--label requires a value")?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--timeout-seconds" => {
                arguments.timeout_seconds = values
                    .next()
                    .ok_or("--timeout-seconds requires a number")?
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "--timeout-seconds must be a positive integer")?;
            }
            "--resume-account" => {
                let raw = values
                    .next()
                    .ok_or("--resume-account requires an account UUID")?
                    .to_string_lossy()
                    .into_owned();
                arguments.resume_account = Some(
                    raw.parse::<AccountId>()
                        .map_err(|_| "--resume-account must be a valid account UUID")?,
                );
            }
            "--help" | "-h" => {
                println!(
                    "Usage: codex-usage-opencode-go-probe [--database PATH] [--label LABEL] [--timeout-seconds N] [--resume-account ACCOUNT_ID]\n\nWithout --resume-account, starts OpenCode Console device authorization, opens the verification page in the Windows default browser, and waits for approval. The resulting OAuth access and refresh tokens are stored in Windows Credential Manager. --resume-account retries usage against a saved account/session without reopening the browser."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(arguments)
}

fn announce_cli_account_reference(account: &AccountRecord) {
    if env::var_os("CODEX_USAGE_CLI_CHILD").is_some()
        && let Some(account_ref) = account.account_ref.as_deref()
    {
        println!("CODEX_USAGE_ACCOUNT_REF={account_ref}");
    }
}

fn format_money(value: Option<f64>) -> String {
    value
        .map(|value| format!("${value:.4}"))
        .unwrap_or_else(|| "unknown".to_owned())
}

struct DeviceLogin {
    user_id: String,
    email: String,
    workspace_id: Option<String>,
    workspace_name: Option<String>,
    material: AccountAuthMaterial,
}

struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_url: Url,
    expires_in: Duration,
    interval: Duration,
}

struct DeviceTokens {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

async fn login_with_device_code(
    transport: &dyn UsageHttpTransport,
    timeout: Duration,
) -> Result<DeviceLogin, Box<dyn std::error::Error>> {
    let device = request_device_code(transport).await?;
    println!("OpenCode sign-in is opening in your default browser.");
    println!("If prompted, enter this code: {}", device.user_code);
    WindowsDefaultBrowserLauncher
        .open(&device.verification_url)
        .await?;

    let tokens = poll_device_token(transport, &device, timeout).await?;
    let user = get_authed_json(transport, "api/user", &tokens.access_token).await?;
    let user_id = json_string(&user, "id").ok_or("OpenCode user response omitted id")?;
    let email = json_string(&user, "email").ok_or("OpenCode user response omitted email")?;
    let orgs = get_authed_json(transport, "api/orgs", &tokens.access_token).await?;
    let selected_org = orgs
        .as_array()
        .and_then(|items| items.iter().find(|org| json_string(org, "id").is_some()));
    let workspace_id = selected_org.and_then(|org| json_string(org, "id"));
    let workspace_name = selected_org.and_then(|org| json_string(org, "name"));
    let expires_in = ChronoDuration::seconds(tokens.expires_in);
    let material = AccountAuthMaterial {
        bearer_token: Some(tokens.access_token.clone()),
        oauth_access_token: Some(tokens.access_token),
        oauth_refresh_token: Some(tokens.refresh_token),
        oauth_expires_at_utc: Some(Utc::now() + expires_in),
        ..AccountAuthMaterial::default()
    };

    Ok(DeviceLogin {
        user_id,
        email,
        workspace_id,
        workspace_name,
        material,
    })
}

async fn request_device_code(
    transport: &dyn UsageHttpTransport,
) -> Result<DeviceCode, Box<dyn std::error::Error>> {
    let response = send_json(
        transport,
        Method::POST,
        "auth/device/code",
        Some(json!({ "client_id": DEFAULT_OPENCODE_CONSOLE_CLIENT_ID })),
        None,
    )
    .await?;
    if !response.is_success() {
        return Err(format!(
            "OpenCode device authorization could not start (HTTP {})",
            response.status_code
        )
        .into());
    }
    let root: Value = serde_json::from_str(&response.body)?;
    let device_code =
        json_string(&root, "device_code").ok_or("device response omitted device_code")?;
    let user_code = json_string(&root, "user_code").ok_or("device response omitted user_code")?;
    let verification_url = json_string(&root, "verification_uri_complete")
        .ok_or("device response omitted verification_uri_complete")?;
    let verification_url = Url::parse(&verification_url)?;
    if verification_url.scheme() != "https"
        || verification_url.host_str() != Some("opencode.ai")
        || !verification_url.username().is_empty()
        || verification_url.password().is_some()
    {
        return Err("OpenCode returned an unexpected device verification URL".into());
    }
    let expires_in = json_u64(&root, "expires_in")
        .filter(|value| *value > 0)
        .ok_or("device response omitted a valid expires_in")?;
    let interval = json_u64(&root, "interval")
        .filter(|value| *value > 0)
        .unwrap_or(5);

    Ok(DeviceCode {
        device_code,
        user_code,
        verification_url,
        expires_in: Duration::from_secs(expires_in),
        interval: Duration::from_secs(interval.clamp(1, 30)),
    })
}

async fn poll_device_token(
    transport: &dyn UsageHttpTransport,
    device: &DeviceCode,
    timeout: Duration,
) -> Result<DeviceTokens, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout.min(device.expires_in);
    let mut interval = device.interval;
    loop {
        if Instant::now() >= deadline {
            return Err(
                "OpenCode device authorization timed out; please try adding the account again"
                    .into(),
            );
        }
        sleep(interval.min(deadline.saturating_duration_since(Instant::now()))).await;
        let response = send_json(
            transport,
            Method::POST,
            "auth/device/token",
            Some(json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device.device_code,
                "client_id": DEFAULT_OPENCODE_CONSOLE_CLIENT_ID,
            })),
            None,
        )
        .await?;
        let root: Value = serde_json::from_str(&response.body).map_err(|_| {
            format!(
                "OpenCode device-token request returned an invalid response (HTTP {})",
                response.status_code
            )
        })?;
        if response.is_success() {
            let access_token = json_string(&root, "access_token")
                .ok_or("OpenCode token response omitted access_token")?;
            let refresh_token = json_string(&root, "refresh_token")
                .ok_or("OpenCode token response omitted refresh_token")?;
            let expires_in = json_u64(&root, "expires_in")
                .and_then(|value| i64::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or("OpenCode token response omitted a valid expires_in")?;
            return Ok(DeviceTokens {
                access_token,
                refresh_token,
                expires_in,
            });
        }

        match json_string(&root, "error").as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = interval.saturating_add(Duration::from_secs(5)),
            Some("expired_token") => {
                return Err("OpenCode device code expired; please try again".into());
            }
            Some("access_denied") => return Err("OpenCode sign-in was denied".into()),
            Some(code) => {
                return Err(format!("OpenCode device authorization failed ({code})").into());
            }
            None => {
                return Err(format!(
                    "OpenCode device authorization failed (HTTP {})",
                    response.status_code
                )
                .into());
            }
        }
    }
}

async fn get_authed_json(
    transport: &dyn UsageHttpTransport,
    path: &str,
    access_token: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = send_json(transport, Method::GET, path, None, Some(access_token)).await?;
    if !response.is_success() {
        return Err(format!(
            "OpenCode account identity request failed (HTTP {})",
            response.status_code
        )
        .into());
    }
    Ok(serde_json::from_str(&response.body)?)
}

async fn send_json(
    transport: &dyn UsageHttpTransport,
    method: Method,
    path: &str,
    body: Option<Value>,
    access_token: Option<&str>,
) -> Result<codex_usage_core::transport::UsageHttpResponse, Box<dyn std::error::Error>> {
    let url = Url::parse(OPENCODE_CONSOLE_BASE_URL)?.join(path)?;
    let mut headers = std::collections::BTreeMap::from([
        ("Accept".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), "CodexUsageMonitor/0.1".to_owned()),
    ]);
    let body = body.map(|value| value.to_string());
    if body.is_some() {
        headers.insert("Content-Type".to_owned(), "application/json".to_owned());
    }
    if let Some(access_token) = access_token {
        headers.insert("Authorization".to_owned(), format!("Bearer {access_token}"));
    }
    Ok(transport
        .send(UsageHttpRequest {
            method,
            url,
            headers,
            body,
        })
        .await?)
}

fn json_string(root: &Value, key: &str) -> Option<String> {
    root.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn json_u64(root: &Value, key: &str) -> Option<u64> {
    root.get(key).and_then(Value::as_u64)
}
