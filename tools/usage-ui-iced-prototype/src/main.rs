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
        text_input, tooltip,
    },
    window,
};
use lucide_icons::{
    LUCIDE_FONT_BYTES,
    iced::{icon_check, icon_palette, icon_trash_2, icon_user_round_plus, icon_x},
};
use tokio::{io::AsyncWriteExt, process::Command as TokioCommand};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

mod codex_switch;
mod dashboard;
mod locale;
mod percent_display;
mod theme;
mod typography;
mod usage_refresh;

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
    .title("Usage Monitor Preview")
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
    account_id: codex_usage_core::accounts::AccountId,
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

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::InitializeTray => {
                preview_log("initialize tray");
                window::latest().then(|window_id| match window_id {
                    Some(window_id) => {
                        preview_log(format!("window available: {window_id:?}"));
                        Task::done(Message::WindowReady(window_id))
                    }
                    None => {
                        preview_log("window::latest returned None");
                        Task::done(Message::TrayFailed(
                            "The preview window was not created".to_owned(),
                        ))
                    }
                })
            }
            Message::WindowReady(window_id) => {
                preview_log(format!("install tray on window: {window_id:?}"));
                self.window_id = Some(window_id);
                let sender = self.tray_sender.clone();
                window::run(window_id, move |_| {
                    let result = install_tray(sender);
                    preview_log(format!("install tray result: {result:?}"));
                    result.err()
                })
                .map(|error| match error {
                    Some(error) => Message::TrayFailed(error),
                    None => Message::TrayReady,
                })
            }
            Message::TrayReady => {
                preview_log("tray ready");
                if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_some() {
                    Task::perform(
                        async {
                            std::thread::sleep(Duration::from_millis(800));
                        },
                        |_| Message::OpenPreview,
                    )
                } else {
                    Task::none()
                }
            }
            Message::TrayFailed(error) => {
                preview_log(format!("tray failed: {error}"));
                Task::none()
            }
            Message::DashboardLoaded(Ok(accounts)) => {
                self.dashboard.set_accounts(accounts);
                Task::none()
            }
            Message::DashboardLoaded(Err(error)) => {
                preview_log(format!("dashboard data load failed: {error}"));
                self.dashboard.set_error();
                Task::none()
            }
            Message::BeginAliasEdit(account_id) => {
                self.dashboard.begin_alias_edit(account_id);
                Task::none()
            }
            Message::MoveAccount(account_id, offset) => {
                if let Err(error) = self.dashboard.move_account(account_id, offset) {
                    preview_log(format!("account order save failed: {error}"));
                }
                Task::none()
            }
            Message::SetAntigravityClaudeGptHidden(hide) => {
                if let Err(error) = self.dashboard.set_hide_antigravity_claude_gpt(hide) {
                    preview_log(format!("antigravity group preference save failed: {error}"));
                }
                Task::none()
            }
            Message::AccountNameHovered(account_id) => {
                self.dashboard.set_hovered_account_name(account_id);
                Task::none()
            }
            Message::AccountNameHoverEnded(account_id) => {
                self.dashboard.clear_hovered_account_name(account_id);
                Task::none()
            }
            Message::AliasDraftChanged(account_id, draft) => {
                self.dashboard.update_alias_draft(account_id, draft);
                Task::none()
            }
            Message::CancelAliasEdit(account_id) => {
                self.dashboard.cancel_alias_edit(account_id);
                Task::none()
            }
            Message::SaveAlias(account_id) => {
                let Some((draft, original_label)) = self.dashboard.begin_alias_save(account_id)
                else {
                    return Task::none();
                };
                Task::perform(
                    dashboard::save_account_alias(account_id, draft, original_label),
                    move |result| Message::AliasSaved(account_id, result),
                )
            }
            Message::AliasSaved(account_id, Ok(accounts)) => {
                self.dashboard.set_accounts(accounts);
                self.dashboard.finish_alias_save(account_id, false);
                Task::none()
            }
            Message::AliasSaved(account_id, Err(error)) => {
                preview_log(format!("account name save failed: {error}"));
                self.dashboard.finish_alias_save(account_id, true);
                Task::none()
            }
            Message::PriorityUsageRefreshEvent(provider, event) => match event {
                usage_refresh::RefreshEvent::AccountUpdated(entry) => {
                    self.dashboard.update_account_usage(entry);
                    Task::none()
                }
                usage_refresh::RefreshEvent::Finished(refresh) => {
                    preview_log(format!(
                        "priority usage refresh for {provider:?}: attempted={}, updated={}, not_updated={}",
                        refresh.attempted, refresh.updated, refresh.not_updated
                    ));
                    self.start_remaining_provider_refresh(provider)
                }
                usage_refresh::RefreshEvent::Failed(error) => {
                    preview_log(format!(
                        "priority usage refresh for {provider:?} failed: {error}"
                    ));
                    self.start_remaining_provider_refresh(provider)
                }
            },
            Message::OtherProvidersUsageRefreshEvent(event) => match event {
                usage_refresh::RefreshEvent::AccountUpdated(entry) => {
                    self.dashboard.update_account_usage(entry);
                    Task::none()
                }
                usage_refresh::RefreshEvent::Finished(refresh) => {
                    preview_log(format!(
                        "other providers usage refresh: attempted={}, updated={}, not_updated={}",
                        refresh.attempted, refresh.updated, refresh.not_updated
                    ));
                    self.finish_dashboard_refresh()
                }
                usage_refresh::RefreshEvent::Failed(error) => {
                    preview_log(format!("other providers usage refresh failed: {error}"));
                    self.finish_dashboard_refresh()
                }
            },
            Message::TrayEvent(TrayIconEvent::Click {
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => {
                preview_log(format!("tray left-click: {rect:?}"));
                let Some(window_id) = self.window_id else {
                    preview_log("tray click arrived before the window was ready");
                    return Task::none();
                };
                window::mode(window_id).map(move |mode| Message::TogglePopupFromTray(rect, mode))
            }
            Message::TogglePopupFromTray(rect, mode) => {
                preview_log(format!("tray window mode before toggle: {mode:?}"));
                self.toggle_popup_from_tray(rect, mode)
            }
            Message::TrayEvent(event) => {
                preview_log(format!("tray event ignored: {event:?}"));
                Task::none()
            }
            Message::OpenPreview => {
                preview_log("open preview requested");
                let Some(window_id) = self.window_id else {
                    return Task::none();
                };

                window::run(window_id, |_| {
                    TRAY_ICON.with(|tray| tray.borrow().as_ref().and_then(TrayIcon::rect))
                })
                .map(Message::PreviewRect)
            }
            Message::PreviewRect(Some(rect)) => {
                preview_log(format!("using tray rect: {rect:?}"));
                self.show_window(rect)
            }
            Message::PreviewRect(None) => {
                preview_log("tray rect unavailable; use taskbar-edge preview anchor");
                self.preview_at_taskbar_edge()
            }
            Message::RuntimeEvent(Event::Window(event)) => match event {
                window::Event::CloseRequested => {
                    preview_log("window close requested");
                    self.theme_menu_open = false;
                    self.account_add_menu_open = false;
                    self.dismiss_account_delete_dialog();
                    self.cancel_openrouter_credentials();
                    self.dashboard.close_any_model_visibility_menu();
                    self.hide_popup()
                }
                window::Event::Focused => {
                    self.window_focused = true;
                    Task::none()
                }
                window::Event::Unfocused => {
                    self.window_focused = false;
                    self.last_focus_lost = Some(Instant::now());
                    Task::none()
                }
                _ => Task::none(),
            },
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.openrouter_credentials_open => {
                self.cancel_openrouter_credentials();
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.account_delete_dialog_open
                && !self.account_delete_running
                && self.pending_account_deletion.is_some() =>
            {
                self.pending_account_deletion = None;
                self.account_delete_queued = false;
                self.account_delete_error = None;
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.account_delete_dialog_open && !self.account_delete_running => {
                self.dismiss_account_delete_dialog();
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.account_add_menu_open => {
                self.account_add_menu_open = false;
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.theme_menu_open => {
                self.theme_menu_open = false;
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.dashboard.has_open_model_visibility_menu() => {
                self.dashboard.close_any_model_visibility_menu();
                Task::none()
            }
            Message::RuntimeEvent(_) => Task::none(),
            Message::DragWindow => self.window_id.map(window::drag).unwrap_or_else(Task::none),
            Message::CloseButton => {
                self.theme_menu_open = false;
                self.account_add_menu_open = false;
                self.dismiss_account_delete_dialog();
                self.cancel_openrouter_credentials();
                self.dashboard.close_any_model_visibility_menu();
                self.hide_popup()
            }
            Message::SelectProvider(provider) => {
                self.selected_provider = provider;
                self.theme_menu_open = false;
                self.account_add_menu_open = false;
                self.dismiss_account_delete_dialog();
                self.dashboard.close_any_model_visibility_menu();
                self.dashboard.clear_any_hovered_account_name();
                Task::none()
            }
            Message::RefreshAllUsage => self.start_usage_refresh(),
            Message::RefreshIconTick => {
                self.refresh_icon_rotation_radians = advance_refresh_icon_rotation(
                    self.refresh_icon_rotation_radians,
                    self.dashboard_refresh_running,
                );
                self.dashboard.advance_usage_animation(Instant::now());
                Task::none()
            }
            Message::ToggleThemeMenu => {
                self.theme_menu_open = !self.theme_menu_open;
                self.account_add_menu_open = false;
                self.dismiss_account_delete_dialog();
                Task::none()
            }
            Message::DismissThemeMenu => {
                self.theme_menu_open = false;
                Task::none()
            }
            Message::ToggleAccountAddMenu => {
                if !self.account_add_running
                    && !self.openrouter_credentials_open
                    && !self.account_delete_running
                {
                    self.account_add_menu_open = !self.account_add_menu_open;
                    self.theme_menu_open = false;
                    self.dismiss_account_delete_dialog();
                }
                Task::none()
            }
            Message::DismissAccountAddMenu => {
                self.account_add_menu_open = false;
                Task::none()
            }
            Message::ToggleAccountDeleteDialog => {
                if self.account_add_running
                    || self.openrouter_credentials_open
                    || self.account_delete_running
                {
                    return Task::none();
                }
                if self.account_delete_dialog_open {
                    self.dismiss_account_delete_dialog();
                } else {
                    self.account_delete_dialog_open = true;
                    self.account_add_menu_open = false;
                    self.theme_menu_open = false;
                    self.pending_account_deletion = None;
                    self.account_delete_error = None;
                }
                Task::none()
            }
            Message::DismissAccountDeleteDialog => {
                self.dismiss_account_delete_dialog();
                Task::none()
            }
            Message::SelectAccountForDeletion(account_id) => {
                if self.account_delete_running {
                    return Task::none();
                }
                let Some(entry) = self
                    .dashboard
                    .account_entries()
                    .iter()
                    .find(|entry| entry.account.id == account_id)
                else {
                    return Task::none();
                };
                let Some(provider) = PROVIDER_TABS
                    .iter()
                    .find(|tab| {
                        dashboard::belongs_to_provider(&entry.account.provider_id, tab.provider)
                    })
                    .map(|tab| tab.provider)
                else {
                    return Task::none();
                };
                self.pending_account_deletion = Some(PendingAccountDeletion {
                    account_id,
                    provider,
                    display_name: entry.account.display_name().to_owned(),
                    email: entry.account.email.clone(),
                });
                self.account_delete_error = None;
                Task::none()
            }
            Message::CancelAccountDeletion => {
                if !self.account_delete_running {
                    self.pending_account_deletion = None;
                    self.account_delete_queued = false;
                    self.account_delete_error = None;
                }
                Task::none()
            }
            Message::ConfirmAccountDeletion => {
                if self.pending_account_deletion.is_none() {
                    return Task::none();
                }
                if self.account_delete_running || self.account_delete_queued {
                    return Task::none();
                }
                if self.dashboard_refresh_running {
                    self.account_delete_queued = true;
                    self.account_delete_error = None;
                    return Task::none();
                }
                self.begin_pending_account_deletion()
            }
            Message::AccountDeletionCompleted(account_id, result) => {
                self.account_delete_running = false;
                self.account_delete_queued = false;
                if self
                    .pending_account_deletion
                    .as_ref()
                    .is_some_and(|pending| pending.account_id == account_id)
                {
                    match result {
                        Ok(accounts) => {
                            self.dashboard.set_accounts(accounts);
                            self.pending_account_deletion = None;
                            self.account_delete_dialog_open = false;
                            self.account_delete_error = None;
                        }
                        Err(error) => {
                            self.account_delete_error = Some(error);
                        }
                    }
                }
                Task::none()
            }
            Message::ChooseAccountProvider(provider) => {
                self.account_add_menu_open = false;
                self.theme_menu_open = false;
                if self.account_add_running {
                    return Task::none();
                }
                if provider == UsageProvider::OpenRouter {
                    self.openrouter_api_key.clear();
                    self.openrouter_management_key.clear();
                    self.openrouter_credentials_open = true;
                    self.account_add_status = None;
                    Task::none()
                } else {
                    self.begin_account_add(provider, None)
                }
            }
            Message::OpenRouterApiKeyChanged(value) => {
                self.openrouter_api_key = value;
                Task::none()
            }
            Message::OpenRouterManagementKeyChanged(value) => {
                self.openrouter_management_key = value;
                Task::none()
            }
            Message::SubmitOpenRouterCredentials => {
                let api_key = self.openrouter_api_key.trim().to_owned();
                if api_key.is_empty() || self.account_add_running {
                    return Task::none();
                }
                let management_key = self.openrouter_management_key.trim().to_owned();
                self.openrouter_api_key.clear();
                self.openrouter_management_key.clear();
                self.openrouter_credentials_open = false;
                self.begin_account_add(UsageProvider::OpenRouter, Some((api_key, management_key)))
            }
            Message::CancelOpenRouterCredentials => {
                self.cancel_openrouter_credentials();
                Task::none()
            }
            Message::AccountAddCompleted(provider, result) => {
                self.account_add_running = false;
                self.account_add_cancel = None;
                match result {
                    Err(error) if error == ACCOUNT_ADD_CANCELLED => {
                        self.account_add_status = None;
                        Task::none()
                    }
                    Ok(()) => {
                        self.account_add_status = Some(AccountAddStatus::Added(provider));
                        self.selected_provider = provider;
                        Task::perform(dashboard::load_saved_accounts(), Message::DashboardLoaded)
                    }
                    Err(error) => {
                        preview_log(format!("account add failed for {provider:?}"));
                        self.account_add_status = Some(AccountAddStatus::Failed(error));
                        Task::none()
                    }
                }
            }
            Message::CancelAccountAdd => {
                if let Some(cancel) = self.account_add_cancel.take() {
                    let _ = cancel.try_send(());
                }
                Task::none()
            }
            Message::DismissAccountAddStatus => {
                if !self.account_add_running {
                    self.account_add_status = None;
                }
                Task::none()
            }
            Message::ToggleModelVisibilityMenu(account_id) => {
                self.dashboard.toggle_model_visibility_menu(account_id);
                Task::none()
            }
            Message::CloseModelVisibilityMenu(account_id) => {
                self.dashboard.close_model_visibility_menu(account_id);
                Task::none()
            }
            Message::SetModelVisibility(model_id, is_visible) => {
                if let Err(error) = self.dashboard.set_model_visibility(model_id, is_visible) {
                    preview_log(format!("model visibility preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SetModelQuotaDisplay(show_all) => {
                self.dashboard.set_show_all_model_quotas(show_all);
                Task::none()
            }
            Message::SetAntigravityQuotaGroups(show_groups) => {
                self.dashboard
                    .set_show_antigravity_quota_groups(show_groups);
                Task::none()
            }
            Message::SelectTheme(theme_id) => {
                self.theme_id = theme_id;
                self.backdrop_image = backdrop_image_handle(theme_id);
                self.theme_menu_open = false;
                self.account_add_menu_open = false;
                self.dismiss_account_delete_dialog();
                if let Err(error) = save_theme(theme_id) {
                    preview_log(format!("theme preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SwitchCodexDesktopAccount(account_id) => {
                if !self.dashboard.begin_codex_switch(account_id) {
                    return Task::none();
                }
                Task::perform(
                    codex_switch::switch_codex_desktop_account(account_id),
                    move |result| Message::CodexDesktopSwitchFinished(account_id, result),
                )
            }
            Message::CodexDesktopSwitchFinished(account_id, result) => {
                if let Err(error) = &result {
                    preview_log(format!("Codex desktop switch failed: {error}"));
                }
                self.dashboard.finish_codex_switch(account_id, result);
                Task::none()
            }
            Message::SelectPercentDisplay(mode) => {
                percent_display::set_current(mode);
                self.theme_menu_open = false;
                if let Err(error) = percent_display::save(mode) {
                    preview_log(format!("percent display preference save failed: {error}"));
                }
                Task::none()
            }
        }
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

    fn view(&self) -> Element<'_, Message> {
        let active_theme = self.theme_id.definition();
        let provider_tab_bar = provider_tab_bar(self.selected_provider, active_theme);
        let title_bar = container(
            row![
                mouse_area(Space::new().width(Fill).height(Length::Fill))
                    .on_press(Message::DragWindow),
                add_account_button(self.account_add_running, active_theme, self.language),
                delete_account_button(
                    self.account_delete_dialog_open,
                    self.account_add_running
                        || self.openrouter_credentials_open
                        || self.account_delete_running,
                    active_theme,
                    self.language,
                ),
                refresh_button(
                    self.dashboard_refresh_running,
                    self.refresh_icon_rotation_radians,
                    active_theme,
                    self.language,
                ),
                theme_button(active_theme),
                close_window_button(active_theme),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
            .width(Fill),
        )
        .width(Fill)
        .height(44)
        .padding([4, 10])
        .style(move |_| container::Style {
            background: Some(Background::Color(if active_theme.colors.is_light {
                active_theme.colors.control_surface()
            } else {
                Color::from_rgba(0.0, 0.0, 0.0, TITLE_BAR_SHADE_OPACITY)
            })),
            border: Border {
                radius: iced::border::Radius::default().top(WINDOW_FRAME_RADIUS),
                ..Border::default()
            },
            ..Default::default()
        });

        let title_bar_separator = container(Space::new().width(Fill).height(Length::Fill))
            .width(Fill)
            .height(1)
            .style(move |_| container::Style {
                background: Some(Background::Color(active_theme.colors.border(
                    if active_theme.colors.is_light {
                        0.38
                    } else {
                        0.16
                    },
                ))),
                ..Default::default()
            });

        let account_add_status: Element<'_, Message> = self
            .account_add_status
            .as_ref()
            .map(|status| account_add_status_banner(status, active_theme, self.language))
            .unwrap_or_else(|| Space::new().width(Fill).height(0).into());

        let foreground = column![
            title_bar,
            title_bar_separator,
            provider_tab_bar,
            account_add_status,
            dashboard::view(
                &self.dashboard,
                self.selected_provider,
                active_theme,
                self.language,
            )
        ]
        .spacing(0)
        .width(Fill)
        .height(Fill);

        let backdrop: Element<'_, Message> = if let Some(backdrop_image) = &self.backdrop_image {
            image(backdrop_image.clone())
                .width(Fill)
                .height(Fill)
                .content_fit(ContentFit::Cover)
                .opacity(
                    active_theme
                        .backdrop
                        .map_or(1.0, |backdrop| backdrop.image_opacity),
                )
                .into()
        } else {
            Space::new().width(Fill).height(Fill).into()
        };

        let shade: Element<'_, Message> = if let Some(backdrop) = active_theme.backdrop {
            container(Space::new().width(Fill).height(Fill))
                .width(Fill)
                .height(Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(Color::from_rgba(
                        0.0,
                        0.0,
                        0.0,
                        backdrop.shade_opacity,
                    ))),
                    border: Border {
                        radius: WINDOW_FRAME_RADIUS.into(),
                        ..Border::default()
                    },
                    ..Default::default()
                })
                .into()
        } else {
            Space::new().width(Fill).height(Fill).into()
        };

        let page: Element<'_, Message> = stack![backdrop, shade, foreground]
            .width(Fill)
            .height(Fill)
            .into();

        let content = if self.openrouter_credentials_open {
            let dismiss_area = mouse_area(Space::new().width(Fill).height(Fill))
                .on_press(Message::CancelOpenRouterCredentials);
            let credentials_dialog = container(openrouter_credentials_dialog(
                &self.openrouter_api_key,
                &self.openrouter_management_key,
                self.language,
                active_theme,
            ))
            .width(Fill)
            .height(Fill)
            .center(Fill);

            stack![page, dismiss_area, credentials_dialog]
                .width(Fill)
                .height(Fill)
                .into()
        } else if self.account_delete_dialog_open {
            let scrim = container(Space::new().width(Fill).height(Fill))
                .width(Fill)
                .height(Fill)
                .style(|_| container::Style {
                    background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.54))),
                    ..Default::default()
                });
            let dismiss_area = mouse_area(Space::new().width(Fill).height(Fill))
                .on_press(Message::DismissAccountDeleteDialog);
            let dialog = if let Some(pending) = &self.pending_account_deletion {
                account_deletion_confirmation_dialog(
                    pending,
                    self.account_delete_running,
                    self.account_delete_queued,
                    self.account_delete_error.as_deref(),
                    self.language,
                    active_theme,
                )
            } else {
                account_deletion_picker_dialog(
                    self.dashboard.account_entries(),
                    self.language,
                    active_theme,
                )
            };
            let dialog_layer = container(dialog).width(Fill).height(Fill).center(Fill);

            stack![page, scrim, dismiss_area, dialog_layer]
                .width(Fill)
                .height(Fill)
                .into()
        } else if self.account_add_menu_open {
            let dismiss_area = column![
                Space::new().width(Fill).height(45),
                mouse_area(Space::new().width(Fill).height(Fill))
                    .on_press(Message::DismissAccountAddMenu),
            ]
            .width(Fill)
            .height(Fill);

            let account_menu_layer = container(account_add_dropdown(self.language, active_theme))
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::Start)
                .padding([44, 110]);

            stack![page, dismiss_area, account_menu_layer]
                .width(Fill)
                .height(Fill)
                .into()
        } else if self.theme_menu_open {
            let dismiss_area = column![
                Space::new().width(Fill).height(45),
                mouse_area(Space::new().width(Fill).height(Fill))
                    .on_press(Message::DismissThemeMenu),
            ]
            .width(Fill)
            .height(Fill);

            let theme_menu_layer = container(theme_dropdown(self.theme_id, self.language))
                .width(Fill)
                .height(Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::Start)
                .padding([44, 50]);

            stack![page, dismiss_area, theme_menu_layer]
                .width(Fill)
                .height(Fill)
                .into()
        } else {
            page
        };

        // Draw the frame last so backdrop images cannot cover its rim.
        let frame_outline = container(Space::new().width(Fill).height(Fill))
            .width(Fill)
            .height(Fill)
            .style(move |_| window_frame_outline_style(active_theme));
        let content = stack![content, frame_outline].width(Fill).height(Fill);

        container(content)
            .width(Fill)
            .height(Fill)
            .style(move |_| window_frame_style(active_theme))
            .into()
    }
}

fn sibling_account_cli_path(current_executable: &Path) -> std::path::PathBuf {
    let extension = std::env::consts::EXE_EXTENSION;
    let filename = if extension.is_empty() {
        "codex-usage".to_owned()
    } else {
        format!("codex-usage.{extension}")
    };
    current_executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(filename)
}

fn spawn_account_add_worker<F, Fut>(
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
const ACCOUNT_ADD_CANCELLED: &str = "account add cancelled";

async fn add_account(
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

    if provider == UsageProvider::OpenRouter {
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

fn kill_process_tree(process_id: u32) {
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

fn account_add_failure_detail(stdout: &[u8], stderr: &[u8]) -> String {
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

fn redact_and_limit_account_add_error(mut message: String, secrets: &[String]) -> String {
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
    SelectAccountForDeletion(codex_usage_core::accounts::AccountId),
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
    SwitchCodexDesktopAccount(codex_usage_core::accounts::AccountId),
    CodexDesktopSwitchFinished(codex_usage_core::accounts::AccountId, Result<(), String>),
    SelectProvider(UsageProvider),
    DashboardLoaded(Result<Vec<dashboard::AccountUsageEntry>, String>),
    PriorityUsageRefreshEvent(UsageProvider, usage_refresh::RefreshEvent),
    OtherProvidersUsageRefreshEvent(usage_refresh::RefreshEvent),
    BeginAliasEdit(codex_usage_core::accounts::AccountId),
    AccountNameHovered(codex_usage_core::accounts::AccountId),
    AccountNameHoverEnded(codex_usage_core::accounts::AccountId),
    AliasDraftChanged(codex_usage_core::accounts::AccountId, String),
    CancelAliasEdit(codex_usage_core::accounts::AccountId),
    ToggleModelVisibilityMenu(codex_usage_core::accounts::AccountId),
    CloseModelVisibilityMenu(codex_usage_core::accounts::AccountId),
    SetModelVisibility(String, bool),
    SetModelQuotaDisplay(bool),
    SetAntigravityQuotaGroups(bool),
    SetAntigravityClaudeGptHidden(bool),
    MoveAccount(codex_usage_core::accounts::AccountId, isize),
    SaveAlias(codex_usage_core::accounts::AccountId),
    AliasSaved(
        codex_usage_core::accounts::AccountId,
        Result<Vec<dashboard::AccountUsageEntry>, String>,
    ),
    AccountDeletionCompleted(
        codex_usage_core::accounts::AccountId,
        Result<Vec<dashboard::AccountUsageEntry>, String>,
    ),
}

/// Pressing the tray icon takes focus from the popup just before the click
/// arrives, so focus lost this recently still counts as "in front".
const TRAY_CLICK_FOCUS_GRACE: Duration = Duration::from_millis(500);

/// A tray click hides the popup only when it is visible and in front. A popup
/// left open behind other windows is brought forward instead, so one click
/// always shows it.
fn tray_click_should_hide(
    visible: bool,
    focused: bool,
    last_focus_lost: Option<Instant>,
    now: Instant,
) -> bool {
    visible
        && (focused
            || last_focus_lost
                .is_some_and(|lost| now.saturating_duration_since(lost) <= TRAY_CLICK_FOCUS_GRACE))
}

fn install_tray(sender: Sender<TrayIconEvent>) -> Result<(), String> {
    let icon = Icon::from_rgba(icon_pixels(), 32, 32).map_err(|error| error.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Usage Monitor · Preview")
        .build()
        .map_err(|error| error.to_string())?;

    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            &event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = sender.try_send(event);
        }
    }));

    TRAY_ICON.with(|slot| *slot.borrow_mut() = Some(tray));
    Ok(())
}

fn tray_event_stream() -> impl Stream<Item = Message> {
    let receiver = TRAY_EVENT_RECEIVER
        .get()
        .expect("tray event receiver is initialized before the UI")
        .clone();
    iced::stream::channel(32, async move |mut output| {
        while let Ok(event) = receiver.recv().await {
            if output.send(Message::TrayEvent(event)).await.is_err() {
                break;
            }
        }
    })
}

fn refresh_icon_tick_stream() -> impl Stream<Item = Message> {
    let (sender, receiver) = async_channel::bounded::<()>(1);
    thread::spawn(move || {
        loop {
            thread::sleep(REFRESH_ICON_TICK);
            if sender.send_blocking(()).is_err() {
                break;
            }
        }
    });

    iced::futures::stream::unfold(receiver, |receiver| async move {
        receiver
            .recv()
            .await
            .ok()
            .map(|()| (Message::RefreshIconTick, receiver))
    })
}

fn advance_refresh_icon_rotation(rotation: f32, refreshing: bool) -> f32 {
    if !refreshing {
        return 0.0;
    }

    (rotation + 15.0_f32.to_radians()).rem_euclid(std::f32::consts::TAU)
}

fn should_run_popup_animation_ticks(
    popup_visible: bool,
    blocking_dialog_open: bool,
    refreshing: bool,
    usage_animation_active: bool,
) -> bool {
    popup_visible && !blocking_dialog_open && (refreshing || usage_animation_active)
}

fn refresh_icon_handle(theme: &'static ThemeDefinition) -> image::Handle {
    let handles = REFRESH_ICON_HANDLES.get_or_init(|| {
        THEME_MANIFEST
            .iter()
            .map(|theme| (theme.id, render_refresh_icon(theme.colors.text)))
            .collect()
    });
    handles
        .iter()
        .find(|(theme_id, _)| *theme_id == theme.id)
        .map(|(_, handle)| handle.clone())
        .expect("every active theme must exist in the theme manifest")
}

fn render_refresh_icon(color: [u8; 3]) -> image::Handle {
    const SIZE: u32 = 96;
    const CENTER: (f32, f32) = (12.0, 12.0);
    const RADIUS: f32 = 8.0;
    const STROKE_WIDTH: f32 = 2.0;

    let scale = SIZE as f32 / 24.0;
    let mut pixels = vec![0; (SIZE * SIZE * 4) as usize];
    for end_angle in [35.0, -145.0] {
        draw_refresh_arc(
            &mut pixels,
            SIZE,
            color,
            CENTER,
            RADIUS,
            STROKE_WIDTH,
            end_angle + 160.0,
            end_angle,
            scale,
        );
        draw_refresh_arrowhead(
            &mut pixels,
            SIZE,
            color,
            CENTER,
            RADIUS,
            STROKE_WIDTH,
            end_angle,
            scale,
        );
    }

    image::Handle::from_rgba(SIZE, SIZE, pixels)
}

fn draw_refresh_arc(
    pixels: &mut [u8],
    size: u32,
    color: [u8; 3],
    center: (f32, f32),
    radius: f32,
    stroke_width: f32,
    start_angle: f32,
    end_angle: f32,
    scale: f32,
) {
    const SEGMENTS: usize = 48;
    let angle_span = (start_angle - end_angle).rem_euclid(360.0);
    let mut previous = point_on_circle(center, radius, start_angle);

    for segment in 1..=SEGMENTS {
        let progress = segment as f32 / SEGMENTS as f32;
        let angle = start_angle - angle_span * progress;
        let next = point_on_circle(center, radius, angle);
        draw_refresh_line(pixels, size, color, previous, next, stroke_width, scale);
        previous = next;
    }
}

fn draw_refresh_arrowhead(
    pixels: &mut [u8],
    size: u32,
    color: [u8; 3],
    center: (f32, f32),
    radius: f32,
    stroke_width: f32,
    angle_degrees: f32,
    scale: f32,
) {
    let angle = angle_degrees.to_radians();
    let tip = (
        center.0 + radius * angle.cos(),
        center.1 + radius * angle.sin(),
    );
    let tangent = (angle.sin(), -angle.cos());
    let base = (tip.0 - tangent.0 * 3.2, tip.1 - tangent.1 * 3.2);
    let perpendicular = (-tangent.1 * 2.2, tangent.0 * 2.2);

    for wing in [1.0, -1.0] {
        draw_refresh_line(
            pixels,
            size,
            color,
            tip,
            (
                base.0 + perpendicular.0 * wing,
                base.1 + perpendicular.1 * wing,
            ),
            stroke_width,
            scale,
        );
    }
}

fn point_on_circle(center: (f32, f32), radius: f32, angle_degrees: f32) -> (f32, f32) {
    let angle = angle_degrees.to_radians();
    (
        center.0 + radius * angle.cos(),
        center.1 + radius * angle.sin(),
    )
}

fn draw_refresh_line(
    pixels: &mut [u8],
    size: u32,
    color: [u8; 3],
    start: (f32, f32),
    end: (f32, f32),
    stroke_width: f32,
    scale: f32,
) {
    let start = (start.0 * scale, start.1 * scale);
    let end = (end.0 * scale, end.1 * scale);
    let radius = stroke_width * scale / 2.0;
    let delta = (end.0 - start.0, end.1 - start.1);
    let length_squared = delta.0 * delta.0 + delta.1 * delta.1;
    let min_x = (start.0.min(end.0) - radius - 1.0).floor().max(0.0) as u32;
    let max_x = (start.0.max(end.0) + radius + 1.0)
        .ceil()
        .min(size as f32 - 1.0) as u32;
    let min_y = (start.1.min(end.1) - radius - 1.0).floor().max(0.0) as u32;
    let max_y = (start.1.max(end.1) + radius + 1.0)
        .ceil()
        .min(size as f32 - 1.0) as u32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let point = (x as f32 + 0.5, y as f32 + 0.5);
            let projection = if length_squared == 0.0 {
                0.0
            } else {
                (((point.0 - start.0) * delta.0 + (point.1 - start.1) * delta.1) / length_squared)
                    .clamp(0.0, 1.0)
            };
            let closest = (
                start.0 + projection * delta.0,
                start.1 + projection * delta.1,
            );
            let distance = ((point.0 - closest.0).powi(2) + (point.1 - closest.1).powi(2)).sqrt();
            let alpha = ((radius + 0.5 - distance).clamp(0.0, 1.0) * 255.0) as u8;
            let pixel_index = ((y * size + x) * 4) as usize;
            if alpha > pixels[pixel_index + 3] {
                pixels[pixel_index..pixel_index + 4]
                    .copy_from_slice(&[color[0], color[1], color[2], alpha]);
            }
        }
    }
}

fn icon_pixels() -> Vec<u8> {
    let mut pixels = vec![0_u8; 32 * 32 * 4];
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            if dx * dx + dy * dy <= 14 * 14 {
                let index = (y * 32 + x) * 4;
                pixels[index..index + 4].copy_from_slice(&[126, 111, 250, 255]);
            }
        }
    }
    pixels
}

#[derive(Clone, Copy, Debug)]
struct PhysicalWorkArea {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

#[derive(Clone, Copy)]
enum MonitorEdge {
    Left,
    Top,
    Right,
    Bottom,
}

fn reserve_auto_hide_bar(
    work_area: &mut PhysicalWorkArea,
    monitor: PhysicalWorkArea,
    edge: MonitorEdge,
    thickness: f32,
) {
    match edge {
        MonitorEdge::Left => work_area.left = work_area.left.max(monitor.left + thickness),
        MonitorEdge::Top => work_area.top = work_area.top.max(monitor.top + thickness),
        MonitorEdge::Right => work_area.right = work_area.right.min(monitor.right - thickness),
        MonitorEdge::Bottom => work_area.bottom = work_area.bottom.min(monitor.bottom - thickness),
    }
}

fn full_monitor_work_area(monitor: Size, scale_factor: f32) -> PhysicalWorkArea {
    PhysicalWorkArea {
        left: 0.0,
        top: 0.0,
        right: monitor.width * scale_factor,
        bottom: monitor.height * scale_factor,
    }
}

#[cfg(target_os = "windows")]
fn monitor_work_area(rect: tray_icon::Rect) -> Option<PhysicalWorkArea> {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
        UI::{
            Shell::{
                ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETAUTOHIDEBAREX, APPBARDATA,
                SHAppBarMessage,
            },
            WindowsAndMessaging::GetWindowRect,
        },
    };

    let center = POINT {
        x: (rect.position.x + f64::from(rect.size.width) / 2.0).round() as i32,
        y: (rect.position.y + f64::from(rect.size.height) / 2.0).round() as i32,
    };
    let monitor = unsafe { MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }

    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }

    let monitor_bounds = PhysicalWorkArea {
        left: info.rcMonitor.left as f32,
        top: info.rcMonitor.top as f32,
        right: info.rcMonitor.right as f32,
        bottom: info.rcMonitor.bottom as f32,
    };
    let mut work_area = PhysicalWorkArea {
        left: info.rcWork.left as f32,
        top: info.rcWork.top as f32,
        right: info.rcWork.right as f32,
        bottom: info.rcWork.bottom as f32,
    };

    for (native_edge, edge) in [
        (ABE_LEFT, MonitorEdge::Left),
        (ABE_TOP, MonitorEdge::Top),
        (ABE_RIGHT, MonitorEdge::Right),
        (ABE_BOTTOM, MonitorEdge::Bottom),
    ] {
        let mut appbar_data = APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            uEdge: native_edge,
            rc: info.rcMonitor,
            ..Default::default()
        };
        let appbar = unsafe { SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut appbar_data) }
            as windows_sys::Win32::Foundation::HWND;
        if appbar.is_null() {
            continue;
        }

        let mut appbar_bounds = windows_sys::Win32::Foundation::RECT::default();
        if unsafe { GetWindowRect(appbar, &mut appbar_bounds) } == 0 {
            continue;
        }

        let thickness = match edge {
            MonitorEdge::Left | MonitorEdge::Right => {
                (appbar_bounds.right - appbar_bounds.left) as f32
            }
            MonitorEdge::Top | MonitorEdge::Bottom => {
                (appbar_bounds.bottom - appbar_bounds.top) as f32
            }
        };
        if thickness > 0.0 {
            reserve_auto_hide_bar(&mut work_area, monitor_bounds, edge, thickness);
            break;
        }
    }

    (work_area.right > work_area.left && work_area.bottom > work_area.top).then_some(work_area)
}

