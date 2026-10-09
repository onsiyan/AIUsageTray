//! Tray icon events, the usage-animation and reset clocks, and popup placement.

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
    let menu = crate::tray_menu::build(crate::locale::default_language())?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("AI Usage Tray")
        // A left click opens the popup; the menu is for the right button.
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
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

/// A point at the bottom-right screen edge, used when the tray icon cannot
/// report where it is.
pub(super) fn taskbar_edge_anchor(monitor_size: Size, scale_factor: f32) -> tray_icon::Rect {
    tray_icon::Rect {
        position: tray_icon::menu::dpi::PhysicalPosition::new(
            (monitor_size.width * scale_factor - 12.0) as f64,
            (monitor_size.height * scale_factor) as f64,
        ),
        size: tray_icon::menu::dpi::PhysicalSize::new(24, 0),
    }
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

/// Frames for usage bars easing to new values. The refresh icon spins by
/// itself (see `spinner`) and needs no ticks.
pub(super) fn usage_animation_tick_stream() -> impl Stream<Item = Message> {
    tick_stream(USAGE_ANIMATION_TICK, || Message::UsageAnimationTick)
}

/// Re-renders reset countdowns and notices resets that pass while the popup
/// is open.
pub(super) fn reset_clock_stream() -> impl Stream<Item = Message> {
    tick_stream(RESET_CLOCK_TICK, || Message::ResetClockTick)
}

fn tick_stream(period: Duration, message: fn() -> Message) -> impl Stream<Item = Message> {
    let (sender, receiver) = async_channel::bounded::<()>(1);
    thread::spawn(move || {
        loop {
            thread::sleep(period);
            if sender.send_blocking(()).is_err() {
                break;
            }
        }
    });

    iced::futures::stream::unfold(receiver, move |receiver| async move {
        receiver.recv().await.ok().map(|()| (message(), receiver))
    })
}

pub(super) fn should_run_popup_animation_ticks(
    popup_visible: bool,
    blocking_dialog_open: bool,
    usage_animation_active: bool,
) -> bool {
    popup_visible && !blocking_dialog_open && usage_animation_active
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

/// The display scaling (1.0 = 100%) of the screen the tray icon is on.
#[cfg(target_os = "windows")]
pub(super) fn monitor_scale_factor(rect: tray_icon::Rect) -> Option<f32> {
    use windows_sys::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
        UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    };

    let center = POINT {
        x: (rect.position.x + f64::from(rect.size.width) / 2.0).round() as i32,
        y: (rect.position.y + f64::from(rect.size.height) / 2.0).round() as i32,
    };
    let monitor = unsafe { MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    let result = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    (result == 0 && dpi_y > 0).then(|| dpi_y as f32 / 96.0)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn monitor_scale_factor(_: tray_icon::Rect) -> Option<f32> {
    None
}

/// Share of the screen's usable height the popup aims to fill. A laptop
/// screen stays at the designed size; large screens enlarge it.
const POPUP_HEIGHT_SHARE: f32 = 0.72;
const MAX_POPUP_ZOOM: f32 = 2.0;

/// How much to enlarge the popup on a screen whose usable height is
/// `work_area_height` (in Windows' scaled units). Never shrinks it below the
/// designed size, and keeps steps of 1/20 so text renders crisply.
pub(super) fn popup_zoom(work_area_height: f32) -> f32 {
    if !work_area_height.is_finite() || work_area_height <= 0.0 {
        return 1.0;
    }
    let zoom = (work_area_height * POPUP_HEIGHT_SHARE / WINDOW_HEIGHT).clamp(1.0, MAX_POPUP_ZOOM);
    (zoom * 20.0).floor() / 20.0
}

/// The popup in the middle of the work area.
pub(super) fn popup_centered(scale_factor: f32, zoom: f32, work_area: PhysicalWorkArea) -> Point {
    let scale_factor = scale_factor.max(1.0);
    let width = WINDOW_WIDTH * zoom * scale_factor;
    let height = WINDOW_HEIGHT * zoom * scale_factor;
    let x = work_area.left + ((work_area.right - work_area.left) - width).max(0.0) / 2.0;
    let y = work_area.top + ((work_area.bottom - work_area.top) - height).max(0.0) / 2.0;
    Point::new(x / scale_factor, y / scale_factor)
}

/// The popup with its top-left corner at `corner` (screen pixels), moved
/// back onto the work area if it would hang off it.
pub(super) fn popup_at(
    corner: (f32, f32),
    scale_factor: f32,
    zoom: f32,
    work_area: PhysicalWorkArea,
) -> Point {
    let scale_factor = scale_factor.max(1.0);
    let width = WINDOW_WIDTH * zoom * scale_factor;
    let height = WINDOW_HEIGHT * zoom * scale_factor;
    let x = corner.0.clamp(
        work_area.left,
        (work_area.right - width).max(work_area.left),
    );
    let y = corner.1.clamp(
        work_area.top,
        (work_area.bottom - height).max(work_area.top),
    );
    Point::new(x / scale_factor, y / scale_factor)
}

/// The size to ask for so the popup ends up `zoom` times its base size.
/// iced scales a resize by the zoom already applied (`current_zoom`) as
/// well as the screen's, so that part is taken back out; the new zoom is
/// applied after the resize.
pub(super) fn popup_resize(zoom: f32, current_zoom: f32) -> Size {
    let current_zoom = if current_zoom > 0.0 {
        current_zoom
    } else {
        1.0
    };
    Size::new(
        WINDOW_WIDTH * zoom / current_zoom,
        WINDOW_HEIGHT * zoom / current_zoom,
    )
}

/// A fixed zoom for previewing other screen sizes during development.
pub(super) fn preview_zoom() -> Option<f32> {
    std::env::var("USAGE_UI_PREVIEW_ZOOM")
        .ok()?
        .parse::<f32>()
        .ok()
        .filter(|zoom| (1.0..=MAX_POPUP_ZOOM).contains(zoom))
}

pub(super) fn popup_position(
    rect: tray_icon::Rect,
    scale_factor: f32,
    zoom: f32,
    work_area: PhysicalWorkArea,
) -> Point {
    let scale_factor = scale_factor.max(1.0);
    let icon_left = rect.position.x as f32;
    let icon_top = rect.position.y as f32;
    let icon_width = rect.size.width as f32;
    let icon_height = rect.size.height as f32;
    let width = WINDOW_WIDTH * zoom * scale_factor;
    let height = WINDOW_HEIGHT * zoom * scale_factor;
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
