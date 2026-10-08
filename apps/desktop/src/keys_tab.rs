//! The Keys page: API keys the user keeps here to copy later, for any
//! service, and the keys of the API-key accounts already added. Keys live in
//! Windows Credential Manager; the page holds only their masked form, and
//! reads a key again when it is copied or shown.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;
use lucide_icons::iced::{
    icon_check, icon_copy, icon_eye, icon_eye_off, icon_key_round, icon_pencil, icon_trash_2,
};
use usage_monitor_core::accounts::AccountId;
use usage_monitor_core::vault::{self, CopyFormat, VaultKey};

use crate::cost_tab::{muted_line, segmented, tr};
use crate::dashboard::{AccountUsageEntry, account_name, belongs_to_provider};

/// A copied key is taken off the clipboard after this long, unless something
/// else was copied since.
const CLEAR_AFTER: Duration = Duration::from_secs(30);
const NAME_INPUT: &str = "keys-name";
const SECRET_INPUT: &str = "keys-secret";
const FORMAT_FILE: &str = "keys-copy-format.txt";
const ERROR_COLOR: Color = Color::from_rgb8(0xE0, 0x6C, 0x5F);

/// Which key a row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum KeyRef {
    Vault(String),
    Account(AccountId, AccountField),
}

/// The two keys an account can hold: its key, and for some providers a
/// second value (OpenRouter's management key, xAI's team ID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountField {
    Primary,
    Secondary,
}

/// A key that is shown or copied, kept out of debug output.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(..)")
    }
}

#[derive(Debug, Clone)]
pub(super) enum KeysChange {
    Query(String),
    Format(CopyFormat),
    Hover(Option<KeyRef>),
    Copy(KeyRef),
    /// The clipboard time ran out for the copy that left it at this number.
    ClearClipboard(u32),
    Reveal(KeyRef),
    Hide,
    /// Open the form for a new key, or to edit the key with this id.
    OpenForm(Option<String>),
    CloseForm,
    FormName(String),
    FormService(String),
    FormEnvVar(String),
    FormSecret(Secret),
    Save,
    /// Ask before removing this key, or stop asking.
    AskRemove(Option<String>),
    Remove(String),
}

/// One row: a key as the list shows it, never the key itself.
#[derive(Debug, Clone)]
struct Row {
    reference: KeyRef,
    name: String,
    service: String,
    env_var: String,
    masked: String,
}

#[derive(Default)]
struct KeyForm {
    /// The id of the key being edited; `None` for a new key.
    editing: Option<String>,
    name: String,
    service: String,
    env_var: String,
    secret: String,
    /// The service was typed or picked, so a pasted key no longer sets it.
    service_edited: bool,
    /// The variable was typed, so the service no longer sets it.
    env_var_edited: bool,
    problem: Option<String>,
}

pub(super) struct KeysTab {
    vault: Vec<Row>,
    accounts: Vec<Row>,
    error: Option<String>,
    query: String,
    format: CopyFormat,
    form: Option<KeyForm>,
    revealed: Option<(KeyRef, Secret)>,
    confirm_remove: Option<String>,
    /// The last copy, and the clipboard's number right after it.
    copied: Option<(KeyRef, u32)>,
    hovered: Option<KeyRef>,
}

impl Default for KeysTab {
    fn default() -> Self {
        Self {
            vault: Vec::new(),
            accounts: Vec::new(),
            error: None,
            query: String::new(),
            format: CopyFormat::Plain,
            form: None,
            revealed: None,
            confirm_remove: None,
            copied: None,
            hovered: None,
        }
    }
}

impl KeysTab {
    pub(super) fn load() -> Self {
        Self {
            format: load_format(),
            ..Self::default()
        }
    }

    /// Reads the list again, when the page opens or a key changed.
    pub(super) fn refresh(&mut self, entries: &[AccountUsageEntry]) {
        match store::list() {
            Ok(keys) => {
                self.vault = keys.iter().map(vault_row).collect();
                self.error = None;
            }
            Err(error) => {
                preview_log(format!("reading the key vault failed: {error}"));
                self.vault.clear();
                self.error = Some(error);
            }
        }
        self.accounts = account_rows(entries);
    }

