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
        Space, button, container, image, mouse_area, row, scrollable, stack, text, text_input,
    },
    window,
};
use lucide_icons::{
    LUCIDE_FONT_BYTES,
    iced::{
        icon_check, icon_circle_dollar_sign, icon_monitor, icon_palette, icon_server, icon_star,
        icon_trash_2, icon_user_round_plus, icon_x,
    },
};
use tokio::{io::AsyncWriteExt, process::Command as TokioCommand};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

mod account_add;
mod app_log;
mod autostart;
mod chrome;
mod codex_switch;
mod cost_tab;
mod custom_theme;
mod custom_theme_dialog;
mod dashboard;
mod dialogs;
mod display_options;
mod graphics;
mod hint;
mod key_services;
mod keys_tab;
mod locale;
mod memory_saver;
mod percent_display;
mod smooth_scroll;
mod spinner;
mod tab_icons;
mod tab_manager;
mod tabs;
mod theme;
mod theme_menu;
mod tray;
mod tray_menu;
mod typography;
mod update;
mod update_check;
mod usage_refresh;
mod view;
mod welcome;

use account_add::*;
use chrome::*;
use dialogs::*;
use graphics::*;
use tab_manager::*;
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
static REFRESH_ICON_HANDLES: std::sync::Mutex<Vec<([u8; 3], image::Handle)>> =
    std::sync::Mutex::new(Vec::new());

const USAGE_ANIMATION_TICK: Duration = Duration::from_millis(50);
/// How often an open popup updates countdowns and checks for passed resets.
const RESET_CLOCK_TICK: Duration = Duration::from_secs(30);

fn main() -> iced::Result {
    if another_instance_is_running() {
        return Ok(());
    }
    // wgpu prefers Direct3D 12, whose drivers hold about 100 MB more than
    // Vulkan's for the same popup. Without Vulkan iced falls back to drawing
    // on the CPU. `WGPU_BACKEND` set by the user still wins.
    if std::env::var_os("WGPU_BACKEND").is_none() {
        // SAFETY: no other thread has started yet.
        unsafe { std::env::set_var("WGPU_BACKEND", "vulkan") };
    }
    app_log::start();
    let (tray_sender, tray_receiver) = async_channel::bounded::<TrayIconEvent>(32);
    let _ = TRAY_EVENT_RECEIVER.set(tray_receiver.clone());
    // The tray icon lives on this thread for the whole run, independent of
    // the popup window, which the memory saver closes while it is hidden.
    if let Err(error) = install_tray(tray_sender) {
        crate::app_log::write(format!("tray failed: {error}"));
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
            if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_none() {
                app.update_checked_at = Some(Instant::now());
                boot.push(update_check::check());
            }
            if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_some() {
                boot.push(Task::done(Message::OpenPreview));
                // Opens one menu or dialog for screenshots during development.
                let menu = match std::env::var("USAGE_UI_PREVIEW_MENU").as_deref() {
                    Ok("theme") => Some(Message::ToggleThemeMenu),
                    Ok("tabs") => Some(Message::ToggleTabManager),
                    Ok("tab-editor") => {
                        boot.push(Task::done(Message::ToggleTabManager));
                        Some(Message::NewCustomTab)
                    }
                    Ok("tab-edit") => {
                        boot.push(Task::done(Message::ToggleTabManager));
                        Some(Message::EditCustomTab(1))
                    }
                    Ok("deepseek-tab") => Some(Message::SelectTab(DashboardTab::Provider(
                        UsageProvider::DeepSeek,
                    ))),
                    Ok("openrouter-tab") => Some(Message::SelectTab(DashboardTab::Provider(
                        UsageProvider::OpenRouter,
                    ))),
                    Ok("add") => Some(Message::ToggleAccountAddMenu),
                    Ok("cost-tab") => Some(Message::SelectTab(DashboardTab::Cost)),
                    Ok("cost-machine") => {
                        boot.push(Task::done(Message::SelectTab(DashboardTab::Cost)));
                        Some(Message::CostView(cost_tab::CostView::Machine(
                            cost_tab::MachineChange::OpenForm,
                        )))
                    }
                    Ok("cost-vps") => {
                        boot.push(Task::done(Message::SelectTab(DashboardTab::Cost)));
                        Some(Message::CostView(cost_tab::CostView::Scope(
                            cost_tab::Scope::Machine("vps".to_owned()),
                        )))
                    }
                    Ok("cost-detail") => {
                        boot.push(Task::done(Message::SelectTab(DashboardTab::Cost)));
                        Some(Message::CostView(cost_tab::CostView::Period(
                            cost_tab::Period::Week,
                        )))
                    }
                    Ok("cost-all") => {
                        boot.push(Task::done(Message::SelectTab(DashboardTab::Cost)));
                        Some(Message::CostView(cost_tab::CostView::Period(
                            cost_tab::Period::All,
                        )))
                    }
                    Ok("welcome") => Some(Message::WelcomePreview(false)),
                    Ok("custom-theme") => Some(Message::OpenCustomTheme),
                    Ok("welcome-accounts") => Some(Message::WelcomePreview(true)),
                    Ok("deepseek") => Some(Message::ChooseAccountProvider(UsageProvider::DeepSeek)),
                    Ok("copilot") => Some(Message::ChooseAccountProvider(UsageProvider::Copilot)),
                    Ok("cursor") => Some(Message::ChooseAccountProvider(UsageProvider::Cursor)),
                    Ok("kimi") => Some(Message::ChooseAccountProvider(UsageProvider::Kimi)),
                    Ok("zai") => Some(Message::ChooseAccountProvider(UsageProvider::Zai)),
                    Ok("xai") => Some(Message::ChooseAccountProvider(UsageProvider::Xai)),
                    Ok("minimax") => Some(Message::ChooseAccountProvider(UsageProvider::MiniMax)),
                    Ok("openrouter") => {
                        Some(Message::ChooseAccountProvider(UsageProvider::OpenRouter))
                    }
                    _ => None,
                };
                if let Some(menu) = menu {
                    boot.push(Task::done(menu));
                }
            }
            (app, Task::batch(boot))
        },
        App::update,
        App::popup_view,
    )
    .title("AI Usage Tray")
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
        icon: graphics::window_icon(),
        platform_specific: window::settings::PlatformSpecific {
            skip_taskbar: !display_options::show_in_taskbar(),
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
    DeepSeek,
    Copilot,
    Cursor,
    Kimi,
    Zai,
    Xai,
    MiniMax,
    MiMo,
}

