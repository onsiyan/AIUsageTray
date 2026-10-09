//! The tray icon's right-click menu: open the popup or the Cost page, refresh,
//! the two settings that matter while the popup is closed, the log folder,
//! and quit; and a newer release, once one is found.

use std::cell::RefCell;
use std::sync::OnceLock;

use async_channel::{Receiver, Sender};
use iced::futures::{SinkExt, Stream};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};

use crate::Message;
use crate::locale::Language;

/// What a menu item asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayAction {
    Open,
    Cost,
    Refresh,
    StartWithWindows,
    MemorySaver,
    /// Shown once a newer release is found.
    Update,
    OpenLogs,
    Quit,
}

impl TrayAction {
    const ALL: [Self; 8] = [
        Self::Open,
        Self::Cost,
        Self::Refresh,
        Self::StartWithWindows,
        Self::MemorySaver,
        Self::Update,
        Self::OpenLogs,
        Self::Quit,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Cost => "cost",
            Self::Refresh => "refresh",
            Self::StartWithWindows => "start-with-windows",
            Self::MemorySaver => "memory-saver",
            Self::Update => "update",
            Self::OpenLogs => "open-logs",
            Self::Quit => "quit",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    fn label(self, language: Language) -> &'static str {
        let (english, arabic) = match self {
            Self::Open => ("Open AI Usage Tray", "فتح AI Usage Tray"),
            Self::Cost => ("API value of your use", "قيمة استخدامك بأسعار API"),
            Self::Refresh => ("Refresh all", "تحديث الكل"),
            Self::StartWithWindows => ("Start with Windows", "التشغيل مع بدء Windows"),
            Self::MemorySaver => ("Memory saver", "موفّر الذاكرة"),
            Self::Update => ("Download version", "تنزيل الإصدار"),
            Self::OpenLogs => ("Open log folder", "فتح مجلد السجل"),
            Self::Quit => ("Quit", "إنهاء"),
        };
        match language {
            Language::English => english,
            Language::Arabic => arabic,
        }
    }
}

/// The items whose check marks follow settings changed elsewhere.
struct CheckItems {
    start_with_windows: CheckMenuItem,
    memory_saver: CheckMenuItem,
}

thread_local! {
    static CHECK_ITEMS: RefCell<Option<CheckItems>> = const { RefCell::new(None) };
    /// The menu, so the update item can join it later.
    static MENU: RefCell<Option<(Menu, Option<MenuItem>)>> = const { RefCell::new(None) };
}

static ACTION_RECEIVER: OnceLock<Receiver<TrayAction>> = OnceLock::new();

/// Builds the menu and starts passing its clicks to the app.
pub(crate) fn build(language: Language, memory_saver: bool) -> Result<Menu, String> {
    allow_dark_menus();
    let item =
        |action: TrayAction| MenuItem::with_id(action.id(), action.label(language), true, None);
    let check = |action: TrayAction, checked: bool| {
        CheckMenuItem::with_id(action.id(), action.label(language), true, checked, None)
    };
    let title = MenuItem::new(
        concat!("AI Usage Tray ", env!("CARGO_PKG_VERSION")),
        false,
        None,
    );
    let start_with_windows = check(TrayAction::StartWithWindows, crate::autostart::is_enabled());
    let memory_saver = check(TrayAction::MemorySaver, memory_saver);
    let menu = Menu::with_items(&[
        &title,
        &PredefinedMenuItem::separator(),
        &item(TrayAction::Open),
        &item(TrayAction::Cost),
        &item(TrayAction::Refresh),
        &PredefinedMenuItem::separator(),
        &start_with_windows,
        &memory_saver,
        &PredefinedMenuItem::separator(),
        &item(TrayAction::OpenLogs),
        &item(TrayAction::Quit),
    ])
    .map_err(|error| error.to_string())?;
    CHECK_ITEMS.with(|slot| {
        *slot.borrow_mut() = Some(CheckItems {
            start_with_windows,
            memory_saver,
        });
    });
    MENU.with(|slot| *slot.borrow_mut() = Some((menu.clone(), None)));

    let (sender, receiver): (Sender<TrayAction>, _) = async_channel::bounded(8);
    let _ = ACTION_RECEIVER.set(receiver);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(action) = TrayAction::from_id(event.id.as_ref()) {
            let _ = sender.try_send(action);
        }
    }));
    Ok(menu)
}

/// The menu's clicks, as messages.
pub(crate) fn action_stream() -> impl Stream<Item = Message> {
    let receiver = ACTION_RECEIVER.get().cloned();
    iced::stream::channel(8, async move |mut output| {
        let Some(receiver) = receiver else {
            return;
        };
        while let Ok(action) = receiver.recv().await {
            if output.send(Message::TrayMenu(action)).await.is_err() {
                break;
            }
        }
    })
}

/// Adds the item that opens a newer release's page, under the title, or
/// names the newer version on it.
pub(crate) fn show_update(version: &str, language: Language) {
    let label = format!("{} {version}", TrayAction::Update.label(language));
    MENU.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some((menu, update)) = slot.as_mut() else {
            return;
        };
        if let Some(item) = update {
            item.set_text(label);
            return;
        }
        let item = MenuItem::with_id(TrayAction::Update.id(), label, true, None);
        if menu.insert(&item, 1).is_ok() {
            *update = Some(item);
        }
    });
}

pub(crate) fn set_start_with_windows_checked(checked: bool) {
    CHECK_ITEMS.with(|slot| {
        if let Some(items) = slot.borrow().as_ref() {
            items.start_with_windows.set_checked(checked);
        }
    });
}

pub(crate) fn set_memory_saver_checked(checked: bool) {
    CHECK_ITEMS.with(|slot| {
        if let Some(items) = slot.borrow().as_ref() {
            items.memory_saver.set_checked(checked);
        }
    });
}

/// Lets Windows draw the menu dark when apps use dark mode. Windows has no
/// public switch for this; `uxtheme.dll` exports it by ordinal on Windows 10
/// 1903 and later, and older systems simply keep the light menu.
fn allow_dark_menus() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        const SET_PREFERRED_APP_MODE: usize = 135;
        const FLUSH_MENU_THEMES: usize = 136;
        // PreferredAppMode::AllowDark: follow the system's app mode.
        const ALLOW_DARK: i32 = 1;

        let library: Vec<u16> = "uxtheme.dll\0".encode_utf16().collect();
        // SAFETY: the name is NUL-terminated; uxtheme.dll is a system library
        // that stays loaded for the life of the process.
        let module = unsafe { LoadLibraryW(library.as_ptr()) };
        if module.is_null() {
            return;
        }
        // SAFETY: an ordinal in the low word stands in for the name, as
        // GetProcAddress allows; the exports' signatures are
        // `PreferredAppMode SetPreferredAppMode(PreferredAppMode)` and
        // `void FlushMenuThemes(void)`.
        unsafe {
            if let Some(set_mode) = GetProcAddress(module, SET_PREFERRED_APP_MODE as *const u8) {
                let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
                set_mode(ALLOW_DARK);
            }
            if let Some(flush) = GetProcAddress(module, FLUSH_MENU_THEMES as *const u8) {
                let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
                flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_reads_back_from_its_id_and_has_both_labels() {
        for action in TrayAction::ALL {
            assert_eq!(TrayAction::from_id(action.id()), Some(action));
            assert!(!action.label(Language::English).is_empty());
            assert!(!action.label(Language::Arabic).is_empty());
        }
        assert_eq!(TrayAction::from_id("other"), None);
    }
}
