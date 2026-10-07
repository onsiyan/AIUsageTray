//! Adds accounts by running the CLI's provider sign-in, and cancels it.

use super::*;
use usage_monitor_core::{
    oauth_loopback::CALLBACK_BIND_FAILURE,
    providers::{codex_device, copilot, openai},
    transport::ReqwestUsageHttpTransport,
};

/// A one-time code the user enters on the provider's page while the app
/// waits: GitHub's for Copilot, OpenAI's for Codex.
#[derive(Debug, Clone)]
pub(super) struct DeviceSignIn {
    pub provider: UsageProvider,
    pub user_code: String,
    pub verification_uri: String,
}

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

    if credentials.is_some() {
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

/// Signs in to GitHub with a device code, then adds the Copilot account
/// with the token. `codes` receives the code to show, then `None` once it
/// was entered.
pub(super) async fn add_copilot_account(
    codes: Sender<Option<DeviceSignIn>>,
    cancel: Receiver<()>,
) -> Result<(), String> {
    let transport = ReqwestUsageHttpTransport::new(std::time::Duration::from_secs(30))
        .map_err(|error| format!("Could not reach GitHub: {error}"))?;
    let code = tokio::select! {
        code = copilot::request_device_code(&transport) => code.map_err(|error| error.to_string())?,
        Ok(()) = cancel.recv() => return Err(ACCOUNT_ADD_CANCELLED.to_owned()),
    };
    let _ = codes
        .send(Some(DeviceSignIn {
            provider: UsageProvider::Copilot,
            user_code: code.user_code.clone(),
            verification_uri: code.verification_uri.clone(),
        }))
        .await;
    let token = tokio::select! {
        token = copilot::poll_for_token(&transport, &code) => token.map_err(|error| error.to_string()),
        Ok(()) = cancel.recv() => Err(ACCOUNT_ADD_CANCELLED.to_owned()),
    };
    let _ = codes.send(None).await;
    add_account(
        UsageProvider::Copilot,
        Some((token?, String::new())),
        cancel,
    )
    .await
}

/// Adds a Codex account with the usual browser sign-in. When Windows will
/// not let the app listen for its reply (a reserved port), signs in with a
/// code entered on OpenAI's page instead, shown as `codes` receives it.
pub(super) async fn add_codex_account(
    codes: Sender<Option<DeviceSignIn>>,
    cancel: Receiver<()>,
) -> Result<(), String> {
    match add_account(UsageProvider::Codex, None, cancel.clone()).await {
        Err(error) if error.contains(CALLBACK_BIND_FAILURE) => {}
        result => return result,
    }
    let transport = ReqwestUsageHttpTransport::new(std::time::Duration::from_secs(30))
        .map_err(|error| format!("Could not reach OpenAI: {error}"))?;
    let client_id = openai::oauth_definition().client_id;
    let code = tokio::select! {
        code = codex_device::request_device_code(&transport, &client_id) => code.map_err(|error| error.to_string())?,
        Ok(()) = cancel.recv() => return Err(ACCOUNT_ADD_CANCELLED.to_owned()),
    };
    let _ = codes
        .send(Some(DeviceSignIn {
            provider: UsageProvider::Codex,
            user_code: code.user_code.clone(),
            verification_uri: codex_device::VERIFICATION_URL.to_owned(),
        }))
        .await;
    let granted = tokio::select! {
        granted = codex_device::poll_for_authorization(&transport, &code) => granted.map_err(|error| error.to_string()),
        Ok(()) = cancel.recv() => Err(ACCOUNT_ADD_CANCELLED.to_owned()),
    };
    let _ = codes.send(None).await;
    let granted = granted?;
    add_account(
        UsageProvider::Codex,
        Some((granted.authorization_code, granted.code_verifier)),
        cancel,
    )
    .await
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

/// Opens a web page in the default browser.
pub(super) fn open_in_browser(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    #[cfg(windows)]
    {
        let operation: Vec<u16> = "open\0".encode_utf16().collect();
        let url: Vec<u16> = format!("{url}\0").encode_utf16().collect();
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                url.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
    }
}

/// Puts `text` on the Windows clipboard.
///
/// Written directly with the Win32 API: iced's clipboard is tied to one
/// window and does nothing once the tray popup has been closed and reopened.
pub(super) fn copy_to_clipboard(text: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::GlobalFree,
            System::{
                DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
                Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            },
        };
        const CF_UNICODETEXT: u32 = 13;
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let bytes = wide.len() * std::mem::size_of::<u16>();
        // SAFETY: the allocation is `bytes` long and filled from `wide`; once
        // SetClipboardData succeeds the system owns it, otherwise it is freed.
        unsafe {
            // With no owner window, EmptyClipboard leaves the clipboard
            // ownerless and SetClipboardData then fails, so one of the app's
            // own windows takes ownership. Another program may hold the
            // clipboard for a moment; try a few times.
            let owner = own_window();
            if !(0..5).any(|attempt| {
                if attempt > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                OpenClipboard(owner) != 0
            }) {
                return;
            }
            EmptyClipboard();
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !memory.is_null() {
                let target = GlobalLock(memory).cast::<u16>();
                if target.is_null() {
                    GlobalFree(memory);
                } else {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                    GlobalUnlock(memory);
                    if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
                        GlobalFree(memory);
                    }
                }
            }
            CloseClipboard();
        }
    }
    #[cfg(not(windows))]
    let _ = text;
}

/// A top-level window of this process, to own the clipboard.
#[cfg(windows)]
fn own_window() -> windows_sys::Win32::Foundation::HWND {
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM},
        System::Threading::GetCurrentProcessId,
        UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId},
    };
    unsafe extern "system" fn find(window: HWND, found: LPARAM) -> i32 {
        let mut process = 0;
        // SAFETY: `found` points at the HWND slot owned by `own_window`.
        unsafe {
            GetWindowThreadProcessId(window, &mut process);
            if process == GetCurrentProcessId() {
                *(found as *mut HWND) = window;
                return 0;
            }
        }
        1
    }
    let mut window: HWND = std::ptr::null_mut();
    // SAFETY: the callback only writes through the pointer to `window`,
    // which lives until EnumWindows returns.
    unsafe {
        EnumWindows(Some(find), &mut window as *mut HWND as LPARAM);
    }
    window
}
