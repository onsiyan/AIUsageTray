use codex_usage_core::{
    accounts::{AccountRecord, AccountStore, OPENAI},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialProvider, AccountAuthMaterialStore,
        StoredAuthMaterialProvider,
    },
    providers::registry::ProviderRegistryConfig,
    refresh::{RefreshCadence, RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::SqliteStore,
    transport::{ReqwestUsageHttpTransport, UsageHttpRequest, UsageHttpTransport},
    usage::UsageSnapshotStore,
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore,
    browser_cookies::{BrowserKind, BrowserLoginOptions, WindowsBrowserCookieImporter},
};
use reqwest::Method;
use serde_json::Value;
use std::{collections::BTreeMap, env, path::PathBuf, sync::Arc, time::Duration};
use url::Url;

#[derive(Debug, Default)]
struct Arguments {
    database: Option<PathBuf>,
    label: Option<String>,
    browser: Option<BrowserKind>,
    profile_id: Option<String>,
    list_profiles: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Arguments {
        database,
        label,
        browser,
        profile_id,
        list_profiles,
    } = parse_arguments()?;
    let importer = WindowsBrowserCookieImporter::from_process()?;
    if list_profiles {
        let profiles = importer.discover_profiles(browser)?;
        if profiles.is_empty() {
            println!("No supported Chromium browser profiles were found.");
        }
        for profile in profiles {
            let display_name = profile
                .display_name
                .as_deref()
                .map(|name| format!(" — {name}"))
                .unwrap_or_default();
            println!("{} / {}{display_name}", profile.browser, profile.profile_id);
        }
        return Ok(());
    }

    let database_path = database.unwrap_or_else(default_database_path);
    if let Some(parent) = database_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let sqlite = Arc::new(SqliteStore::open(&database_path)?);
    let account_store: Arc<dyn AccountStore> = sqlite.clone();
    let snapshot_store: Arc<dyn UsageSnapshotStore> = sqlite;
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let login_url = Url::parse("https://chatgpt.com/")?;

    println!("Opening ChatGPT in your default browser if no signed-in session is available.");
    println!("No password is handled by this tool; only the existing browser session is imported.");
    let login = importer
        .open_and_wait_for_provider(
            OPENAI,
            &login_url,
            BrowserLoginOptions {
                browser,
                profile_id: profile_id.clone(),
                ..BrowserLoginOptions::default()
            },
        )
        .await?;
    let material = AccountAuthMaterial {
        cookies: login.imported.cookies,
        user_agent: Some("CodexUsageMonitor/0.1".to_owned()),
        ..AccountAuthMaterial::default()
    };
    let session_identity = fetch_browser_identity(transport.as_ref(), &material).await?;

    let existing = account_store.list().await?.into_iter().find(|account| {
        account.provider_id == OPENAI && account.email.eq_ignore_ascii_case(&session_identity.email)
    });
    let mut account = if let Some(account) = existing {
        account
    } else {
        let label = label
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Codex — {}", session_identity.email));
        AccountRecord::create(label, &session_identity.email, None, OPENAI, None)?
    };
    account.browser_profile_id = Some(login.imported.profile_id);
    account_store.upsert(&account).await?;

    let secure_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    secure_store.save(account.id, &material).await?;
    let auth = Arc::new(StoredAuthMaterialProvider::new(secure_store.clone()))
        as Arc<dyn AccountAuthMaterialProvider>;
    let runtime = UsageRuntime::from_dependencies_with_auth_store(
        account_store,
        snapshot_store,
        transport,
        auth,
        secure_store,
        ProviderRegistryConfig::default(),
        RefreshCoordinatorConfig {
            cadence: RefreshCadence::Manual,
            ..RefreshCoordinatorConfig::default()
        },
    )?;

    println!("Account: {}", account.email);
    println!("Database: {}", database_path.display());
    let outcome = runtime
        .refresh_account(account, RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        let message = outcome
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_else(|| format!("refresh ended with {:?}", outcome.status));
        return Err(message.into());
    }

    let snapshot = outcome
        .snapshot
        .expect("updated refresh outcome must contain a snapshot");
    println!(
        "OK | source={} | plan={} | confidence={}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.plan_type.as_deref().unwrap_or("unknown"),
        snapshot.data_confidence
    );
    for window in snapshot.all_rate_windows() {
        let reset = window
            .reset_at_utc
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_owned());
        println!(
            "  {}: {:.2}% remaining | reset {}",
            window.name,
            window.remaining_percent(),
            reset
        );
    }
    if let Some(credits) = snapshot.credits {
        println!(
            "  credits: balance={} | available={}",
            credits
                .balance
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "unknown".to_owned()),
            credits
                .credits_available
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    Ok(())
}

