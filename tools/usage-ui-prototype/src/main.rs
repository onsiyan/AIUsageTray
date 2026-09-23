slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let window = UsageWindow::new()?;
    let tray = UsageTray::new()?;

    let weak_window = window.as_weak();
    tray.on_toggle_window(move || {
        if let Some(window) = weak_window.upgrade() {
            if window.window().is_visible() {
                let _ = window.hide();
            } else {
                let _ = window.show();
            }
        }
    });

    let weak_window = window.as_weak();
    tray.on_show_window(move || {
        if let Some(window) = weak_window.upgrade() {
            let _ = window.show();
        }
    });

    let weak_window = window.as_weak();
    window.on_dismiss_requested(move || {
        if let Some(window) = weak_window.upgrade() {
            let _ = window.hide();
        }
    });

    let weak_window = window.as_weak();
    window.on_refresh_requested(move || {
        if let Some(window) = weak_window.upgrade() {
            window.set_status_message("تحديث تجريبي اكتمل الآن — لا يوجد اتصال بالمزودين".into());
        }
    });

    let weak_window = window.as_weak();
    window.on_add_account_requested(move || {
        if let Some(window) = weak_window.upgrade() {
            window.set_status_message("إضافة الحساب غير مفعّلة في هذه المعاينة".into());
        }
    });

    tray.on_quit_app(|| {
        let _ = slint::quit_event_loop();
    });

    window
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    window.show()?;
    tray.show()?;
    slint::run_event_loop()
}