impl UsageProvider {
    const fn cli_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::OpenCodeGo => "opencode-go",
            Self::OpenRouter => "openrouter",
            Self::DeepSeek => "deepseek",
            Self::Copilot => "copilot",
            Self::Cursor => "cursor",
            Self::Kimi => "kimi",
            Self::Zai => "zai",
            Self::Xai => "xai",
            Self::MiniMax => "minimax",
            Self::MiMo => "mimo",
        }
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::OpenCodeGo => "OpenCode Go",
            Self::OpenRouter => "OpenRouter",
            Self::DeepSeek => "DeepSeek",
            Self::Copilot => "Copilot",
            Self::Cursor => "Cursor",
            Self::Kimi => "Kimi Code",
            Self::Zai => "z.ai",
            Self::Xai => "xAI",
            Self::MiniMax => "MiniMax",
            Self::MiMo => "Xiaomi MiMo",
        }
    }

    /// Providers added by pasting an API key rather than signing in.
    const fn uses_api_key(self) -> bool {
        matches!(
            self,
            Self::OpenRouter
                | Self::DeepSeek
                | Self::Kimi
                | Self::Zai
                | Self::Xai
                | Self::MiniMax
                | Self::MiMo
        )
    }
}

/// A tab of the popup: one provider's accounts, the accounts the user
/// starred, or a custom tab gathering several providers' accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardTab {
    Provider(UsageProvider),
    Favorites,
    /// What Codex and Claude Code usage on this PC would cost.
    Cost,
    /// The API keys kept to copy later.
    Keys,
    Custom {
        id: u32,
        providers: tabs::ProviderSet,
    },
}

impl DashboardTab {
    /// Keeps each tab's scroll position apart.
    fn scroll_key(self) -> String {
        match self {
            Self::Provider(provider) => provider.cli_name().to_owned(),
            Self::Favorites => "favorites".to_owned(),
            Self::Cost => "cost".to_owned(),
            Self::Keys => "keys".to_owned(),
            Self::Custom { id, .. } => format!("custom-{id}"),
        }
    }

    /// A page opened from the title bar rather than a tab in the tab bar.
    fn is_page(self) -> bool {
        matches!(self, Self::Cost | Self::Keys)
    }

