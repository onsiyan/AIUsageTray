use chrono::{DateTime, Local, Utc};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Event, Fill, Length, Point,
    Rectangle, Size, Vector,
    advanced::{
        Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
        widget::{self, Tree},
    },
    widget::{
        button, checkbox, column, container, image, mouse_area, progress_bar, rich_text, row,
        scrollable, space, span, text, text_input,
    },
};
use lucide_icons::iced::{
    icon_arrow_left_right, icon_check, icon_chevron_down, icon_chevron_up, icon_eye, icon_eye_off,
    icon_pencil, icon_star, icon_x,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs, io,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use usage_monitor_core::{
    accounts::{AccountId, AccountRecord, AccountStatus, AccountStore},
    codex_desktop,
    storage::{SqliteStore, default_accounts_database_path},
    usage::{
        RateLimitWindow, SpendSnapshot, UsageCreditInventory, UsageCreditRecord, UsageMetric,
        UsagePrimaryWindowKind, UsageSnapshot, UsageSnapshotStore, UsageWindowKind,
    },
};

mod animation;
mod card;
mod model_menu;
mod model_quotas;
mod persistence;
mod usage_rows;
mod widgets;

use animation::*;
use card::*;
use model_menu::*;
use model_quotas::*;
use persistence::*;
use usage_rows::*;
use widgets::*;

pub use persistence::{delete_saved_account, load_saved_accounts, save_account_alias};

const ACCOUNT_ORDER_FILE: &str = "account-order.txt";
const FAVORITE_ACCOUNTS_FILE: &str = "favorite-accounts.txt";
/// Rename (24) + favorite (24) + move up (22) + move down (22) + three 1px gaps.
const HOVER_CONTROLS_WIDTH: f32 = 95.0;
const HIDE_ANTIGRAVITY_CLAUDE_GPT_FILE: &str = "antigravity-hide-claude-gpt.txt";

/// Whether the Antigravity "Claude and GPT models" group is hidden.
static HIDE_ANTIGRAVITY_CLAUDE_GPT: AtomicBool = AtomicBool::new(false);

fn antigravity_claude_gpt_hidden() -> bool {
    HIDE_ANTIGRAVITY_CLAUDE_GPT.load(Ordering::Relaxed)
}

use crate::{
    DashboardTab, Message, PROVIDER_TABS, UsageProvider,
    locale::{self, Language, Text},
    typography,
};

#[derive(Debug, Clone)]
pub struct AccountUsageEntry {
    pub account: AccountRecord,
    pub snapshot: Option<UsageSnapshot>,
}

pub struct DashboardState {
    entries: Vec<AccountUsageEntry>,
    is_loading: bool,
    has_loaded: bool,
    failed: bool,
    alias_editor: Option<AliasEditor>,
    hovered_account_name: Option<AccountId>,
    usage_animation: UsageAnimationState,
    open_model_visibility_menu: Option<AccountId>,
    model_visibility: ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    show_antigravity_quota_groups: bool,
    codex_desktop: CodexDesktopState,
    /// User-chosen display order; accounts not listed keep their saved order
    /// after the listed ones.
    account_order: Vec<AccountId>,
    /// Starred accounts, in the order the Favorites tab shows them.
    favorites: Vec<AccountId>,
    /// Accounts each custom tab picked one by one, by tab id.
    custom_tab_accounts: HashMap<u32, Vec<AccountId>>,
}

/// Which saved Codex account the Codex desktop app is signed in with, and
/// the progress of a switch requested from the dashboard.
#[derive(Default)]
struct CodexDesktopState {
    active_account: Option<AccountId>,
    /// Email the Antigravity desktop app is signed in with.
    antigravity_email: Option<String>,
    switching: Option<AccountId>,
    failure: Option<(AccountId, String)>,
}

#[derive(Default)]
struct ModelVisibilityPreferences {
    hidden_model_ids: HashSet<String>,
    visible_model_ids: HashSet<String>,
}

impl ModelVisibilityPreferences {
    fn is_visible(&self, model_id: &str) -> bool {
        let model_id = normalize_model_id(model_id);
        if self.hidden_model_ids.contains(&model_id) {
            return false;
        }
        if self.visible_model_ids.contains(&model_id) {
            return true;
        }
        let family_id = model_quota_family_id(&model_id);
        if self.hidden_model_ids.contains(&family_id) {
            return false;
        }
        if self.visible_model_ids.contains(&family_id) {
            return true;
        }
        if self
            .visible_model_ids
            .iter()
            .any(|saved_id| model_quota_family_id(saved_id) == family_id)
        {
            return true;
        }
        is_default_pinned_model_family(&family_id)
    }

    fn set_visible(&mut self, model_id: &str, is_visible: bool) {
        let model_id = normalize_model_id(model_id);
        let family_id = model_quota_family_id(&model_id);
        self.hidden_model_ids
            .retain(|saved_id| model_quota_family_id(saved_id) != family_id);
        self.visible_model_ids
            .retain(|saved_id| model_quota_family_id(saved_id) != family_id);
        if is_visible {
            if !is_default_pinned_model_family(&family_id) {
                self.visible_model_ids.insert(family_id);
            }
        } else if is_default_pinned_model_family(&family_id) {
            self.hidden_model_ids.insert(family_id);
        }
    }
}

struct AliasEditor {
    account_id: AccountId,
    draft: String,
    original_label: String,
    is_saving: bool,
    failed: bool,
}

impl DashboardState {
    pub fn loading() -> Self {
        Self {
            entries: Vec::new(),
            is_loading: true,
            has_loaded: false,
            failed: false,
            alias_editor: None,
            hovered_account_name: None,
            usage_animation: UsageAnimationState::default(),
            open_model_visibility_menu: None,
            model_visibility: load_model_visibility_preferences(),
            show_all_model_quotas: false,
            show_antigravity_quota_groups: true,
            codex_desktop: CodexDesktopState {
                active_account: current_codex_desktop_account(),
                antigravity_email: current_antigravity_app_email(),
                ..CodexDesktopState::default()
            },
            account_order: load_account_ids(ACCOUNT_ORDER_FILE),
            favorites: load_account_ids(FAVORITE_ACCOUNTS_FILE),
            custom_tab_accounts: HashMap::new(),
        }
        .with_loaded_display_preferences()
    }

    fn with_loaded_display_preferences(self) -> Self {
        HIDE_ANTIGRAVITY_CLAUDE_GPT.store(load_hide_antigravity_claude_gpt(), Ordering::Relaxed);
        self
    }

    pub fn set_hide_antigravity_claude_gpt(&mut self, hide: bool) -> io::Result<()> {
        HIDE_ANTIGRAVITY_CLAUDE_GPT.store(hide, Ordering::Relaxed);
        let directory = crate::theme::preference_directory()?;
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join(HIDE_ANTIGRAVITY_CLAUDE_GPT_FILE),
            if hide { "hide" } else { "show" },
        )
    }

    /// The accounts of one tab in display order.
    fn ordered_entries(&self, tab: DashboardTab) -> Vec<&AccountUsageEntry> {
        match tab {
            DashboardTab::Provider(provider) => {
                let mut accounts = self
                    .entries
                    .iter()
                    .filter(|entry| belongs_to_provider(&entry.account.provider_id, provider))
                    .collect::<Vec<_>>();
                accounts.sort_by_key(|entry| account_rank(&self.account_order, entry.account.id));
                accounts
            }
            DashboardTab::Favorites => self
                .favorites
                .iter()
                .filter_map(|id| self.entries.iter().find(|entry| entry.account.id == *id))
                .collect(),
            DashboardTab::Custom { .. } => {
                let mut accounts = self
                    .entries
                    .iter()
                    .filter(|entry| self.custom_tab_includes(tab, &entry.account))
                    .collect::<Vec<_>>();
                accounts.sort_by_key(|entry| account_rank(&self.account_order, entry.account.id));
                accounts
            }
        }
    }

    /// Whether a provider or custom tab lists `account`.
    fn custom_tab_includes(&self, tab: DashboardTab, account: &AccountRecord) -> bool {
        let picked = match tab {
            DashboardTab::Custom { id, .. } => self
                .custom_tab_accounts
                .get(&id)
                .is_some_and(|accounts| accounts.contains(&account.id)),
            _ => false,
        };
        picked
            || PROVIDER_TABS.iter().any(|provider_tab| {
                tab.includes_provider(provider_tab.provider)
                    && belongs_to_provider(&account.provider_id, provider_tab.provider)
            })
    }

    /// Keeps the custom tabs' picked accounts in step with the tab layout.
    pub fn set_custom_tab_accounts(&mut self, accounts: HashMap<u32, Vec<AccountId>>) {
        self.custom_tab_accounts = accounts;
    }

    /// Every account with its provider and card name, in display order, for
    /// picking accounts into a custom tab.
    pub fn accounts_by_provider(&self) -> Vec<(UsageProvider, AccountId, String)> {
        PROVIDER_TABS
            .iter()
            .flat_map(|provider_tab| {
                self.ordered_entries(DashboardTab::Provider(provider_tab.provider))
                    .into_iter()
                    .map(|entry| {
                        (
                            provider_tab.provider,
                            entry.account.id,
                            account_name(&entry.account),
                        )
                    })
            })
            .collect()
    }

    /// The accounts a tab shows, in display order; refreshed first while the
    /// tab is open.
    pub fn tab_account_ids(&self, tab: DashboardTab) -> Vec<AccountId> {
        self.ordered_entries(tab)
            .iter()
            .map(|entry| entry.account.id)
            .collect()
    }

    fn is_favorite(&self, account_id: AccountId) -> bool {
        self.favorites.contains(&account_id)
    }

    /// Stars an account, adding it to the end of the Favorites tab, or
    /// removes its star.
    pub fn toggle_favorite(&mut self, account_id: AccountId) {
        if self.is_favorite(account_id) {
            self.favorites.retain(|id| *id != account_id);
        } else {
            self.favorites.push(account_id);
        }
    }

    /// Saves the account order and the favorites.
    pub fn save_account_lists(&self) -> io::Result<()> {
        save_account_ids(ACCOUNT_ORDER_FILE, &self.account_order)?;
        save_account_ids(FAVORITE_ACCOUNTS_FILE, &self.favorites)
    }

    /// Moves an account one place up (`-1`) or down (`1`) within the shown
    /// tab. Returns whether the order changed and needs saving.
    pub fn move_account(
        &mut self,
        tab: DashboardTab,
        account_id: AccountId,
        offset: isize,
    ) -> bool {
        let mut tab_ids = self
            .ordered_entries(tab)
            .iter()
            .map(|entry| entry.account.id)
            .collect::<Vec<_>>();
        let Some(index) = tab_ids.iter().position(|id| *id == account_id) else {
            return false;
        };
        let Some(target) = index
            .checked_add_signed(offset)
            .filter(|target| *target < tab_ids.len())
        else {
            return false;
        };
        tab_ids.swap(index, target);

        if tab == DashboardTab::Favorites {
            // Starred accounts that no longer exist keep their place after
            // the shown ones; they disappear for good when unstarred.
            let missing = self
                .favorites
                .iter()
                .filter(|id| !tab_ids.contains(id))
                .copied()
                .collect::<Vec<_>>();
            self.favorites = tab_ids.into_iter().chain(missing).collect();
            return true;
        }

        // Rebuild the global order: other tabs keep their places, and this
        // tab's slots take its new sequence.
        let mut all = self.entries.iter().collect::<Vec<_>>();
        all.sort_by_key(|entry| account_rank(&self.account_order, entry.account.id));
        let mut reordered = tab_ids.into_iter();
        self.account_order = all
            .iter()
            .map(|entry| {
                if self.custom_tab_includes(tab, &entry.account) {
                    reordered.next().unwrap_or(entry.account.id)
                } else {
                    entry.account.id
                }
            })
            .collect();
        true
    }

    /// Marks a Codex desktop switch as running. Returns false while another
    /// switch is still in progress.
    pub fn begin_codex_switch(&mut self, account_id: AccountId) -> bool {
        if self.codex_desktop.switching.is_some() {
            return false;
        }
        self.codex_desktop.switching = Some(account_id);
        self.codex_desktop.failure = None;
        true
    }

    pub fn finish_codex_switch(&mut self, account_id: AccountId, result: Result<(), String>) {
        if self.codex_desktop.switching == Some(account_id) {
            self.codex_desktop.switching = None;
        }
        if let Err(error) = result {
            self.codex_desktop.failure = Some((account_id, error));
        }
        self.codex_desktop.active_account = current_codex_desktop_account();
        self.codex_desktop.antigravity_email = current_antigravity_app_email();
    }

    pub fn set_accounts(&mut self, mut entries: Vec<AccountUsageEntry>) {
        let now_utc = Utc::now();
        for entry in &mut entries {
            if let Some(snapshot) = entry.snapshot.as_mut() {
                snapshot.clear_elapsed_resets(now_utc);
            }
        }
        self.usage_animation
            .update(&self.entries, &entries, Instant::now());
        let account_ids = entries
            .iter()
            .map(|entry| entry.account.id)
            .collect::<HashSet<_>>();
        if self
            .open_model_visibility_menu
            .is_some_and(|account_id| !account_ids.contains(&account_id))
        {
            self.open_model_visibility_menu = None;
        }
        if self
            .hovered_account_name
            .is_some_and(|account_id| !account_ids.contains(&account_id))
        {
            self.hovered_account_name = None;
        }
        self.entries = entries;
        self.is_loading = false;
        self.has_loaded = true;
        self.failed = false;
        self.codex_desktop.active_account = current_codex_desktop_account();
        self.codex_desktop.antigravity_email = current_antigravity_app_email();
    }

    /// Applies one refreshed account in place. Unlike `set_accounts`, this
    /// neither copies every account nor rereads desktop app state, since a
    /// refresh delivers accounts one by one.
    pub fn update_account_usage(&mut self, mut entry: AccountUsageEntry) {
        if let Some(snapshot) = entry.snapshot.as_mut() {
            snapshot.clear_elapsed_resets(Utc::now());
        }
        let now = Instant::now();
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|existing| existing.account.id == entry.account.id)
        {
            self.usage_animation.update_account(existing, &entry, now);
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
        self.is_loading = false;
        self.has_loaded = true;
        self.failed = false;
    }

    /// Shows windows whose reset time has passed since they were read as
    /// unused. Returns whether any did, so the caller can fetch new readings.
    pub fn clear_elapsed_resets(&mut self) -> bool {
        let now_utc = Utc::now();
        let now = Instant::now();
        let mut changed = false;
        for existing in &mut self.entries {
            let Some(mut snapshot) = existing.snapshot.clone() else {
                continue;
            };
            if !snapshot.clear_elapsed_resets(now_utc) {
                continue;
            }
            let next = AccountUsageEntry {
                account: existing.account.clone(),
                snapshot: Some(snapshot),
            };
            self.usage_animation.update_account(existing, &next, now);
            *existing = next;
            changed = true;
        }
        changed
    }

    /// Rereads which accounts the Codex and Antigravity apps are signed in
    /// with.
    pub fn refresh_desktop_apps(&mut self) {
        self.codex_desktop.active_account = current_codex_desktop_account();
        self.codex_desktop.antigravity_email = current_antigravity_app_email();
    }

    pub fn account_entries(&self) -> &[AccountUsageEntry] {
        &self.entries
    }

    pub fn toggle_model_visibility_menu(&mut self, account_id: AccountId) {
        self.open_model_visibility_menu = if self.open_model_visibility_menu == Some(account_id) {
            None
        } else {
            Some(account_id)
        };
    }

    pub fn close_model_visibility_menu(&mut self, account_id: AccountId) {
        if self.open_model_visibility_menu == Some(account_id) {
            self.open_model_visibility_menu = None;
        }
    }

    pub fn close_any_model_visibility_menu(&mut self) {
        self.open_model_visibility_menu = None;
    }

    pub fn model_visibility_menu_open(&self, account_id: AccountId) -> bool {
        self.open_model_visibility_menu == Some(account_id)
    }

    pub fn has_open_model_visibility_menu(&self) -> bool {
        self.open_model_visibility_menu.is_some()
    }

    pub fn set_hovered_account_name(&mut self, account_id: AccountId) {
        self.hovered_account_name = Some(account_id);
    }

    pub fn clear_hovered_account_name(&mut self, account_id: AccountId) {
        if self.hovered_account_name == Some(account_id) {
            self.hovered_account_name = None;
        }
    }

    pub fn clear_any_hovered_account_name(&mut self) {
        self.hovered_account_name = None;
    }

    pub fn has_active_usage_animation(&self) -> bool {
        self.usage_animation.is_active()
    }

    pub fn advance_usage_animation(&mut self, now: Instant) {
        self.usage_animation.advance(now);
    }

    fn account_name_is_hovered(&self, account_id: AccountId) -> bool {
        self.hovered_account_name == Some(account_id)
    }

    pub fn set_model_visibility(&mut self, model_id: String, is_visible: bool) -> io::Result<()> {
        self.model_visibility.set_visible(&model_id, is_visible);
        save_model_visibility_preferences(&self.model_visibility)
    }

    pub fn set_show_all_model_quotas(&mut self, show_all: bool) {
        self.show_all_model_quotas = show_all;
        self.show_antigravity_quota_groups = false;
    }

    pub fn set_show_antigravity_quota_groups(&mut self, show_groups: bool) {
        self.show_antigravity_quota_groups = show_groups;
    }

    pub fn set_error(&mut self) {
        self.is_loading = false;
        self.failed = true;
    }

    pub fn begin_alias_edit(&mut self, account_id: AccountId) {
        let Some((display_name, original_label)) = self
            .entries
            .iter()
            .find(|entry| entry.account.id == account_id)
            .map(|entry| (account_name(&entry.account), entry.account.label.clone()))
        else {
            return;
        };

        self.clear_hovered_account_name(account_id);
        self.alias_editor = Some(AliasEditor {
            account_id,
            draft: display_name,
            original_label,
            is_saving: false,
            failed: false,
        });
    }

    pub fn update_alias_draft(&mut self, account_id: AccountId, draft: String) {
        if let Some(editor) = self
            .alias_editor
            .as_mut()
            .filter(|editor| editor.account_id == account_id && !editor.is_saving)
        {
            editor.draft = draft;
            editor.failed = false;
        }
    }

    pub fn cancel_alias_edit(&mut self, account_id: AccountId) {
        if self
            .alias_editor
            .as_ref()
            .is_some_and(|editor| editor.account_id == account_id && !editor.is_saving)
        {
            self.alias_editor = None;
        }
    }

    pub fn begin_alias_save(&mut self, account_id: AccountId) -> Option<(String, String)> {
        let editor = self
            .alias_editor
            .as_mut()
            .filter(|editor| editor.account_id == account_id && !editor.is_saving)?;
        editor.is_saving = true;
        editor.failed = false;
        Some((editor.draft.clone(), editor.original_label.clone()))
    }

    pub fn finish_alias_save(&mut self, account_id: AccountId, failed: bool) {
        if let Some(editor) = self
            .alias_editor
            .as_mut()
            .filter(|editor| editor.account_id == account_id)
        {
            if failed {
                editor.is_saving = false;
                editor.failed = true;
            } else {
                self.alias_editor = None;
            }
        }
    }
}

