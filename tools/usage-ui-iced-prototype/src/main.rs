#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell, fs::OpenOptions, io::Write, sync::OnceLock, thread_local, time::Duration,
};

use async_channel::{Receiver, Sender};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Fill, Length, Point, Shadow, Size,
    Subscription, Task, Theme, event,
    futures::{SinkExt, Stream},
    widget::{Space, button, column, container, mouse_area, row},
    window,
};
use lucide_icons::{LUCIDE_FONT_BYTES, iced::icon_x};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const WINDOW_WIDTH: f32 = 424.0;
const WINDOW_HEIGHT: f32 = 690.0;
const GAP: f32 = 8.0;

thread_local! {
    static TRAY_ICON: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

static TRAY_EVENT_RECEIVER: OnceLock<Receiver<TrayIconEvent>> = OnceLock::new();

fn main() -> iced::Result {
    let (tray_sender, tray_receiver) = async_channel::bounded::<TrayIconEvent>(32);
    let _ = TRAY_EVENT_RECEIVER.set(tray_receiver.clone());
    let boot_sender = tray_sender.clone();
    iced::application(
        move || {
            (
                App::new(boot_sender.clone()),
                Task::done(Message::InitializeTray),
            )
        },
        App::update,
        App::view,
    )
    .title("Usage Monitor Preview")
    .theme(Theme::Dark)
    .font(LUCIDE_FONT_BYTES)
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
            skip_taskbar: false,
            ..Default::default()
        },
        ..Default::default()
    })
    .subscription(App::subscription)
    .run()
}

struct App {
    tray_sender: Sender<TrayIconEvent>,
    window_id: Option<window::Id>,
}

impl App {
    fn new(tray_sender: Sender<TrayIconEvent>) -> Self {
        Self {
            tray_sender,
            window_id: None,
        }
    }

    fn subscription(_: &Self) -> Subscription<Message> {
        Subscription::batch([
            event::listen().map(Message::RuntimeEvent),
            Subscription::run(tray_event_stream),
        ])
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
                            "لم يتم إنشاء نافذة المعاينة".to_owned(),
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
            Message::TrayEvent(TrayIconEvent::Click {
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => {
                preview_log(format!("tray left-click: {rect:?}"));
                self.show_window(rect)
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
            Message::RuntimeEvent(Event::Window(event)) => {
                preview_log(format!("window event: {event:?}"));
                match event {
                    window::Event::CloseRequested => self.hide_popup(),
                    _ => Task::none(),
                }
            }
            Message::RuntimeEvent(_) => Task::none(),
            Message::DragWindow => self.window_id.map(window::drag).unwrap_or_else(Task::none),
            Message::CloseButton => self.hide_popup(),
        }
    }

    fn show_window(&self, tray_rect: tray_icon::Rect) -> Task<Message> {
        preview_log("show or restore window from tray");
        if let Some(window_id) = self.window_id {
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
        }
    }

    fn hide_popup(&mut self) -> Task<Message> {
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
        let title_bar = container(
            row![
                mouse_area(Space::new().width(Fill).height(Length::Fill))
                    .on_press(Message::DragWindow),
                close_window_button(),
            ]
            .spacing(0)
            .align_y(Alignment::Center)
            .width(Fill),
        )
        .width(Fill)
        .height(44)
        .padding([4, 10]);

        let title_bar_separator = container(Space::new().width(Fill).height(Length::Fill))
            .width(Fill)
            .height(1)
            .style(|_| container::Style {
                background: Some(Background::Color(Color::WHITE)),
                ..Default::default()
            });

        let content = column![
            title_bar,
            title_bar_separator,
            Space::new().width(Fill).height(Fill)
        ]
        .spacing(0)
        .width(Fill)
        .height(Fill);

        container(content)
            .width(Fill)
            .height(Fill)
            .style(|_| window_frame_style())
            .into()
    }
}

#[derive(Debug, Clone)]
enum Message {
    InitializeTray,
    WindowReady(window::Id),
    TrayReady,
    TrayFailed(String),
    TrayEvent(TrayIconEvent),
    OpenPreview,
    PreviewRect(Option<tray_icon::Rect>),
    RuntimeEvent(Event),
    DragWindow,
    CloseButton,
}

fn install_tray(sender: Sender<TrayIconEvent>) -> Result<(), String> {
    let icon = Icon::from_rgba(icon_pixels(), 32, 32).map_err(|error| error.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Usage Monitor · Preview")
        .build()
        .map_err(|error| error.to_string())?;

    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = sender.send_blocking(event);
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
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        },
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

    (work_area.right > work_area.left && work_area.bottom > work_area.top)
        .then_some(work_area)
}

#[cfg(not(target_os = "windows"))]
fn monitor_work_area(_: tray_icon::Rect) -> Option<PhysicalWorkArea> {
    None
}

fn popup_position(
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
    let x = (icon_left + icon_width / 2.0 - width / 2.0)
        .clamp(work_area.left, (work_area.right - width).max(work_area.left));
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

fn close_window_button() -> Element<'static, Message> {
    button(icon_x().size(18))
        .on_press(Message::CloseButton)
        .width(36)
        .height(36)
        .style(|theme, status| {
            let mut style = button::text(theme, status);
            style.background = None;
            style.text_color = Color::WHITE;
            style.border = Border::default();
            style.shadow = Shadow::default();
            style
        })
        .into()
}

fn window_frame_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(18, 20, 27))),
        border: Border {
            color: Color::WHITE,
            width: 1.0,
            radius: 16.0.into(),
        },
        text_color: None,
        shadow: Shadow::default(),
        snap: false,
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