#[cfg(not(target_os = "windows"))]
fn monitor_work_area(_: tray_icon::Rect) -> Option<PhysicalWorkArea> {
    None
}

fn popup_position(rect: tray_icon::Rect, scale_factor: f32, work_area: PhysicalWorkArea) -> Point {
    let scale_factor = scale_factor.max(1.0);
    let icon_left = rect.position.x as f32;
    let icon_top = rect.position.y as f32;
    let icon_width = rect.size.width as f32;
    let icon_height = rect.size.height as f32;
    let width = WINDOW_WIDTH * scale_factor;
    let height = WINDOW_HEIGHT * scale_factor;
    let x = (icon_left + icon_width / 2.0 - width / 2.0).clamp(
        work_area.left,
        (work_area.right - width).max(work_area.left),
    );
    let above = icon_top - height - GAP * scale_factor;
    let below = icon_top + icon_height + GAP * scale_factor;
    let y = if above >= work_area.top && above + height <= work_area.bottom {
        above
    } else if below >= work_area.top && below + height <= work_area.bottom {
        below
    } else {
        above.clamp(
            work_area.top,
            (work_area.bottom - height).max(work_area.top),
        )
    };

    Point::new(x / scale_factor, y / scale_factor)
}

fn backdrop_image_handle(theme_id: ThemeId) -> Option<image::Handle> {
    let backdrop = theme_id.definition().backdrop?;

    match rounded_backdrop_image(backdrop) {
        Ok(image) => Some(image),
        Err(error) => {
            preview_log(format!("theme backdrop preparation failed: {error}"));
            None
        }
    }
}