pub fn view(
    state: &DashboardState,
    tab: DashboardTab,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let accounts = state.ordered_entries(tab);
    let account_count = accounts.len();

    let body: Element<'static, Message> = if state.is_loading && !state.has_loaded {
        centered_note(locale::text(language, Text::LoadingAccounts), theme)
    } else if state.failed && !state.has_loaded {
        centered_note(
            locale::text(language, Text::AccountsDatabaseUnavailable),
            theme,
        )
    } else if accounts.is_empty() {
        centered_note(
            locale::text(
                language,
                if tab == DashboardTab::Favorites {
                    Text::NoFavoriteAccounts
                } else {
                    Text::NoAccountsForProvider
                },
            ),
            theme,
        )
    } else {
        let mut account_sections = Vec::with_capacity(accounts.len() * 2);
        for (index, entry) in accounts.into_iter().enumerate() {
            if index > 0 {
                account_sections.push(account_separator(theme));
            }
            account_sections.push(account_card(
                entry,
                (index > 0, index + 1 < account_count),
                state.alias_editor.as_ref(),
                &state.usage_animation,
                state.account_name_is_hovered(entry.account.id),
                state.is_favorite(entry.account.id),
                state.model_visibility_menu_open(entry.account.id),
                &state.model_visibility,
                state.show_all_model_quotas,
                state.show_antigravity_quota_groups,
                &state.codex_desktop,
                theme,
                language,
            ));
        }

        crate::smooth_scroll::smooth_scroll(
            tab.scroll_key(),
            scrollable(column(account_sections).spacing(0).width(Fill))
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::hidden(),
                ))
                .width(Fill)
                .height(Fill),
        )
    };

    container(body)
        .padding([8, 10])
        .width(Fill)
        .height(Fill)
        .into()
}

