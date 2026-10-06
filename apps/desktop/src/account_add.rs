//! Adds accounts by running the CLI's provider sign-in, and cancels it.

use super::*;

pub(super) fn sibling_account_cli_path(current_executable: &Path) -> std::path::PathBuf {
    let extension = std::env::consts::EXE_EXTENSION;
    let filename = if extension.is_empty() {
        "usage-monitor-cli".to_owned()
    } else {
        format!("usage-monitor-cli.{extension}")
    };
    current_executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(filename)
}

pub(super) fn spawn_account_add_worker<F, Fut>(
    work: F,
) -> Result<async_channel::Receiver<Result<(), String>>, String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let (sender, receiver) = async_channel::bounded(1);
    thread::Builder::new()
        .name("usage-account-add".to_owned())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(work()),
                Err(error) => Err(format!(
                    "Could not start the account login runtime: {error}"
                )),
            };
            let _ = sender.send_blocking(result);
        })
        .map_err(|error| format!("Could not start the account login worker: {error}"))?;

    Ok(receiver)
}

/// Result of an account add the user cancelled; it is not shown as a failure.
pub(super) const ACCOUNT_ADD_CANCELLED: &str = "account add cancelled";

pub(super) async fn add_account(
    provider: UsageProvider,
    credentials: Option<(String, String)>,
    cancel: Receiver<()>,
) -> Result<(), String> {
    let current_executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate this application: {error}"))?;
    let cli_path = sibling_account_cli_path(&current_executable);
    if !cli_path.is_file() {
        return Err(format!(
            "The account tool is missing next to this application: {}. Build or install the account tool and provider login helpers together.",
            cli_path.display()
        ));
    }

    let mut command = TokioCommand::new(&cli_path);
    // This child is an implementation detail of the desktop sign-in flow.
    // Keep Windows from creating a visible console for any provider while the
    // app continues to capture its piped output and report failures itself.
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
        .args(["--json", "account", "add", provider.cli_name()])
        .stdin(if credentials.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if provider.uses_api_key() {
        command.arg("--credentials-stdin");
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start the account tool: {error}"))?;

    let mut secrets = Vec::new();
    if let Some((api_key, management_key)) = credentials {
        secrets.push(api_key.clone());
        if !management_key.is_empty() {
            secrets.push(management_key.clone());
        }

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "The account tool did not accept credentials.".to_owned())?;
        stdin
            .write_all(api_key.as_bytes())
            .await
            .map_err(|error| format!("Could not pass the API key securely: {error}"))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| format!("Could not pass the API key securely: {error}"))?;
        stdin
            .write_all(management_key.as_bytes())
            .await
            .map_err(|error| format!("Could not pass the management key securely: {error}"))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| format!("Could not pass the management key securely: {error}"))?;
        drop(stdin);
    }

    let process_id = child.id();
    let output = tokio::select! {
        output = child.wait_with_output() => output
            .map_err(|error| format!("The account sign-in flow could not finish: {error}"))?,
        Ok(()) = cancel.recv() => {
            // The account tool runs the provider's login helper, which holds
            // the sign-in callback port until it times out. End the whole
            // process tree so a new sign-in can start immediately.
            if let Some(process_id) = process_id {
                kill_process_tree(process_id);
            }
            return Err(ACCOUNT_ADD_CANCELLED.to_owned());
        }
    };
    if output.status.success() {
        Ok(())
    } else {
        let details = account_add_failure_detail(&output.stdout, &output.stderr);
        Err(redact_and_limit_account_add_error(details, &secrets))
    }
}

pub(super) fn kill_process_tree(process_id: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &process_id.to_string(), "/T", "/F"])
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &process_id.to_string()])
            .status();
    }
}

pub(super) fn account_add_failure_detail(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
    let message = serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });

    match (message, stderr.is_empty(), stdout.is_empty()) {
        (Some(message), false, _) => format!("{message}\n{stderr}"),
        (Some(message), true, _) => message,
        (None, false, _) => stderr,
        (None, true, false) => stdout,
        (None, true, true) => "The provider sign-in did not complete.".to_owned(),
    }
}

pub(super) fn redact_and_limit_account_add_error(
    mut message: String,
    secrets: &[String],
) -> String {
    let mut secrets = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        message = message.replace(secret.as_str(), "[hidden]");
    }

    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut limited = message.chars().take(360).collect::<String>();
    if message.chars().count() > 360 {
        limited.push('…');
    }
    if limited.is_empty() {
        "The provider sign-in did not complete.".to_owned()
    } else {
        limited
    }
}