fn rounded_backdrop_image(backdrop: theme::ThemeBackdrop) -> Result<image::Handle, String> {
    let decoded = ::image::load_from_memory(backdrop.image_bytes)
        .map_err(|error| format!("could not decode image: {error}"))?
        .to_rgba8();
    let cover_scale =
        (WINDOW_WIDTH / decoded.width() as f32).max(WINDOW_HEIGHT / decoded.height() as f32);
    let image_scale = backdrop.image_scale.max(0.01);
    let crop_width = (WINDOW_WIDTH / (cover_scale * image_scale))
        .round()
        .clamp(1.0, decoded.width() as f32) as u32;
    let crop_height = (WINDOW_HEIGHT / (cover_scale * image_scale))
        .round()
        .clamp(1.0, decoded.height() as f32) as u32;
    let cropped = ::image::imageops::crop_imm(
        &decoded,
        (decoded.width() - crop_width) / 2,
        (decoded.height() - crop_height) / 2,
        crop_width,
        crop_height,
    )
    .to_image();

    let output_width = WINDOW_WIDTH as u32 * BACKDROP_PIXEL_SCALE;
    let output_height = WINDOW_HEIGHT as u32 * BACKDROP_PIXEL_SCALE;
    let mut output = ::image::imageops::resize(
        &cropped,
        output_width,
        output_height,
        ::image::imageops::FilterType::Lanczos3,
    );
    let radius = WINDOW_FRAME_RADIUS * BACKDROP_PIXEL_SCALE as f32;

    for (x, y, pixel) in output.enumerate_pixels_mut() {
        let coverage = rounded_rectangle_coverage(
            x as f32 + 0.5,
            y as f32 + 0.5,
            output_width as f32,
            output_height as f32,
            radius,
        );
        pixel.0[3] = (f32::from(pixel.0[3]) * coverage).round() as u8;
    }

    Ok(image::Handle::from_rgba(
        output_width,
        output_height,
        output.into_raw(),
    ))
}