async fn fetch_browser_identity(
    transport: &dyn UsageHttpTransport,
    material: &AccountAuthMaterial,
) -> Result<BrowserIdentity, Box<dyn std::error::Error>> {
    let cookie = material
        .cookie_header()
        .ok_or("the browser did not provide a usable ChatGPT session")?;
    let response = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url: Url::parse("https://chatgpt.com/api/auth/session")?,
            headers: BTreeMap::from([
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Cookie".to_owned(), cookie),
                (
                    "User-Agent".to_owned(),
                    material.user_agent.clone().unwrap_or_default(),
                ),
            ]),
            body: None,
        })
        .await?;
    if !response.is_success() {
        return Err(format!(
            "ChatGPT browser session validation returned HTTP {}",
            response.status_code
        )
        .into());
    }
    let root: Value = serde_json::from_str(&response.body)?;
    let email = root
        .get("user")
        .and_then(|user| user.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .ok_or("ChatGPT session response did not contain a signed-in email")?
        .to_owned();
    Ok(BrowserIdentity { email })
}

#[derive(Debug)]
struct BrowserIdentity {
    email: String,
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    parse_arguments_from(env::args_os().skip(1))
}

fn parse_arguments_from(
    values: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut arguments = Arguments::default();
    let mut values = values.into_iter();
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
            "--browser" => {
                let value = values
                    .next()
                    .ok_or("--browser requires chrome, edge, brave, or chromium")?
                    .to_string_lossy()
                    .into_owned();
                arguments.browser = Some(
                    BrowserKind::ALL
                        .into_iter()
                        .find(|browser| browser.as_str().eq_ignore_ascii_case(&value))
                        .ok_or("--browser must be chrome, edge, brave, or chromium")?,
                );
            }
            "--profile-id" => {
                arguments.profile_id = Some(
                    values
                        .next()
                        .ok_or("--profile-id requires a profile id")?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--list-profiles" => arguments.list_profiles = true,
            "--help" | "-h" => {
                println!(
                    "Usage: codex-usage-codex-probe [--database PATH] [--label LABEL] [--browser chrome|edge|brave|chromium] [--profile-id ID]\n       codex-usage-codex-probe --list-profiles [--browser chrome|edge|brave|chromium]\n\nImports the signed-in ChatGPT session from a supported Chromium profile, verifies its email, stores cookies per account in Windows Credential Manager, and queries WHAM directly. Use --list-profiles to find a profile id when more than one profile is available. It does not read Codex auth files or launch Codex CLI/app-server."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    if arguments.profile_id.is_some() && arguments.browser.is_none() {
        return Err("--profile-id requires --browser".into());
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::{BrowserKind, parse_arguments_from};
    use std::ffi::OsString;

    #[test]
    fn arguments_can_pin_a_browser_profile_for_multi_account_import() {
        let arguments = parse_arguments_from(
            [
                "--browser",
                "edge",
                "--profile-id",
                "Profile 2",
                "--label",
                "Codex Work",
            ]
            .map(OsString::from),
        )
        .unwrap();

        assert_eq!(arguments.browser, Some(BrowserKind::Edge));
        assert_eq!(arguments.profile_id.as_deref(), Some("Profile 2"));
        assert_eq!(arguments.label.as_deref(), Some("Codex Work"));
        assert!(!arguments.list_profiles);
    }

    #[test]
    fn arguments_reject_an_unknown_browser_name() {
        let error = parse_arguments_from(["--browser", "opera"].map(OsString::from))
            .unwrap_err()
            .to_string();

        assert!(error.contains("chrome, edge, brave, or chromium"));
    }

    #[test]
    fn arguments_can_list_profiles_without_opening_a_login_flow() {
        let arguments =
            parse_arguments_from(["--list-profiles", "--browser", "chrome"].map(OsString::from))
                .unwrap();

        assert!(arguments.list_profiles);
        assert_eq!(arguments.browser, Some(BrowserKind::Chrome));
    }
}

fn default_database_path() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("CodexUsageMonitor-Rust-CodexProbe")
        .join("accounts.db")
}
