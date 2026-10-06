#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::Path,
    process::Stdio,
    sync::OnceLock,
    thread, thread_local,
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Event, Fill, Length, Point, Shadow,
    Size, Subscription, Task, Theme, event,
    futures::{SinkExt, Stream},
    keyboard,
    widget::{
        Space, button, column, container, image, mouse_area, row, scrollable, stack, text,
        text_input,
    },
    window,
};
use lucide_icons::{
    LUCIDE_FONT_BYTES,
    iced::{icon_check, icon_palette, icon_star, icon_trash_2, icon_user_round_plus, icon_x},
};
use tokio::{io::AsyncWriteExt, process::Command as TokioCommand};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

mod account_add;
mod chrome;
mod codex_switch;
mod dashboard;
mod dialogs;
mod graphics;
mod hint;
mod locale;
mod memory_saver;
mod percent_display;
mod smooth_scroll;
mod spinner;
mod theme;
mod theme_menu;
mod tray;
mod typography;
mod update;
mod usage_refresh;
mod view;

use account_add::*;
use chrome::*;
use dialogs::*;
use graphics::*;
use theme_menu::*;
use tray::*;

use percent_display::PercentDisplay;
use theme::{THEME_MANIFEST, ThemeDefinition, ThemeId, load_saved_theme, save_theme};

const WINDOW_WIDTH: f32 = 424.0;
const WINDOW_HEIGHT: f32 = 690.0;
// Window chrome is an application invariant, not a per-theme preference.
const WINDOW_FRAME_RADIUS: f32 = 16.0;
const WINDOW_FRAME_BORDER_WIDTH: f32 = 1.0;
const TITLE_BAR_SHADE_OPACITY: f32 = 0.34;
const PROVIDER_TAB_SHADE_OPACITY: f32 = 0.28;
const BACKDROP_PIXEL_SCALE: u32 = 3;
const GAP: f32 = 8.0;

thread_local! {
    static TRAY_ICON: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

static TRAY_EVENT_RECEIVER: OnceLock<Receiver<TrayIconEvent>> = OnceLock::new();
static REFRESH_ICON_HANDLES: OnceLock<Vec<(ThemeId, image::Handle)>> = OnceLock::new();

const USAGE_ANIMATION_TICK: Duration = Duration::from_millis(50);
/// How often an open popup updates countdowns and checks for passed resets.
const RESET_CLOCK_TICK: Duration = Duration::from_secs(30);

fn main() -> iced::Result {
    if another_instance_is_running() {
        return Ok(());
    }
    let (tray_sender, tray_receiver) = async_channel::bounded::<TrayIconEvent>(32);
    let _ = TRAY_EVENT_RECEIVER.set(tray_receiver.clone());
    // The tray icon lives on this thread for the whole run, independent of
    // the popup window, which the memory saver closes while it is hidden.
    if let Err(error) = install_tray(tray_sender) {
        preview_log(format!("tray failed: {error}"));
    }
    iced::daemon(
        move || {
            let mut boot = vec![Task::perform(
                dashboard::load_saved_accounts(),
                Message::DashboardLoaded,
            )];
            let mut app = App::new();
            if !app.memory_saver {
                // Ready ahead of the first tray click so it opens instantly.
                boot.push(app.open_popup_window().discard());
            }
            if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_some() {
                boot.push(Task::done(Message::OpenPreview));
            }
            (app, Task::batch(boot))
        },
        App::update,
        App::popup_view,
    )
    .title("Usage Monitor")
    .theme(|_: &App, _: window::Id| Theme::Dark)
    .scale_factor(|app: &App, _: window::Id| app.ui_zoom)
    .style(|_, theme| {
        // Let the rounded frame reveal the desktop outside its opaque bounds.
        let mut style = iced::theme::default(theme);
        style.background_color = Color::TRANSPARENT;
        style
    })
    .default_font(typography::BODY)
    .font(LUCIDE_FONT_BYTES)
    .font(include_bytes!(
        "../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"
    ))
    .font(include_bytes!(
        "../assets/fonts/ibm-plex-sans/IBMPlexSans-Medium.ttf"
    ))
    .font(include_bytes!(
        "../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"
    ))
    .font(include_bytes!(
        "../assets/fonts/ibm-plex-sans/IBMPlexSans-Bold.ttf"
    ))
    .subscription(App::subscription)
    .run()
}

/// The popup opens hidden and is shown once it is placed next to the tray.
fn popup_window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(WINDOW_WIDTH, WINDOW_HEIGHT),
        visible: false,
        min_size: Some(Size::new(WINDOW_WIDTH, WINDOW_HEIGHT)),
        resizable: false,
        closeable: true,
        minimizable: true,
        decorations: false,
        transparent: true,
        level: window::Level::Normal,
        exit_on_close_request: false,
        platform_specific: window::settings::PlatformSpecific {
            skip_taskbar: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageProvider {
    Codex,
    Claude,
    Antigravity,
    OpenCodeGo,
    OpenRouter,
}

impl UsageProvider {
    const fn cli_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::OpenCodeGo => "opencode-go",
            Self::OpenRouter => "openrouter",
        }
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::OpenCodeGo => "OpenCode Go",
            Self::OpenRouter => "OpenRouter",
        }
    }
}