    /// Forgets shown keys and open forms when the page is left.
    pub(super) fn close(&mut self) {
        self.revealed = None;
        self.form = None;
        self.confirm_remove = None;
        self.hovered = None;
    }

    pub(super) fn change(
        &mut self,
        change: KeysChange,
        entries: &[AccountUsageEntry],
    ) -> Task<Message> {
        match change {
            KeysChange::Query(query) => self.query = query,
            KeysChange::Format(format) => {
                self.format = format;
                save_format(format);
            }
            KeysChange::Hover(reference) => self.hovered = reference,
            KeysChange::Copy(reference) => return self.copy(reference),
            KeysChange::ClearClipboard(sequence) => {
                crate::account_add::clear_clipboard_if_unchanged(sequence);
                if self
                    .copied
                    .as_ref()
                    .is_some_and(|(_, copied)| *copied == sequence)
                {
                    self.copied = None;
                }
            }
            KeysChange::Reveal(reference) => match self.secret(&reference) {
                Ok(secret) => self.revealed = Some((reference, Secret(secret))),
                Err(error) => self.error = Some(error),
            },
            KeysChange::Hide => self.revealed = None,
            KeysChange::OpenForm(editing) => {
                self.confirm_remove = None;
                let form = match editing {
                    None => KeyForm::default(),
                    Some(id) => match store::get(&id) {
                        Ok(Some(key)) => KeyForm {
                            editing: Some(id),
                            name: key.name,
                            service: key.service,
                            env_var: key.env_var,
                            secret: key.secret,
                            service_edited: true,
                            env_var_edited: true,
                            problem: None,
                        },
                        Ok(None) => {
                            self.refresh(entries);
                            return Task::none();
                        }
                        Err(error) => {
                            self.error = Some(error);
                            return Task::none();
                        }
                    },
                };
                self.form = Some(form);
                return iced::widget::operation::focus(NAME_INPUT);
            }
            KeysChange::CloseForm => self.form = None,
            KeysChange::FormName(name) => {
                if let Some(form) = &mut self.form {
                    form.name = name;
                }
            }
            KeysChange::FormService(service) => {
                if let Some(form) = &mut self.form {
                    form.service = service;
                    form.service_edited = !form.service.trim().is_empty();
                    if !form.env_var_edited {
                        form.env_var = vault::suggested_env_var(&form.service);
                    }
                }
            }
            KeysChange::FormEnvVar(env_var) => {
                if let Some(form) = &mut self.form {
                    form.env_var = env_var;
                    form.env_var_edited = !form.env_var.trim().is_empty();
                }
            }
            KeysChange::FormSecret(Secret(secret)) => {
                if let Some(form) = &mut self.form {
                    form.secret = secret;
                    // A pasted key names its service when its start says so.
                    if !form.service_edited
                        && let Some(service) = vault::detect_service(&form.secret)
                    {
                        form.service = service.to_owned();
                        if !form.env_var_edited {
                            form.env_var = vault::suggested_env_var(service);
                        }
                    }
                }
            }
            KeysChange::Save => {
                if let Some(form) = &mut self.form {
                    let key = match &form.editing {
                        None => {
                            VaultKey::new(&form.name, &form.service, &form.env_var, &form.secret)
                        }
                        Some(id) => match store::get(id) {
                            Ok(Some(mut key)) => key
                                .edit(&form.name, &form.service, &form.env_var, &form.secret)
                                .map(|()| key),
                            Ok(None) => Err("This key was removed.".to_owned()),
                            Err(error) => Err(error),
                        },
                    };
                    match key.and_then(|key| store::save(&key)) {
                        Ok(()) => {
                            self.form = None;
                            self.revealed = None;
                            self.refresh(entries);
                        }
                        Err(problem) => form.problem = Some(problem),
                    }
                }
            }
            KeysChange::AskRemove(id) => self.confirm_remove = id,
            KeysChange::Remove(id) => {
                self.confirm_remove = None;
                if let Err(error) = store::remove(&id) {
                    self.error = Some(error);
                }
                let reference = KeyRef::Vault(id);
                if self
                    .revealed
                    .as_ref()
                    .is_some_and(|(shown, _)| *shown == reference)
                {
                    self.revealed = None;
                }
                self.refresh(entries);
            }
        }
        Task::none()
    }

