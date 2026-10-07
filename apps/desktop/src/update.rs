//! Handles every app message.

use super::*;

impl App {
    pub(super) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::DashboardLoaded(Ok(accounts)) => {
                let no_accounts = accounts.is_empty();
                self.dashboard.set_accounts(accounts);
                // The welcome is for a first run only: people who already
                // have accounts never see it.
                if !self.welcome_checked {
                    self.welcome_checked = true;
                    if !welcome::is_done() {
                        if no_accounts {
                            self.welcome = Some(welcome::Welcome::new());
                            if !self.popup_visible {
                                return self.show_window(None);
                            }
                        } else {
                            welcome::mark_done();
                        }
                    }
                }
                Task::none()
            }
            Message::WelcomeToggleProvider(provider) => {
                if let Some(welcome) = self.welcome.as_mut() {
                    welcome.toggle(provider);
                }
                Task::none()
            }
            Message::WelcomeOpenStep(index) => {
                if let Some(welcome) = self.welcome.as_mut()
                    && index < welcome.chosen.len()
                    && !self.account_add_running
                {
                    welcome.open(index);
                    self.account_add_status = None;
                }
                Task::none()
            }
            Message::WelcomeBack => {
                if let Some(welcome) = self.welcome.as_mut()
                    && !self.account_add_running
                {
                    welcome.step = welcome::Step::Choose;
                    self.account_add_status = None;
                }
                Task::none()
            }
            Message::WelcomeFinish => {
                if self.account_add_running {
                    return Task::none();
                }
                if let Some(welcome) = self.welcome.take() {
                    if let Some(first) = welcome.chosen.first() {
                        self.tab_layout.show_only_providers(&welcome.chosen);
                        self.save_tab_layout();
                        self.selected_tab = self.tab_layout.resolve(DashboardTab::Provider(*first));
                    }
                    welcome::mark_done();
                }
                self.account_add_status = None;
                Task::none()
            }
            Message::WelcomePreview(at_accounts) => {
                let mut welcome = welcome::Welcome::new();
                if at_accounts {
                    for provider in [
                        UsageProvider::Codex,
                        UsageProvider::Cursor,
                        UsageProvider::Xai,
                    ] {
                        welcome.toggle(provider);
                    }
                    welcome.open(1);
                } else {
                    welcome.toggle(UsageProvider::Codex);
                    welcome.toggle(UsageProvider::Claude);
                }
                self.welcome_checked = true;
                self.welcome = Some(welcome);
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
                if self
                    .dashboard
                    .move_account(self.selected_tab, account_id, offset)
                    && let Err(error) = self.dashboard.save_account_lists()
                {
                    preview_log(format!("account order save failed: {error}"));
                }
                Task::none()
            }
            Message::ToggleFavorite(account_id) => {
                self.dashboard.toggle_favorite(account_id);
                if let Err(error) = self.dashboard.save_account_lists() {
                    preview_log(format!("favorite accounts save failed: {error}"));
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
            Message::UsageRefreshEvent(event) => match event {
                usage_refresh::RefreshEvent::AccountUpdated(entry) => {
                    self.dashboard.update_account_usage(*entry);
                    Task::none()
                }
                usage_refresh::RefreshEvent::Finished(refresh) => {
                    preview_log(format!(
                        "usage refresh: attempted={}, updated={}, not_updated={}",
                        refresh.attempted, refresh.updated, refresh.not_updated
                    ));
                    self.finish_dashboard_refresh()
                }
                usage_refresh::RefreshEvent::Failed(error) => {
                    preview_log(format!("usage refresh failed: {error}"));
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
                    return self.show_window(Some(rect));
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
                let rect = TRAY_ICON.with(|tray| tray.borrow().as_ref().and_then(TrayIcon::rect));
                self.show_window(rect)
            }
            Message::RuntimeEvent(Event::Window(event)) => match event {
                window::Event::CloseRequested => {
                    preview_log("window close requested");
                    self.theme_menu_open = false;
                    self.account_add_menu_open = false;
                    self.dismiss_account_delete_dialog();
                    self.cancel_credentials();
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
            })) if self.tab_manager_open => {
                if self.tab_editor.is_some() {
                    self.tab_editor = None;
                } else {
                    self.close_tab_manager();
                }
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.credentials_provider.is_some() => {
                self.cancel_credentials();
                Task::none()
            }
            Message::RuntimeEvent(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })) if self.device_sign_in.is_some() => self.update(Message::CancelAccountAdd),
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
                self.cancel_credentials();
                self.dashboard.close_any_model_visibility_menu();
                self.hide_popup()
            }
            message @ (Message::OpenCustomTheme
            | Message::CloseCustomTheme
            | Message::CustomThemeLight(_)
            | Message::CustomThemeBackground(_)
            | Message::CustomThemeAccent(_)
            | Message::CustomThemeBackgroundInput(_)
            | Message::CustomThemeAccentInput(_)
            | Message::CustomThemeDim(_)
            | Message::ChooseCustomImage
            | Message::CustomImageChosen(_)
            | Message::RemoveCustomImage) => self.update_custom_theme(message),
            message @ (Message::ToggleTabManager
            | Message::DismissTabManager
            | Message::ToggleTabVisible(_)
            | Message::SetTabPercentDisplay(..)
            | Message::MoveTab(..)
            | Message::NewCustomTab
            | Message::EditCustomTab(_)
            | Message::DeleteCustomTab(_)
            | Message::TabEditorNameChanged(_)
            | Message::TabEditorToggleProvider(_)
            | Message::TabEditorToggleAccount(..)
            | Message::SaveTabEditor
            | Message::CancelTabEditor
            | Message::ChooseTabIcon
            | Message::TabIconChosen(_)
            | Message::RemoveTabIcon) => self.update_tabs(message),
            Message::SelectTab(tab) => {
                self.selected_tab = tab;
                self.theme_menu_open = false;
                self.account_add_menu_open = false;
                self.dismiss_account_delete_dialog();
                self.dashboard.close_any_model_visibility_menu();
                self.dashboard.clear_any_hovered_account_name();
                if tab == DashboardTab::Cost {
                    self.cost.scan_if_due()
                } else {
                    Task::none()
                }
            }
            Message::CostScanned(result) => {
                self.cost.finish(*result);
                Task::none()
            }
            Message::CostView(change) => {
                self.cost.change(change);
                Task::none()
            }
            Message::RefreshAllUsage => {
                let usage = self.start_usage_refresh(usage_refresh::RefreshTrigger::Manual);
                if self.selected_tab == DashboardTab::Cost {
                    Task::batch([usage, self.cost.scan()])
                } else {
                    usage
                }
            }
            Message::UsageAnimationTick => {
                self.dashboard.advance_usage_animation(Instant::now());
                Task::none()
            }
            Message::ResetClockTick => {
                // A window reset while the popup was open: show it as unused
                // right away and fetch the provider's new reading.
                let usage = if self.dashboard.clear_elapsed_resets() {
                    self.start_usage_refresh(usage_refresh::RefreshTrigger::Automatic)
                } else {
                    Task::none()
                };
                // An open Cost tab keeps up with the logs.
                if self.selected_tab == DashboardTab::Cost {
                    Task::batch([usage, self.cost.scan_if_due()])
                } else {
                    usage
                }
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
                    && self.credentials_provider.is_none()
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
                    || self.credentials_provider.is_some()
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
                    display_name: dashboard::account_name(&entry.account),
                    email: if dashboard::name_is_email(&entry.account) {
                        String::new()
                    } else {
                        dashboard::shown_email(&entry.account.email).to_owned()
                    },
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
                // Cursor takes the signed-in Cursor app's session; without
                // one, the user pastes a session cookie from cursor.com.
                let needs_credentials = provider.uses_api_key()
                    || (provider == UsageProvider::Cursor
                        && usage_monitor_core::providers::cursor::local_app_session().is_none());
                if needs_credentials {
                    self.api_key_input.clear();
                    self.management_key_input.clear();
                    self.credentials_provider = Some(provider);
                    self.account_add_status = None;
                    Task::none()
                } else {
                    self.begin_account_add(provider, None)
                }
            }
            Message::ApiKeyChanged(value) => {
                self.api_key_input = value;
                Task::none()
            }
            Message::ManagementKeyChanged(value) => {
                self.management_key_input = value;
                Task::none()
            }
            Message::SubmitCredentials => {
                let api_key = self.api_key_input.trim().to_owned();
                if api_key.is_empty() || self.account_add_running {
                    return Task::none();
                }
                let Some(provider) = self.credentials_provider else {
                    return Task::none();
                };
                let management_key = self.management_key_input.trim().to_owned();
                if provider == UsageProvider::Xai && management_key.is_empty() {
                    return Task::none();
                }
                self.api_key_input.clear();
                self.management_key_input.clear();
                self.credentials_provider = None;
                self.begin_account_add(provider, Some((api_key, management_key)))
            }
            Message::DeviceSignInCode(code) => {
                // A code arriving after a cancel belongs to no sign-in.
                if !self.account_add_running {
                    self.device_sign_in = None;
                    return Task::none();
                }
                // As CodexBar does, the code is copied, ready to paste on the
                // provider's page.
                if let Some(code) = &code {
                    copy_to_clipboard(&code.user_code);
                }
                self.device_sign_in = code;
                Task::none()
            }
            Message::CopyDeviceCode => {
                if let Some(code) = &self.device_sign_in {
                    copy_to_clipboard(&code.user_code);
                }
                Task::none()
            }
            Message::OpenDeviceCodePage => {
                if let Some(code) = &self.device_sign_in {
                    copy_to_clipboard(&code.user_code);
                    open_in_browser(&code.verification_uri);
                }
                Task::none()
            }
            Message::OpenCursorSite => {
                open_in_browser("https://cursor.com/dashboard");
                Task::none()
            }
            Message::OpenMiMoSite => {
                open_in_browser("https://platform.xiaomimimo.com/#/console/balance");
                Task::none()
            }
            Message::CancelCredentials => {
                self.cancel_credentials();
                Task::none()
            }
            Message::AccountAddCompleted(provider, result) => {
                self.account_add_running = false;
                self.account_add_cancel = None;
                self.device_sign_in = None;
                match result {
                    Err(error) if error == ACCOUNT_ADD_CANCELLED => {
                        self.account_add_status = None;
                        Task::none()
                    }
                    Ok(()) => {
                        self.account_add_status = Some(AccountAddStatus::Added(provider));
                        // Show the new account even if its tab was hidden.
                        if self.tab_layout.tab_showing(provider).is_none() {
                            self.tab_layout.show_provider(provider);
                            self.save_tab_layout();
                        }
                        if let Some(tab) = self.tab_layout.tab_showing(provider) {
                            self.selected_tab = tab;
                        }
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
                self.device_sign_in = None;
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
            Message::SwitchAntigravityAppAccount(account_id) => {
                if !self.dashboard.begin_codex_switch(account_id) {
                    return Task::none();
                }
                Task::perform(
                    codex_switch::switch_antigravity_app_account(account_id),
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
            Message::SetUiZoom(zoom) => {
                self.ui_zoom = zoom;
                Task::none()
            }
            Message::SetMemorySaver(enabled) => {
                self.memory_saver = enabled;
                if let Err(error) = memory_saver::save(enabled) {
                    preview_log(format!("memory saver preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SetShowAccountDetails(shown) => {
                display_options::set_show_account_details(shown);
                if let Err(error) = display_options::save_show_account_details(shown) {
                    preview_log(format!("account details preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SetShadeResetTimes(shaded) => {
                display_options::set_shade_reset_times(shaded);
                if let Err(error) = display_options::save_shade_reset_times(shaded) {
                    preview_log(format!("reset shade preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SetShowInTaskbar(shown) => {
                display_options::set_show_in_taskbar(shown);
                if let Err(error) = display_options::save_show_in_taskbar(shown) {
                    preview_log(format!("taskbar preference save failed: {error}"));
                }
                // The taskbar button is set when the window is made, so the
                // open window is replaced by one made with the new choice.
                self.theme_menu_open = false;
                match self.window_id.take() {
                    Some(window_id) if self.popup_visible => {
                        window::close::<Message>(window_id).chain(self.show_window(None))
                    }
                    Some(window_id) => window::close(window_id),
                    None => Task::none(),
                }
            }
            Message::SetShowTeamBudgets(shown) => {
                display_options::set_show_team_budgets(shown);
                if let Err(error) = display_options::save_show_team_budgets(shown) {
                    preview_log(format!("team budgets preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SelectResetCredits(mode) => {
                display_options::set_reset_credits(mode);
                if let Err(error) = display_options::save_reset_credits(mode) {
                    preview_log(format!("reset credits preference save failed: {error}"));
                }
                Task::none()
            }
            Message::SelectPercentDisplay(mode) => {
                percent_display::set_default(mode);
                self.theme_menu_open = false;
                if let Err(error) = percent_display::save(mode) {
                    preview_log(format!("percent display preference save failed: {error}"));
                }
                Task::none()
            }
        }
    }
}