fn rounded_rectangle_coverage(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    let corner_x = (x - width / 2.0).abs() - (width / 2.0 - radius);
    let corner_y = (y - height / 2.0).abs() - (height / 2.0 - radius);
    let outside_distance = corner_x.max(0.0).hypot(corner_y.max(0.0));
    let inside_distance = corner_x.max(corner_y).min(0.0);
    let signed_distance = outside_distance + inside_distance - radius;

    (0.5 - signed_distance).clamp(0.0, 1.0)
}

fn add_account_button(
    adding: bool,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let button = button(container(icon_user_round_plus().size(17)).center(Fill))
        .on_press_maybe((!adding).then_some(Message::ToggleAccountAddMenu))
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background =
                if adding || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                    Some(Background::Color(active_theme.colors.hover()))
                } else {
                    None
                };
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    tooltip(
        button,
        text(locale::text(language, locale::Text::AddAccount)).size(typography::METADATA_SIZE),
        tooltip::Position::Bottom,
    )
    .delay(Duration::from_millis(350))
    .into()
}

fn delete_account_button(
    open: bool,
    disabled: bool,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let delete_color = active_theme.colors.danger_hover();
    let button = button(container(icon_trash_2().size(16)).center(Fill))
        .on_press_maybe((!disabled).then_some(Message::ToggleAccountDeleteDialog))
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let highlighted =
                open || matches!(status, button::Status::Hovered | button::Status::Pressed);
            let mut style = button::text(theme, status);
            style.background = highlighted.then(|| {
                Background::Color(delete_color.scale_alpha(if active_theme.colors.is_light {
                    0.14
                } else {
                    0.24
                }))
            });
            style.text_color = if highlighted {
                delete_color
            } else {
                active_theme.colors.text()
            };
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    tooltip(
        button,
        text(locale::text(language, locale::Text::DeleteAccount)).size(typography::METADATA_SIZE),
        tooltip::Position::Bottom,
    )
    .delay(Duration::from_millis(350))
    .into()
}