    /// The same tab, even if a custom tab's providers changed since.
    fn same_tab(self, other: Self) -> bool {
        match (self, other) {
            (Self::Custom { id, .. }, Self::Custom { id: other, .. }) => id == other,
            _ => self == other,
        }
    }

    /// Whether the tab lists accounts by provider and includes `provider`.
    fn includes_provider(self, provider: UsageProvider) -> bool {
        match self {
            Self::Provider(own) => own == provider,
            Self::Favorites | Self::Cost | Self::Keys => false,
            Self::Custom { providers, .. } => providers.contains(provider),
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
    ProviderTab {
        provider: UsageProvider::DeepSeek,
        label: "DeepSeek",
    },
    ProviderTab {
        provider: UsageProvider::Copilot,
        label: "Copilot",
    },
    ProviderTab {
        provider: UsageProvider::Cursor,
        label: "Cursor",
    },
    ProviderTab {
        provider: UsageProvider::Kimi,
        label: "Kimi Code",
    },
    ProviderTab {
        provider: UsageProvider::Zai,
        label: "z.ai",
    },
    ProviderTab {
        provider: UsageProvider::Xai,
        label: "xAI",
    },
    ProviderTab {
        provider: UsageProvider::MiniMax,
        label: "MiniMax",
    },
    // Xiaomi MiMo is hidden for now: it needs a pasted console cookie. Its
    // provider code stays; listing it here again shows it everywhere.
];

static PROVIDER_LOGOS: OnceLock<[image::Handle; 13]> = OnceLock::new();
static LIGHT_THEME_PROVIDER_LOGOS: OnceLock<[image::Handle; 8]> = OnceLock::new();

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
    /// The provider whose API-key dialog is open.
    credentials_provider: Option<UsageProvider>,
    api_key_input: String,
    management_key_input: String,
    account_add_running: bool,
    account_add_cancel: Option<Sender<()>>,
    /// The GitHub code shown while a Copilot sign-in waits for the user.
    device_sign_in: Option<DeviceSignIn>,
    account_add_status: Option<AccountAddStatus>,
    selected_tab: DashboardTab,
    /// The tab to go back to when the Cost page closes.
    tab_before_page: DashboardTab,
    /// The user's arrangement of the tab bar.
    tab_layout: tabs::TabLayout,
    tab_manager_open: bool,
    /// The custom tab being created (`id: None`) or edited in the manager.
    tab_editor: Option<tab_manager::TabEditor>,
    /// The images the user picked for custom tabs, by tab id.
    tab_icons: std::collections::HashMap<u32, image::Handle>,
    /// The image file dialog is open.
    tab_icon_picking: bool,
    dashboard_refresh_running: bool,
    popup_visible: bool,
    window_focused: bool,
    last_focus_lost: Option<Instant>,
    dashboard: dashboard::DashboardState,
    cost: cost_tab::CostTab,
    keys: keys_tab::KeysTab,
    language: locale::Language,
    memory_saver: bool,
    /// How much the popup is enlarged on the screen it is shown on.
    ui_zoom: f32,
    /// The first-run welcome, while it is shown.
    welcome: Option<welcome::Welcome>,
    /// Whether the first account load decided about the welcome yet.
    welcome_checked: bool,
    custom_theme_open: bool,
    custom_background_input: String,
    custom_accent_input: String,
    /// The image file dialog for the custom theme is open.
    custom_image_picking: bool,
    custom_image_error: Option<String>,
    /// A newer release, once one is found.
    update: Option<update_check::Release>,
    update_checked_at: Option<Instant>,
    /// The popup's note about the update was closed.
    update_note_closed: bool,
}

impl App {
    fn new() -> Self {
        let theme_id = load_saved_theme();
        percent_display::set_default(percent_display::load_saved());
        display_options::load_saved();
        let tab_layout = tabs::load_saved();
        let mut dashboard = dashboard::DashboardState::loading();
        dashboard.set_custom_tab_accounts(tab_layout.custom_accounts());
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
            credentials_provider: None,
            api_key_input: String::new(),
            management_key_input: String::new(),
            account_add_running: false,
            account_add_cancel: None,
            device_sign_in: None,
            account_add_status: None,
            selected_tab: tab_layout.resolve(DashboardTab::Provider(UsageProvider::Codex)),
            tab_before_page: tab_layout.resolve(DashboardTab::Provider(UsageProvider::Codex)),
            tab_icons: tab_icons::load_saved(&tab_layout),
            tab_icon_picking: false,
            tab_layout,
            tab_manager_open: false,
            tab_editor: None,
            dashboard_refresh_running: false,
            popup_visible: false,
            window_focused: false,
            last_focus_lost: None,
            dashboard,
            cost: cost_tab::CostTab::load(),
            keys: keys_tab::KeysTab::default(),
            language: locale::default_language(),
            memory_saver: memory_saver::load_saved(),
            ui_zoom: 1.0,
            welcome: None,
            welcome_checked: false,
            custom_theme_open: false,
            custom_background_input: String::new(),
            custom_accent_input: String::new(),
            custom_image_picking: false,
            custom_image_error: None,
            update: None,
            update_checked_at: None,
            update_note_closed: false,
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
            Subscription::run(tray_menu::action_stream),
        ];

