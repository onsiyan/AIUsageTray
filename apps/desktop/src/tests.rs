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
fn popup_animation_ticks_run_only_for_visible_usage_animations() {
    assert!(should_run_popup_animation_ticks(true, false, true));
    assert!(!should_run_popup_animation_ticks(true, false, false));
    assert!(!should_run_popup_animation_ticks(true, true, true));
    assert!(!should_run_popup_animation_ticks(false, false, true));
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
    let mut app = App::new();
    let account = usage_monitor_core::accounts::AccountRecord::create(
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
    let mut app = App::new();
    app.dashboard_refresh_running = true;

    let _ = app.update(Message::ToggleAccountDeleteDialog);

    assert!(app.account_delete_dialog_open);
}

#[test]
fn account_deletion_waits_for_usage_refresh_then_starts() {
    let mut app = App::new();
    let account = usage_monitor_core::accounts::AccountRecord::create(
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

    let _ = app.update(Message::UsageRefreshEvent(
        usage_refresh::RefreshEvent::Failed("refresh failed".to_owned()),
    ));

    assert!(!app.dashboard_refresh_running);
    assert!(!app.account_delete_queued);
    assert!(app.account_delete_running);
}

#[test]
fn queued_account_deletion_can_be_canceled() {
    let mut app = App::new();
    let account = usage_monitor_core::accounts::AccountRecord::create(
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
    let mut app = App::new();
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
        1.0,
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
fn popup_grows_on_large_screens_and_keeps_its_size_on_a_laptop() {
    // 14" laptop, 1920x1200 at 150%: 735 units of usable height.
    assert_eq!(popup_zoom(735.0), 1.0);
    // 27" 1440p at 100%.
    assert_eq!(popup_zoom(1392.0), 1.45);
    // 27" 1080p at 100%.
    assert_eq!(popup_zoom(1032.0), 1.05);
    // 4K at 100% would be huge; capped.
    assert_eq!(popup_zoom(2112.0), 2.0);
    assert_eq!(popup_zoom(0.0), 1.0);
    assert_eq!(popup_zoom(f32::NAN), 1.0);
}

#[test]
fn enlarged_popup_still_fits_its_screen() {
    let work_area = PhysicalWorkArea {
        left: 0.0,
        top: 0.0,
        right: 2560.0,
        bottom: 1392.0,
    };
    let zoom = popup_zoom(1392.0);
    let position = popup_position(
        tray_icon::Rect {
            position: PhysicalPosition::new(2400.0, 1400.0),
            size: PhysicalSize::new(24, 40),
        },
        1.0,
        zoom,
        work_area,
    );

    assert!(position.x + WINDOW_WIDTH * zoom <= 2560.0);
    assert!(position.y >= 0.0);
    assert!(position.y + WINDOW_HEIGHT * zoom <= 1392.0);
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
        1.0,
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

#[test]
fn memory_saver_closes_the_hidden_popup_window() {
    let mut app = App::new();
    app.memory_saver = false;
    app.window_id = Some(window::Id::unique());
    app.popup_visible = true;
    let _ = app.hide_popup();
    assert!(
        app.window_id.is_some(),
        "by default the window is kept for an instant reopen"
    );

    app.memory_saver = true;
    app.popup_visible = true;
    let _ = app.hide_popup();
    assert!(
        app.window_id.is_none(),
        "the memory saver closes the window"
    );
}

#[test]
fn tray_icon_is_the_app_icon_at_32_pixels() {
    let pixels = icon_pixels();
    assert_eq!(pixels.len(), 32 * 32 * 4);
    assert!(pixels.chunks(4).any(|pixel| pixel[3] == 255));
}

#[test]
fn the_window_has_the_app_icon() {
    assert!(crate::graphics::window_icon().is_some());
}
