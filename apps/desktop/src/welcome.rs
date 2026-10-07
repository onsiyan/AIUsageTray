//! The welcome shown on first run: the user picks the providers they use,
//! then adds an account to each in turn. Finishing shows only the chosen
//! providers' tabs; the tab manager can change that later.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;
use std::fs;

const DONE_FILE: &str = "welcome_done.txt";

/// Whether the welcome was finished or skipped before.
pub fn is_done() -> bool {
    theme::preference_directory()
        .map(|directory| directory.join(DONE_FILE).exists())
        .unwrap_or(true)
}

pub fn mark_done() {
    let result = theme::preference_directory().and_then(|directory| {
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(DONE_FILE), "done")
    });
    if let Err(error) = result {
        preview_log(format!("welcome state save failed: {error}"));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Choose,
    /// Adding accounts to the `index`th chosen provider.
    Accounts {
        index: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Welcome {
    pub step: Step,
    /// The chosen providers, in tab-bar order.
    pub chosen: Vec<UsageProvider>,
    /// Whether the Cursor app is signed in on this computer, checked when
    /// the Cursor step opens.
    pub cursor_signed_in: bool,
}

impl Welcome {
    pub fn new() -> Self {
        Self {
            step: Step::Choose,
            chosen: Vec::new(),
            cursor_signed_in: false,
        }
    }

    pub fn toggle(&mut self, provider: UsageProvider) {
        if let Some(index) = self.chosen.iter().position(|chosen| *chosen == provider) {
            self.chosen.remove(index);
        } else {
            self.chosen.push(provider);
            self.chosen.sort_by_key(|provider| tab_order(*provider));
        }
    }

    pub fn current(&self) -> Option<UsageProvider> {
        match self.step {
            Step::Choose => None,
            Step::Accounts { index } => self.chosen.get(index).copied(),
        }
    }

    /// Opens the accounts step at `index`, checking for the Cursor app when
    /// that step is Cursor's.
    pub fn open(&mut self, index: usize) {
        self.step = Step::Accounts { index };
        if self.current() == Some(UsageProvider::Cursor) {
            self.cursor_signed_in =
                usage_monitor_core::providers::cursor::local_app_session().is_some();
        }
    }
}

fn tab_order(provider: UsageProvider) -> usize {
    PROVIDER_TABS
        .iter()
        .position(|tab| tab.provider == provider)
        .unwrap_or(usize::MAX)
}

/// How each provider's account is added, in a few words.
pub fn method(provider: UsageProvider) -> locale::Text {
    match provider {
        UsageProvider::Codex
        | UsageProvider::Claude
        | UsageProvider::Antigravity
        | UsageProvider::OpenCodeGo => locale::Text::WelcomeBrowserSignIn,
        UsageProvider::Copilot => locale::Text::WelcomeGitHubSignIn,
        UsageProvider::Cursor => locale::Text::WelcomeCursorApp,
        UsageProvider::Xai => locale::Text::WelcomeTwoKeys,
        UsageProvider::MiMo => locale::Text::WelcomeCookie,
        UsageProvider::OpenRouter
        | UsageProvider::DeepSeek
        | UsageProvider::Kimi
        | UsageProvider::Zai
        | UsageProvider::MiniMax => locale::Text::WelcomeApiKey,
    }
}

impl App {
    pub(super) fn welcome_view<'a>(&'a self, welcome: &'a Welcome) -> Element<'a, Message> {
        let active_theme = self.theme_id.definition();
        let language = self.language;
        let title_bar = row![
            mouse_area(Space::new().width(Fill).height(Length::Fill)).on_press(Message::DragWindow),
            close_window_button(active_theme),
        ]
        .align_y(Alignment::Center)
        .width(Fill)
        .height(44)
        .padding([4, 10]);

        let body = match welcome.step {
            Step::Choose => choose_view(welcome, language, active_theme),
            Step::Accounts { index } => {
                let provider = welcome.chosen[index];
                let account_count = self
                    .dashboard
                    .accounts_by_provider()
                    .iter()
                    .filter(|(owner, _, _)| *owner == provider)
                    .count();
                let status = self
                    .account_add_status
                    .as_ref()
                    .map(|status| account_add_status_banner(status, active_theme, language));
                accounts_view(
                    welcome,
                    index,
                    account_count,
                    self.account_add_running,
                    status,
                    language,
                    active_theme,
                )
            }
        };

        column![
            title_bar,
            container(body).width(Fill).height(Fill).padding([4, 22])
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }
}

fn heading<'a>(
    title: &'static str,
    subtitle: &'static str,
    active_theme: &'static ThemeDefinition,
) -> Element<'a, Message> {
    column![
        text(title)
            .size(typography::ACCOUNT_NAME_SIZE + 4.0)
            .font(typography::EMPHASIS)
            .color(active_theme.colors.text()),
        text(subtitle)
            .size(typography::LABEL_SIZE)
            .font(typography::MEDIUM)
            .color(active_theme.colors.muted_text()),
    ]
    .spacing(6)
    .into()
}

fn choose_view<'a>(
    welcome: &'a Welcome,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'a, Message> {
    let rows = PROVIDER_TABS.iter().copied().map(|tab| {
        let selected = welcome.chosen.contains(&tab.provider);
        provider_row(tab, selected, language, active_theme)
    });
    // Room on the right keeps the scrollbar off the rows.
    let list = scrollable(column(rows).spacing(4).width(Fill).padding(iced::Padding {
        right: 10.0,
        ..iced::Padding::ZERO
    }))
    .direction(iced::widget::scrollable::Direction::Vertical(
        iced::widget::scrollable::Scrollbar::new()
            .width(4)
            .scroller_width(4),
    ))
    .height(Fill);
    let buttons = row![
        account_dialog_button(
            locale::text(language, locale::Text::WelcomeSkip),
            false,
            true,
            active_theme,
            Message::WelcomeFinish,
        ),
        Space::new().width(Fill),
        account_dialog_button(
            locale::text(language, locale::Text::WelcomeContinue),
            true,
            !welcome.chosen.is_empty(),
            active_theme,
            Message::WelcomeOpenStep(0),
        ),
    ]
    .align_y(Alignment::Center);

    column![
        heading(
            locale::text(language, locale::Text::WelcomeTitle),
            locale::text(language, locale::Text::WelcomeChooseHint),
            active_theme,
        ),
        list,
        buttons,
    ]
    .spacing(14)
    .padding(iced::Padding {
        bottom: 18.0,
        ..iced::Padding::ZERO
    })
    .into()
}

fn provider_row(
    tab: ProviderTab,
    selected: bool,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    // A checkbox: filled with the text color when chosen, so it reads in
    // every theme.
    let mark = container(if selected {
        Element::from(
            icon_check::<Theme>()
                .size(14)
                .color(active_theme.colors.window_surface()),
        )
    } else {
        Space::new().width(14).height(14).into()
    })
    .center(22)
    .style(move |_| container::Style {
        background: selected.then(|| Background::Color(active_theme.colors.text())),
        border: Border {
            color: active_theme
                .colors
                .text()
                .scale_alpha(if selected { 1.0 } else { 0.45 }),
            width: 1.5,
            radius: 6.0.into(),
        },
        ..Default::default()
    });
    button(
        row![
            image(provider_logo_handle(
                tab.provider,
                active_theme.colors.is_light
            ))
            .width(26)
            .height(26)
            .content_fit(ContentFit::Contain),
            column![
                text(tab.label)
                    .size(typography::LABEL_SIZE)
                    .font(typography::EMPHASIS)
                    .color(active_theme.colors.text()),
                text(locale::text(language, method(tab.provider)))
                    .size(typography::METADATA_SIZE)
                    .font(typography::MEDIUM)
                    .color(active_theme.colors.muted_text()),
            ]
            .spacing(1)
            .width(Fill),
            mark,
        ]
        .spacing(11)
        .align_y(Alignment::Center),
    )
    .on_press(Message::WelcomeToggleProvider(tab.provider))
    .width(Fill)
    .padding([7, 10])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        style.background = Some(Background::Color(if hovered {
            active_theme.colors.hover()
        } else {
            active_theme.colors.control_surface().scale_alpha(0.5)
        }));
        style.text_color = active_theme.colors.text();
        style.border = Border {
            color: if selected {
                active_theme.colors.text().scale_alpha(0.55)
            } else {
                active_theme.colors.border(0.14)
            },
            width: 1.0,
            radius: 9.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

fn accounts_view<'a>(
    welcome: &'a Welcome,
    index: usize,
    account_count: usize,
    adding: bool,
    status: Option<Element<'a, Message>>,
    language: locale::Language,
    active_theme: &'static ThemeDefinition,
) -> Element<'a, Message> {
    let provider = welcome.chosen[index];
    let label = PROVIDER_TABS
        .iter()
        .find(|tab| tab.provider == provider)
        .map_or(provider.display_name(), |tab| tab.label);
    let is_last = index + 1 == welcome.chosen.len();
    let cursor_missing = provider == UsageProvider::Cursor && !welcome.cursor_signed_in;

    let progress = match language {
        locale::Language::English => format!("{} of {}", index + 1, welcome.chosen.len()),
        locale::Language::Arabic => format!("{} من {}", index + 1, welcome.chosen.len()),
    };
    let added = match language {
        locale::Language::English => format!("Accounts added: {account_count}"),
        locale::Language::Arabic => format!("الحسابات المضافة: {account_count}"),
    };
    let note = if cursor_missing {
        locale::Text::WelcomeCursorMissing
    } else {
        match method(provider) {
            locale::Text::WelcomeBrowserSignIn => locale::Text::WelcomeBrowserSignInDetail,
            locale::Text::WelcomeGitHubSignIn => locale::Text::WelcomeGitHubSignInDetail,
            locale::Text::WelcomeCursorApp => locale::Text::WelcomeCursorAppDetail,
            locale::Text::WelcomeTwoKeys => locale::Text::WelcomeTwoKeysDetail,
            _ => locale::Text::WelcomeApiKeyDetail,
        }
    };

    let card = container(
        column![
            row![
                image(provider_logo_handle(provider, active_theme.colors.is_light))
                    .width(40)
                    .height(40)
                    .content_fit(ContentFit::Contain),
                column![
                    text(label)
                        .size(typography::ACCOUNT_NAME_SIZE + 2.0)
                        .font(typography::EMPHASIS)
                        .color(active_theme.colors.text()),
                    text(added)
                        .size(typography::METADATA_SIZE)
                        .font(typography::MEDIUM)
                        .color(active_theme.colors.muted_text()),
                ]
                .spacing(2),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            text(locale::text(language, note))
                .size(typography::LABEL_SIZE)
                .font(typography::MEDIUM)
                .color(active_theme.colors.text()),
            row![account_dialog_button(
                locale::text(
                    language,
                    if cursor_missing {
                        locale::Text::WelcomeCheckAgain
                    } else if account_count > 0 {
                        locale::Text::WelcomeAddAnother
                    } else {
                        locale::Text::AddAccount
                    },
                ),
                true,
                !adding,
                active_theme,
                if cursor_missing {
                    Message::WelcomeOpenStep(index)
                } else {
                    Message::ChooseAccountProvider(provider)
                },
            ),],
        ]
        .spacing(14),
    )
    .width(Fill)
    .padding(16)
    .style(move |_| account_menu_surface_style(active_theme));

    let back = if index == 0 {
        Message::WelcomeBack
    } else {
        Message::WelcomeOpenStep(index - 1)
    };
    let next = if is_last {
        Message::WelcomeFinish
    } else {
        Message::WelcomeOpenStep(index + 1)
    };
    let next_label = match (is_last, account_count > 0) {
        (true, _) => locale::Text::WelcomeFinish,
        (false, true) => locale::Text::WelcomeNext,
        (false, false) => locale::Text::WelcomeSkipProvider,
    };
    let buttons = row![
        account_dialog_button(
            locale::text(language, locale::Text::WelcomeBackLabel),
            false,
            !adding,
            active_theme,
            back,
        ),
        Space::new().width(Fill),
        account_dialog_button(
            locale::text(language, next_label),
            true,
            !adding,
            active_theme,
            next,
        ),
    ]
    .align_y(Alignment::Center);

    column![
        heading(
            locale::text(language, locale::Text::WelcomeAddTitle),
            locale::text(language, locale::Text::WelcomeAddHint),
            active_theme,
        ),
        text(progress)
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(active_theme.colors.muted_text()),
        card,
    ]
    .push(status)
    .push(Space::new().height(Fill))
    .push(buttons)
    .spacing(14)
    .padding(iced::Padding {
        bottom: 18.0,
        ..iced::Padding::ZERO
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chosen_providers_keep_tab_order_and_cursor_says_it_needs_the_app() {
        let mut welcome = Welcome::new();
        welcome.toggle(UsageProvider::DeepSeek);
        welcome.toggle(UsageProvider::Codex);
        welcome.toggle(UsageProvider::Claude);
        welcome.toggle(UsageProvider::Claude);
        assert_eq!(
            welcome.chosen,
            [UsageProvider::Codex, UsageProvider::DeepSeek]
        );
        assert_eq!(welcome.current(), None);
        welcome.open(1);
        assert_eq!(welcome.current(), Some(UsageProvider::DeepSeek));
        assert_eq!(
            method(UsageProvider::Cursor),
            locale::Text::WelcomeCursorApp
        );
        assert_eq!(method(UsageProvider::Xai), locale::Text::WelcomeTwoKeys);
    }
}
