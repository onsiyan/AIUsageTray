//! Opening the app again (a pinned taskbar icon, the Start menu, or a
//! shortcut) while it already runs opens the running app's popup instead of
//! doing nothing.

use iced::futures::Stream;

use crate::Message;

#[cfg(windows)]
const EVENT_NAME: &str = r"Local\UsageMonitor.Desktop.Open";

#[cfg(windows)]
fn wide_name() -> Vec<u16> {
    EVENT_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// The running app's open request, made before this second launch exits.
#[cfg(windows)]
pub(crate) fn ask_running_app_to_open() {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent},
    };
    let name = wide_name();
    // SAFETY: `name` is NUL-terminated and outlives the call; the handle is
    // closed below.
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if !event.is_null() {
        // SAFETY: `event` is a valid handle opened above.
        unsafe {
            SetEvent(event);
            CloseHandle(event);
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn ask_running_app_to_open() {}

/// Creates the event a second launch sets; called once at start, before any
/// second launch can look for it. The handle lives as long as the process.
#[cfg(windows)]
pub(crate) fn listen() {
    use windows_sys::Win32::System::Threading::CreateEventW;
    let name = wide_name();
    // SAFETY: `name` is NUL-terminated and outlives the call. Auto-reset, so
    // each wait takes one request.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if !event.is_null() {
        EVENT.store(event as usize, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
pub(crate) fn listen() {}

#[cfg(windows)]
static EVENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One message for every later launch of the app.
pub(crate) fn open_requests() -> impl Stream<Item = Message> {
    let (sender, receiver) = async_channel::bounded::<()>(1);
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
        let event = EVENT.load(std::sync::atomic::Ordering::Relaxed);
        if event != 0 {
            std::thread::spawn(move || {
                loop {
                    // SAFETY: the event handle stays open for the process.
                    unsafe { WaitForSingleObject(event as _, INFINITE) };
                    if sender.send_blocking(()).is_err() {
                        break;
                    }
                }
            });
        }
    }
    #[cfg(not(windows))]
    drop(sender);
    iced::futures::stream::unfold(receiver, |receiver| async move {
        receiver.recv().await.ok().map(|()| {
            (
                Message::TrayMenu(crate::tray_menu::TrayAction::Open),
                receiver,
            )
        })
    })
}
