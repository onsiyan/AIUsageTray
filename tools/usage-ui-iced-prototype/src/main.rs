#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell, fs::OpenOptions, io::Write, sync::OnceLock, thread_local, time::Duration,
};

use async_channel::{Receiver, Sender};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Fill, Length, Point, Shadow, Size,
    Subscription, Task, Theme, event,
    futures::{SinkExt, Stream},
    widget::{column, container, progress_bar, row, text},
    window,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const WINDOW_WIDTH: f32 = 424.0;
const WINDOW_HEIGHT: f32 = 646.0;
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
    .window(window::Settings {
        size: Size::new(WINDOW_WIDTH, WINDOW_HEIGHT),
        visible: false,
        min_size: Some(Size::new(WINDOW_WIDTH, WINDOW_HEIGHT)),
        resizable: true,
        closeable: true,
        minimizable: true,
        decorations: true,
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
    tray_error: Option<String>,
}

impl App {
    fn new(tray_sender: Sender<TrayIconEvent>) -> Self {
        Self {
            tray_sender,
            window_id: None,
            tray_error: None,
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
                self.tray_error = Some(error);
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
        }
    }

    fn show_window(&self, tray_rect: tray_icon::Rect) -> Task<Message> {
        preview_log("show or restore window from tray");
        if let Some(window_id) = self.window_id {
            window::scale_factor(window_id).then(move |scale_factor| {
                window::monitor_size(window_id).then(move |monitor_size| {
                    let monitor_size = monitor_size.unwrap_or(Size::new(1920.0, 1080.0));
                    let position = popup_position(tray_rect, scale_factor, monitor_size);
                    preview_log(format!(
                        "show popup: scale={scale_factor} monitor={monitor_size:?} position={position:?}"
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
        let header = row![
            column![
                text("مراقبة الاستخدام").size(19).color(Color::WHITE),
                text("الحسابات والخطط").size(12).color(muted()),
            ]
            .spacing(2)
            .align_x(Alignment::End),
            container(text("U").size(15).color(Color::WHITE))
                .width(34)
                .height(34)
                .center(Length::Fill)
                .style(|_| surface_style(Color::from_rgb8(100, 86, 228))),
        ]
        .spacing(12)
        .align_y(Alignment::Center);

        let summary = container(
            row![
                column![
                    text("٥ مزوّدين").size(12).color(muted()),
                    text("ملخص الاستخدام").size(17).color(Color::WHITE),
                ]
                .spacing(5)
                .align_x(Alignment::End),
                container(
                    text("محدّث الآن")
                        .size(11)
                        .color(Color::from_rgb8(119, 226, 183))
                )
                .padding([5, 9])
                .style(|_| surface_style(Color::from_rgb8(32, 62, 55))),
            ]
            .align_y(Alignment::Center)
            .width(Fill),
        )
        .padding(16)
        .style(|_| card_style());

        let accounts = column![
            provider_card(
                "OpenAI Codex",
                "Pro · maj***@gmail.com",
                24,
                76,
                5,
                "5 ساعات",
                "أسبوعي",
                0
            ),
            provider_card(
                "Antigravity",
                "Google · الحساب الأساسي",
                43,
                97,
                10,
                "أسبوعي",
                "5 ساعات",
                1
            ),
            provider_card(
                "Claude",
                "Free · account@example.com",
                57,
                100,
                12,
                "أسبوعي",
                "5 ساعات",
                2
            ),
            provider_card("OpenRouter", "الرصيد المتاح", 82, 0, 0, "الرصيد", "", 3),
            provider_card("OpenCode Go", "خطة Go", 61, 0, 0, "الاستخدام", "", 4),
        ]
        .spacing(9);

        let footer = row![
            text("واجهة تجريبية · بيانات غير حقيقية")
                .size(11)
                .color(muted()),
            container(
                text("إدارة الحسابات  ↗")
                    .size(12)
                    .color(Color::from_rgb8(174, 165, 255))
            )
            .width(Fill)
            .align_x(Alignment::End),
        ]
        .align_y(Alignment::Center);

        let content = column![header, summary, accounts, footer]
            .spacing(15)
            .padding(18)
            .width(Fill);

        let content = if let Some(error) = &self.tray_error {
            content.push(text(error).size(11).color(Color::from_rgb8(255, 160, 150)))
        } else {
            content
        };

        container(content)
            .width(Fill)
            .height(Fill)
            .style(|_| surface_style(Color::from_rgb8(18, 20, 27)))
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

fn popup_position(rect: tray_icon::Rect, scale_factor: f32, monitor: Size) -> Point {
    let scale_factor = scale_factor.max(1.0);
    let icon_left = rect.position.x as f32;
    let icon_top = rect.position.y as f32;
    let icon_width = rect.size.width as f32;
    let icon_height = rect.size.height as f32;
    let width = WINDOW_WIDTH * scale_factor;
    let height = WINDOW_HEIGHT * scale_factor;
    let monitor_width = monitor.width * scale_factor;
    let monitor_height = monitor.height * scale_factor;

    let x =
        (icon_left + icon_width / 2.0 - width / 2.0).clamp(0.0, (monitor_width - width).max(0.0));
    let above = icon_top - height - GAP * scale_factor;
    let y = if above >= 0.0 {
        above
    } else {
        (icon_top + icon_height + GAP * scale_factor).min((monitor_height - height).max(0.0))
    };

    Point::new(x / scale_factor, y / scale_factor)
}

fn provider_card(
    name: &'static str,
    detail: &'static str,
    primary_percent: u16,
    secondary_percent: u16,
    secondary_reset: u16,
    primary_name: &'static str,
    secondary_name: &'static str,
    accent_index: usize,
) -> Element<'static, Message> {
    let accent = match accent_index {
        0 => Color::from_rgb8(137, 125, 255),
        1 => Color::from_rgb8(86, 204, 138),
        2 => Color::from_rgb8(241, 157, 97),
        3 => Color::from_rgb8(88, 175, 255),
        _ => Color::from_rgb8(220, 121, 202),
    };

    let primary = row![
        text(format!("{primary_percent}%"))
            .size(14)
            .color(Color::WHITE),
        text(primary_name).size(11).color(muted()),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let primary_bar = progress_bar(0.0..=100.0, primary_percent as f32)
        .style(move |_| progress_style(accent, false));

    let mut metrics = column![primary, primary_bar].spacing(5);
    if !secondary_name.is_empty() {
        let secondary = row![
            text(format!("{secondary_percent}%"))
                .size(12)
                .color(muted()),
            text(secondary_name).size(11).color(muted()),
            container(
                text(format!("يعاد خلال {secondary_reset}س"))
                    .size(10)
                    .color(muted())
            )
            .width(Fill)
            .align_x(Alignment::End),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let secondary_bar = progress_bar(0.0..=100.0, secondary_percent as f32)
            .style(move |_| progress_style(accent, true));
        metrics = metrics.push(secondary).push(secondary_bar);
    }

    container(
        column![
            row![
                container(text("●").size(13).color(accent)).width(22),
                column![
                    text(name).size(14).color(Color::WHITE),
                    text(detail).size(10).color(muted()),
                ]
                .spacing(3)
                .align_x(Alignment::End),
                container(text("⋯").size(20).color(muted()))
                    .width(Fill)
                    .align_x(Alignment::End),
            ]
            .align_y(Alignment::Center),
            metrics,
        ]
        .spacing(10),
    )
    .padding([12, 14])
    .style(|_| card_style())
    .into()
}

fn surface_style(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color: Color::from_rgb8(45, 48, 59),
            width: 1.0,
            radius: 14.0.into(),
        },
        text_color: None,
        shadow: Shadow::default(),
        snap: false,
    }
}

fn card_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(27, 29, 38))),
        border: Border {
            color: Color::from_rgb8(45, 48, 59),
            width: 1.0,
            radius: 12.0.into(),
        },
        text_color: None,
        shadow: Shadow::default(),
        snap: false,
    }
}

fn progress_style(accent: Color, subdued: bool) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(Color::from_rgb8(43, 45, 56)),
        bar: Background::Color(if subdued {
            Color::from_rgba8(
                (accent.r * 255.0) as u8,
                (accent.g * 255.0) as u8,
                (accent.b * 255.0) as u8,
                0.5,
            )
        } else {
            accent
        }),
        border: Border::default().rounded(4.0),
    }
}

fn muted() -> Color {
    Color::from_rgb8(145, 148, 162)
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
    fn popup_is_centered_on_tray_and_opens_above_bottom_taskbar() {
        let position = popup_position(
            tray_icon::Rect {
                position: PhysicalPosition::new(1200.0, 1040.0),
                size: PhysicalSize::new(24, 24),
            },
            1.0,
            Size::new(1920.0, 1080.0),
        );

        assert!((position.x - (1200.0 + 12.0 - WINDOW_WIDTH / 2.0)).abs() < 0.01);
        assert!(position.y < 1040.0);
    }

    #[test]
    fn popup_flips_below_top_taskbar_and_clamps_to_monitor_width() {
        let position = popup_position(
            tray_icon::Rect {
                position: PhysicalPosition::new(1900.0, 0.0),
                size: PhysicalSize::new(24, 24),
            },
            1.0,
            Size::new(1920.0, 1080.0),
        );

        assert!(position.x + WINDOW_WIDTH <= 1920.0);
        assert!(position.y > 24.0);
    }
}