    /// Copies a key in the chosen format, marked to stay out of clipboard
    /// history, and takes it off the clipboard after a while.
    fn copy(&mut self, reference: KeyRef) -> Task<Message> {
        let secret = match self.secret(&reference) {
            Ok(secret) => secret,
            Err(error) => {
                self.error = Some(error);
                return Task::none();
            }
        };
        let env_var = self
            .row(&reference)
            .map(|row| row.env_var.clone())
            .unwrap_or_default();
        let text = vault::copy_text(self.format, &env_var, &secret);
        let Some(sequence) = crate::account_add::copy_secret_to_clipboard(&text) else {
            self.error = Some("Couldn't use the clipboard; try again.".to_owned());
            return Task::none();
        };
        self.copied = Some((reference, sequence));
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            std::thread::sleep(CLEAR_AFTER);
            let _ = sender.send_blocking(());
        });
        Task::perform(async move { receiver.recv().await }, move |_| {
            Message::Keys(KeysChange::ClearClipboard(sequence))
        })
    }

    fn row(&self, reference: &KeyRef) -> Option<&Row> {
        self.vault
            .iter()
            .chain(&self.accounts)
            .find(|row| row.reference == *reference)
    }

    /// Reads a key from where it is kept.
    fn secret(&self, reference: &KeyRef) -> Result<String, String> {
        match reference {
            KeyRef::Vault(id) => store::get(id)?
                .map(|key| key.secret)
                .ok_or_else(|| "This key was removed.".to_owned()),
            KeyRef::Account(account_id, field) => store::account_value(*account_id, *field)?
                .ok_or_else(|| "This account's key is no longer saved.".to_owned()),
        }
    }
}

fn vault_row(key: &VaultKey) -> Row {
    Row {
        reference: KeyRef::Vault(key.id.clone()),
        name: key.name.clone(),
        service: key.service.clone(),
        env_var: key.env_var.clone(),
        masked: key.masked(),
    }
}

/// The values an API-key provider's account keeps, with the variables the
/// CLI reads them from.
fn account_fields(
    provider: UsageProvider,
) -> &'static [(AccountField, &'static str, &'static str)] {
    use AccountField::{Primary, Secondary};
    match provider {
        UsageProvider::DeepSeek => &[(Primary, "API key", "DEEPSEEK_API_KEY")],
        UsageProvider::OpenRouter => &[
            (Primary, "API key", "OPENROUTER_API_KEY"),
            (Secondary, "Management key", "OPENROUTER_MANAGEMENT_API_KEY"),
        ],
        UsageProvider::Kimi => &[(Primary, "API key", "KIMI_CODE_API_KEY")],
        UsageProvider::Zai => &[(Primary, "API key", "Z_AI_API_KEY")],
        UsageProvider::MiniMax => &[(Primary, "API key", "MINIMAX_CODING_API_KEY")],
        UsageProvider::Xai => &[
            (Primary, "Management key", "XAI_MANAGEMENT_API_KEY"),
            (Secondary, "Team ID", "XAI_TEAM_ID"),
        ],
        _ => &[],
    }
}

fn account_rows(entries: &[AccountUsageEntry]) -> Vec<Row> {
    let mut rows = Vec::new();
    for entry in entries {
        let Some(provider) = PROVIDER_TABS
            .iter()
            .map(|tab| tab.provider)
            .find(|provider| belongs_to_provider(&entry.account.provider_id, *provider))
        else {
            continue;
        };
        for (field, label, env_var) in account_fields(provider) {
            let value = match store::account_value(entry.account.id, *field) {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(error) => {
                    preview_log(format!("reading an account's key failed: {error}"));
                    continue;
                }
            };
            rows.push(Row {
                reference: KeyRef::Account(entry.account.id, *field),
                name: account_name(&entry.account),
                service: format!("{} · {label}", provider.display_name()),
                env_var: (*env_var).to_owned(),
                // A team ID is not a secret; everything else is masked.
                masked: if *label == "Team ID" {
                    value
                } else {
                    vault::masked(&value)
                },
            });
        }
    }
    rows
}