        let blocking_dialog_open = app.credentials_provider.is_some()
            || app.device_sign_in.is_some()
            || app.account_delete_dialog_open
            || app.tab_manager_open
            || app.custom_theme_open;
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
        // Screenshot runs end the app abruptly; a refresh cut off after a
        // provider rotated a sign-in token would lose the new token.
        if self.dashboard_refresh_running
            || std::env::var_os("USAGE_UI_PREVIEW_NO_REFRESH").is_some()
        {
            return Task::none();
        }

        self.dashboard_refresh_running = true;
        let first = match self.selected_tab {
            DashboardTab::Provider(provider) => usage_refresh::RefreshFirst::Provider(provider),
            tab => usage_refresh::RefreshFirst::Accounts(self.dashboard.tab_account_ids(tab)),
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
        // Copilot signs in on GitHub with a device code, and Codex may fall
        // back to one; the code shows in its own dialog while the worker
        // waits for it to be entered.
        let (code_sender, code_receiver) = async_channel::bounded(2);
        let may_show_code = credentials.is_none()
            && matches!(provider, UsageProvider::Copilot | UsageProvider::Codex);
        let completion_receiver = match spawn_account_add_worker(move || async move {
            match provider {
                UsageProvider::Copilot if credentials.is_none() => {
                    add_copilot_account(code_sender, cancel_receiver).await
                }
                UsageProvider::Codex if credentials.is_none() => {
                    add_codex_account(code_sender, cancel_receiver).await
                }
                _ => add_account(provider, credentials, cancel_receiver).await,
            }
        }) {
            Ok(receiver) => receiver,
            Err(error) => {
                return Task::done(Message::AccountAddCompleted(provider, Err(error)));
            }
        };

        let completion = Task::perform(
            async move {
                completion_receiver.recv().await.unwrap_or_else(|error| {
                    Err(format!(
                        "The account login worker ended unexpectedly: {error}"
                    ))
                })
            },
            move |result| Message::AccountAddCompleted(provider, result),
        );
        if may_show_code {
            Task::batch([
                completion,
                Task::run(code_receiver, Message::DeviceSignInCode),
            ])
        } else {
            completion
        }
    }

    fn cancel_credentials(&mut self) {
        self.credentials_provider = None;
        self.api_key_input.clear();
        self.management_key_input.clear();
    }

    /// Opens the popup window hidden; it is shown once placed by the tray.
    fn open_popup_window(&mut self) -> Task<window::Id> {
        let (window_id, open_task) = window::open(popup_window_settings());
        self.window_id = Some(window_id);
        open_task
    }