/// The account's email as shown to the user. Accounts added from an API key
/// have no email; they hold a placeholder on the reserved `.invalid` domain,
/// which is never shown.
pub(crate) fn shown_email(email: &str) -> &str {
    let is_placeholder = email.trim().rsplit_once('@').is_some_and(|(_, domain)| {
        let domain = domain.to_ascii_lowercase();
        domain == "invalid" || domain.ends_with(".invalid")
    });
    if is_placeholder { "" } else { email }
}

/// The name an account card shows: the user's alias, else the email for
/// providers signed in with one (Codex, Claude, Antigravity), else the label.
pub(crate) fn account_name(account: &AccountRecord) -> String {
    if account.alias.is_none() && name_is_email(account) {
        return account.email.clone();
    }
    account.display_name().to_owned()
}

/// Whether [`account_name`] already shows the account's email.
pub(crate) fn name_is_email(account: &AccountRecord) -> bool {
    account.alias.is_none()
        && !shown_email(&account.email).is_empty()
        && [
            UsageProvider::Codex,
            UsageProvider::Claude,
            UsageProvider::Antigravity,
        ]
        .into_iter()
        .any(|provider| belongs_to_provider(&account.provider_id, provider))
}

pub(crate) fn belongs_to_provider(provider_id: &str, provider: UsageProvider) -> bool {
    match provider {
        UsageProvider::Codex => matches!(provider_id, "openai" | "codex"),
        UsageProvider::Claude => provider_id == "claude",
        UsageProvider::Antigravity => provider_id == "antigravity",
        UsageProvider::OpenCodeGo => provider_id == "opencodego",
        UsageProvider::OpenRouter => provider_id == "openrouter",
        UsageProvider::DeepSeek => provider_id == "deepseek",
        UsageProvider::Copilot => provider_id == "copilot",
    }
}

#[cfg(test)]
mod tests;