fn load_format() -> CopyFormat {
    let saved = crate::theme::preference_directory()
        .ok()
        .and_then(|directory| std::fs::read_to_string(directory.join(FORMAT_FILE)).ok());
    match saved.as_deref().map(str::trim) {
        Some("powershell") => CopyFormat::PowerShell,
        Some("posix") => CopyFormat::Posix,
        Some("dotenv") => CopyFormat::DotEnv,
        _ => CopyFormat::Plain,
    }
}

fn save_format(format: CopyFormat) {
    let name = match format {
        CopyFormat::Plain => "plain",
        CopyFormat::PowerShell => "powershell",
        CopyFormat::Posix => "posix",
        CopyFormat::DotEnv => "dotenv",
    };
    if let Ok(directory) = crate::theme::preference_directory() {
        let _ = std::fs::create_dir_all(&directory);
        if let Err(error) = std::fs::write(directory.join(FORMAT_FILE), name) {
            preview_log(format!("saving the copy format failed: {error}"));
        }
    }
}

#[cfg(windows)]
mod store {
    use usage_monitor_core::accounts::AccountId;
    use usage_monitor_core::vault::VaultKey;
    use usage_monitor_windows::vault;

    use super::AccountField;

    pub(super) fn list() -> Result<Vec<VaultKey>, String> {
        vault::list()
    }

    pub(super) fn get(id: &str) -> Result<Option<VaultKey>, String> {
        vault::get(id)
    }

    pub(super) fn save(key: &VaultKey) -> Result<(), String> {
        vault::save(key)
    }

    pub(super) fn remove(id: &str) -> Result<(), String> {
        vault::remove(id)
    }

    pub(super) fn account_value(
        account_id: AccountId,
        field: AccountField,
    ) -> Result<Option<String>, String> {
        let material = usage_monitor_windows::read_auth_material(account_id)
            .map_err(|error| error.to_string())?;
        Ok(material
            .and_then(|material| match field {
                AccountField::Primary => material.bearer_token,
                AccountField::Secondary => material.secondary_bearer_token,
            })
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }
}

#[cfg(not(windows))]
mod store {
    use usage_monitor_core::accounts::AccountId;
    use usage_monitor_core::vault::VaultKey;

    use super::AccountField;

    const UNSUPPORTED: &str = "The key vault is only available on Windows.";

