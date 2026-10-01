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
    iced::{icon_check, icon_palette, icon_trash_2, icon_user_round_plus, icon_x},
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
mod percent_display;
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

const REFRESH_ICON_TICK: Duration = Duration::from_millis(50);

fn main() -> iced::Result {
    let (tray_sender, tray_receiver) = async_channel::bounded::<TrayIconEvent>(32);
    let _ = TRAY_EVENT_RECEIVER.set(tray_receiver.clone());
    let boot_sender = tray_sender.clone();
    iced::application(
        move || {
            (
                App::new(boot_sender.clone()),
                Task::batch([
                    Task::done(Message::InitializeTray),
                    Task::perform(dashboard::load_saved_accounts(), Message::DashboardLoaded),
                ]),
            )
        },
        App::update,
        App::view,
    )
    .title("Usage Monitor")
    .theme(|_: &App| Theme::Dark)
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
        "../assets/fonts/ibm-plex-sans/IBMPlexSans-Bold.ttf"
    ))
    .window(window::Settings {
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
    })
    .subscription(App::subscription)
    .run()
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
    tray_sender: Sender<TrayIconEvent>,
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
    selected_provider: UsageProvider,
    dashboard_refresh_running: bool,
    popup_visible: bool,
    window_focused: bool,
    last_focus_lost: Option<Instant>,
    refresh_icon_rotation_radians: f32,
    dashboard: dashboard::DashboardState,
    language: locale::Language,
}

impl App {
    fn new(tray_sender: Sender<TrayIconEvent>) -> Self {
        let theme_id = load_saved_theme();
        percent_display::set_current(percent_display::load_saved());
        Self {
            tray_sender,
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
            selected_provider: UsageProvider::Codex,
            dashboard_refresh_running: false,
            popup_visible: false,
            window_focused: false,
            last_focus_lost: None,
            refresh_icon_rotation_radians: 0.0,
            dashboard: dashboard::DashboardState::loading(),
            language: locale::default_language(),
        }
    }

    fn subscription(app: &Self) -> Subscription<Message> {
        let mut subscriptions = vec![
            event::listen().map(Message::RuntimeEvent),
            Subscription::run(tray_event_stream),
        ];

        let blocking_dialog_open =
            app.openrouter_credentials_open || app.account_delete_dialog_open;
        if should_run_popup_animation_ticks(
            app.popup_visible,
            blocking_dialog_open,
            app.dashboard_refresh_running,
            app.dashboard.has_active_usage_animation(),
        ) {
            subscriptions.push(Subscription::run(refresh_icon_tick_stream));
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
            self.show_window(tray_rect)
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

    fn show_window(&mut self, tray_rect: tray_icon::Rect) -> Task<Message> {
        preview_log("show or restore window from tray");
        self.popup_visible = true;
        let show_task = if let Some(window_id) = self.window_id {
            window::scale_factor(window_id).then(move |scale_factor| {
                window::monitor_size(window_id).then(move |monitor_size| {
                    let monitor_size = monitor_size.unwrap_or(Size::new(1920.0, 1080.0));
                    let work_area = monitor_work_area(tray_rect)
                        .unwrap_or_else(|| full_monitor_work_area(monitor_size, scale_factor));
                    let position = popup_position(tray_rect, scale_factor, work_area);
                    preview_log(format!(
                        "show popup: scale={scale_factor} work_area={work_area:?} position={position:?}"
                    ));
                    window::move_to::<Message>(window_id, position)
                        .chain(window::set_mode::<Message>(
                            window_id,
                            window::Mode::Windowed,
                        ))
                        .chain(window::gain_focus::<Message>(window_id))
                })
            })
        } else {
            Task::none()
        };

        Task::batch([show_task, self.start_usage_refresh()])
    }

    fn start_usage_refresh(&mut self) -> Task<Message> {
        if self.dashboard_refresh_running {
            return Task::none();
        }

        self.dashboard_refresh_running = true;
        self.refresh_icon_rotation_radians = 0.0;
        let provider = self.selected_provider;
        Task::run(
            usage_refresh::refresh_accounts_for_provider(provider, true),
            move |event| Message::PriorityUsageRefreshEvent(provider, event),
        )
    }

    fn start_remaining_provider_refresh(&self, provider: UsageProvider) -> Task<Message> {
        Task::run(
            usage_refresh::refresh_accounts_for_provider(provider, false),
            Message::OtherProvidersUsageRefreshEvent,
        )
    }

    fn finish_dashboard_refresh(&mut self) -> Task<Message> {
        self.dashboard_refresh_running = false;
        self.dashboard.refresh_desktop_apps();
        self.refresh_icon_rotation_radians = 0.0;
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

    fn hide_popup(&mut self) -> Task<Message> {
        self.popup_visible = false;
        self.window_id
            .map(|id| window::set_mode(id, window::Mode::Hidden))
            .unwrap_or_else(Task::none)
    }

    fn preview_at_taskbar_edge(&self) -> Task<Message> {
        let Some(window_id) = self.window_id else {
            return Task::none();
        };

        window::scale_factor(window_id).then(move |scale_factor| {
            window::monitor_size(window_id).map(move |monitor_size| {
                let monitor_size = monitor_size.unwrap_or(Size::new(1920.0, 1080.0));
                let anchor = tray_icon::Rect {
                    position: tray_icon::menu::dpi::PhysicalPosition::new(
                        (monitor_size.width * scale_factor - 12.0) as f64,
                        (monitor_size.height * scale_factor) as f64,
                    ),
                    size: tray_icon::menu::dpi::PhysicalSize::new(24, 0),
                };
                Message::PreviewRect(Some(anchor))
            })
        })
    }
}

#[derive(Debug, Clone)]
enum Message {
    InitializeTray,
    WindowReady(window::Id),
    TrayReady,
    TrayFailed(String),
    TrayEvent(TrayIconEvent),
    TogglePopupFromTray(tray_icon::Rect, window::Mode),
    OpenPreview,
    PreviewRect(Option<tray_icon::Rect>),
    RuntimeEvent(Event),
    DragWindow,
    CloseButton,
    RefreshAllUsage,
    RefreshIconTick,
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
    SwitchCodexDesktopAccount(usage_monitor_core::accounts::AccountId),
    SwitchAntigravityAppAccount(usage_monitor_core::accounts::AccountId),
    CodexDesktopSwitchFinished(usage_monitor_core::accounts::AccountId, Result<(), String>),
    SelectProvider(UsageProvider),
    DashboardLoaded(Result<Vec<dashboard::AccountUsageEntry>, String>),
    PriorityUsageRefreshEvent(UsageProvider, usage_refresh::RefreshEvent),
    OtherProvidersUsageRefreshEvent(usage_refresh::RefreshEvent),
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