fn theme_button(active_theme: &'static ThemeDefinition) -> Element<'static, Message> {
    button(container(icon_palette().size(16)).center(Fill))
        .on_press(Message::ToggleThemeMenu)
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background =
                if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                    Some(Background::Color(active_theme.colors.hover()))
                } else {
                    None
                };
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

fn refresh_button(
    refreshing: bool,
    rotation_radians: f32,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let refresh_icon = image::Image::new(refresh_icon_handle(active_theme))
        .width(16)
        .height(16)
        .rotation(rotation_radians);

    let button = button(container(refresh_icon).center(Fill))
        .on_press_maybe((!refreshing).then_some(Message::RefreshAllUsage))
        .width(30)
        .height(29)
        .padding(0)
        .style(move |theme: &Theme, status| {
            let mut style = button::text(theme, status);
            style.background = if refreshing
                || matches!(status, button::Status::Hovered | button::Status::Pressed)
            {
                Some(Background::Color(active_theme.colors.hover()))
            } else {
                None
            };
            style.text_color = active_theme.colors.text();
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });

    tooltip(
        button,
        text(locale::text(
            language,
            if refreshing {
                locale::Text::RefreshingUsage
            } else {
                locale::Text::RefreshUsage
            },
        ))
        .size(typography::BODY_SIZE)
        .color(active_theme.colors.text()),
        tooltip::Position::Bottom,
    )
    .padding(8)
    .gap(5)
    .delay(Duration::from_millis(350))
    .style(move |_| container::Style {
        background: Some(Background::Color(active_theme.colors.control_surface())),
        text_color: Some(active_theme.colors.text()),
        border: Border {
            color: active_theme.colors.border(if active_theme.colors.is_light {
                0.35
            } else {
                0.22
            }),
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: Shadow::default(),
        ..Default::default()
    })
    .into()
}

fn provider_tab_bar(
    selected_provider: UsageProvider,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let tabs = PROVIDER_TABS
        .iter()
        .copied()
        .map(|tab| provider_tab(tab, selected_provider, active_theme));

    container(row(tabs).spacing(2).width(Fill))
        .width(Fill)
        .height(37)
        .padding([3, 8])
        .style(move |_| container::Style {
            background: Some(Background::Color(if active_theme.colors.is_light {
                active_theme.colors.control_surface()
            } else {
                Color::from_rgba(0.0, 0.0, 0.0, PROVIDER_TAB_SHADE_OPACITY)
            })),
            ..Default::default()
        })
        .into()
}

fn provider_tab(
    tab: ProviderTab,
    selected_provider: UsageProvider,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let selected = tab.provider == selected_provider;

    let tab_button = button(
        container(
            image(provider_logo_handle(
                tab.provider,
                active_theme.colors.is_light,
            ))
            .width(24)
            .height(24)
            .content_fit(ContentFit::Contain),
        )
        .center(Fill),
    )
    .on_press(Message::SelectProvider(tab.provider))
    .width(Fill)
    .height(31)
    .padding(0)
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        let highlighted =
            selected || matches!(status, button::Status::Hovered | button::Status::Pressed);
        style.background = highlighted.then(|| Background::Color(active_theme.colors.hover()));
        style.text_color = active_theme.colors.text();
        style.border = Border {
            radius: 7.0.into(),
            ..Border::default()
        };
        style.shadow = Shadow::default();
        style
    });

    tooltip(
        tab_button,
        text(tab.label).size(typography::METADATA_SIZE),
        tooltip::Position::Bottom,
    )
    .delay(Duration::from_millis(350))
    .into()
}

fn provider_logo_handle(provider: UsageProvider, light_theme: bool) -> image::Handle {
    if light_theme {
        let logo_index = match provider {
            UsageProvider::Codex => Some(0),
            UsageProvider::OpenRouter => Some(1),
            _ => None,
        };
        if let Some(logo_index) = logo_index {
            let logos = LIGHT_THEME_PROVIDER_LOGOS.get_or_init(|| {
                [
                    decode_provider_logo(include_bytes!("../assets/providers/chatgpt.png"), true),
                    decode_provider_logo(
                        include_bytes!("../assets/providers/openrouter.png"),
                        true,
                    ),
                ]
            });
            return logos[logo_index].clone();
        }
    }

    let logos = PROVIDER_LOGOS.get_or_init(|| {
        [
            decode_provider_logo(include_bytes!("../assets/providers/chatgpt.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/claude.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/antigravity.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/opencode-go.png"), false),
            decode_provider_logo(include_bytes!("../assets/providers/openrouter.png"), false),
        ]
    });

    logos[match provider {
        UsageProvider::Codex => 0,
        UsageProvider::Claude => 1,
        UsageProvider::Antigravity => 2,
        UsageProvider::OpenCodeGo => 3,
        UsageProvider::OpenRouter => 4,
    }]
    .clone()
}

