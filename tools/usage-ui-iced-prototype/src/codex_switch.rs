//! "Use in Codex": signs the Codex desktop app in with a saved Codex account
//! and restarts it. See `codex_usage_core::codex_desktop` for how the account
//! is shared with the app without the two competing for one refresh token.

use codex_usage_core::accounts::AccountId;

/// Runs the switch on its own Tokio runtime, since the UI executor is not
/// Tokio-based and the OAuth refresh needs one.
pub async fn switch_codex_desktop_account(account_id: AccountId) -> Result<(), String> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("codex-desktop-switch".to_owned())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("could not start the switch runtime: {error}"))
                .and_then(|runtime| runtime.block_on(imp::switch(account_id)));
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
        accounts::{AccountId, AccountStore},
        auth::OAuthCredentialStore,
        codex_desktop::{self, CodexDesktopPaths},
        oauth_loopback::CodexOAuthCallbackListenerFactory,
        oauth_service::OAuthAuthorizationService,
        providers::openai,
        storage::{SqliteStore, default_accounts_database_path},
        transport::ReqwestUsageHttpTransport,
    };
    use codex_usage_windows_auth::{WindowsCredentialManagerStore, WindowsDefaultBrowserLauncher};
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

        restart_codex_app()
    }

    /// Closes the Codex desktop app (package `OpenAI.Codex`) and starts it
    /// again so it reads the new `auth.json`.
    fn restart_codex_app() -> Result<(), String> {
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$package = Get-AppxPackage -Name 'OpenAI.Codex' | Select-Object -First 1
if (-not $package) { Write-Error 'The Codex app is not installed.'; exit 2 }
$root = $package.InstallLocation.TrimEnd('\') + '\'
$running = @(Get-Process | Where-Object {
    $_.Path -and $_.Path.StartsWith($root, [StringComparison]::OrdinalIgnoreCase) -and
    $_.ProcessName -ne 'codex-windows-sandbox-service'
})
foreach ($process in $running) { [void]$process.CloseMainWindow() }
$deadline = (Get-Date).AddSeconds(6)
while ((Get-Date) -lt $deadline -and @($running | Where-Object { -not $_.HasExited }).Count -gt 0) {
    Start-Sleep -Milliseconds 250
    foreach ($process in $running) { $process.Refresh() }
}
$running | Where-Object { -not $_.HasExited } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Start-Process "shell:AppsFolder\$($package.PackageFamilyName)!App"
"#;
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("could not restart Codex: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("");
            Err(format!(
                "Codex was switched but could not be restarted; restart it manually. {detail}"
            ))
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use codex_usage_core::accounts::AccountId;

    pub async fn switch(_account_id: AccountId) -> Result<(), String> {
        Err("Switching the Codex app is only available on Windows".to_owned())
    }
}