/// A tab of the popup: one provider's accounts, or the accounts the user
/// starred from any provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardTab {
    Provider(UsageProvider),
    Favorites,
}

impl DashboardTab {
    /// Keeps each tab's scroll position apart.
    const fn scroll_key(self) -> &'static str {
        match self {
            Self::Provider(provider) => provider.cli_name(),
            Self::Favorites => "favorites",
        }
    }
}

/// Where the Favorites tab sits among the provider tabs.
const FAVORITES_TAB_POSITION: usize = 2;

#[derive(Clone, Copy)]
struct ProviderTab {
    provider: UsageProvider,
    label: &'static str,
}

#[derive(Clone)]
enum AccountAddStatus {
    Running(UsageProvider),
    Added(UsageProvider),
    Failed(String),
}

#[derive(Clone)]
struct PendingAccountDeletion {
    account_id: usage_monitor_core::accounts::AccountId,
    provider: UsageProvider,
    display_name: String,
    email: String,
}

const PROVIDER_TABS: &[ProviderTab] = &[
    ProviderTab {
        provider: UsageProvider::Codex,
        label: "Codex",
    },
    ProviderTab {
        provider: UsageProvider::Claude,
        label: "Claude",
    },
    ProviderTab {
        provider: UsageProvider::Antigravity,
        label: "Antigravity",
    },
    ProviderTab {
        provider: UsageProvider::OpenCodeGo,
        label: "OpenCode Go",
    },
    ProviderTab {
        provider: UsageProvider::OpenRouter,
        label: "OpenRouter",
    },
];

static PROVIDER_LOGOS: OnceLock<[image::Handle; 5]> = OnceLock::new();
static LIGHT_THEME_PROVIDER_LOGOS: OnceLock<[image::Handle; 2]> = OnceLock::new();

struct App {
    window_id: Option<window::Id>,
    theme_id: ThemeId,
    backdrop_image: Option<image::Handle>,
    theme_menu_open: bool,
    account_add_menu_open: bool,
    account_delete_dialog_open: bool,
    pending_account_deletion: Option<PendingAccountDeletion>,
    account_delete_running: bool,
    account_delete_queued: bool,
    account_delete_error: Option<String>,
    openrouter_credentials_open: bool,
    openrouter_api_key: String,
    openrouter_management_key: String,
    account_add_running: bool,
    account_add_cancel: Option<Sender<()>>,
    account_add_status: Option<AccountAddStatus>,
    selected_tab: DashboardTab,
    dashboard_refresh_running: bool,
    popup_visible: bool,
    window_focused: bool,
    last_focus_lost: Option<Instant>,
    dashboard: dashboard::DashboardState,
    language: locale::Language,
    memory_saver: bool,
    /// How much the popup is enlarged on the screen it is shown on.
    ui_zoom: f32,
}