fn account_add_dropdown(
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let providers = PROVIDER_TABS
        .iter()
        .copied()
        .map(|provider| account_provider_choice(provider, active_theme));

    container(
        column![
            text(locale::text(language, locale::Text::ChooseProvider))
                .size(typography::LABEL_SIZE)
                .color(active_theme.colors.muted_text()),
            column(providers).spacing(2),
        ]
        .spacing(6),
    )
    .width(204)
    .padding(8)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

fn account_provider_choice(
    provider: ProviderTab,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(
        row![
            image(provider_logo_handle(
                provider.provider,
                active_theme.colors.is_light,
            ))
            .width(24)
            .height(24)
            .content_fit(ContentFit::Contain),
            text(provider.label)
                .size(typography::CONTROL_SIZE)
                .color(active_theme.colors.text())
                .width(Fill),
        ]
        .spacing(9)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ChooseAccountProvider(provider.provider))
    .width(Fill)
    .height(34)
    .padding([3, 7])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = matches!(status, button::Status::Hovered | button::Status::Pressed)
            .then(|| Background::Color(active_theme.colors.hover()));
        style.text_color = active_theme.colors.text();
        style.border = Border {
            radius: 7.0.into(),
            ..Border::default()
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

fn account_deletion_picker_dialog(
    accounts: &[dashboard::AccountUsageEntry],
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let mut provider_groups = Vec::new();
    let mut account_count = 0;

    for provider_tab in PROVIDER_TABS.iter().copied() {
        let provider_accounts = accounts
            .iter()
            .filter(|entry| {
                dashboard::belongs_to_provider(&entry.account.provider_id, provider_tab.provider)
            })
            .collect::<Vec<_>>();
        if provider_accounts.is_empty() {
            continue;
        }

        account_count += provider_accounts.len();
        let mut account_rows = Vec::with_capacity(provider_accounts.len());
        for entry in provider_accounts {
            let account_id = entry.account.id;
            let display_name = entry.account.display_name().to_owned();
            let email = entry.account.email.clone();
            account_rows.push(
                button(
                    row![
                        column![
                            text(display_name)
                                .size(typography::LABEL_SIZE)
                                .color(active_theme.colors.text()),
                            text(email)
                                .size(typography::METADATA_SIZE)
                                .color(active_theme.colors.muted_text()),
                        ]
                        .spacing(1)
                        .width(Fill),
                        icon_trash_2()
                            .size(14)
                            .color(active_theme.colors.danger_hover()),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .width(Fill),
                )
                .on_press(Message::SelectAccountForDeletion(account_id))
                .width(Fill)
                .height(42)
                .padding([4, 8])
                .style(move |framework_theme: &Theme, status| {
                    let mut style = button::text(framework_theme, status);
                    style.background =
                        matches!(status, button::Status::Hovered | button::Status::Pressed)
                            .then(|| Background::Color(active_theme.colors.hover()));
                    style.text_color = active_theme.colors.text();
                    style.border = Border {
                        color: active_theme.colors.border(0.12),
                        width: 1.0,
                        radius: 6.0.into(),
                    };
                    style.shadow = Shadow::default();
                    style
                })
                .into(),
            );
        }

        let heading = row![
            image(provider_logo_handle(
                provider_tab.provider,
                active_theme.colors.is_light,
            ))
            .width(20)
            .height(20)
            .content_fit(ContentFit::Contain),
            text(provider_tab.label)
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        provider_groups.push(
            column![heading, column(account_rows).spacing(2)]
                .spacing(5)
                .width(Fill)
                .into(),
        );
    }

    let list_height = if account_count == 0 {
        78.0
    } else {
        (account_count as f32 * 44.0 + provider_groups.len() as f32 * 28.0).clamp(140.0, 390.0)
    };
    let account_list: Element<'static, Message> = if provider_groups.is_empty() {
        container(
            text(locale::text(language, locale::Text::NoSavedAccounts))
                .size(typography::BODY_SIZE)
                .color(active_theme.colors.muted_text()),
        )
        .width(Fill)
        .height(Length::Fixed(list_height))
        .center(Fill)
        .into()
    } else {
        scrollable(column(provider_groups).spacing(10).width(Fill))
            .direction(iced::widget::scrollable::Direction::Vertical(
                iced::widget::scrollable::Scrollbar::hidden(),
            ))
            .height(Length::Fixed(list_height))
            .into()
    };

    container(
        column![
            text(locale::text(language, locale::Text::ManageAccounts))
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            text(locale::text(language, locale::Text::ChooseAccountToDelete))
                .size(typography::METADATA_SIZE)
                .color(active_theme.colors.muted_text()),
            account_list,
            row![
                Space::new().width(Fill).height(1),
                account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    true,
                    active_theme,
                    Message::DismissAccountDeleteDialog,
                ),
            ]
            .align_y(Alignment::Center)
            .width(Fill),
        ]
        .spacing(9)
        .width(Fill),
    )
    .width(380)
    .padding(15)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

fn account_deletion_confirmation_dialog(
    pending: &PendingAccountDeletion,
    deleting: bool,
    queued: bool,
    error: Option<&str>,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let account_identity = row![
        image(provider_logo_handle(
            pending.provider,
            active_theme.colors.is_light,
        ))
        .width(28)
        .height(28)
        .content_fit(ContentFit::Contain),
        column![
            text(pending.provider.display_name())
                .size(typography::METADATA_SIZE)
                .color(active_theme.colors.muted_text()),
            text(pending.display_name.clone())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            text(pending.email.clone())
                .size(typography::METADATA_SIZE)
                .color(active_theme.colors.muted_text()),
        ]
        .spacing(2)
        .width(Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .width(Fill);

    let error_message: Element<'static, Message> = error
        .map(|error| {
            column![
                text(locale::text(language, locale::Text::AccountDeletionFailed))
                    .size(typography::LABEL_SIZE)
                    .color(active_theme.colors.danger_hover()),
                text(error.to_owned())
                    .size(typography::METADATA_SIZE)
                    .color(active_theme.colors.muted_text()),
            ]
            .spacing(3)
            .width(Fill)
            .into()
        })
        .unwrap_or_else(|| Space::new().width(Fill).height(0).into());

    let refresh_notice: Element<'static, Message> = if queued {
        text(locale::text(language, locale::Text::WaitForUsageRefresh))
            .size(typography::METADATA_SIZE)
            .color(active_theme.colors.muted_text())
            .into()
    } else {
        Space::new().width(Fill).height(0).into()
    };

    container(
        column![
            text(locale::text(language, locale::Text::ConfirmAccountDeletion))
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(active_theme.colors.text()),
            account_identity,
            text(locale::text(language, locale::Text::AccountDeletionWarning))
                .size(typography::BODY_SIZE)
                .color(active_theme.colors.muted_text()),
            refresh_notice,
            error_message,
            row![
                account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    !deleting,
                    active_theme,
                    Message::CancelAccountDeletion,
                ),
                destructive_dialog_button(
                    locale::text(
                        language,
                        if deleting {
                            locale::Text::DeletingAccount
                        } else if queued {
                            locale::Text::Waiting
                        } else {
                            locale::Text::Delete
                        },
                    ),
                    !deleting && !queued,
                    active_theme,
                    Message::ConfirmAccountDeletion,
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill),
        ]
        .spacing(12)
        .width(Fill),
    )
    .width(372)
    .padding(16)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

fn destructive_dialog_button(
    label: &'static str,
    enabled: bool,
    active_theme: &'static ThemeDefinition,
    message: Message,
) -> Element<'static, Message> {
    const DANGER: Color = Color::from_rgb8(166, 48, 48);

    button(text(label).size(typography::CONTROL_SIZE))
        .on_press_maybe(enabled.then_some(message))
        .padding([7, 12])
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = Some(Background::Color(
                if enabled && matches!(status, button::Status::Hovered | button::Status::Pressed) {
                    active_theme.colors.danger_hover()
                } else if enabled {
                    DANGER
                } else {
                    active_theme.colors.control_surface()
                },
            ));
            style.text_color = if enabled {
                Color::WHITE
            } else {
                active_theme.colors.muted_text()
            };
            style.border = Border {
                color: active_theme.colors.border(0.24),
                width: 1.0,
                radius: 7.0.into(),
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

fn openrouter_credentials_dialog<'a>(
    api_key: &'a str,
    management_key: &'a str,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'a, Message> {
    let api_key_input = text_input(
        locale::text(language, locale::Text::OpenRouterApiKey),
        api_key,
    )
    .secure(true)
    .size(typography::CONTROL_SIZE)
    .padding([8, 10])
    .on_input(Message::OpenRouterApiKeyChanged)
    .width(Fill)
    .style(move |framework_theme, status| {
        account_key_input_style(framework_theme, status, active_theme)
    });

    let management_key_input = text_input(
        locale::text(language, locale::Text::OpenRouterManagementKey),
        management_key,
    )
    .secure(true)
    .size(typography::CONTROL_SIZE)
    .padding([8, 10])
    .on_input(Message::OpenRouterManagementKeyChanged)
    .width(Fill)
    .style(move |framework_theme, status| {
        account_key_input_style(framework_theme, status, active_theme)
    });

    let submit_enabled = !api_key.trim().is_empty();
    container(
        column![
            text(locale::text(language, locale::Text::OpenRouterTitle))
                .size(typography::ACCOUNT_NAME_SIZE)
                .color(active_theme.colors.text()),
            text(locale::text(
                language,
                locale::Text::OpenRouterCredentialHint
            ))
            .size(typography::METADATA_SIZE)
            .color(active_theme.colors.muted_text()),
            api_key_input,
            management_key_input,
            row![
                account_dialog_button(
                    locale::text(language, locale::Text::Cancel),
                    false,
                    true,
                    active_theme,
                    Message::CancelOpenRouterCredentials,
                ),
                account_dialog_button(
                    locale::text(language, locale::Text::AddAccount),
                    true,
                    submit_enabled,
                    active_theme,
                    Message::SubmitOpenRouterCredentials,
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill),
        ]
        .spacing(10)
        .width(Fill),
    )
    .width(344)
    .padding(16)
    .style(move |_| account_menu_surface_style(active_theme))
    .into()
}

fn account_key_input_style(
    framework_theme: &Theme,
    status: text_input::Status,
    active_theme: &'static ThemeDefinition,
) -> text_input::Style {
    let mut style = text_input::default(framework_theme, status);
    let border_color = match status {
        text_input::Status::Focused { .. } => active_theme.accent_color().scale_alpha(0.78),
        text_input::Status::Hovered => active_theme.colors.border(0.30),
        _ => active_theme.colors.border(0.18),
    };
    style.background = Background::Color(active_theme.colors.control_surface());
    style.border = Border {
        color: border_color,
        width: 1.0,
        radius: 7.0.into(),
    };
    style.icon = active_theme.colors.muted_text();
    style.placeholder = active_theme.colors.muted_text();
    style.value = active_theme.colors.text();
    style.selection = active_theme.accent_color().scale_alpha(0.42);
    style
}

fn account_dialog_button(
    label: &'static str,
    primary: bool,
    enabled: bool,
    active_theme: &'static ThemeDefinition,
    message: Message,
) -> Element<'static, Message> {
    button(text(label).size(typography::CONTROL_SIZE))
        .on_press_maybe(enabled.then_some(message))
        .padding([7, 12])
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = if primary && enabled {
                Some(Background::Color(active_theme.accent_color()))
            } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                Some(Background::Color(active_theme.colors.hover()))
            } else {
                Some(Background::Color(active_theme.colors.control_surface()))
            };
            style.text_color = if primary && enabled {
                Color::WHITE
            } else if enabled {
                active_theme.colors.text()
            } else {
                active_theme.colors.muted_text()
            };
            style.border = Border {
                color: active_theme.colors.border(0.24),
                width: 1.0,
                radius: 7.0.into(),
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

fn account_add_status_banner(
    status: &AccountAddStatus,
    active_theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let content: Element<'static, Message> = match status {
        AccountAddStatus::Running(provider) => row![
            text(locale::text(language, locale::Text::AccountAddRunning))
                .size(typography::LABEL_SIZE)
                .color(active_theme.colors.muted_text()),
            text(provider.display_name())
                .size(typography::LABEL_SIZE)
                .color(active_theme.colors.text()),
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into(),
        AccountAddStatus::Added(provider) => row![
            text(locale::text(language, locale::Text::AccountAdded))
                .size(typography::LABEL_SIZE)
                .color(active_theme.colors.muted_text()),
            text(provider.display_name())
                .size(typography::LABEL_SIZE)
                .color(active_theme.colors.text()),
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into(),
        AccountAddStatus::Failed(error) => column![
            text(locale::text(language, locale::Text::AccountAddFailed))
                .size(typography::LABEL_SIZE)
                .color(active_theme.colors.danger_hover()),
            text(error.clone())
                .size(typography::METADATA_SIZE)
                .color(active_theme.colors.muted_text()),
        ]
        .spacing(2)
        .width(Fill)
        .into(),
    };

    let close_button: Element<'static, Message> = if matches!(status, AccountAddStatus::Running(_))
    {
        button(text(locale::text(language, locale::Text::Cancel)).size(typography::METADATA_SIZE))
            .on_press(Message::CancelAccountAdd)
            .padding([3, 9])
            .style(move |framework_theme: &Theme, state| {
                let mut style = button::text(framework_theme, state);
                style.background = Some(Background::Color(
                    if matches!(state, button::Status::Hovered | button::Status::Pressed) {
                        active_theme.colors.hover()
                    } else {
                        active_theme.colors.control_surface()
                    },
                ));
                style.text_color = active_theme.colors.text();
                style.border = Border {
                    color: active_theme.colors.border(0.20),
                    width: 1.0,
                    radius: 6.0.into(),
                };
                style
            })
            .into()
    } else {
        button(container(icon_x().size(13)).center(Fill))
            .on_press(Message::DismissAccountAddStatus)
            .width(22)
            .height(22)
            .padding(0)
            .style(move |framework_theme: &Theme, state| {
                let mut style = button::text(framework_theme, state);
                style.background =
                    matches!(state, button::Status::Hovered | button::Status::Pressed)
                        .then(|| Background::Color(active_theme.colors.hover()));
                style.text_color = active_theme.colors.muted_text();
                style.border = Border {
                    radius: 6.0.into(),
                    ..Border::default()
                };
                style
            })
            .into()
    };

    container(
        row![content, close_button]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([6, 12])
    .style(move |_| container::Style {
        background: Some(Background::Color(active_theme.colors.control_surface())),
        border: Border {
            color: active_theme.colors.border(0.20),
            width: 1.0,
            ..Border::default()
        },
        ..Default::default()
    })
    .into()
}

fn account_menu_surface_style(active_theme: &'static ThemeDefinition) -> container::Style {
    container::Style {
        background: Some(Background::Color(active_theme.colors.window_surface())),
        border: Border {
            color: active_theme.colors.border(0.28),
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        ..Default::default()
    }
}

fn decode_provider_logo(bytes: &[u8], black_foreground: bool) -> image::Handle {
    const MAX_LOGO_DIMENSION: u32 = 192;

    let decoded = ::image::load_from_memory(bytes)
        .expect("embedded provider logo must be a valid image")
        .to_rgba8();
    let max_dimension = decoded.width().max(decoded.height());
    let mut pixels = if max_dimension > MAX_LOGO_DIMENSION {
        let scale = MAX_LOGO_DIMENSION as f32 / max_dimension as f32;
        let width = (decoded.width() as f32 * scale).round().max(1.0) as u32;
        let height = (decoded.height() as f32 * scale).round().max(1.0) as u32;
        ::image::imageops::resize(
            &decoded,
            width,
            height,
            ::image::imageops::FilterType::Lanczos3,
        )
    } else {
        decoded
    };

    if black_foreground {
        for pixel in pixels.pixels_mut() {
            if pixel[3] > 0 {
                pixel[0] = 0;
                pixel[1] = 0;
                pixel[2] = 0;
            }
        }
    }

    image::Handle::from_rgba(pixels.width(), pixels.height(), pixels.into_raw())
}

fn theme_dropdown(current_theme: ThemeId, language: locale::Language) -> Element<'static, Message> {
    let mut items = THEME_MANIFEST
        .iter()
        .copied()
        .map(|theme| theme_choice_row(theme, current_theme))
        .collect::<Vec<_>>();

    items.push(
        container(
            text(locale::text(language, locale::Text::PercentDisplayTitle))
                .size(typography::METADATA_SIZE)
                .color(Color::from_rgba(1.0, 1.0, 1.0, 0.6)),
        )
        .padding([6, 9])
        .into(),
    );
    let current_display = percent_display::current();
    for (mode, label) in [
        (PercentDisplay::Remaining, locale::Text::ShowRemaining),
        (PercentDisplay::Used, locale::Text::ShowUsed),
    ] {
        items.push(percent_display_choice_row(
            mode,
            locale::text(language, label),
            mode == current_display,
        ));
    }

    container(column(items).spacing(2))
        .width(156)
        .padding(6)
        .style(theme_dropdown_surface_style)
        .into()
}

fn theme_choice_row(
    theme_choice: ThemeDefinition,
    current_theme: ThemeId,
) -> Element<'static, Message> {
    let selected = theme_choice.id == current_theme;
    let check: Element<'static, Message> = if selected {
        icon_check::<Theme>().size(15).color(Color::WHITE).into()
    } else {
        Space::new().width(16).height(15).into()
    };
    let swatch_color = theme_choice.swatch_color();

    let swatch = container(Space::new().width(Fill).height(Fill))
        .width(16)
        .height(16)
        .style(move |_| container::Style {
            background: Some(Background::Color(swatch_color)),
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.22),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        });

    button(
        row![
            swatch,
            text(theme_choice.label)
                .size(typography::LABEL_SIZE)
                .color(Color::WHITE)
                .width(Fill),
            check,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::SelectTheme(theme_choice.id))
    .width(Fill)
    .height(34)
    .padding([4, 9])
    .style(move |theme: &Theme, status| theme_menu_item_style(theme, selected, status))
    .into()
}

fn percent_display_choice_row(
    mode: PercentDisplay,
    label: &'static str,
    selected: bool,
) -> Element<'static, Message> {
    let check: Element<'static, Message> = if selected {
        icon_check::<Theme>().size(15).color(Color::WHITE).into()
    } else {
        Space::new().width(16).height(15).into()
    };

    button(
        row![
            text(label)
                .size(typography::LABEL_SIZE)
                .color(Color::WHITE)
                .width(Fill),
            check,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::SelectPercentDisplay(mode))
    .width(Fill)
    .height(34)
    .padding([4, 9])
    .style(move |theme: &Theme, status| theme_menu_item_style(theme, selected, status))
    .into()
}

fn theme_dropdown_surface_style(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::BLACK)),
        border: Border {
            radius: 14.0.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        ..Default::default()
    }
}

fn theme_menu_item_style(
    framework_theme: &Theme,
    selected: bool,
    status: button::Status,
) -> button::Style {
    let mut style = button::text(framework_theme, status);
    style.background =
        if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.14)))
        } else {
            None
        };
    style.text_color = Color::WHITE;
    style.border = Border {
        radius: 8.0.into(),
        ..Border::default()
    };
    style.shadow = Shadow::default();
    style
}

fn close_window_button(active_theme: &'static ThemeDefinition) -> Element<'static, Message> {
    button(container(icon_x().size(16)).center(Fill))
        .on_press(Message::CloseButton)
        .width(30)
        .height(29)
        .padding(0)
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            style.background =
                hovered.then(|| Background::Color(active_theme.colors.danger_hover()));
            style.text_color = if hovered {
                Color::WHITE
            } else {
                active_theme.colors.text()
            };
            style.border = Border {
                radius: 8.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        })
        .into()
}

fn window_frame_style(theme: &'static ThemeDefinition) -> container::Style {
    let surface = if theme.backdrop.is_some() {
        Color::TRANSPARENT
    } else {
        theme.colors.window_surface()
    };

    container::Style {
        background: Some(Background::Color(surface)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: WINDOW_FRAME_RADIUS.into(),
        },
        text_color: None,
        shadow: Shadow::default(),
        snap: false,
    }
}

fn window_frame_outline_style(theme: &'static ThemeDefinition) -> container::Style {
    container::Style {
        border: Border {
            color: if theme.colors.is_light {
                theme.colors.border(0.58)
            } else {
                Color::WHITE
            },
            width: WINDOW_FRAME_BORDER_WIDTH,
            radius: WINDOW_FRAME_RADIUS.into(),
        },
        ..Default::default()
    }
}

fn preview_log(message: impl std::fmt::Display) {
    if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_none() {
        return;
    }

    let path =
        std::env::temp_dir().join(format!("usage-ui-iced-preview-{}.log", std::process::id()));
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tray_icon::menu::dpi::{PhysicalPosition, PhysicalSize};

    #[test]
    fn tray_click_brings_a_buried_popup_forward_instead_of_hiding_it() {
        let now = Instant::now();
        assert!(!tray_click_should_hide(false, false, None, now));
        assert!(tray_click_should_hide(true, true, None, now));
        // Focus moved to the taskbar as the tray icon was pressed.
        assert!(tray_click_should_hide(
            true,
            false,
            Some(now - Duration::from_millis(120)),
            now
        ));
        // Open but buried behind other windows for a while.
        assert!(!tray_click_should_hide(
            true,
            false,
            Some(now - Duration::from_secs(5)),
            now
        ));
        assert!(!tray_click_should_hide(true, false, None, now));
    }

    #[test]
    fn rounded_backdrop_mask_clears_corners_and_antialiases_the_edge() {
        assert_eq!(
            rounded_rectangle_coverage(0.5, 0.5, 120.0, 180.0, 16.0),
            0.0
        );
        assert_eq!(
            rounded_rectangle_coverage(60.0, 90.0, 120.0, 180.0, 16.0),
            1.0
        );
        assert_eq!(
            rounded_rectangle_coverage(16.0, 0.0, 120.0, 180.0, 16.0),
            0.5
        );
    }

    #[test]
    fn refresh_icon_rotates_only_while_refreshing() {
        assert_eq!(advance_refresh_icon_rotation(0.7, false), 0.0);

        let first_tick = advance_refresh_icon_rotation(0.0, true);
        let second_tick = advance_refresh_icon_rotation(first_tick, true);
        assert!(first_tick > 0.0);
        assert!(second_tick > first_tick);
        assert!(
            advance_refresh_icon_rotation(std::f32::consts::TAU - 0.01, true)
                < std::f32::consts::TAU
        );
    }

    #[test]
    fn popup_animation_ticks_pause_while_a_full_dialog_covers_the_content() {
        assert!(should_run_popup_animation_ticks(true, false, true, false));
        assert!(should_run_popup_animation_ticks(true, false, false, true));
        assert!(!should_run_popup_animation_ticks(true, true, true, false));
        assert!(!should_run_popup_animation_ticks(true, true, false, true));
        assert!(!should_run_popup_animation_ticks(false, false, true, false));
    }

    #[test]
    fn account_add_uses_the_existing_cli_provider_names() {
        assert_eq!(UsageProvider::Codex.cli_name(), "codex");
        assert_eq!(UsageProvider::Claude.cli_name(), "claude");
        assert_eq!(UsageProvider::Antigravity.cli_name(), "antigravity");
        assert_eq!(UsageProvider::OpenCodeGo.cli_name(), "opencode-go");
        assert_eq!(UsageProvider::OpenRouter.cli_name(), "openrouter");
    }

    #[test]
    fn account_deletion_requires_selection_and_returns_to_picker_on_cancel() {
        let (sender, _receiver) = async_channel::bounded(4);
        let mut app = App::new(sender);
        let account = codex_usage_core::accounts::AccountRecord::create(
            "Codex account",
            "codex@example.com",
            None,
            "openai",
            None,
        )
        .unwrap();
        let account_id = account.id;
        app.dashboard
            .set_accounts(vec![dashboard::AccountUsageEntry {
                account,
                snapshot: None,
            }]);

        let _ = app.update(Message::ToggleAccountDeleteDialog);
        assert!(app.account_delete_dialog_open);

        let _ = app.update(Message::ConfirmAccountDeletion);
        assert!(!app.account_delete_running);

        let _ = app.update(Message::SelectAccountForDeletion(account_id));
        assert_eq!(
            app.pending_account_deletion
                .as_ref()
                .map(|pending| pending.provider),
            Some(UsageProvider::Codex)
        );

        let _ = app.update(Message::CancelAccountDeletion);
        assert!(app.account_delete_dialog_open);
        assert!(app.pending_account_deletion.is_none());
        assert!(!app.account_delete_running);
    }

    #[test]
    fn account_deletion_picker_can_open_while_usage_refreshes() {
        let (sender, _receiver) = async_channel::bounded(4);
        let mut app = App::new(sender);
        app.dashboard_refresh_running = true;

        let _ = app.update(Message::ToggleAccountDeleteDialog);

        assert!(app.account_delete_dialog_open);
    }

    #[test]
    fn account_deletion_waits_for_usage_refresh_then_starts() {
        let (sender, _receiver) = async_channel::bounded(4);
        let mut app = App::new(sender);
        let account = codex_usage_core::accounts::AccountRecord::create(
            "Codex account",
            "codex@example.com",
            None,
            "openai",
            None,
        )
        .unwrap();
        let account_id = account.id;
        app.dashboard
            .set_accounts(vec![dashboard::AccountUsageEntry {
                account,
                snapshot: None,
            }]);

        let _ = app.update(Message::ToggleAccountDeleteDialog);
        let _ = app.update(Message::SelectAccountForDeletion(account_id));
        app.dashboard_refresh_running = true;
        let _ = app.update(Message::ConfirmAccountDeletion);

        assert!(!app.account_delete_running);
        assert!(app.account_delete_queued);
        assert!(app.pending_account_deletion.is_some());

        let _ = app.update(Message::OtherProvidersUsageRefreshEvent(
            usage_refresh::RefreshEvent::Failed("refresh failed".to_owned()),
        ));

        assert!(!app.dashboard_refresh_running);
        assert!(!app.account_delete_queued);
        assert!(app.account_delete_running);
    }

    #[test]
    fn queued_account_deletion_can_be_canceled() {
        let (sender, _receiver) = async_channel::bounded(4);
        let mut app = App::new(sender);
        let account = codex_usage_core::accounts::AccountRecord::create(
            "Codex account",
            "codex@example.com",
            None,
            "openai",
            None,
        )
        .unwrap();
        let account_id = account.id;
        app.dashboard
            .set_accounts(vec![dashboard::AccountUsageEntry {
                account,
                snapshot: None,
            }]);

        let _ = app.update(Message::ToggleAccountDeleteDialog);
        let _ = app.update(Message::SelectAccountForDeletion(account_id));
        app.dashboard_refresh_running = true;
        let _ = app.update(Message::ConfirmAccountDeletion);
        let _ = app.update(Message::CancelAccountDeletion);

        assert!(!app.account_delete_queued);
        assert!(app.pending_account_deletion.is_none());
    }

    #[test]
    fn account_add_worker_provides_the_tokio_runtime_needed_by_child_processes() {
        let receiver = spawn_account_add_worker(|| async {
            tokio::runtime::Handle::try_current()
                .map(|_| ())
                .map_err(|error| format!("Tokio runtime is unavailable: {error}"))
        })
        .unwrap();

        assert!(receiver.recv_blocking().unwrap().is_ok());
    }

    #[test]
    fn account_add_failure_extracts_cli_message_and_redacts_credentials() {
        let detail = account_add_failure_detail(
            br#"{"schema_version":1,"error":{"code":"account_add_failed","message":"login rejected"}}"#,
            b"diagnostic output",
        );
        assert!(detail.contains("login rejected"));
        assert!(detail.contains("diagnostic output"));

        let safe = redact_and_limit_account_add_error(
            "provider echoed api-secret and admin-secret".to_owned(),
            &["api-secret".to_owned(), "admin-secret".to_owned()],
        );
        assert_eq!(safe, "provider echoed [hidden] and [hidden]");
        assert!(!safe.contains("api-secret"));
        assert!(!safe.contains("admin-secret"));
    }

    #[test]
    fn tray_click_uses_the_actual_window_mode_when_cached_visibility_is_stale() {
        let (sender, _receiver) = async_channel::bounded(4);
        let mut app = App::new(sender);
        let tray_rect = tray_icon::Rect {
            position: PhysicalPosition::new(900.0, 700.0),
            size: PhysicalSize::new(24, 24),
        };

        // The preview startup path can leave the cached state saying "open"
        // even when the native window is still hidden. A tray click must show
        // that window instead of consuming the click to hide it.
        app.popup_visible = true;
        let _open_task = app.toggle_popup_from_tray(tray_rect, window::Mode::Hidden);
        assert!(app.popup_visible);

        // Conversely, a stale cached "closed" value must not prevent a click
        // from closing a native window that is actually visible and in front.
        app.popup_visible = false;
        app.window_focused = true;
        let _close_task = app.toggle_popup_from_tray(tray_rect, window::Mode::Windowed);
        assert!(!app.popup_visible);

        // A visible window buried behind other windows is brought forward.
        app.window_focused = false;
        let _raise_task = app.toggle_popup_from_tray(tray_rect, window::Mode::Windowed);
        assert!(app.popup_visible);
    }

    #[test]
    fn popup_stays_inside_work_area_above_bottom_taskbar() {
        let position = popup_position(
            tray_icon::Rect {
                position: PhysicalPosition::new(1608.0, 1200.0),
                size: PhysicalSize::new(48, 87),
            },
            1.5,
            PhysicalWorkArea {
                left: 0.0,
                top: 0.0,
                right: 1920.0,
                bottom: 1102.0,
            },
        );

        assert!((position.x - 856.0).abs() < 0.01);
        assert!((position.y - (1102.0 - WINDOW_HEIGHT * 1.5) / 1.5).abs() < 0.01);
        assert!(position.x + WINDOW_WIDTH <= 1920.0 / 1.5);
        assert!(position.y + WINDOW_HEIGHT <= 1102.0 / 1.5);
    }

    #[test]
    fn auto_hide_taskbar_is_reserved_even_when_monitor_work_area_includes_it() {
        let monitor = PhysicalWorkArea {
            left: 0.0,
            top: 0.0,
            right: 1920.0,
            bottom: 1200.0,
        };
        let mut work_area = monitor;

        reserve_auto_hide_bar(&mut work_area, monitor, MonitorEdge::Bottom, 98.0);
        assert_eq!(work_area.bottom, 1102.0);

        reserve_auto_hide_bar(&mut work_area, monitor, MonitorEdge::Bottom, 98.0);
        assert_eq!(work_area.bottom, 1102.0);
    }

    #[test]
    fn popup_flips_below_top_taskbar_and_clamps_to_work_area() {
        let position = popup_position(
            tray_icon::Rect {
                position: PhysicalPosition::new(1900.0, 0.0),
                size: PhysicalSize::new(24, 24),
            },
            1.5,
            PhysicalWorkArea {
                left: 0.0,
                top: 87.0,
                right: 1920.0,
                bottom: 1200.0,
            },
        );

        assert!(position.x + WINDOW_WIDTH <= 1920.0 / 1.5);
        assert!(position.y >= 87.0 / 1.5);
        assert!(position.y + WINDOW_HEIGHT <= 1200.0 / 1.5);
    }
}
