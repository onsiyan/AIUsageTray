//! Hands the app's memory back to Windows while the popup is hidden. Most of
//! it belongs to the GPU driver (about 200 MB with an NVIDIA card) and is
//! only touched while drawing, so it can wait in the page cache and come
//! back the next time the popup opens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Whether the popup is hidden; a trim waits for it.
static HIDDEN: AtomicBool = AtomicBool::new(true);
/// Lets the last frame finish and a quick reopen cancel the trim.
const DELAY: Duration = Duration::from_secs(3);
/// Long enough for the hidden popup to be made ready at start.
pub(super) const AFTER_START: Duration = Duration::from_secs(15);

pub(super) fn set_hidden(hidden: bool) {
    HIDDEN.store(hidden, Ordering::Relaxed);
}

/// Trims the working set shortly, if the popup is still hidden then.
pub(super) fn trim_soon() {
    trim_after(DELAY);
}

pub(super) fn trim_after(delay: Duration) {
    if cfg!(test) {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        if HIDDEN.load(Ordering::Relaxed) {
            trim();
        }
    });
}

#[cfg(windows)]
fn trim() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // SAFETY: the pseudo handle of this process; `usize::MAX` for both
    // sizes asks Windows to remove as many pages as it can.
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
fn trim() {}
