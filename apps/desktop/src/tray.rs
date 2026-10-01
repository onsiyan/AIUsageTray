//! Tray icon events, the refresh-icon animation clock, and popup placement.

use super::*;

/// Pressing the tray icon takes focus from the popup just before the click
/// arrives, so focus lost this recently still counts as "in front".
pub(super) const TRAY_CLICK_FOCUS_GRACE: Duration = Duration::from_millis(500);

/// A tray click hides the popup only when it is visible and in front. A popup
/// left open behind other windows is brought forward instead, so one click
/// always shows it.
pub(super) fn tray_click_should_hide(
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

pub(super) fn install_tray(sender: Sender<TrayIconEvent>) -> Result<(), String> {
    let icon = Icon::from_rgba(icon_pixels(), 32, 32).map_err(|error| error.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Usage Monitor")
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

pub(super) fn tray_event_stream() -> impl Stream<Item = Message> {
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

pub(super) fn refresh_icon_tick_stream() -> impl Stream<Item = Message> {
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

pub(super) fn advance_refresh_icon_rotation(rotation: f32, refreshing: bool) -> f32 {
    if !refreshing {
        return 0.0;
    }

    (rotation + 15.0_f32.to_radians()).rem_euclid(std::f32::consts::TAU)
}

pub(super) fn should_run_popup_animation_ticks(
    popup_visible: bool,
    blocking_dialog_open: bool,
    refreshing: bool,
    usage_animation_active: bool,
) -> bool {
    popup_visible && !blocking_dialog_open && (refreshing || usage_animation_active)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PhysicalWorkArea {
    pub(super) left: f32,
    pub(super) top: f32,
    pub(super) right: f32,
    pub(super) bottom: f32,
}

#[derive(Clone, Copy)]
pub(super) enum MonitorEdge {
    Left,
    Top,
    Right,
    Bottom,
}

pub(super) fn reserve_auto_hide_bar(
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

pub(super) fn full_monitor_work_area(monitor: Size, scale_factor: f32) -> PhysicalWorkArea {
    PhysicalWorkArea {
        left: 0.0,
        top: 0.0,
        right: monitor.width * scale_factor,
        bottom: monitor.height * scale_factor,
    }
}

#[cfg(target_os = "windows")]
pub(super) fn monitor_work_area(rect: tray_icon::Rect) -> Option<PhysicalWorkArea> {
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
pub(super) fn monitor_work_area(_: tray_icon::Rect) -> Option<PhysicalWorkArea> {
    None
}

pub(super) fn popup_position(
    rect: tray_icon::Rect,
    scale_factor: f32,
    work_area: PhysicalWorkArea,
) -> Point {
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