    pub(super) fn list() -> Result<Vec<VaultKey>, String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub(super) fn get(_id: &str) -> Result<Option<VaultKey>, String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub(super) fn save(_key: &VaultKey) -> Result<(), String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub(super) fn remove(_id: &str) -> Result<(), String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub(super) fn account_value(
        _account_id: AccountId,
        _field: AccountField,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
}

fn change(change: KeysChange) -> Message {
    Message::Keys(change)
}

pub(super) fn view(
    state: &KeysTab,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let mut sections = column![header(state, theme, language)].spacing(12);
    if let Some(form) = &state.form {
        sections = sections.push(key_form(form, theme, language));
    }
    if let Some(error) = &state.error {
        sections = sections.push(
            text(error.clone())
                .size(typography::METADATA_SIZE)
                .color(ERROR_COLOR),
        );
    }

    let total = state.vault.len() + state.accounts.len();
    if total > 0 {
        sections = sections.push(copy_format(state.format, theme, language));
    }
    if total > 6 {
        sections = sections.push(
            text_input(
                tr(
                    language,
                    "Search by name or service",
                    "ابحث بالاسم أو الخدمة",
                ),
                &state.query,
            )
            .on_input(|query| change(KeysChange::Query(query)))
            .size(typography::LABEL_SIZE)
            .padding([5, 8])
            .width(Fill)
            .style(move |framework_theme, status| {
                crate::dialogs::account_key_input_style(framework_theme, status, theme)
            }),
        );
    }

    let matching = |rows: &[Row]| -> Vec<Row> {
        let query = state.query.trim().to_lowercase();
        rows.iter()
            .filter(|row| {
                query.is_empty()
                    || [&row.name, &row.service, &row.env_var]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    };

    let vault_rows = matching(&state.vault);
    if state.vault.is_empty() {
        if state.form.is_none() {
            sections = sections.push(muted_line(
                tr(
                    language,
                    "Keep any API key here to copy it in one click: OpenAI, Anthropic, Gemini, or any other service. Keys are stored in Windows Credential Manager for your Windows user only, and a copied key stays out of clipboard history and is cleared after 30 seconds.",
                    "احفظ هنا أي مفتاح API لتنسخه بضغطة: OpenAI أو Anthropic أو Gemini أو أي خدمة أخرى. تُحفظ المفاتيح في Credential Manager لمستخدم Windows الخاص بك فقط، والمفتاح المنسوخ لا يدخل سجل الحافظة ويُمسح بعد 30 ثانية.",
                ),
                theme,
            ));
        }
    } else {
        let mut list = column![].spacing(2);
        for row in vault_rows {
            list = list.push(key_row(row, state, true, theme, language));
        }
        sections = sections.push(list);
    }

    let account_rows = matching(&state.accounts);
    if !account_rows.is_empty() {
        let mut list = column![section_title(
            tr(language, "From your accounts", "من حساباتك"),
            theme
        )]
        .spacing(2);
        for row in account_rows {
            list = list.push(key_row(row, state, false, theme, language));
        }
        sections = sections.push(list);
    }

    let content = crate::smooth_scroll::smooth_scroll(
        DashboardTab::Keys.scroll_key(),
        scrollable(sections.width(Fill))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(Fill),
    );
    container(content)
        .padding([10, 12])
        .width(Fill)
        .height(Fill)
        .into()
}

fn section_title(
    title: &'static str,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    container(
        text(title)
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
    )
    .padding([4, 0])
    .into()
}

fn header(
    state: &KeysTab,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let mut line = row![
        text(tr(language, "API keys", "مفاتيح API"))
            .size(typography::ACCOUNT_NAME_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text())
            .width(Fill),
    ]
    .align_y(Alignment::Center);
    if state.form.is_none() {
        line = line.push(text_button(
            tr(language, "+ Add key", "+ إضافة مفتاح"),
            true,
            change(KeysChange::OpenForm(None)),
            theme,
        ));
    }
    line.into()
}

fn copy_format(
    format: CopyFormat,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let option = |label: &'static str, value: CopyFormat| {
        (label, format == value, change(KeysChange::Format(value)))
    };
    row![
        text(tr(language, "Copy as", "انسخ بصيغة"))
            .size(typography::METADATA_SIZE)
            .font(typography::MEDIUM)
            .color(theme.colors.muted_text()),
        segmented(
            vec![
                option(tr(language, "Key", "المفتاح"), CopyFormat::Plain),
                option("PowerShell", CopyFormat::PowerShell),
                option("bash", CopyFormat::Posix),
                option(".env", CopyFormat::DotEnv),
            ],
            theme,
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn key_row(
    row_data: Row,
    state: &KeysTab,
    editable: bool,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let Row {
        reference,
        name,
        service,
        env_var,
        masked,
    } = row_data;
    let hovered = state.hovered.as_ref() == Some(&reference);
    let copied = state
        .copied
        .as_ref()
        .is_some_and(|(copied, _)| *copied == reference);
    let revealed = state
        .revealed
        .as_ref()
        .filter(|(shown, _)| *shown == reference)
        .map(|(_, Secret(secret))| secret.clone());
    let id = match &reference {
        KeyRef::Vault(id) => Some(id.clone()),
        KeyRef::Account(..) => None,
    };
    let confirming = id.is_some() && state.confirm_remove == id;

    let detail = if copied {
        text(tr(
            language,
            "Copied · cleared from the clipboard in 30 s",
            "نُسخ · يُمسح من الحافظة خلال 30 ثانية",
        ))
        .size(typography::COMPACT_SIZE)
        .font(typography::MEDIUM)
        .color(copied_color(theme))
    } else {
        let key_text = revealed.clone().unwrap_or(masked);
        let mut parts = Vec::new();
        if !service.is_empty() {
            parts.push(service);
        }
        parts.push(key_text);
        text(parts.join("  ·  "))
            .size(typography::COMPACT_SIZE)
            .font(typography::MEDIUM)
            .color(theme.colors.muted_text())
    };
    let mut label = column![
        text(name)
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
        detail,
    ];
    if !env_var.is_empty() && hovered && !copied {
        label = label.push(
            text(env_var)
                .size(typography::COMPACT_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.muted_text().scale_alpha(0.8)),
        );
    }

    let mut actions = row![].spacing(2).align_y(Alignment::Center);
    if confirming && let Some(id) = &id {
        actions = actions
            .push(text_button(
                tr(language, "Cancel", "إلغاء"),
                false,
                change(KeysChange::AskRemove(None)),
                theme,
            ))
            .push(danger_button(
                tr(language, "Remove", "حذف"),
                change(KeysChange::Remove(id.clone())),
                theme,
            ));
    } else {
        if editable
            && hovered
            && let Some(id) = &id
        {
            actions = actions
                .push(icon_button(
                    icon_pencil::<Theme>()
                        .size(14)
                        .color(theme.colors.muted_text()),
                    tr(language, "Edit", "تعديل"),
                    change(KeysChange::OpenForm(Some(id.clone()))),
                    theme,
                ))
                .push(icon_button(
                    icon_trash_2::<Theme>()
                        .size(14)
                        .color(theme.colors.muted_text()),
                    tr(language, "Remove", "حذف"),
                    change(KeysChange::AskRemove(Some(id.clone()))),
                    theme,
                ));
        }
        let (eye, eye_label, eye_message) = if revealed.is_some() {
            (
                icon_eye_off::<Theme>(),
                tr(language, "Hide", "إخفاء"),
                KeysChange::Hide,
            )
        } else {
            (
                icon_eye::<Theme>(),
                tr(language, "Show", "إظهار"),
                KeysChange::Reveal(reference.clone()),
            )
        };
        actions = actions
            .push(icon_button(
                eye.size(15).color(theme.colors.muted_text()),
                eye_label,
                change(eye_message),
                theme,
            ))
            .push(icon_button(
                if copied {
                    icon_check::<Theme>().size(15).color(copied_color(theme))
                } else {
                    icon_copy::<Theme>().size(15).color(theme.colors.text())
                },
                tr(language, "Copy", "نسخ"),
                change(KeysChange::Copy(reference.clone())),
                theme,
            ));
    }

    let line = row![
        container(
            icon_key_round::<Theme>()
                .size(15)
                .color(theme.colors.muted_text())
        )
        .width(18)
        .center_x(18),
        label.width(Fill),
        actions,
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let area = container(line)
        .padding([5, 6])
        .width(Fill)
        .style(move |_| container::Style {
            background: hovered.then(|| Background::Color(theme.colors.hover().scale_alpha(0.5))),
            border: Border {
                radius: 7.0.into(),
                ..Border::default()
            },
            ..Default::default()
        });
    mouse_area(area)
        .on_enter(change(KeysChange::Hover(Some(reference))))
        .on_exit(change(KeysChange::Hover(None)))
        .into()
}

fn key_form(
    form: &KeyForm,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let input = |placeholder: &'static str, value: &str| {
        text_input(placeholder, value)
            .size(typography::LABEL_SIZE)
            .padding([5, 8])
            .width(Fill)
            .style(move |framework_theme, status| {
                crate::dialogs::account_key_input_style(framework_theme, status, theme)
            })
    };
    let label = |english: &'static str, arabic: &'static str| {
        text(tr(language, english, arabic))
            .size(typography::METADATA_SIZE)
            .font(typography::MEDIUM)
            .color(theme.colors.muted_text())
            .width(78)
    };
    let field = |name: Element<'static, Message>, input: Element<'static, Message>| {
        row![name, input].spacing(8).align_y(Alignment::Center)
    };
    let mut fields = column![
        text(if form.editing.is_some() {
            tr(language, "Edit key", "تعديل المفتاح")
        } else {
            tr(language, "New key", "مفتاح جديد")
        })
        .size(typography::LABEL_SIZE)
        .font(typography::EMPHASIS)
        .color(theme.colors.text()),
        field(
            label("Key", "المفتاح").into(),
            input("sk-…", &form.secret)
                .id(SECRET_INPUT)
                .secure(true)
                .on_input(|secret| change(KeysChange::FormSecret(Secret(secret))))
                .on_submit(change(KeysChange::Save))
                .into(),
        ),
        field(
            label("Name", "الاسم").into(),
            input(
                tr(
                    language,
                    "Work, Personal, Project X…",
                    "العمل، الشخصي، مشروع…"
                ),
                &form.name,
            )
            .id(NAME_INPUT)
            .on_input(|name| change(KeysChange::FormName(name)))
            .on_submit(change(KeysChange::Save))
            .into(),
        ),
        field(
            label("Service", "الخدمة").into(),
            input("OpenAI, Anthropic, Gemini…", &form.service)
                .on_input(|service| change(KeysChange::FormService(service)))
                .on_submit(change(KeysChange::Save))
                .into(),
        ),
        field(
            label("Variable", "المتغير").into(),
            input("OPENAI_API_KEY", &form.env_var)
                .on_input(|env_var| change(KeysChange::FormEnvVar(env_var)))
                .on_submit(change(KeysChange::Save))
                .into(),
        ),
    ]
    .spacing(6);
    if let Some(problem) = &form.problem {
        fields = fields.push(
            text(problem.clone())
                .size(typography::METADATA_SIZE)
                .color(ERROR_COLOR),
        );
    }
    fields = fields.push(muted_line(
        tr(
            language,
            "The variable is used when copying for PowerShell, bash, or a .env file.",
            "يُستخدم المتغير عند النسخ بصيغة PowerShell أو bash أو ملف .env.",
        ),
        theme,
    ));
    fields = fields.push(
        row![
            Space::new().width(Fill),
            text_button(
                tr(language, "Cancel", "إلغاء"),
                false,
                change(KeysChange::CloseForm),
                theme,
            ),
            text_button(
                tr(language, "Save", "حفظ"),
                true,
                change(KeysChange::Save),
                theme,
            ),
        ]
        .spacing(6),
    );
    container(fields)
        .padding([10, 10])
        .width(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.hover())),
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..Default::default()
        })
        .into()
}

fn copied_color(theme: &'static ThemeDefinition) -> Color {
    if theme.colors.is_light {
        Color::from_rgb8(0x1F, 0x9D, 0x63)
    } else {
        Color::from_rgb8(0x3D, 0xD6, 0x8C)
    }
}

fn icon_button(
    icon: impl Into<Element<'static, Message>>,
    label: &'static str,
    message: Message,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let button = button(container(icon).center(Fill))
        .on_press(message)
        .width(26)
        .height(26)
        .padding(0)
        .style(move |framework_theme: &Theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then(|| Background::Color(theme.colors.hover()));
            style.border = Border {
                radius: 6.0.into(),
                ..Border::default()
            };
            style.shadow = Shadow::default();
            style
        });
    hint::hint(button, label, theme)
}

fn text_button(
    label: &'static str,
    strong: bool,
    message: Message,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let accent = copied_color(theme);
    button(
        text(label)
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
    )
    .on_press(message)
    .padding([3, 8])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        style.background = Some(Background::Color(if strong {
            accent.scale_alpha(if hovered { 0.34 } else { 0.22 })
        } else if hovered {
            theme.colors.hover()
        } else {
            Color::TRANSPARENT
        }));
        style.border = Border {
            color: theme.colors.border(0.35),
            width: 1.0,
            radius: 6.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

fn danger_button(
    label: &'static str,
    message: Message,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::METADATA_SIZE)
            .font(typography::EMPHASIS)
            .color(Color::WHITE),
    )
    .on_press(message)
    .padding([3, 8])
    .style(move |framework_theme: &Theme, status| {
        let mut style = button::text(framework_theme, status);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        style.background = Some(Background::Color(ERROR_COLOR.scale_alpha(if hovered {
            1.0
        } else {
            0.85
        })));
        style.border = Border {
            color: theme.colors.border(0.2),
            width: 0.0,
            radius: 6.0.into(),
        };
        style.shadow = Shadow::default();
        style
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_show_in_debug_output() {
        let change = KeysChange::FormSecret(Secret("sk-live-1234".to_owned()));
        assert!(!format!("{change:?}").contains("sk-live"));
    }

    #[test]
    fn api_key_providers_offer_their_keys() {
        assert_eq!(account_fields(UsageProvider::OpenRouter).len(), 2);
        assert_eq!(account_fields(UsageProvider::DeepSeek).len(), 1);
        // Signed-in accounts hold refresh tokens, which are never offered.
        assert!(account_fields(UsageProvider::Codex).is_empty());
        assert!(account_fields(UsageProvider::Claude).is_empty());
        assert!(account_fields(UsageProvider::Copilot).is_empty());
        assert!(account_fields(UsageProvider::Cursor).is_empty());
    }
}
