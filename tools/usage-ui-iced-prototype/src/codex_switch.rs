//! "Use in Codex" / "Use in Antigravity": signs a desktop app in with a saved
//! account and restarts it. See `codex_usage_core::codex_desktop` and
//! `codex_usage_core::antigravity_desktop` for how each app shares the
//! account with this monitor.

use codex_usage_core::accounts::AccountId;

/// Signs the Codex desktop app in with a saved Codex account.
pub async fn switch_codex_desktop_account(account_id: AccountId) -> Result<(), String> {
    run_switch(move || imp::switch(account_id)).await
}

/// Signs the Antigravity desktop app in with a saved Antigravity account.
pub async fn switch_antigravity_app_account(account_id: AccountId) -> Result<(), String> {
    run_switch(move || imp::switch_antigravity(account_id)).await
}

/// Runs a switch on its own Tokio runtime, since the UI executor is not
/// Tokio-based and the OAuth refresh needs one.
async fn run_switch<F, Fut>(switch: F) -> Result<(), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("desktop-app-switch".to_owned())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("could not start the switch runtime: {error}"))
                .and_then(|runtime| runtime.block_on(switch()));
            let _ = sender.send_blocking(result);
        })
        .map_err(|error| format!("could not start the switch: {error}"))?;
    receiver
        .recv()
        .await
        .unwrap_or_else(|_| Err("the switch ended unexpectedly".to_owned()))
}

