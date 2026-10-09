//! The Keys page: API keys the user keeps here to copy later, for any
//! service, and the keys of the API-key accounts already added. Keys live in
//! Windows Credential Manager; the page holds only their masked form, and
//! reads a key again when it is copied or shown.

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;
use lucide_icons::iced::{
    icon_check, icon_copy, icon_ellipsis, icon_eye, icon_eye_off, icon_info, icon_key_round,
    icon_pencil, icon_plug_zap, icon_trash_2,
};
use usage_monitor_core::accounts::AccountId;
use usage_monitor_core::vault::{self, VaultKey};

use crate::cost_tab::tr;
use crate::dashboard::{AccountUsageEntry, account_name, belongs_to_provider};
use crate::key_check::{self, Outcome};

/// How long a row says its key was copied.
const COPIED_FOR: Duration = Duration::from_secs(2);
const NAME_INPUT: &str = "keys-name";
const SECRET_INPUT: &str = "keys-secret";
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
    Hover(Option<KeyRef>),
    Copy(KeyRef),
    /// The "copied" mark of this copy has been shown long enough.
    CopiedShown(u32),
    Reveal(KeyRef),
    Hide,
    /// Open the form for a new key, or to edit the key with this id.
    OpenForm(Option<String>),
    CloseForm,
    FormName(String),
    /// Pick a listed service by its place in the list, or `None` for one
    /// the user names.
    PickService(Option<usize>),
    FormService(String),
    FormSecret(Secret),
    Save,
    /// Ask before removing this key, or stop asking.
    AskRemove(Option<String>),
    Remove(String),
    /// Test this saved key against its service.
    Test(String),
    Tested(String, Outcome),
}

/// One row: a key as the list shows it, never the key itself.
#[derive(Debug, Clone)]
struct Row {
    reference: KeyRef,
    name: String,
    service: String,
    masked: String,
    logo: Option<RowLogo>,
}

/// The logo a row shows in place of the key icon.
#[derive(Debug, Clone, Copy)]
enum RowLogo {
    Provider(UsageProvider),
    /// A service's place in [`key_services::SERVICES`].
    Service(usize),
}

impl RowLogo {
    fn handle(self, light_theme: bool) -> iced::widget::image::Handle {
        match self {
            Self::Provider(provider) => provider_logo_handle(provider, light_theme),
            Self::Service(index) => key_services::SERVICES[index].logo(light_theme),
        }
    }
}

#[derive(Default)]
struct KeyForm {
    /// The id of the key being edited; `None` for a new key.
    editing: Option<String>,
    name: String,
    service: String,
    secret: String,
    /// The service is one the user names rather than a listed one.
    other_service: bool,
    /// The service was picked, so a pasted key no longer sets it.
    service_edited: bool,
    problem: Option<String>,
}

#[derive(Default)]
pub(super) struct KeysTab {
    vault: Vec<Row>,
    accounts: Vec<Row>,
    error: Option<String>,
    query: String,
    form: Option<KeyForm>,
    revealed: Option<(KeyRef, Secret)>,
    confirm_remove: Option<String>,
    /// The last copy, and its number among the page's copies.
    copied: Option<(KeyRef, u32)>,
    copies: u32,
    hovered: Option<KeyRef>,
    /// Key tests by key id: `None` while one runs.
    tests: std::collections::HashMap<String, Option<Outcome>>,
}