    fn tray_menu_action(&mut self, action: tray_menu::TrayAction) -> Task<Message> {
        use tray_menu::TrayAction;
        preview_log(format!("tray menu: {action:?}"));
        let tray_rect = || TRAY_ICON.with(|tray| tray.borrow().as_ref().and_then(TrayIcon::rect));
        match action {
            TrayAction::Open => self.show_window(tray_rect()),
            TrayAction::Cost => {
                let show = self.show_window(tray_rect());
                if self.selected_tab == DashboardTab::Cost {
                    show
                } else {
                    show.chain(Task::done(Message::ToggleCostPage))
                }
            }
            TrayAction::Refresh => self.update(Message::RefreshAllUsage),
            TrayAction::StartWithWindows => {
                let wanted = !autostart::is_enabled();
                if let Err(error) = autostart::set(wanted) {
                    crate::app_log::write(format!("start with Windows change failed: {error}"));
                }
                tray_menu::set_start_with_windows_checked(autostart::is_enabled());
                Task::none()
            }
            TrayAction::MemorySaver => self.update(Message::SetMemorySaver(!self.memory_saver)),
            TrayAction::Update => self.update(Message::OpenUpdate),
            TrayAction::OpenLogs => {
                app_log::open_folder();
                Task::none()
            }
            TrayAction::Quit => {
                // Removed first, so no stale icon is left in the tray.
                TRAY_ICON.with(|tray| tray.borrow_mut().take());
                iced::exit()
            }
        }
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
    /// An item of the tray icon's right-click menu.
    TrayMenu(tray_menu::TrayAction),
    TogglePopupFromTray(tray_icon::Rect, window::Mode),
    OpenPreview,
    RuntimeEvent(Event),
    DragWindow,
    CloseButton,
    RefreshAllUsage,
    UsageAnimationTick,
    ResetClockTick,
    /// What the update check found: a newer release, or nothing.
    UpdateChecked(Option<update_check::Release>),
    /// Opens the newer release's page.
    OpenUpdate,
    CloseUpdateNote,
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
    ApiKeyChanged(String),
    ManagementKeyChanged(String),
    SubmitCredentials,
    /// A Copilot sign-in's GitHub code, or `None` once it was entered.
    DeviceSignInCode(Option<DeviceSignIn>),
    OpenDeviceCodePage,
    CopyDeviceCode,
    OpenCursorSite,
    OpenMiMoSite,
    CancelCredentials,
    AccountAddCompleted(UsageProvider, Result<(), String>),
    CancelAccountAdd,
    DismissAccountAddStatus,
    SelectTheme(ThemeId),
    SelectPercentDisplay(PercentDisplay),
    SetMemorySaver(bool),
    SetShowAccountDetails(bool),
    SetShowTeamBudgets(bool),
    SetShadeResetTimes(bool),
    SetShowInTaskbar(bool),
    OpenCustomTheme,
    CloseCustomTheme,
    CustomThemeLight(bool),
    CustomThemeBackground([u8; 3]),
    CustomThemeAccent([u8; 3]),
    CustomThemeBackgroundInput(String),
    CustomThemeAccentInput(String),
    CustomThemeDim(custom_theme::Dim),
    ChooseCustomImage,
    CustomImageChosen(Result<bool, String>),
    RemoveCustomImage,
    WelcomeToggleProvider(UsageProvider),
    WelcomeOpenStep(usize),
    WelcomeBack,
    WelcomeFinish,
    /// Opens the welcome for screenshots; `true` starts at an accounts step.
    WelcomePreview(bool),
    SelectResetCredits(display_options::ResetCreditVisibility),
    SetUiZoom(f32),
    SwitchCodexDesktopAccount(usage_monitor_core::accounts::AccountId),
    SwitchAntigravityAppAccount(usage_monitor_core::accounts::AccountId),
    CodexDesktopSwitchFinished(usage_monitor_core::accounts::AccountId, Result<(), String>),
    SelectTab(DashboardTab),
    /// Opens the Cost page, or goes back to the tab it was opened from.
    ToggleCostPage,
    CostScanned(Box<Result<usage_monitor_core::cost::CostReport, String>>),
    /// Other machines were read over SSH.
    CostMachinesSynced(cost_tab::SyncResults),
    CostView(cost_tab::CostView),
    ToggleKeysPage,
    Keys(keys_tab::KeysChange),
    ToggleTabManager,
    DismissTabManager,
    ToggleTabVisible(usize),
    /// Sets how the tab with this key reads its percentages.
    SetTabPercentDisplay(String, PercentDisplay),
    MoveTab(usize, isize),
    NewCustomTab,
    EditCustomTab(u32),
    DeleteCustomTab(u32),
    TabEditorNameChanged(String),
    TabEditorToggleProvider(UsageProvider),
    TabEditorToggleAccount(UsageProvider, usage_monitor_core::accounts::AccountId),
    SaveTabEditor,
    CancelTabEditor,
    ChooseTabIcon,
    TabIconChosen(Result<Option<tab_icons::TabIcon>, String>),
    RemoveTabIcon,
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

    let path = std::env::temp_dir().join(format!("ai-usage-tray-{}.log", std::process::id()));
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
