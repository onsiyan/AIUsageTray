#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{
    Manager, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_positioner::{Position, WindowExt};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open usage preview", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            let tray = TrayIconBuilder::new()
                .icon(tray_icon())
                .tooltip("Usage Monitor preview")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        toggle_popup(tray.app_handle());
                    }
                })
                .build(app)?;

            // Lets this isolated preview be opened for a screenshot without
            // changing the normal tray-first startup behavior.
            if std::env::var_os("USAGE_UI_PREVIEW_OPEN_ON_START").is_some() {
                if let Ok(Some(rect)) = tray.rect() {
                    let event = TrayIconEvent::Click {
                        id: tray.id().clone(),
                        position: rect.position.to_physical::<f64>(1.0),
                        rect,
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                    };
                    tauri_plugin_positioner::on_tray_event(app.handle(), &event);
                    show_popup(app.handle());
                }
            }

            app.on_menu_event(|app, event| match event.id().as_ref() {
                "open" => show_popup(app),
                "quit" => app.exit(0),
                _ => {}
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![hide_popup])
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::Focused(false)) {
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to run Usage Monitor Tauri preview");
}

#[tauri::command]
fn hide_popup(window: tauri::WebviewWindow) {
    let _ = window.hide();
}

fn toggle_popup(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            show_popup(app);
        }
    }
}

fn show_popup(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.move_window(Position::TrayBottomCenter);
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn tray_icon() -> tauri::image::Image<'static> {
    let size = 32_u32;
    let mut pixels = vec![0_u8; (size * size * 4) as usize];

    for y in 2..30 {
        for x in 2..30 {
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            if dx * dx + dy * dy <= 14 * 14 {
                let index = ((y * size + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&[120, 111, 245, 255]);
            }
        }
    }

    // A small white center distinguishes the prototype icon from the final app mark.
    for y in 11..21 {
        for x in 11..21 {
            let index = ((y * size + x) * 4) as usize;
            pixels[index..index + 4].copy_from_slice(&[245, 246, 255, 255]);
        }
    }

    tauri::image::Image::new_owned(pixels, size, size)
}