impl KeysTab {
    /// Reads the list again, when the page opens or a key changed.
    pub(super) fn refresh(&mut self, entries: &[AccountUsageEntry]) {
        match store::list() {
            Ok(keys) => {
                self.vault = keys.iter().map(vault_row).collect();
                self.error = None;
            }
            Err(error) => {
                crate::app_log::write(format!("reading the key vault failed: {error}"));
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
        self.tests.clear();
    }

    pub(super) fn change(
        &mut self,
        change: KeysChange,
        entries: &[AccountUsageEntry],
    ) -> Task<Message> {
        match change {
            KeysChange::Query(query) => self.query = query,
            KeysChange::Hover(reference) => self.hovered = reference,
            KeysChange::Copy(reference) => return self.copy(reference),
            KeysChange::CopiedShown(copy) => {
                if self
                    .copied
                    .as_ref()
                    .is_some_and(|(_, shown)| *shown == copy)
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
                            other_service: !key.service.is_empty()
                                && key_services::find(&key.service).is_none(),
                            service: key.service,
                            secret: key.secret,
                            service_edited: true,
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
            KeysChange::PickService(index) => {
                if let Some(form) = &mut self.form {
                    form.service_edited = true;
                    match index.and_then(|index| key_services::SERVICES.get(index)) {
                        Some(service) => {
                            form.other_service = false;
                            form.service = service.name.to_owned();
                        }
                        None => {
                            if !form.other_service {
                                form.service.clear();
                            }
                            form.other_service = true;
                        }
                    }
                }
            }
            KeysChange::FormService(service) => {
                if let Some(form) = &mut self.form {
                    form.service = service;
                    form.service_edited = true;
                }
            }
            KeysChange::FormSecret(Secret(secret)) => {
                if let Some(form) = &mut self.form {
                    form.secret = secret;
                    // A pasted key names its service when its start says so.
                    if !form.service_edited
                        && let Some(service) = vault::detect_service(&form.secret)
                    {
                        let listed = key_services::find(service);
                        form.other_service = listed.is_none();
                        form.service = listed.map_or(service, |listed| listed.name).to_owned();
                    }
                }
            }
            KeysChange::Save => {
                if let Some(form) = &mut self.form {
                    let name = if form.name.trim().is_empty() {
                        default_name(&self.vault, form.editing.as_deref(), &form.service)
                    } else {
                        form.name.clone()
                    };
                    let key = match &form.editing {
                        None => VaultKey::new(&name, &form.service, &form.secret),
                        Some(id) => match store::get(id) {
                            Ok(Some(mut key)) => {
                                key.edit(&name, &form.service, &form.secret).map(|()| key)
                            }
                            Ok(None) => Err("This key was removed.".to_owned()),
                            Err(error) => Err(error),
                        },
                    };
                    match key.and_then(|key| store::save(&key)) {
                        Ok(()) => {
                            if let Some(id) = &form.editing {
                                self.tests.remove(id);
                            }
                            self.form = None;
                            self.revealed = None;
                            self.refresh(entries);
                        }
                        Err(problem) => form.problem = Some(problem),
                    }
                }
            }
            KeysChange::Test(id) => {
                if self.tests.get(&id).is_some_and(Option::is_none) {
                    return Task::none();
                }
                let key = match store::get(&id) {
                    Ok(Some(key)) => key,
                    Ok(None) => {
                        self.refresh(entries);
                        return Task::none();
                    }
                    Err(error) => {
                        self.error = Some(error);
                        return Task::none();
                    }
                };
                self.tests.insert(id.clone(), None);
                return Task::perform(key_check::test(key.service, key.secret), move |outcome| {
                    Message::Keys(KeysChange::Tested(id.clone(), outcome))
                });
            }
            KeysChange::Tested(id, outcome) => {
                if let Outcome::Failed(problem) = &outcome {
                    crate::app_log::write(format!("key test failed: {problem}"));
                }
                // A result for a key changed or closed meanwhile is dropped.
                if let Some(test) = self.tests.get_mut(&id) {
                    *test = Some(outcome);
                }
            }
            KeysChange::AskRemove(id) => self.confirm_remove = id,
            KeysChange::Remove(id) => {
                self.confirm_remove = None;
                self.tests.remove(&id);
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

    /// Copies a key and marks its row for a moment.
    fn copy(&mut self, reference: KeyRef) -> Task<Message> {
        let secret = match self.secret(&reference) {
            Ok(secret) => secret,
            Err(error) => {
                self.error = Some(error);
                return Task::none();
            }
        };
        crate::account_add::copy_to_clipboard(&secret);
        self.copies = self.copies.wrapping_add(1);
        let copy = self.copies;
        self.copied = Some((reference, copy));
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            std::thread::sleep(COPIED_FOR);
            let _ = sender.send_blocking(());
        });
        Task::perform(async move { receiver.recv().await }, move |_| {
            Message::Keys(KeysChange::CopiedShown(copy))
        })
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
        masked: key.masked(),
        logo: key_services::index_of(&key.service).map(RowLogo::Service),
    }
}

/// A name for a key saved without one: its service's, numbered when another
/// kept key already has it.
fn default_name(vault: &[Row], editing: Option<&str>, service: &str) -> String {
    let base = match service.trim() {
        "" => "API key",
        service => service,
    };
    let taken = |name: &str| {
        vault.iter().any(|row| {
            row.name.eq_ignore_ascii_case(name)
                && !matches!((&row.reference, editing), (KeyRef::Vault(id), Some(editing)) if id == editing)
        })
    };
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|number| format!("{base} {number}"))
        .find(|name| !taken(name))
        .expect("some number is free")
}

/// The values an API-key provider's account keeps.
fn account_fields(provider: UsageProvider) -> &'static [(AccountField, &'static str)] {
    use AccountField::{Primary, Secondary};
    match provider {
        UsageProvider::DeepSeek
        | UsageProvider::Kimi
        | UsageProvider::Zai
        | UsageProvider::MiniMax => &[(Primary, "API key")],
        UsageProvider::OpenRouter => &[(Primary, "API key"), (Secondary, "Management key")],
        UsageProvider::Xai => &[(Primary, "Management key"), (Secondary, "Team ID")],
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
        for (field, label) in account_fields(provider) {
            let value = match store::account_value(entry.account.id, *field) {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(error) => {
                    crate::app_log::write(format!("reading an account's key failed: {error}"));
                    continue;
                }
            };
            rows.push(Row {
                reference: KeyRef::Account(entry.account.id, *field),
                name: account_name(&entry.account),
                // Every row is an API key; only the other kinds are named.
                service: if *label == "API key" {
                    provider.display_name().to_string()
                } else {
                    format!("{} · {label}", provider.display_name())
                },
                // A team ID is not a secret; everything else is masked.
                masked: if *label == "Team ID" {
                    value
                } else {
                    vault::masked(&value)
                },
                logo: Some(RowLogo::Provider(provider)),
            });
        }
    }
    rows
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

    if state.vault.len() + state.accounts.len() > 6 {
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
                    || [&row.name, &row.service]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    };

    let vault_rows = matching(&state.vault);
    if !state.vault.is_empty() {
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
    let about = hint::hint(
        container(
            icon_info::<Theme>()
                .size(14)
                .color(theme.colors.muted_text()),
        )
        .padding(2),
        tr(
            language,
            "A vault for your API keys, for quick access: keep them here and copy one in a click. The app reads no usage from these keys.",
            "خزنة لمفاتيح API للوصول السريع: احفظها هنا وانسخ أيًّا منها بضغطة. البرنامج لا يجلب أي استخدام من هذه المفاتيح.",
        ),
        theme,
    );
    let mut line = row![
        text(tr(language, "API keys", "مفاتيح API"))
            .size(typography::ACCOUNT_NAME_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
        about,
        Space::new().width(Fill),
    ]
    .spacing(6)
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
        masked,
        logo,
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
    let testable = id.is_some() && key_check::can_test(&service);
    let test = id.as_ref().and_then(|id| state.tests.get(id));

    let detail = if copied {
        text(tr(language, "Copied", "نُسخ"))
            .size(typography::COMPACT_SIZE)
            .font(typography::MEDIUM)
            .color(copied_color(theme))
    } else if let Some(test) = test {
        let (words, color) = match test {
            None => (
                tr(language, "Testing…", "جارٍ الاختبار…").to_owned(),
                theme.colors.muted_text(),
            ),
            Some(Outcome::Works) => (
                tr(language, "Works", "يعمل").to_owned(),
                copied_color(theme),
            ),
            Some(Outcome::Rejected) => (
                tr(language, "Rejected by the service", "رفضته الخدمة").to_owned(),
                ERROR_COLOR,
            ),
            Some(Outcome::Failed(problem)) => (
                format!("{}: {problem}", tr(language, "Test failed", "فشل الاختبار")),
                ERROR_COLOR,
            ),
        };
        text(format!("{service}  ·  {words}"))
            .size(typography::COMPACT_SIZE)
            .font(typography::MEDIUM)
            .color(color)
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
    let label = column![
        text(name)
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
        detail,
    ];

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
        if testable && let Some(id) = &id {
            actions = actions.push(icon_button(
                icon_plug_zap::<Theme>()
                    .size(15)
                    .color(theme.colors.muted_text()),
                tr(
                    language,
                    "Test the connection (free, spends nothing)",
                    "اختبار الاتصال (مجاني، لا يستهلك شيئًا)",
                ),
                change(KeysChange::Test(id.clone())),
                theme,
            ));
        }
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

    let icon: Element<'static, Message> = match logo {
        Some(logo) => iced::widget::image(logo.handle(theme.colors.is_light))
            .width(20)
            .height(20)
            .into(),
        None => icon_key_round::<Theme>()
            .size(15)
            .color(theme.colors.muted_text())
            .into(),
    };
    let line = row![
        container(icon).width(22).center_x(22),
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
    // The grid shows no names, so the picked one is named by its label.
    let picked_service = (!form.other_service)
        .then(|| key_services::find(&form.service).map(|service| service.name))
        .flatten();
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
                    "Optional: Work, Personal…",
                    "اختياري: العمل، الشخصي…"
                ),
                &form.name,
            )
            .id(NAME_INPUT)
            .on_input(|name| change(KeysChange::FormName(name)))
            .on_submit(change(KeysChange::Save))
            .into(),
        ),
        row![
            label("Service", "الخدمة"),
            text(picked_service.unwrap_or_default())
                .size(typography::METADATA_SIZE)
                .font(typography::MEDIUM)
                .color(theme.colors.text()),
        ]
        .align_y(Alignment::Center),
        service_picker(form, theme, language),
    ]
    .spacing(6);
    if form.other_service {
        fields = fields.push(field(
            label("Service name", "اسم الخدمة").into(),
            input(tr(language, "Service name", "اسم الخدمة"), &form.service)
                .on_input(|service| change(KeysChange::FormService(service)))
                .on_submit(change(KeysChange::Save))
                .into(),
        ));
    }
    if let Some(problem) = &form.problem {
        fields = fields.push(
            text(problem.clone())
                .size(typography::METADATA_SIZE)
                .color(ERROR_COLOR),
        );
    }
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

/// A service's square in the picker, and the logo inside it.
const SERVICE_TILE: f32 = 36.0;
const SERVICE_LOGO: f32 = 24.0;
const SERVICE_GAP: f32 = 5.0;

/// The listed services as a grid of logos, each named on hover, then "Other".
fn service_picker(
    form: &KeyForm,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let picked = (!form.other_service)
        .then(|| key_services::index_of(&form.service))
        .flatten();
    let tile = |content: Element<'static, Message>,
                name: &'static str,
                selected: bool,
                message: Message|
     -> Element<'static, Message> {
        let tile = button(container(content).center(Fill))
            .on_press(message)
            .width(SERVICE_TILE)
            .height(SERVICE_TILE)
            .padding(0)
            .style(move |framework_theme: &Theme, status| {
                let mut style = button::text(framework_theme, status);
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                style.background = (selected || hovered).then(|| {
                    Background::Color(if selected {
                        copied_color(theme).scale_alpha(0.22)
                    } else {
                        theme.colors.hover()
                    })
                });
                style.border = Border {
                    color: if selected {
                        copied_color(theme)
                    } else {
                        theme.colors.border(0.3)
                    },
                    width: 1.0,
                    radius: 8.0.into(),
                };
                style.shadow = Shadow::default();
                style
            });
        hint::hint(tile, name, theme)
    };
    let mut tiles = row![].spacing(SERVICE_GAP);
    for (index, service) in key_services::SERVICES.iter().enumerate() {
        tiles = tiles.push(tile(
            iced::widget::image(service.logo(theme.colors.is_light))
                .width(SERVICE_LOGO)
                .height(SERVICE_LOGO)
                .into(),
            service.name,
            picked == Some(index),
            change(KeysChange::PickService(Some(index))),
        ));
    }
    tiles = tiles.push(tile(
        icon_ellipsis::<Theme>()
            .size(18)
            .color(theme.colors.muted_text())
            .into(),
        tr(language, "Other…", "أخرى…"),
        form.other_service,
        change(KeysChange::PickService(None)),
    ));
    tiles.wrap().vertical_spacing(SERVICE_GAP).into()
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

    #[test]
    fn unnamed_keys_take_their_service_name() {
        let row = |id: &str, name: &str| Row {
            reference: KeyRef::Vault(id.to_owned()),
            name: name.to_owned(),
            service: String::new(),
            masked: String::new(),
            logo: None,
        };
        let vault = [row("a", "Claude"), row("b", "claude 2"), row("c", "OpenAI")];
        assert_eq!(default_name(&vault, None, "Claude"), "Claude 3");
        assert_eq!(default_name(&vault, None, "Groq"), "Groq");
        assert_eq!(default_name(&vault, None, " "), "API key");
        // A key being edited does not count against its own name.
        assert_eq!(default_name(&vault, Some("c"), "OpenAI"), "OpenAI");
    }
}