#[cfg(target_os = "windows")]
mod imp {
    use codex_usage_core::{
        accounts::{ANTIGRAVITY, AccountId, AccountStore},
        auth::OAuthCredentialStore,
        codex_desktop::{self, CodexDesktopPaths},
        oauth_loopback::{CodexOAuthCallbackListenerFactory, LoopbackOAuthCallbackListenerFactory},
        oauth_service::OAuthAuthorizationService,
        providers::{antigravity, openai},
        storage::{SqliteStore, default_accounts_database_path},
        transport::ReqwestUsageHttpTransport,
    };
    use codex_usage_windows_auth::{
        WindowsCredentialManagerStore, WindowsDefaultBrowserLauncher, antigravity_app,
    };
    use std::{os::windows::process::CommandExt, process::Command, sync::Arc, time::Duration};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub async fn switch(account_id: AccountId) -> Result<(), String> {
        let store = SqliteStore::open(default_accounts_database_path())
            .map_err(|error| format!("could not open the accounts database: {error}"))?;
        let account = store
            .get(account_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the account no longer exists".to_owned())?;
        drop(store);

        let paths = CodexDesktopPaths::from_environment();
        let credentials = Arc::new(WindowsCredentialManagerStore);

        codex_desktop::ensure_file_credential_store(&paths).map_err(|error| error.to_string())?;

        // Close Codex before touching auth.json: it may write its in-memory
        // tokens while shutting down, which would overwrite the new account.
        let app = close_codex_app()?;

        let result = install_linked_account(&paths, &credentials, account_id, &account).await;
        // Start Codex again even when the switch failed, on whichever account
        // auth.json now holds.
        let started = start_codex_app(&app);
        result.and(started)
    }

    async fn install_linked_account(
        paths: &CodexDesktopPaths,
        credentials: &Arc<WindowsCredentialManagerStore>,
        account_id: AccountId,
        account: &codex_usage_core::accounts::AccountRecord,
    ) -> Result<(), String> {
        let paths = paths.clone();
        let credentials = Arc::clone(credentials);
        // Pull the latest tokens of the account Codex is leaving back into
        // the monitor before the link moves; reading through the store syncs.
        if let Some(previous) = codex_desktop::active_account(&paths)
            && previous != account_id
        {
            credentials
                .get(previous)
                .await
                .map_err(|error| format!("could not save the current Codex account: {error}"))?;
        }

        let tokens = if codex_desktop::active_account(&paths) == Some(account_id) {
            None
        } else {
            // A fresh refresh gives Codex a valid access token and moves the
            // single-use refresh token forward; the store keeps the new one.
            let transport = Arc::new(
                ReqwestUsageHttpTransport::new(Duration::from_secs(45))
                    .map_err(|error| format!("could not create the transport: {error}"))?,
            );
            let authorization = OAuthAuthorizationService::new(
                transport,
                Arc::clone(&credentials),
                Arc::new(CodexOAuthCallbackListenerFactory),
                Arc::new(WindowsDefaultBrowserLauncher),
            );
            let mut tokens = authorization
                .access_token(account_id, &openai::oauth_definition())
                .await
                .map_err(|error| format!("could not refresh this account's sign-in: {error}"))?;
            if tokens.id_token.is_none() {
                tokens.id_token = credentials
                    .get(account_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|credential| credential.id_token);
            }
            Some(tokens)
        };
        if let Some(tokens) = tokens {
            codex_desktop::install_account(
                &paths,
                account_id,
                account.workspace_id.as_deref(),
                &tokens,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Closes every process of the installed Codex app packages (stable and
    /// Beta) and returns the package family to start again: the one that was
    /// running, otherwise the stable app when installed.
    fn close_codex_app() -> Result<String, String> {
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$packages = @(Get-AppxPackage | Where-Object { $_.Name -in @('OpenAI.Codex', 'OpenAI.CodexBeta') } |
    Sort-Object { if ($_.Name -eq 'OpenAI.Codex') { 0 } else { 1 } })
if ($packages.Count -eq 0) { [Console]::Error.WriteLine('The Codex app is not installed.'); exit 2 }
$launch = $null
$running = @()
foreach ($package in $packages) {
    $root = $package.InstallLocation.TrimEnd('\') + '\'
    $processes = @(Get-Process | Where-Object {
        $_.Path -and $_.Path.StartsWith($root, [StringComparison]::OrdinalIgnoreCase) -and
        $_.ProcessName -ne 'codex-windows-sandbox-service'
    })
    if ($processes.Count -gt 0 -and -not $launch) { $launch = $package.PackageFamilyName }
    $running += $processes
}
if (-not $launch) { $launch = $packages[0].PackageFamilyName }
foreach ($process in $running) { [void]$process.CloseMainWindow() }
$deadline = (Get-Date).AddSeconds(6)
while ((Get-Date) -lt $deadline -and @($running | Where-Object { -not $_.HasExited }).Count -gt 0) {
    Start-Sleep -Milliseconds 250
    foreach ($process in $running) { $process.Refresh() }
}
$running | Where-Object { -not $_.HasExited } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Write-Output $launch
"#;
        let family = run_powershell(SCRIPT, "could not close Codex")?;
        let family = family.trim();
        if family.is_empty() || family.contains(['"', '\'', '`', '$', ';']) {
            return Err("could not identify the Codex app package".to_owned());
        }
        Ok(family.to_owned())
    }

    fn start_codex_app(package_family: &str) -> Result<(), String> {
        let script = format!("Start-Process 'shell:AppsFolder\\{package_family}!App'");
        run_powershell(
            &script,
            "Codex was switched but could not be started again; open it manually",
        )
        .map(|_| ())
    }

    pub async fn switch_antigravity(account_id: AccountId) -> Result<(), String> {
        let store = SqliteStore::open(default_accounts_database_path())
            .map_err(|error| format!("could not open the accounts database: {error}"))?;
        let accounts = store.list().await.map_err(|error| error.to_string())?;
        drop(store);
        if !accounts
            .iter()
            .any(|account| account.id == account_id && account.provider_id == ANTIGRAVITY)
        {
            return Err("the account no longer exists".to_owned());
        }
        let known_emails = accounts
            .iter()
            .filter(|account| account.provider_id == ANTIGRAVITY)
            .map(|account| account.email.clone())
            .collect::<Vec<_>>();

        // Refresh first: a failure here leaves the running app untouched.
        let credentials = Arc::new(WindowsCredentialManagerStore);
        let transport = Arc::new(
            ReqwestUsageHttpTransport::new(Duration::from_secs(45))
                .map_err(|error| format!("could not create the transport: {error}"))?,
        );
        let authorization = OAuthAuthorizationService::new(
            transport,
            Arc::clone(&credentials),
            Arc::new(LoopbackOAuthCallbackListenerFactory),
            Arc::new(WindowsDefaultBrowserLauncher),
        );
        let mut tokens = authorization
            .access_token(account_id, &antigravity::oauth_definition())
            .await
            .map_err(|error| format!("could not refresh this account's sign-in: {error}"))?;
        if tokens.id_token.is_none() {
            tokens.id_token = credentials
                .get(account_id)
                .await
                .ok()
                .flatten()
                .and_then(|credential| credential.id_token);
        }

        // Close the app before writing: a running app can write its own
        // sign-in back while shutting down.
        let executable = close_antigravity_app()?;
        let result =
            antigravity_app::install(&tokens, &known_emails).map_err(|error| error.to_string());
        let started = start_antigravity_app(&executable);
        result.and(started)
    }

    /// Closes the Antigravity app and its language server, returning the
    /// executable to start again.
    fn close_antigravity_app() -> Result<String, String> {
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$main = Get-Process -Name 'Antigravity' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path } | Select-Object -First 1
$exe = if ($main) { $main.Path } else { Join-Path $env:LOCALAPPDATA 'Programs\Antigravity\Antigravity.exe' }
if (-not (Test-Path $exe)) { [Console]::Error.WriteLine('The Antigravity app is not installed.'); exit 2 }
$root = (Split-Path $exe -Parent).TrimEnd('\') + '\'
$running = @(Get-Process | Where-Object {
    $_.Path -and $_.Path.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)
})
foreach ($process in $running) { [void]$process.CloseMainWindow() }
$deadline = (Get-Date).AddSeconds(8)
while ((Get-Date) -lt $deadline -and @($running | Where-Object { -not $_.HasExited }).Count -gt 0) {
    Start-Sleep -Milliseconds 250
    foreach ($process in $running) { $process.Refresh() }
}
$running | Where-Object { -not $_.HasExited } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Write-Output $exe
"#;
        let executable = run_powershell(SCRIPT, "could not close Antigravity")?;
        let executable = executable.trim();
        if executable.is_empty() || executable.contains(['"', '\'', '`', '$', ';']) {
            return Err("could not identify the Antigravity app".to_owned());
        }
        Ok(executable.to_owned())
    }

    fn start_antigravity_app(executable: &str) -> Result<(), String> {
        let script = format!("Start-Process -FilePath '{executable}'");
        run_powershell(
            &script,
            "Antigravity was switched but could not be started again; open it manually",
        )
        .map(|_| ())
    }

    fn run_powershell(script: &str, failure: &str) -> Result<String, String> {
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("{failure}: {error}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_owned();
        Err(if detail.is_empty() {
            failure.to_owned()
        } else {
            format!("{failure}: {detail}")
        })
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use codex_usage_core::accounts::AccountId;

    pub async fn switch(_account_id: AccountId) -> Result<(), String> {
        Err("Switching the Codex app is only available on Windows".to_owned())
    }

    pub async fn switch_antigravity(_account_id: AccountId) -> Result<(), String> {
        Err("Switching the Antigravity app is only available on Windows".to_owned())
    }
}