impl App {
    fn new() -> Self {
        let theme_id = load_saved_theme();
        percent_display::set_current(percent_display::load_saved());
        Self {
            window_id: None,
            theme_id,
            backdrop_image: backdrop_image_handle(theme_id),
            theme_menu_open: false,
            account_add_menu_open: false,
            account_delete_dialog_open: false,
            pending_account_deletion: None,
            account_delete_running: false,
            account_delete_queued: false,
            account_delete_error: None,
            openrouter_credentials_open: false,
            openrouter_api_key: String::new(),
            openrouter_management_key: String::new(),
            account_add_running: false,
            account_add_cancel: None,
            account_add_status: None,
            selected_tab: DashboardTab::Provider(UsageProvider::Codex),
            dashboard_refresh_running: false,
            popup_visible: false,
            window_focused: false,
            last_focus_lost: None,
            dashboard: dashboard::DashboardState::loading(),
            language: locale::default_language(),
            memory_saver: memory_saver::load_saved(),
            ui_zoom: 1.0,
        }
    }

    /// The popup is the only window the app opens.
    fn popup_view(&self, _: window::Id) -> Element<'_, Message> {
        self.view()
    }

    fn subscription(app: &Self) -> Subscription<Message> {
        let mut subscriptions = vec![
            event::listen_with(runtime_event).map(Message::RuntimeEvent),
            Subscription::run(tray_event_stream),
        ];

        let blocking_dialog_open =
            app.openrouter_credentials_open || app.account_delete_dialog_open;
        if should_run_popup_animation_ticks(
            app.popup_visible,
            blocking_dialog_open,
            app.dashboard.has_active_usage_animation(),
        ) {
            subscriptions.push(Subscription::run(usage_animation_tick_stream));
        }
        if app.popup_visible {
            subscriptions.push(Subscription::run(reset_clock_stream));
        }

        Subscription::batch(subscriptions)
    }

    fn toggle_popup_from_tray(
        &mut self,
        tray_rect: tray_icon::Rect,
        window_mode: window::Mode,
    ) -> Task<Message> {
        self.popup_visible = window_mode != window::Mode::Hidden;
        if tray_click_should_hide(
            self.popup_visible,
            self.window_focused,
            self.last_focus_lost,
            Instant::now(),
        ) {
            preview_log("hide popup from tray");
            self.hide_popup()
        } else {
            self.show_window(Some(tray_rect))
        }
    }

    fn dismiss_account_delete_dialog(&mut self) {
        if self.account_delete_running {
            return;
        }
        self.account_delete_dialog_open = false;
        self.pending_account_deletion = None;
        self.account_delete_queued = false;
        self.account_delete_error = None;
    }

    fn begin_pending_account_deletion(&mut self) -> Task<Message> {
        let Some(pending) = self.pending_account_deletion.as_ref() else {
            self.account_delete_queued = false;
            return Task::none();
        };
        if self.account_delete_running || self.dashboard_refresh_running {
            return Task::none();
        }

        let account_id = pending.account_id;
        self.account_delete_queued = false;
        self.account_delete_running = true;
        self.account_delete_error = None;
        Task::perform(dashboard::delete_saved_account(account_id), move |result| {
            Message::AccountDeletionCompleted(account_id, result)
        })
    }

    /// Shows the popup next to the tray icon, opening its window first when
    /// it was closed. Without a tray position it sits at the taskbar edge.
    fn show_window(&mut self, tray_rect: Option<tray_icon::Rect>) -> Task<Message> {
        preview_log("show or restore window from tray");
        self.popup_visible = true;
        let (window_id, open_task) = match self.window_id {
            Some(window_id) => (window_id, Task::none()),
            None => {
                let open_task = self.open_popup_window();
                (self.window_id.expect("just opened"), open_task.discard())
            }
        };
        let show_task = window::scale_factor(window_id).then(move |scale_factor| {
            window::monitor_size(window_id).then(move |monitor_size| {
                let monitor_size = monitor_size.unwrap_or(Size::new(1920.0, 1080.0));
                let tray_rect =
                    tray_rect.unwrap_or_else(|| taskbar_edge_anchor(monitor_size, scale_factor));
                let work_area = monitor_work_area(tray_rect)
                    .unwrap_or_else(|| full_monitor_work_area(monitor_size, scale_factor));
                let monitor_scale = monitor_scale_factor(tray_rect).unwrap_or(scale_factor);
                let zoom = preview_zoom()
                    .unwrap_or_else(|| popup_zoom((work_area.bottom - work_area.top) / monitor_scale));
                let position = popup_position(tray_rect, scale_factor, zoom, work_area);
                preview_log(format!(
                    "show popup: scale={scale_factor} zoom={zoom} work_area={work_area:?} position={position:?}"
                ));
                Task::done(Message::SetUiZoom(zoom))
                    .chain(window::resize::<Message>(
                        window_id,
                        Size::new(WINDOW_WIDTH * zoom, WINDOW_HEIGHT * zoom),
                    ))
                    .chain(window::move_to::<Message>(window_id, position))
                    .chain(window::set_mode::<Message>(
                        window_id,
                        window::Mode::Windowed,
                    ))
                    .chain(window::gain_focus::<Message>(window_id))
            })
        });

        Task::batch([
            open_task.chain(show_task),
            self.start_usage_refresh(usage_refresh::RefreshTrigger::Automatic),
        ])
    }

    fn start_usage_refresh(&mut self, trigger: usage_refresh::RefreshTrigger) -> Task<Message> {
        if self.dashboard_refresh_running {
            return Task::none();
        }

        self.dashboard_refresh_running = true;
        let first = match self.selected_tab {
            DashboardTab::Provider(provider) => usage_refresh::RefreshFirst::Provider(provider),
            DashboardTab::Favorites => {
                usage_refresh::RefreshFirst::Accounts(self.dashboard.favorite_accounts().to_vec())
            }
        };
        Task::run(
            usage_refresh::refresh_accounts(first, trigger),
            Message::UsageRefreshEvent,
        )
    }

    fn finish_dashboard_refresh(&mut self) -> Task<Message> {
        self.dashboard_refresh_running = false;
        self.dashboard.refresh_desktop_apps();
        if self.account_delete_queued {
            self.begin_pending_account_deletion()
        } else {
            Task::none()
        }
    }

    fn begin_account_add(
        &mut self,
        provider: UsageProvider,
        credentials: Option<(String, String)>,
    ) -> Task<Message> {
        self.account_add_running = true;
        self.account_add_status = Some(AccountAddStatus::Running(provider));
        let (cancel_sender, cancel_receiver) = async_channel::bounded(1);
        self.account_add_cancel = Some(cancel_sender);
        let completion_receiver = match spawn_account_add_worker(move || {
            add_account(provider, credentials, cancel_receiver)
        }) {
            Ok(receiver) => receiver,
            Err(error) => {
                return Task::done(Message::AccountAddCompleted(provider, Err(error)));
            }
        };

        Task::perform(
            async move {
                completion_receiver.recv().await.unwrap_or_else(|error| {
                    Err(format!(
                        "The account login worker ended unexpectedly: {error}"
                    ))
                })
            },
            move |result| Message::AccountAddCompleted(provider, result),
        )
    }

    fn cancel_openrouter_credentials(&mut self) {
        self.openrouter_credentials_open = false;
        self.openrouter_api_key.clear();
        self.openrouter_management_key.clear();
    }

    /// Opens the popup window hidden; it is shown once placed by the tray.
    fn open_popup_window(&mut self) -> Task<window::Id> {
        let (window_id, open_task) = window::open(popup_window_settings());
        self.window_id = Some(window_id);
        open_task
    }

    /// Hides the popup. With the memory saver on, its window is closed
    /// instead: with no window left the renderer releases the GPU memory.
    fn hide_popup(&mut self) -> Task<Message> {
        self.popup_visible = false;
        self.window_focused = false;
        if self.memory_saver {
            self.window_id
                .take()
                .map(window::close)
                .unwrap_or_else(Task::none)
        } else {
            self.window_id
                .map(|id| window::set_mode(id, window::Mode::Hidden))
                .unwrap_or_else(Task::none)
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    TrayEvent(TrayIconEvent),
    TogglePopupFromTray(tray_icon::Rect, window::Mode),
    OpenPreview,
    RuntimeEvent(Event),
    DragWindow,
    CloseButton,
    RefreshAllUsage,
    UsageAnimationTick,
    ResetClockTick,
    ToggleThemeMenu,
    DismissThemeMenu,
    ToggleAccountAddMenu,
    DismissAccountAddMenu,
    ToggleAccountDeleteDialog,
    DismissAccountDeleteDialog,
    SelectAccountForDeletion(usage_monitor_core::accounts::AccountId),
    CancelAccountDeletion,
    ConfirmAccountDeletion,
    ChooseAccountProvider(UsageProvider),
    OpenRouterApiKeyChanged(String),
    OpenRouterManagementKeyChanged(String),
    SubmitOpenRouterCredentials,
    CancelOpenRouterCredentials,
    AccountAddCompleted(UsageProvider, Result<(), String>),
    CancelAccountAdd,
    DismissAccountAddStatus,
    SelectTheme(ThemeId),
    SelectPercentDisplay(PercentDisplay),
    SetMemorySaver(bool),
    SetUiZoom(f32),
    SwitchCodexDesktopAccount(usage_monitor_core::accounts::AccountId),
    SwitchAntigravityAppAccount(usage_monitor_core::accounts::AccountId),
    CodexDesktopSwitchFinished(usage_monitor_core::accounts::AccountId, Result<(), String>),
    SelectTab(DashboardTab),
    ToggleFavorite(usage_monitor_core::accounts::AccountId),
    DashboardLoaded(Result<Vec<dashboard::AccountUsageEntry>, String>),
    UsageRefreshEvent(usage_refresh::RefreshEvent),
    BeginAliasEdit(usage_monitor_core::accounts::AccountId),
    AccountNameHovered(usage_monitor_core::accounts::AccountId),
    AccountNameHoverEnded(usage_monitor_core::accounts::AccountId),
    AliasDraftChanged(usage_monitor_core::accounts::AccountId, String),
    CancelAliasEdit(usage_monitor_core::accounts::AccountId),
    ToggleModelVisibilityMenu(usage_monitor_core::accounts::AccountId),
    CloseModelVisibilityMenu(usage_monitor_core::accounts::AccountId),
    SetModelVisibility(String, bool),
    SetModelQuotaDisplay(bool),
    SetAntigravityQuotaGroups(bool),
    SetAntigravityClaudeGptHidden(bool),
    MoveAccount(usage_monitor_core::accounts::AccountId, isize),
    SaveAlias(usage_monitor_core::accounts::AccountId),
    AliasSaved(
        usage_monitor_core::accounts::AccountId,
        Result<Vec<dashboard::AccountUsageEntry>, String>,
    ),
    AccountDeletionCompleted(
        usage_monitor_core::accounts::AccountId,
        Result<Vec<dashboard::AccountUsageEntry>, String>,
    ),
}

/// The only global events the app acts on. Forwarding every event (each
/// mouse move included) rebuilt the whole view for nothing.
fn runtime_event(event: Event, status: event::Status, _: window::Id) -> Option<Event> {
    if status == event::Status::Captured {
        return None;
    }
    match &event {
        Event::Window(
            window::Event::CloseRequested | window::Event::Focused | window::Event::Unfocused,
        )
        | Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            ..
        }) => Some(event),
        _ => None,
    }
}

fn preview_log(message: impl std::fmt::Display) {
    if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_none() {
        return;
    }

    let path = std::env::temp_dir().join(format!("usage-monitor-{}.log", std::process::id()));
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests;

/// Keeps a second launch from adding another tray icon. The named mutex is
/// held, and released by Windows, with this process.
#[cfg(windows)]
fn another_instance_is_running() -> bool {
    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    };
    let name = concat!(r"Local\UsageMonitor.Desktop", "\0")
        .encode_utf16()
        .collect::<Vec<u16>>();
    // SAFETY: `name` is NUL-terminated and outlives the call. The handle is
    // deliberately never closed so the mutex lives as long as the process.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    // SAFETY: reads the calling thread's last error right after the call.
    !handle.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS
}

#[cfg(not(windows))]
fn another_instance_is_running() -> bool {
    false
}
