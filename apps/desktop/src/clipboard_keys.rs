//! Catches API keys the user copies anywhere and keeps them in the vault.
//!
//! Off until the user turns it on. Only text that is one key of a service
//! known by its key's start is taken; everything else on the clipboard is
//! left alone and never kept. Copies the app made itself, and copies a
//! password manager marks as private, are skipped.

use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use iced::futures::Stream;
use usage_monitor_core::vault;

use crate::Message;
use crate::theme::preference_directory;

const ENABLED_FILE: &str = "catch-copied-keys.txt";
const POLL_EVERY: Duration = Duration::from_millis(700);
const MIN_KEY_LENGTH: usize = 20;
const MAX_KEY_LENGTH: usize = 512;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The clipboard's change number right after the app's own last copy.
static OWN_COPY: AtomicU32 = AtomicU32::new(0);

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn load_saved() {
    let on = preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(ENABLED_FILE)).ok())
        .is_some_and(|value| value.trim() == "on");
    ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) fn set_enabled(on: bool) -> io::Result<()> {
    ENABLED.store(on, Ordering::Relaxed);
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(ENABLED_FILE), if on { "on" } else { "off" })
}

/// Marks the clipboard's current content as the app's own copy.
pub(crate) fn note_own_copy() {
    OWN_COPY.store(sequence_number(), Ordering::Relaxed);
}

/// What a copied text is, when it is one key and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopiedKey {
    /// Its start names its service.
    Of(&'static str),
    /// A plain `sk-` key, which several services use.
    PlainSk,
}

pub(crate) fn copied_key(text: &str) -> Option<CopiedKey> {
    let text = text.trim();
    let length = text.chars().count();
    if !(MIN_KEY_LENGTH..=MAX_KEY_LENGTH).contains(&length)
        || text.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    match vault::detect_service(text) {
        Some(service) => Some(CopiedKey::Of(service)),
        None => text.starts_with("sk-").then_some(CopiedKey::PlainSk),
    }
}

/// Each key-like text copied after the watch starts. What was on the
/// clipboard before is not looked at.
pub(crate) fn copied_keys() -> impl Stream<Item = Message> {
    let (sender, receiver) = async_channel::bounded::<String>(4);
    std::thread::spawn(move || {
        let mut seen = sequence_number();
        loop {
            std::thread::sleep(POLL_EVERY);
            if !enabled() {
                break;
            }
            let current = sequence_number();
            if current == seen {
                continue;
            }
            seen = current;
            if current == OWN_COPY.load(Ordering::Relaxed) {
                continue;
            }
            let Some(text) = read_text() else {
                continue;
            };
            if copied_key(&text).is_some() && sender.send_blocking(text).is_err() {
                break;
            }
        }
    });
    iced::futures::stream::unfold(receiver, |receiver| async move {
        let text = receiver.recv().await.ok()?;
        Some((crate::keys_tab::caught_message(text), receiver))
    })
}

#[cfg(windows)]
fn sequence_number() -> u32 {
    // SAFETY: takes no arguments and only reads a counter.
    unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
fn sequence_number() -> u32 {
    0
}

/// The clipboard's text, unless a password manager asked monitors to skip it.
#[cfg(windows)]
fn read_text() -> Option<String> {
    use windows_sys::Win32::System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
            RegisterClipboardFormatW,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    };
    const CF_UNICODETEXT: u32 = 13;
    let private_name: Vec<u16> = "ExcludeClipboardContentFromMonitorProcessing\0"
        .encode_utf16()
        .collect();
    // SAFETY: the name is NUL-terminated; the clipboard is opened before its
    // data is read and closed on every path; the locked memory is read only
    // within its reported size and unlocked before closing.
    unsafe {
        let private = RegisterClipboardFormatW(private_name.as_ptr());
        if !(0..5).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
            OpenClipboard(std::ptr::null_mut()) != 0
        }) {
            return None;
        }
        let text = (|| {
            if private != 0 && IsClipboardFormatAvailable(private) != 0 {
                return None;
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return None;
            }
            let size = GlobalSize(handle) / std::mem::size_of::<u16>();
            // Nothing that long could be a key.
            if size == 0 || size > MAX_KEY_LENGTH * 4 {
                return None;
            }
            let data = GlobalLock(handle).cast::<u16>();
            if data.is_null() {
                return None;
            }
            let wide = std::slice::from_raw_parts(data, size);
            let end = wide.iter().position(|&c| c == 0).unwrap_or(size);
            let text = String::from_utf16(&wide[..end]).ok();
            GlobalUnlock(handle);
            text
        })();
        CloseClipboard();
        text
    }
}

#[cfg(not(windows))]
fn read_text() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_key_is_taken() {
        let of = |service| Some(CopiedKey::Of(service));
        assert_eq!(
            copied_key(
                "  sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123 
"
            ),
            of("Claude")
        );
        assert_eq!(
            copied_key("sk-or-v1-0123456789abcdef0123456789abcdef"),
            of("OpenRouter")
        );
        assert_eq!(
            copied_key("AIzaSyA0123456789abcdefghijklmnopqrstu"),
            of("Google Gemini")
        );
        assert_eq!(
            copied_key("sk-0123456789abcdef0123456789abcdef"),
            of("DeepSeek")
        );
        assert_eq!(
            copied_key("sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKl"),
            Some(CopiedKey::PlainSk)
        );
        // Too short, or more than one word.
        assert_eq!(copied_key("sk-ant-short"), None);
        assert_eq!(
            copied_key("my key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz"),
            None
        );
        assert_eq!(copied_key("an ordinary sentence someone copied"), None);
        assert_eq!(copied_key("https://example.com/a-long-link-copied"), None);
    }
}
