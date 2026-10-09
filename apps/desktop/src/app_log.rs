//! A small log of what went wrong, for bug reports:
//! `%LOCALAPPDATA%\UsageMonitor\logs\ai-usage-tray.log`. It never holds keys,
//! tokens, or email addresses; accounts are named by their reference (`ch1`).
//! Past [`MAX_BYTES`] it moves to `ai-usage-tray.log.old`, so at most two
//! files are kept.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const FILE_NAME: &str = "ai-usage-tray.log";
const MAX_BYTES: u64 = 1024 * 1024;

static WRITING: Mutex<()> = Mutex::new(());

/// The folder the log is in.
pub(super) fn directory() -> Option<PathBuf> {
    let database = usage_monitor_core::storage::default_accounts_database_path();
    Some(database.parent()?.join("logs"))
}

/// Adds a line, stamped with the time (UTC).
pub(super) fn write(message: impl std::fmt::Display) {
    // Tests act out failures; they stay out of the user's log.
    if cfg!(test) {
        return;
    }
    let Some(directory) = directory() else {
        return;
    };
    let Ok(_writing) = WRITING.lock() else {
        return;
    };
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join(FILE_NAME);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_BYTES) {
        let _ = fs::rename(&path, directory.join(format!("{FILE_NAME}.old")));
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let time = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S");
        // One line per entry, whatever the message holds.
        let message = message.to_string().replace(['\r', '\n'], " ");
        let _ = writeln!(file, "{time}Z {message}");
    }
}

/// Logs the start, and any panic before the app goes down.
pub(super) fn start() {
    write(format!(
        "AI Usage Tray {} started ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    ));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        write(format!("panic: {panic}"));
        previous(panic);
    }));
}

/// Opens the log folder in File Explorer.
pub(super) fn open_folder() {
    let Some(directory) = directory() else {
        return;
    };
    let _ = fs::create_dir_all(&directory);
    #[cfg(windows)]
    {
        let operation: Vec<u16> = "open\0".encode_utf16().collect();
        let path: Vec<u16> = format!("{}\0", directory.display())
            .encode_utf16()
            .collect();
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
    }
}
