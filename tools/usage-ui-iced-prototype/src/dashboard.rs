use chrono::{DateTime, Local, Utc};
use codex_usage_core::{
    accounts::{AccountId, AccountRecord, AccountStatus, AccountStore},
    codex_desktop,
    storage::{SqliteStore, default_accounts_database_path},
    usage::{
        RateLimitWindow, SpendSnapshot, UsageCreditInventory, UsageCreditRecord, UsageMetric,
        UsagePrimaryWindowKind, UsageSnapshot, UsageSnapshotStore, UsageWindowKind,
    },
};
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
    icon_pencil, icon_x,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs, io,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const ACCOUNT_ORDER_FILE: &str = "account-order.txt";
/// Rename (24) + move up (22) + move down (22) + two 1px gaps.
const HOVER_CONTROLS_WIDTH: f32 = 70.0;
const HIDE_ANTIGRAVITY_CLAUDE_GPT_FILE: &str = "antigravity-hide-claude-gpt.txt";

/// Whether the Antigravity "Claude and GPT models" group is hidden.
static HIDE_ANTIGRAVITY_CLAUDE_GPT: AtomicBool = AtomicBool::new(false);

fn antigravity_claude_gpt_hidden() -> bool {
    HIDE_ANTIGRAVITY_CLAUDE_GPT.load(Ordering::Relaxed)
}

use crate::{
    Message, PROVIDER_TABS, UsageProvider,
    locale::{self, Language, Text},
    typography,
};

#[derive(Debug, Clone)]
pub struct AccountUsageEntry {
    pub account: AccountRecord,
    pub snapshot: Option<UsageSnapshot>,
}

const USAGE_CHANGE_ANIMATION_DURATION: Duration = Duration::from_millis(420);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct UsagePercentKey {
    account_id: AccountId,
    field: String,
}

#[derive(Debug, Clone, Copy)]
struct UsagePercentTransition {
    from_remaining: f64,
    to_remaining: f64,
    started_at: Instant,
}

impl UsagePercentTransition {
    fn value_at(self, now: Instant) -> f64 {
        let progress = (now.saturating_duration_since(self.started_at).as_secs_f64()
            / USAGE_CHANGE_ANIMATION_DURATION.as_secs_f64())
        .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.from_remaining + (self.to_remaining - self.from_remaining) * eased
    }

    fn is_active(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < USAGE_CHANGE_ANIMATION_DURATION
    }
}

#[derive(Default)]
struct UsageAnimationState {
    transitions: HashMap<UsagePercentKey, UsagePercentTransition>,
}

impl UsageAnimationState {
    fn update(
        &mut self,
        previous_entries: &[AccountUsageEntry],
        next_entries: &[AccountUsageEntry],
        now: Instant,
    ) {
        let previous_values = collect_usage_percent_values(previous_entries);
        let next_values = collect_usage_percent_values(next_entries);
        let previous_transitions = std::mem::take(&mut self.transitions);
        let mut next_transitions = HashMap::new();

        for (key, target) in next_values {
            let Some(previous_target) = previous_values.get(&key).copied() else {
                continue;
            };
            let active_transition = previous_transitions
                .get(&key)
                .copied()
                .filter(|transition| transition.is_active(now));

            if usage_percent_is_close(previous_target, target) {
                if let Some(transition) = active_transition
                    .filter(|transition| usage_percent_is_close(transition.to_remaining, target))
                {
                    next_transitions.insert(key, transition);
                }
                continue;
            }

            let from = active_transition
                .map(|transition| transition.value_at(now))
                .unwrap_or(previous_target);
            if !usage_percent_is_close(from, target) {
                next_transitions.insert(
                    key,
                    UsagePercentTransition {
                        from_remaining: from,
                        to_remaining: target,
                        started_at: now,
                    },
                );
            }
        }

        self.transitions = next_transitions;
    }

    fn is_active(&self) -> bool {
        !self.transitions.is_empty()
    }

    fn advance(&mut self, now: Instant) {
        self.transitions
            .retain(|_, transition| transition.is_active(now));
    }

    fn has_transitions_for(&self, account_id: AccountId) -> bool {
        self.transitions
            .keys()
            .any(|key| key.account_id == account_id)
    }

    fn animated_snapshot(
        &self,
        account_id: AccountId,
        snapshot: &UsageSnapshot,
    ) -> Option<UsageSnapshot> {
        if snapshot.account_id != account_id || !self.has_transitions_for(account_id) {
            return None;
        }

        let now = Instant::now();
        let mut animated = snapshot.clone();
        if let Some(window) = &mut animated.primary {
            animate_used_percent(
                &self.transitions,
                account_id,
                "window:primary",
                &mut window.used_percent,
                now,
            );
        }
        if let Some(window) = &mut animated.secondary {
            animate_used_percent(
                &self.transitions,
                account_id,
                "window:secondary",
                &mut window.used_percent,
                now,
            );
        }
        for window in &mut animated.additional_windows {
            animate_used_percent(
                &self.transitions,
                account_id,
                &format!("window:additional:{}", window.key),
                &mut window.window.used_percent,
                now,
            );
        }
        for metric in &mut animated.metrics {
            if let Some(used_percent) = &mut metric.used_percent {
                animate_used_percent(
                    &self.transitions,
                    account_id,
                    &format!("metric:{}", metric.key),
                    used_percent,
                    now,
                );
            }
        }
        if let Some(spend) = &mut animated.spend
            && let Some(used_percent) = &mut spend.used_percent
        {
            animate_used_percent(&self.transitions, account_id, "spend", used_percent, now);
        }
        if let Some(limit) = animated
            .credits
            .as_mut()
            .and_then(|credits| credits.limit.as_mut())
            && let Some(used_percent) = &mut limit.used_percent
        {
            animate_used_percent(
                &self.transitions,
                account_id,
                "credit-limit",
                used_percent,
                now,
            );
        }

        Some(animated)
    }
}

fn collect_usage_percent_values(entries: &[AccountUsageEntry]) -> HashMap<UsagePercentKey, f64> {
    let mut values = HashMap::new();

    for entry in entries {
        let Some(snapshot) = entry.snapshot.as_ref().filter(|snapshot| {
            snapshot.account_id == entry.account.id
                && providers_match(&entry.account.provider_id, &snapshot.provider_id)
                && !snapshot.observed_email.as_deref().is_some_and(|email| {
                    !email
                        .trim()
                        .eq_ignore_ascii_case(entry.account.email.trim())
                })
        }) else {
            continue;
        };

        if let Some(window) = &snapshot.primary {
            insert_usage_percent(
                &mut values,
                entry.account.id,
                "window:primary",
                window.used_percent,
            );
        }
        if let Some(window) = &snapshot.secondary {
            insert_usage_percent(
                &mut values,
                entry.account.id,
                "window:secondary",
                window.used_percent,
            );
        }
        for window in &snapshot.additional_windows {
            insert_usage_percent(
                &mut values,
                entry.account.id,
                &format!("window:additional:{}", window.key),
                window.window.used_percent,
            );
        }
        for metric in &snapshot.metrics {
            if let Some(used_percent) = metric.used_percent {
                insert_usage_percent(
                    &mut values,
                    entry.account.id,
                    &format!("metric:{}", metric.key),
                    used_percent,
                );
            }
        }
        if let Some(used_percent) = snapshot.spend.as_ref().and_then(|spend| spend.used_percent) {
            insert_usage_percent(&mut values, entry.account.id, "spend", used_percent);
        }
        if let Some(used_percent) = snapshot
            .credits
            .as_ref()
            .and_then(|credits| credits.limit.as_ref())
            .and_then(|limit| limit.used_percent)
        {
            insert_usage_percent(&mut values, entry.account.id, "credit-limit", used_percent);
        }
    }

    values
}

fn insert_usage_percent(
    values: &mut HashMap<UsagePercentKey, f64>,
    account_id: AccountId,
    field: &str,
    used_percent: f64,
) {
    if used_percent.is_finite() {
        values.insert(
            UsagePercentKey {
                account_id,
                field: field.to_owned(),
            },
            (100.0 - used_percent).clamp(0.0, 100.0),
        );
    }
}

fn usage_percent_is_close(left: f64, right: f64) -> bool {
    (left - right).abs() < 0.02
}

fn animate_used_percent(
    transitions: &HashMap<UsagePercentKey, UsagePercentTransition>,
    account_id: AccountId,
    field: &str,
    used_percent: &mut f64,
    now: Instant,
) {
    if !used_percent.is_finite() {
        return;
    }

    let key = UsagePercentKey {
        account_id,
        field: field.to_owned(),
    };
    if let Some(transition) = transitions.get(&key) {
        *used_percent = 100.0 - transition.value_at(now);
    }
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
}

/// Which saved Codex account the Codex desktop app is signed in with, and
/// the progress of a switch requested from the dashboard.
#[derive(Default)]
struct CodexDesktopState {
    active_account: Option<AccountId>,
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
                ..CodexDesktopState::default()
            },
            account_order: load_account_order(),
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
    fn ordered_entries(&self, provider: UsageProvider) -> Vec<&AccountUsageEntry> {
        let mut accounts = self
            .entries
            .iter()
            .filter(|entry| belongs_to_provider(&entry.account.provider_id, provider))
            .collect::<Vec<_>>();
        accounts.sort_by_key(|entry| account_rank(&self.account_order, entry.account.id));
        accounts
    }

    /// Moves an account one place up (`-1`) or down (`1`) within its tab and
    /// saves the new order.
    pub fn move_account(&mut self, account_id: AccountId, offset: isize) -> io::Result<()> {
        let Some(provider) = self
            .entries
            .iter()
            .find(|entry| entry.account.id == account_id)
            .and_then(|entry| provider_tab(&entry.account.provider_id))
        else {
            return Ok(());
        };
        let mut tab_ids = self
            .ordered_entries(provider)
            .iter()
            .map(|entry| entry.account.id)
            .collect::<Vec<_>>();
        let Some(index) = tab_ids.iter().position(|id| *id == account_id) else {
            return Ok(());
        };
        let Some(target) = index
            .checked_add_signed(offset)
            .filter(|target| *target < tab_ids.len())
        else {
            return Ok(());
        };
        tab_ids.swap(index, target);

        // Rebuild the global order: other tabs keep their places, and this
        // tab's slots take its new sequence.
        let mut all = self.entries.iter().collect::<Vec<_>>();
        all.sort_by_key(|entry| account_rank(&self.account_order, entry.account.id));
        let mut reordered = tab_ids.into_iter();
        self.account_order = all
            .iter()
            .map(|entry| {
                if belongs_to_provider(&entry.account.provider_id, provider) {
                    reordered.next().unwrap_or(entry.account.id)
                } else {
                    entry.account.id
                }
            })
            .collect();
        save_account_order(&self.account_order)
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
    }

    pub fn set_accounts(&mut self, entries: Vec<AccountUsageEntry>) {
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
    }

    pub fn update_account_usage(&mut self, entry: AccountUsageEntry) {
        let mut entries = self.entries.clone();
        if let Some(existing) = entries
            .iter_mut()
            .find(|existing| existing.account.id == entry.account.id)
        {
            *existing = entry;
        } else {
            entries.push(entry);
        }
        self.set_accounts(entries);
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
            .map(|entry| {
                (
                    entry.account.display_name().to_owned(),
                    entry.account.label.clone(),
                )
            })
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

fn account_rank(order: &[AccountId], account_id: AccountId) -> usize {
    order
        .iter()
        .position(|id| *id == account_id)
        .unwrap_or(usize::MAX)
}

fn provider_tab(provider_id: &str) -> Option<UsageProvider> {
    PROVIDER_TABS
        .iter()
        .map(|tab| tab.provider)
        .find(|provider| belongs_to_provider(provider_id, *provider))
}

fn load_account_order() -> Vec<AccountId> {
    crate::theme::preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(ACCOUNT_ORDER_FILE)).ok())
        .map(|contents| {
            contents
                .lines()
                .filter_map(|line| line.trim().parse().ok().map(AccountId))
                .collect()
        })
        .unwrap_or_default()
}

fn save_account_order(order: &[AccountId]) -> io::Result<()> {
    let directory = crate::theme::preference_directory()?;
    fs::create_dir_all(&directory)?;
    let contents = order
        .iter()
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(directory.join(ACCOUNT_ORDER_FILE), contents)
}

fn load_hide_antigravity_claude_gpt() -> bool {
    crate::theme::preference_directory()
        .ok()
        .and_then(|directory| {
            fs::read_to_string(directory.join(HIDE_ANTIGRAVITY_CLAUDE_GPT_FILE)).ok()
        })
        .is_some_and(|value| value.trim() == "hide")
}

fn current_codex_desktop_account() -> Option<AccountId> {
    codex_desktop::active_account(&codex_desktop::CodexDesktopPaths::from_environment())
}

pub async fn load_saved_accounts() -> Result<Vec<AccountUsageEntry>, String> {
    let path = default_accounts_database_path();
    // The read-only open never creates the database. Before the first account
    // is added there is simply nothing to show, which is not an error.
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let store = SqliteStore::open_read_only(path).map_err(|error| error.to_string())?;
    let accounts = store.list().await.map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(accounts.len());

    for account in accounts {
        let snapshot = store
            .get_latest(account.id)
            .await
            .map_err(|error| error.to_string())?;
        entries.push(AccountUsageEntry { account, snapshot });
    }

    Ok(entries)
}

fn load_model_visibility_preferences() -> ModelVisibilityPreferences {
    crate::theme::preference_directory()
        .ok()
        .map(|directory| directory.join("model-visibility.txt"))
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| parse_model_visibility_preferences(&contents))
        .unwrap_or_default()
}

fn parse_model_visibility_preferences(contents: &str) -> ModelVisibilityPreferences {
    let mut preferences = ModelVisibilityPreferences::default();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(model_id) = line.strip_prefix("visible:") {
            preferences
                .visible_model_ids
                .insert(normalize_model_id(model_id));
        } else {
            let model_id = line.strip_prefix("hidden:").unwrap_or(line);
            preferences
                .hidden_model_ids
                .insert(normalize_model_id(model_id));
        }
    }
    preferences
}

fn save_model_visibility_preferences(preferences: &ModelVisibilityPreferences) -> io::Result<()> {
    let directory = crate::theme::preference_directory()?;
    fs::create_dir_all(&directory)?;

    // Plain legacy lines still mean hidden. Visible overrides use an explicit
    // prefix so old preference files remain readable and user choices survive.
    let mut preferences = preferences
        .hidden_model_ids
        .iter()
        .cloned()
        .chain(
            preferences
                .visible_model_ids
                .iter()
                .map(|model_id| format!("visible:{model_id}")),
        )
        .collect::<Vec<_>>();
    preferences.sort_unstable();
    fs::write(
        directory.join("model-visibility.txt"),
        preferences.join("\n"),
    )
}

pub async fn save_account_alias(
    account_id: AccountId,
    draft: String,
    original_label: String,
) -> Result<Vec<AccountUsageEntry>, String> {
    let store =
        SqliteStore::open(default_accounts_database_path()).map_err(|error| error.to_string())?;
    store
        .set_alias(
            account_id,
            normalized_alias(&draft, &original_label).as_deref(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "The account no longer exists".to_owned())?;
    drop(store);
    load_saved_accounts().await
}

#[cfg(target_os = "windows")]
pub async fn delete_saved_account(account_id: AccountId) -> Result<Vec<AccountUsageEntry>, String> {
    use codex_usage_core::auth::{AccountAuthMaterialStore, OAuthCredentialStore};
    use codex_usage_windows_auth::{
        WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    };

    let store =
        SqliteStore::open(default_accounts_database_path()).map_err(|error| error.to_string())?;
    if store
        .get(account_id)
        .await
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err("The account no longer exists".to_owned());
    }

    WindowsCredentialManagerStore
        .remove(account_id)
        .await
        .map_err(|error| format!("Could not remove saved OAuth credentials: {error}"))?;
    WindowsCredentialManagerAuthMaterialStore
        .remove(account_id)
        .await
        .map_err(|error| format!("Could not remove saved sign-in credentials: {error}"))?;
    store
        .remove(account_id)
        .await
        .map_err(|error| error.to_string())?;

    drop(store);
    load_saved_accounts().await
}

#[cfg(not(target_os = "windows"))]
pub async fn delete_saved_account(
    _account_id: AccountId,
) -> Result<Vec<AccountUsageEntry>, String> {
    Err("Account deletion is only available on Windows".to_owned())
}

fn normalized_alias(draft: &str, original_label: &str) -> Option<String> {
    let trimmed = draft.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(original_label.trim()) {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn capitalize_first(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };

    first.to_uppercase().chain(characters).collect()
}

pub fn view(
    state: &DashboardState,
    provider: UsageProvider,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let accounts = state.ordered_entries(provider);
    let account_count = accounts.len();

    let body: Element<'static, Message> = if state.is_loading && !state.has_loaded {
        centered_note(locale::text(language, Text::LoadingAccounts), theme)
    } else if state.failed && !state.has_loaded {
        centered_note(
            locale::text(language, Text::AccountsDatabaseUnavailable),
            theme,
        )
    } else if accounts.is_empty() {
        centered_note(locale::text(language, Text::NoAccountsForProvider), theme)
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
                state.model_visibility_menu_open(entry.account.id),
                &state.model_visibility,
                state.show_all_model_quotas,
                state.show_antigravity_quota_groups,
                &state.codex_desktop,
                theme,
                language,
            ));
        }

        scrollable(column(account_sections).spacing(0).width(Fill))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(Fill)
            .into()
    };

    container(body)
        .padding([8, 10])
        .width(Fill)
        .height(Fill)
        .into()
}

pub(crate) fn belongs_to_provider(provider_id: &str, provider: UsageProvider) -> bool {
    match provider {
        UsageProvider::Codex => matches!(provider_id, "openai" | "codex"),
        UsageProvider::Claude => provider_id == "claude",
        UsageProvider::Antigravity => provider_id == "antigravity",
        UsageProvider::OpenCodeGo => provider_id == "opencodego",
        UsageProvider::OpenRouter => provider_id == "openrouter",
    }
}

fn account_card(
    entry: &AccountUsageEntry,
    (can_move_up, can_move_down): (bool, bool),
    alias_editor: Option<&AliasEditor>,
    usage_animation: &UsageAnimationState,
    account_name_hovered: bool,
    model_visibility_menu_open: bool,
    model_visibility: &ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    show_antigravity_quota_groups: bool,
    codex_desktop: &CodexDesktopState,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let account = &entry.account;
    let account_id = account.id;
    let editing = alias_editor.filter(|editor| editor.account_id == account_id);
    let mut identity_children: Vec<Element<'static, Message>> = Vec::with_capacity(4);
    let identity_provider = PROVIDER_TABS
        .iter()
        .find(|tab| belongs_to_provider(&account.provider_id, tab.provider))
        .map(|tab| tab.provider);
    if let Some(provider) = identity_provider {
        identity_children.push(
            image(crate::provider_logo_handle(provider, theme.colors.is_light))
                .width(20)
                .height(20)
                .content_fit(ContentFit::Contain)
                .into(),
        );
    }

    if let Some(editor) = editing {
        let input = text_input(locale::text(language, Text::NamePlaceholder), &editor.draft)
            .size(typography::BODY_SIZE)
            .padding([5, 8])
            .on_input(move |draft| Message::AliasDraftChanged(account_id, draft))
            .on_submit(Message::SaveAlias(account_id))
            .width(Length::Fixed(184.0))
            .style(move |framework_theme, status| {
                let mut style = text_input::default(framework_theme, status);
                let border_color = match status {
                    text_input::Status::Focused { .. } => theme.accent_color().scale_alpha(0.78),
                    text_input::Status::Hovered => theme.colors.border(0.30),
                    _ => theme.colors.border(0.18),
                };
                style.background = Background::Color(theme.colors.control_surface());
                style.border = Border {
                    color: border_color,
                    width: 1.0,
                    radius: 7.0.into(),
                };
                style.icon = muted_text(theme);
                style.placeholder = muted_text(theme);
                style.value = theme.colors.text();
                style.selection = theme.accent_color().scale_alpha(0.42);
                style
            });
        let save_label = if editor.is_saving {
            locale::text(language, Text::Saving)
        } else {
            locale::text(language, Text::Save)
        };
        identity_children.push(input.into());
        identity_children.push(
            button(text(save_label).size(typography::CONTROL_SIZE))
                .padding([5, 9])
                .on_press(Message::SaveAlias(account_id))
                .style(move |framework_theme, status| {
                    alias_action_style(framework_theme, theme, true, status)
                })
                .into(),
        );
        identity_children.push(
            button(text(locale::text(language, Text::Cancel)).size(typography::CONTROL_SIZE))
                .padding([5, 9])
                .on_press(Message::CancelAliasEdit(account_id))
                .style(move |framework_theme, status| {
                    alias_action_style(framework_theme, theme, false, status)
                })
                .into(),
        );
    } else {
        let edit_slot: Element<'static, Message> = if account_name_hovered {
            row![
                edit_name_button(account_id, theme, language),
                move_account_button(account_id, -1, can_move_up, theme, language),
                move_account_button(account_id, 1, can_move_down, theme, language),
            ]
            .spacing(1)
            .align_y(Alignment::Center)
            .width(HOVER_CONTROLS_WIDTH)
            .into()
        } else {
            // Reserve the controls' width so hovering never reflows the header.
            space().width(HOVER_CONTROLS_WIDTH).height(24).into()
        };
        let name_and_edit = row![
            text(account.display_name().to_owned())
                .size(typography::ACCOUNT_NAME_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .wrapping(text::Wrapping::None),
            edit_slot,
        ]
        .spacing(5)
        .align_y(Alignment::Center);
        identity_children.push(
            mouse_area(name_and_edit)
                .on_enter(Message::AccountNameHovered(account_id))
                .on_exit(Message::AccountNameHoverEnded(account_id))
                .into(),
        );
    }
    let identity: Element<'static, Message> = iced::widget::Row::with_children(identity_children)
        .spacing(5)
        .align_y(Alignment::Center)
        .into();

    let visibility_snapshot = entry.snapshot.as_ref().filter(|snapshot| {
        providers_match(&account.provider_id, &snapshot.provider_id)
            && !snapshot
                .observed_email
                .as_deref()
                .is_some_and(|email| !email.trim().eq_ignore_ascii_case(account.email.trim()))
    });
    let model_visibility_entries = if model_visibility_menu_open {
        visibility_snapshot
            .map(|snapshot| model_quota_menu_entries(&snapshot.metrics))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let has_model_quotas = visibility_snapshot
        .is_some_and(|snapshot| snapshot.metrics.iter().any(is_model_quota_metric));
    let has_hidden_model_quotas = if model_visibility_menu_open {
        model_visibility_entries
            .iter()
            .any(|(model_id, _)| !model_visibility.is_visible(model_id))
    } else {
        visibility_snapshot
            .is_some_and(|snapshot| has_hidden_model_quota(&snapshot.metrics, model_visibility))
    };

    let spacer_width = if editing.is_some() {
        Length::Shrink
    } else {
        Fill
    };
    let mut header = row![identity, space().width(spacer_width)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(badge) = status_badge(account.status, theme, language) {
        header = header.push(badge);
    }
    let is_antigravity_account =
        belongs_to_provider(&account.provider_id, UsageProvider::Antigravity);
    let has_antigravity_summary = entry.snapshot.as_ref().is_some_and(|snapshot| {
        providers_match(&account.provider_id, &snapshot.provider_id)
            && !snapshot
                .observed_email
                .as_deref()
                .is_some_and(|email| !email.trim().eq_ignore_ascii_case(account.email.trim()))
            && snapshot.metrics.iter().any(is_antigravity_summary_metric)
    });
    if has_model_quotas || (is_antigravity_account && has_antigravity_summary) {
        header = header.push(model_visibility_button(
            account_id,
            model_visibility_menu_open,
            &model_visibility_entries,
            model_visibility,
            has_hidden_model_quotas,
            show_all_model_quotas,
            is_antigravity_account,
            is_antigravity_account && show_antigravity_quota_groups,
            theme,
            language,
        ));
    }
    let is_codex_account = belongs_to_provider(&account.provider_id, UsageProvider::Codex);
    if is_codex_account && editing.is_none() {
        header = header.push(codex_desktop_button(
            account_id,
            codex_desktop,
            theme,
            language,
        ));
    }
    let mut rows: Vec<Element<'static, Message>> = vec![header.width(Fill).into()];

    if let Some((_, error)) = codex_desktop
        .failure
        .as_ref()
        .filter(|(failed_account, _)| *failed_account == account_id)
    {
        let message = format!(
            "{}: {error}",
            locale::text(language, Text::CodexSwitchFailed)
        );
        rows.push(warning_line(&message, theme));
    }

    if editing.is_some_and(|editor| editor.failed) {
        rows.push(warning_line(
            locale::text(language, Text::NameSaveFailed),
            theme,
        ));
    }

    let stored_snapshot = entry.snapshot.as_ref();
    let animated_snapshot = stored_snapshot
        .and_then(|snapshot| usage_animation.animated_snapshot(account_id, snapshot));
    let snapshot = animated_snapshot.as_ref().or(stored_snapshot);
    let plan_type = snapshot.and_then(|snapshot| snapshot.plan_type.clone());
    if account.workspace_name.is_some() || !account.email.is_empty() || plan_type.is_some() {
        let mut metadata = row![
            text(account.email.clone())
                .size(typography::METADATA_SIZE)
                .color(muted_text(theme)),
            space().width(Fill),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .width(Fill);
        if let Some(workspace_name) = account.workspace_name.clone() {
            metadata = metadata.push(
                text(workspace_name)
                    .size(typography::METADATA_SIZE)
                    .font(typography::EMPHASIS)
                    .color(muted_text(theme)),
            );
        }
        if let Some(plan_type) = plan_type {
            metadata = metadata.push(
                text(capitalize_first(&plan_type))
                    .size(typography::METADATA_SIZE)
                    .font(typography::EMPHASIS)
                    .color(theme.colors.text()),
            );
        }
        rows.push(metadata.into());
    }

    if let Some(snapshot) = snapshot {
        if !providers_match(&account.provider_id, &snapshot.provider_id) {
            rows.push(warning_line(
                locale::text(language, Text::ProviderMismatch),
                theme,
            ));
        } else if snapshot
            .observed_email
            .as_deref()
            .is_some_and(|email| !email.trim().eq_ignore_ascii_case(account.email.trim()))
        {
            let observed_email = snapshot.observed_email.as_deref().unwrap_or_default();
            let message = match language {
                Language::English => format!(
                    "{} ({observed_email}); details hidden",
                    locale::text(language, Text::IdentityMismatch)
                ),
                Language::Arabic => format!(
                    "{} ({observed_email})؛ لم نعرض تفاصيلها",
                    locale::text(language, Text::IdentityMismatch)
                ),
            };
            rows.push(warning_line(&message, theme));
        } else {
            append_snapshot_rows(
                &mut rows,
                snapshot,
                !belongs_to_provider(&account.provider_id, UsageProvider::Codex),
                model_visibility,
                show_all_model_quotas,
                is_antigravity_account && show_antigravity_quota_groups,
                theme,
                language,
            );
        }
    } else {
        rows.push(warning_line(
            locale::text(language, Text::NoSavedUsage),
            theme,
        ));
    }

    container(column(rows).spacing(5).width(Fill))
        .width(Fill)
        .padding([9, 10])
        .into()
}

fn account_separator(theme: &'static crate::theme::ThemeDefinition) -> Element<'static, Message> {
    container(space().height(Length::Fill))
        .width(Fill)
        .height(1)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.text().scale_alpha(0.24))),
            ..Default::default()
        })
        .into()
}

/// "Use in Codex" for a saved Codex account, or a marker on the account the
/// Codex desktop app is currently signed in with.
fn codex_desktop_button(
    account_id: AccountId,
    codex_desktop: &CodexDesktopState,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let is_active = codex_desktop.active_account == Some(account_id);
    let is_switching = codex_desktop.switching == Some(account_id);
    let (glyph, label, tip) = if is_switching {
        (
            icon_arrow_left_right(),
            Text::CodexSwitching,
            Text::CodexSwitching,
        )
    } else if is_active {
        (icon_check(), Text::InCodex, Text::InCodexHint)
    } else {
        (
            icon_arrow_left_right(),
            Text::UseInCodex,
            Text::UseInCodexHint,
        )
    };
    let accent = theme.accent_color();
    let glyph_color = if is_active {
        accent
    } else if is_switching {
        muted_text(theme)
    } else {
        theme.colors.text()
    };
    // Icon-only so it always fits beside long account names; the tooltip
    // names the action.
    let mut control = button(container(glyph.size(14).color(glyph_color)).center(26))
        .width(26)
        .height(26)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            style.background = Some(Background::Color(if is_active {
                accent.scale_alpha(0.18)
            } else if hovered {
                theme.colors.hover()
            } else {
                theme.colors.control_surface()
            }));
            style.text_color = theme.colors.text();
            style.border = Border {
                color: if is_active {
                    accent.scale_alpha(0.62)
                } else {
                    theme.colors.border(0.18)
                },
                width: 1.0,
                radius: 7.0.into(),
            };
            style.shadow = Default::default();
            style
        });
    // The active account can be pressed again to restart Codex on it.
    if codex_desktop.switching.is_none() {
        control = control.on_press(Message::SwitchCodexDesktopAccount(account_id));
    }

    let tip = if is_switching {
        locale::text(language, tip).to_owned()
    } else {
        format!(
            "{} · {}",
            locale::text(language, label),
            locale::text(language, tip)
        )
    };
    crate::hint::hint(control, tip, theme)
}

fn move_account_button(
    account_id: AccountId,
    offset: isize,
    enabled: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let glyph = if offset < 0 {
        icon_chevron_up()
    } else {
        icon_chevron_down()
    };
    let color = if enabled {
        theme.colors.text()
    } else {
        muted_text(theme).scale_alpha(0.45)
    };
    let mut control = button(container(glyph.size(13).color(color)).center(22))
        .width(22)
        .height(24)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = (enabled
                && matches!(status, button::Status::Hovered | button::Status::Pressed))
            .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 6.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });
    if enabled {
        control = control.on_press(Message::MoveAccount(account_id, offset));
    }
    let tip = if offset < 0 {
        Text::MoveAccountUp
    } else {
        Text::MoveAccountDown
    };
    crate::hint::hint(control, locale::text(language, tip), theme)
}

fn edit_name_button(
    account_id: AccountId,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let edit_button =
        button(container(icon_pencil().size(13).color(theme.colors.text())).center(24))
            .on_press(Message::BeginAliasEdit(account_id))
            .width(24)
            .height(24)
            .padding(0)
            .style(move |framework_theme, status| {
                let mut style = button::text(framework_theme, status);
                style.background =
                    matches!(status, button::Status::Hovered | button::Status::Pressed)
                        .then(|| Background::Color(theme.colors.hover()));
                style.text_color = theme.colors.text();
                style.border = Border {
                    radius: 6.0.into(),
                    ..Border::default()
                };
                style.shadow = Default::default();
                style
            });

    crate::hint::hint(edit_button, locale::text(language, Text::EditName), theme)
}

fn alias_action_style(
    framework_theme: &iced::Theme,
    theme: &'static crate::theme::ThemeDefinition,
    primary: bool,
    status: button::Status,
) -> button::Style {
    let mut style = button::text(framework_theme, status);
    let is_hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let background = if is_hovered {
        theme.colors.hover()
    } else if primary {
        theme.accent_color().scale_alpha(0.14)
    } else {
        theme.colors.control_surface()
    };
    style.background = Some(Background::Color(background));
    style.text_color = theme.colors.text();
    style.border = Border {
        color: if primary {
            theme.accent_color().scale_alpha(0.62)
        } else {
            theme.colors.border(0.18)
        },
        width: 1.0,
        radius: 7.0.into(),
    };
    style.shadow = Default::default();
    style
}

fn model_visibility_button(
    account_id: AccountId,
    menu_open: bool,
    model_entries: &[(String, String)],
    model_visibility: &ModelVisibilityPreferences,
    has_hidden_model_quotas: bool,
    show_all_model_quotas: bool,
    is_antigravity_account: bool,
    show_antigravity_quota_groups: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let glyph = if has_hidden_model_quotas {
        icon_eye_off()
    } else {
        icon_eye()
    };
    let button = button(container(glyph.size(14).color(theme.colors.text())).center(26))
        .on_press(Message::ToggleModelVisibilityMenu(account_id))
        .width(26)
        .height(26)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = (menu_open
                || matches!(status, button::Status::Hovered | button::Status::Pressed))
            .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 7.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });

    let trigger = crate::hint::hint(button, locale::text(language, Text::ModelVisibility), theme);
    let menu: Element<'static, Message> = if menu_open {
        model_visibility_menu(
            account_id,
            model_entries,
            model_visibility,
            show_all_model_quotas,
            is_antigravity_account,
            show_antigravity_quota_groups,
            theme,
            language,
        )
    } else {
        space().into()
    };

    ModelVisibilityTrigger {
        trigger,
        menu,
        menu_open,
        dismiss_message: Message::CloseModelVisibilityMenu(account_id),
    }
    .into()
}

struct ModelVisibilityTrigger<'a> {
    trigger: Element<'a, Message>,
    menu: Element<'a, Message>,
    menu_open: bool,
    dismiss_message: Message,
}

impl<'content> Widget<Message, iced::Theme, iced::Renderer> for ModelVisibilityTrigger<'content> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::stateless()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.trigger), Tree::new(&self.menu)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.trigger, &self.menu]);
    }

    fn size(&self) -> Size<Length> {
        self.trigger.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.trigger
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.trigger.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.trigger.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.trigger
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.trigger.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'overlay>(
        &'overlay mut self,
        tree: &'overlay mut Tree,
        layout: Layout<'overlay>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'overlay, Message, iced::Theme, iced::Renderer>> {
        let (trigger_tree, menu_tree) = tree.children.split_at_mut(1);
        let trigger_overlay = self.trigger.as_widget_mut().overlay(
            &mut trigger_tree[0],
            layout,
            renderer,
            viewport,
            translation,
        );
        let menu_overlay = if self.menu_open {
            Some(overlay::Element::new(Box::new(ModelVisibilityPopup {
                target_bounds: layout.bounds() + translation,
                viewport: *viewport,
                menu: &mut self.menu,
                tree: &mut menu_tree[0],
                dismiss_message: self.dismiss_message.clone(),
            })))
        } else {
            None
        };
        let overlays = trigger_overlay
            .into_iter()
            .chain(menu_overlay)
            .collect::<Vec<_>>();

        (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
    }
}

impl<'a> From<ModelVisibilityTrigger<'a>> for Element<'a, Message> {
    fn from(trigger: ModelVisibilityTrigger<'a>) -> Self {
        Element::new(trigger)
    }
}

struct ModelVisibilityPopup<'b, 'a> {
    target_bounds: Rectangle,
    viewport: Rectangle,
    menu: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
    dismiss_message: Message,
}

impl<'borrow, 'content> overlay::Overlay<Message, iced::Theme, iced::Renderer>
    for ModelVisibilityPopup<'borrow, 'content>
where
    'content: 'borrow,
{
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        let viewport = if self.viewport.width > 0.0 && self.viewport.height > 0.0 {
            self.viewport
        } else {
            Rectangle::with_size(bounds)
        };
        let node = self.menu.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let size = node.size();
        let below = viewport.y + viewport.height - self.target_bounds.y - self.target_bounds.height;
        let above = self.target_bounds.y - viewport.y;
        let y = if below >= size.height || below >= above {
            self.target_bounds.y + self.target_bounds.height
        } else {
            self.target_bounds.y - size.height
        }
        .clamp(
            viewport.y,
            (viewport.y + viewport.height - size.height).max(viewport.y),
        );
        let x = (self.target_bounds.x + self.target_bounds.width - size.width).clamp(
            viewport.x,
            (viewport.x + viewport.width - size.width).max(viewport.x),
        );
        node.move_to(Point::new(x, y))
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ) && !cursor.is_over(layout.bounds())
        {
            shell.publish(self.dismiss_message.clone());
            shell.capture_event();
            return;
        }

        self.menu.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &self.viewport,
        );
    }

    fn draw(
        &self,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.menu.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &self.viewport,
        );
    }

    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.menu
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.menu
            .as_widget()
            .mouse_interaction(self.tree, layout, cursor, &self.viewport, renderer)
    }

    fn overlay<'overlay>(
        &'overlay mut self,
        layout: Layout<'overlay>,
        renderer: &iced::Renderer,
    ) -> Option<overlay::Element<'overlay, Message, iced::Theme, iced::Renderer>> {
        self.menu
            .as_widget_mut()
            .overlay(self.tree, layout, renderer, &self.viewport, Vector::ZERO)
    }
}

fn model_visibility_menu(
    account_id: AccountId,
    model_entries: &[(String, String)],
    model_visibility: &ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    is_antigravity_account: bool,
    show_antigravity_quota_groups: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let model_rows = model_entries
        .iter()
        .map(|(model_id, display_name)| {
            let model_id = model_id.clone();
            checkbox(model_visibility.is_visible(&model_id))
                .label(display_name.clone())
                .size(12)
                .spacing(7)
                .text_size(typography::METADATA_SIZE)
                .font(typography::BODY)
                .on_toggle(move |is_visible| {
                    Message::SetModelVisibility(model_id.clone(), is_visible)
                })
                .style(move |framework_theme, status| {
                    if theme.colors.is_light {
                        checkbox_style(theme, status)
                    } else {
                        iced::widget::checkbox::primary(framework_theme, status)
                    }
                })
                .into()
        })
        .collect::<Vec<Element<'static, Message>>>();

    let close_button = button(container(icon_x().size(12).color(muted_text(theme))).center(22))
        .on_press(Message::CloseModelVisibilityMenu(account_id))
        .width(22)
        .height(22)
        .padding(0)
        .style(move |framework_theme, status| {
            let mut style = button::text(framework_theme, status);
            style.background = matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then(|| Background::Color(theme.colors.hover()));
            style.text_color = theme.colors.text();
            style.border = Border {
                radius: 5.0.into(),
                ..Border::default()
            };
            style.shadow = Default::default();
            style
        });
    let mut mode_buttons = row![
        model_quota_mode_button(
            locale::text(language, Text::AllModels),
            show_all_model_quotas && !show_antigravity_quota_groups,
            true,
            theme,
        ),
        model_quota_mode_button(
            locale::text(language, Text::PinnedModels),
            !show_all_model_quotas && !show_antigravity_quota_groups,
            false,
            theme,
        ),
    ]
    .spacing(4)
    .width(Fill);
    if is_antigravity_account {
        mode_buttons = mode_buttons.push(antigravity_group_mode_button(
            locale::text(language, Text::AntigravityGroups),
            show_antigravity_quota_groups,
            theme,
        ));
    }

    let mut menu_content = column![
        row![
            text(locale::text(language, Text::ModelVisibility))
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
            space().width(Fill),
            close_button,
        ]
        .align_y(Alignment::Center)
        .width(Fill),
        mode_buttons,
    ]
    .spacing(5)
    .width(Fill);
    if is_antigravity_account && show_antigravity_quota_groups {
        menu_content = menu_content.push(
            checkbox(!antigravity_claude_gpt_hidden())
                .label(locale::text(language, Text::ShowClaudeGptGroup))
                .size(12)
                .spacing(7)
                .text_size(typography::METADATA_SIZE)
                .font(typography::BODY)
                .on_toggle(|show| Message::SetAntigravityClaudeGptHidden(!show))
                .style(move |framework_theme, status| {
                    if theme.colors.is_light {
                        checkbox_style(theme, status)
                    } else {
                        iced::widget::checkbox::primary(framework_theme, status)
                    }
                }),
        );
    }
    if !model_entries.is_empty() {
        let model_list = scrollable(column(model_rows).spacing(2))
            .height(Length::Fixed(176.0))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ));
        menu_content = menu_content.push(model_list);
    }

    container(menu_content)
        .width(250)
        .padding([7, 8])
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.window_surface())),
            border: Border {
                color: theme.colors.border(0.28),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn checkbox_style(
    theme: &'static crate::theme::ThemeDefinition,
    status: iced::widget::checkbox::Status,
) -> iced::widget::checkbox::Style {
    use iced::widget::checkbox::{Status, Style};

    let (is_checked, is_hovered, is_disabled) = match status {
        Status::Active { is_checked } => (is_checked, false, false),
        Status::Hovered { is_checked } => (is_checked, true, false),
        Status::Disabled { is_checked } => (is_checked, false, true),
    };
    let fill = if is_checked {
        theme.accent_color()
    } else if is_hovered {
        Color::from_rgb8(232, 239, 248)
    } else {
        theme.colors.control_surface()
    };
    let border = if is_checked {
        theme.accent_color()
    } else {
        theme.colors.border(0.58)
    };

    Style {
        background: Background::Color(if is_disabled {
            fill.scale_alpha(0.55)
        } else {
            fill
        }),
        icon_color: Color::WHITE,
        border: Border {
            color: border,
            width: 1.0,
            radius: 3.0.into(),
        },
        text_color: Some(theme.colors.text()),
    }
}

fn model_quota_mode_button(
    label: &'static str,
    selected: bool,
    show_all: bool,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::COMPACT_SIZE)
            .font(if selected {
                typography::EMPHASIS
            } else {
                typography::BODY
            }),
    )
    .on_press(Message::SetModelQuotaDisplay(show_all))
    .padding([4, 6])
    .width(Fill)
    .style(move |framework_theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = Some(Background::Color(if selected {
            theme.accent_color().scale_alpha(0.14)
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            theme.colors.hover()
        } else {
            theme.colors.control_surface()
        }));
        style.text_color = theme.colors.text();
        style.border = Border {
            color: if selected {
                theme.accent_color().scale_alpha(0.55)
            } else {
                theme.colors.border(0.18)
            },
            width: 1.0,
            radius: 5.0.into(),
        };
        style.shadow = Default::default();
        style
    })
    .into()
}

fn antigravity_group_mode_button(
    label: &'static str,
    selected: bool,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    button(
        text(label)
            .size(typography::COMPACT_SIZE)
            .font(if selected {
                typography::EMPHASIS
            } else {
                typography::BODY
            }),
    )
    .on_press(Message::SetAntigravityQuotaGroups(true))
    .padding([4, 6])
    .width(Fill)
    .style(move |framework_theme, status| {
        let mut style = button::text(framework_theme, status);
        style.background = Some(Background::Color(if selected {
            theme.accent_color().scale_alpha(0.14)
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            theme.colors.hover()
        } else {
            theme.colors.control_surface()
        }));
        style.text_color = theme.colors.text();
        style.border = Border {
            color: if selected {
                theme.accent_color().scale_alpha(0.55)
            } else {
                theme.colors.border(0.18)
            },
            width: 1.0,
            radius: 5.0.into(),
        };
        style.shadow = Default::default();
        style
    })
    .into()
}

fn model_quota_menu_entries(metrics: &[UsageMetric]) -> Vec<(String, String)> {
    let mut families = BTreeMap::new();
    metrics
        .iter()
        .filter(|metric| is_model_quota_metric(metric))
        .for_each(|metric| {
            let family_id = model_quota_family_id(model_quota_id(metric));
            families.entry(family_id.clone()).or_insert_with(|| {
                (
                    family_id.clone(),
                    model_family_display_name(&family_id, &[(*metric).clone()]),
                )
            });
        });
    families.into_values().collect()
}

fn has_hidden_model_quota(
    metrics: &[UsageMetric],
    model_visibility: &ModelVisibilityPreferences,
) -> bool {
    metrics
        .iter()
        .filter(|metric| is_model_quota_metric(metric))
        .any(|metric| {
            let family_id = model_quota_family_id(model_quota_id(metric));
            !model_visibility.is_visible(&family_id)
        })
}

fn is_model_quota_metric(metric: &UsageMetric) -> bool {
    metric
        .metadata
        .get("source")
        .is_some_and(|source| source == "model-quota")
}

fn model_quota_id(metric: &UsageMetric) -> &str {
    metric
        .metadata
        .get("model_id")
        .map(String::as_str)
        .unwrap_or(&metric.key)
}

fn normalize_model_id(model_id: &str) -> String {
    model_id
        .trim()
        .strip_prefix("models/")
        .or_else(|| model_id.trim().strip_prefix("Models/"))
        .unwrap_or(model_id.trim())
        .to_ascii_lowercase()
}

fn visible_model_quota_metrics(
    metrics: &[&UsageMetric],
    model_visibility: &ModelVisibilityPreferences,
    show_all: bool,
) -> Vec<UsageMetric> {
    let mut by_family = BTreeMap::<String, Vec<UsageMetric>>::new();
    for metric in metrics {
        let family_id = model_quota_family_id(model_quota_id(metric));
        by_family
            .entry(family_id)
            .or_default()
            .push((*metric).clone());
    }

    let mut visible = Vec::new();
    for (family_id, entries) in by_family {
        let is_pinned = model_visibility.is_visible(&family_id)
            || entries
                .iter()
                .any(|metric| model_visibility.is_visible(model_quota_id(metric)));
        if show_all || is_pinned {
            visible.push(aggregate_model_family(&family_id, &entries));
        }
    }
    sort_model_metrics(&mut visible);
    visible
}

fn aggregate_model_family(family_id: &str, entries: &[UsageMetric]) -> UsageMetric {
    let mut aggregate = entries[0].clone();
    aggregate.name = model_family_display_name(family_id, entries);
    if let Some(min_remaining) = entries
        .iter()
        .filter_map(UsageMetric::remaining_percent)
        .min_by(f64::total_cmp)
    {
        let used_percent = 100.0 - min_remaining;
        aggregate.used_percent = Some(used_percent);
        aggregate.used_amount = Some(used_percent);
        aggregate.limit_amount = Some(100.0);
        aggregate.remaining_amount = Some(min_remaining);
        aggregate.unit = Some("percent".to_owned());
    } else {
        aggregate.used_percent = None;
        aggregate.used_amount = None;
        aggregate.limit_amount = None;
        aggregate.remaining_amount = None;
        aggregate.unit = None;
    }
    aggregate.reset_at_utc = entries
        .iter()
        .filter_map(|metric| metric.reset_at_utc.as_ref().cloned())
        .min();
    aggregate
}

fn sort_model_metrics(metrics: &mut [UsageMetric]) {
    metrics.sort_by(|left, right| {
        right
            .remaining_percent()
            .unwrap_or(-1.0)
            .total_cmp(&left.remaining_percent().unwrap_or(-1.0))
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn split_thinking_level_suffix(model_id: &str) -> Option<(&str, &str)> {
    ["extra-low", "minimal", "low", "medium", "high", "tiered"]
        .into_iter()
        .find_map(|level| {
            let suffix = format!("-{level}");
            model_id
                .strip_suffix(&suffix)
                .map(|base_id| (base_id, level))
        })
}

fn model_quota_family_id(model_id: &str) -> String {
    let normalized_id = normalize_model_id(model_id);
    if let Some((base_id, _)) = split_thinking_level_suffix(&normalized_id) {
        base_id.to_owned()
    } else if normalized_id.ends_with("-low/high") {
        normalized_id
            .strip_suffix("-low/high")
            .unwrap_or(&normalized_id)
            .to_owned()
    } else if normalized_id.starts_with("claude-") && normalized_id.ends_with("-thinking") {
        normalized_id
            .strip_suffix("-thinking")
            .unwrap_or(&normalized_id)
            .to_owned()
    } else {
        normalized_id
    }
}

fn family_display_name(family_id: &str) -> Option<&'static str> {
    match family_id {
        "gemini-3.1-pro" => Some("Gemini 3.1 Pro"),
        "gemini-3.7-flash" => Some("Gemini 3.7 Flash"),
        "gemini-3.5-flash" => Some("Gemini 3.5 Flash"),
        "gemini-3-flash" => Some("Gemini 3 Flash"),
        "gemini-3.1-flash-image" => Some("Gemini 3.1 Flash Image"),
        "claude-sonnet-4-6" => Some("Claude Sonnet 4.6"),
        "claude-opus-4-6" => Some("Claude Opus 4.6"),
        "claude-opus-4-5" => Some("Claude Opus 4.5"),
        "gpt-oss-120b" => Some("GPT OSS 120B"),
        _ => None,
    }
}

fn model_family_display_name(family_id: &str, entries: &[UsageMetric]) -> String {
    if let Some(name) = family_display_name(family_id) {
        return name.to_owned();
    }
    let normalized = normalize_model_id(family_id);
    if let Some(rest) = normalized.strip_prefix("claude-") {
        let parts = rest.split('-').collect::<Vec<_>>();
        if parts.len() >= 3
            && parts[parts.len() - 2].chars().all(|ch| ch.is_ascii_digit())
            && parts[parts.len() - 1].chars().all(|ch| ch.is_ascii_digit())
        {
            let family = parts[..parts.len() - 2]
                .iter()
                .map(|part| capitalize_first(part))
                .collect::<Vec<_>>()
                .join(" ");
            return format!(
                "Claude {family} {}.{}",
                parts[parts.len() - 2],
                parts[parts.len() - 1]
            );
        }
    }
    let base_name = entries
        .iter()
        .find(|metric| {
            let id = normalize_model_id(model_quota_id(metric));
            split_thinking_level_suffix(&id).is_none() && !id.ends_with("-thinking")
        })
        .or_else(|| entries.first())
        .map(|metric| metric.name.trim())
        .unwrap_or(family_id);
    let lower_name = base_name.to_ascii_lowercase();
    [
        " low/high",
        " extra-low",
        " minimal",
        " low",
        " medium",
        " high",
        " tiered",
    ]
    .iter()
    .find_map(|suffix| {
        lower_name.ends_with(suffix).then(|| {
            base_name[..base_name.len() - suffix.len()]
                .trim()
                .to_owned()
        })
    })
    .unwrap_or_else(|| base_name.to_owned())
}

fn is_default_pinned_model_family(family_id: &str) -> bool {
    matches!(
        family_id,
        "gemini-3.1-pro" | "gemini-3.1-flash-image" | "gemini-3-flash" | "claude-opus-4-6"
    )
}

fn append_model_quota_metrics(
    rows: &mut Vec<Element<'static, Message>>,
    metrics: Vec<UsageMetric>,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    for pair in metrics.chunks(2) {
        let mut quota_row = row![].spacing(6).width(Fill);
        for metric in pair {
            quota_row = quota_row.push(model_quota_tile(metric, theme, language));
        }
        if pair.len() == 1 {
            quota_row = quota_row.push(space().width(Fill));
        }
        rows.push(quota_row.into());
    }
}

fn model_quota_tile(
    metric: &UsageMetric,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let remaining = metric.remaining_percent().and_then(valid_percent);
    let accent = remaining
        .map(|remaining| usage_color(remaining, theme))
        .unwrap_or_else(|| muted_text(theme));
    let mut tile_rows = vec![
        row![
            text(metric.name.clone())
                .size(typography::COMPACT_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .width(Fill),
            text(remaining.map_or_else(
                || locale::text(language, Text::Unavailable).to_owned(),
                |value| format!("{:.0}%", crate::percent_display::current().displayed(value))
            ))
            .size(typography::COMPACT_SIZE)
            .font(typography::STRONG)
            .color(accent),
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .width(Fill)
        .into(),
    ];
    if let Some(remaining) = remaining {
        let shown = crate::percent_display::current().displayed(remaining);
        tile_rows.push(
            progress_bar(0.0..=100.0, shown as f32)
                .girth(4)
                .style(move |_| progress_bar::Style {
                    background: Background::Color(theme.colors.border(0.16)),
                    bar: Background::Color(accent),
                    border: Border {
                        radius: 4.0.into(),
                        ..Border::default()
                    },
                })
                .into(),
        );
    }
    if let Some(reset_at) = metric.reset_at_utc {
        tile_rows.push(
            text(short_reset_countdown(reset_at, Utc::now(), language))
                .size(typography::COMPACT_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
    container(column(tile_rows).spacing(3).width(Fill))
        .padding([5, 6])
        .width(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(accent.scale_alpha(0.16))),
            border: Border {
                color: accent.scale_alpha(0.25),
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn short_reset_countdown(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    language: Language,
) -> String {
    let seconds = (reset_at - now).num_seconds();
    if seconds <= 0 {
        return locale::text(language, Text::ResetReached).to_owned();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    match language {
        Language::English if hours >= 24 => format!("{}d {}h", hours / 24, hours % 24),
        Language::English if hours > 0 => format!("{hours}h {minutes}m"),
        Language::English => format!("{}m", minutes.max(1)),
        Language::Arabic if hours >= 24 => format!("{}ي {}س", hours / 24, hours % 24),
        Language::Arabic if hours > 0 => format!("{hours}س {minutes}د"),
        Language::Arabic => format!("{}د", minutes.max(1)),
    }
}

fn append_snapshot_rows(
    rows: &mut Vec<Element<'static, Message>>,
    snapshot: &UsageSnapshot,
    show_spend_summary: bool,
    model_visibility: &ModelVisibilityPreferences,
    show_all_model_quotas: bool,
    show_antigravity_quota_groups: bool,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    if snapshot.is_stale {
        rows.push(warning_line(
            locale::text(language, Text::StaleUsage),
            theme,
        ));
    }

    if show_antigravity_quota_groups && snapshot.provider_id.eq_ignore_ascii_case("antigravity") {
        append_antigravity_quota_groups(
            rows,
            &snapshot.metrics,
            &snapshot.data_confidence,
            theme,
            language,
        );
        if let Some(inventory) = &snapshot.credit_inventory {
            append_reset_credit_inventory(rows, inventory, theme, language);
        }
        if show_spend_summary {
            append_spend_summary(rows, snapshot.spend.as_ref(), theme, language);
        }
        append_snapshot_source_diagnostics(rows, snapshot, theme, language);
        return;
    }

    let model_metrics = snapshot
        .metrics
        .iter()
        .filter(|metric| is_model_quota_metric(metric))
        .collect::<Vec<_>>();
    let windows = snapshot
        .all_rate_windows()
        .filter(|window| {
            !model_metrics
                .iter()
                .any(|metric| metric.name.trim().eq_ignore_ascii_case(window.name.trim()))
        })
        .collect::<Vec<_>>();
    let metrics = snapshot
        .metrics
        .iter()
        .filter(|metric| !is_openrouter_activity_metric(&snapshot.provider_id, metric))
        .filter(|metric| !is_model_quota_metric(metric))
        .filter(|metric| !duplicates_rate_window(metric, &windows))
        .collect::<Vec<_>>();

    // Reset credits sit under the last weekly lane: the additional weekly
    // window when there is one, otherwise the regular weekly window.
    let reset_inventory_anchor = windows.iter().rposition(|window| {
        let is_primary = snapshot
            .primary
            .as_ref()
            .is_some_and(|primary| std::ptr::eq(primary, *window));
        is_weekly_usage_window(window, is_primary, snapshot.primary_window_kind)
            || is_additional_weekly_window(window)
    });
    let mut reset_inventory_rendered = false;
    if !windows.is_empty() || !metrics.is_empty() {
        for (index, window) in windows.into_iter().enumerate() {
            rows.push(rate_window_row(window, theme, language));
            if !reset_inventory_rendered && reset_inventory_anchor == Some(index) {
                if let Some(inventory) = &snapshot.credit_inventory {
                    if inventory.available_count > 0 {
                        rows.push(space().height(Length::Fixed(8.0)).into());
                    }
                    append_reset_credit_inventory(rows, inventory, theme, language);
                    reset_inventory_rendered = true;
                }
            }
        }
        for metric in metrics {
            rows.push(metric_row(metric, theme, language));
        }
    }

    if !reset_inventory_rendered {
        if let Some(inventory) = &snapshot.credit_inventory {
            append_reset_credit_inventory(rows, inventory, theme, language);
        }
    }

    let visible_models =
        visible_model_quota_metrics(&model_metrics, model_visibility, show_all_model_quotas);
    if !model_metrics.is_empty() && visible_models.is_empty() {
        rows.push(
            text(locale::text(language, Text::NoModelsVisible))
                .size(typography::BODY_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
    append_model_quota_metrics(rows, visible_models, theme, language);

    if show_spend_summary {
        append_spend_summary(rows, snapshot.spend.as_ref(), theme, language);
    }

    append_snapshot_source_diagnostics(rows, snapshot, theme, language);
}

fn append_snapshot_source_diagnostics(
    rows: &mut Vec<Element<'static, Message>>,
    snapshot: &UsageSnapshot,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let visible_diagnostic_count = snapshot
        .source_diagnostics
        .iter()
        .filter(|diagnostic| {
            !is_openrouter_activity_source(&snapshot.provider_id, &diagnostic.source)
        })
        .count();
    if visible_diagnostic_count == 0 {
        return;
    }
    let message = match language {
        Language::English => format!(
            "{} data sources could not be completed",
            visible_diagnostic_count
        ),
        Language::Arabic => format!("تعذر إكمال {} من مصادر البيانات", visible_diagnostic_count),
    };
    rows.push(warning_line(&message, theme));
}

fn is_openrouter_activity_metric(provider_id: &str, metric: &UsageMetric) -> bool {
    provider_id.eq_ignore_ascii_case("openrouter") && metric.key.starts_with("activity.")
}

fn is_openrouter_activity_source(provider_id: &str, source: &str) -> bool {
    provider_id.eq_ignore_ascii_case("openrouter") && source.eq_ignore_ascii_case("activity")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AntigravityQuotaGroup {
    Gemini,
    ClaudeGpt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AntigravityQuotaPeriod {
    Weekly,
    FiveHour,
}

fn is_antigravity_summary_metric(metric: &UsageMetric) -> bool {
    metric
        .metadata
        .get("source")
        .is_some_and(|source| source == "local-quota-summary")
}

fn antigravity_quota_group(metric: &UsageMetric) -> Option<AntigravityQuotaGroup> {
    let group = metric
        .metadata
        .get("group")
        .map(String::as_str)
        .unwrap_or(&metric.name)
        .to_ascii_lowercase();
    if group.contains("gemini") {
        Some(AntigravityQuotaGroup::Gemini)
    } else if group.contains("claude") || group.contains("gpt") || group.contains("3p") {
        Some(AntigravityQuotaGroup::ClaudeGpt)
    } else {
        None
    }
}

fn antigravity_model_quota_group(metric: &UsageMetric) -> Option<AntigravityQuotaGroup> {
    if !is_model_quota_metric(metric) {
        return None;
    }

    let model_id = normalize_model_id(model_quota_id(metric));
    let model_name = metric.name.trim().to_ascii_lowercase();
    if model_id.starts_with("gemini-") || model_name.starts_with("gemini ") {
        Some(AntigravityQuotaGroup::Gemini)
    } else if model_id.starts_with("claude-")
        || model_id.starts_with("gpt-")
        || model_name.starts_with("claude ")
        || model_name.starts_with("gpt ")
    {
        Some(AntigravityQuotaGroup::ClaudeGpt)
    } else {
        None
    }
}

fn antigravity_quota_period(metric: &UsageMetric) -> Option<AntigravityQuotaPeriod> {
    let labels = [
        metric.metadata.get("bucket_id").map(String::as_str),
        metric.metadata.get("raw_bucket").map(String::as_str),
        Some(metric.name.as_str()),
    ];
    for label in labels.into_iter().flatten() {
        let normalized = label.trim().to_ascii_lowercase().replace('_', "-");
        if normalized.contains("week") {
            return Some(AntigravityQuotaPeriod::Weekly);
        }
        if normalized.contains("session")
            || normalized.contains("5-hour")
            || normalized.contains("5 hour")
            || normalized.contains("5h")
            || normalized.contains("five hour")
        {
            return Some(AntigravityQuotaPeriod::FiveHour);
        }
    }

    match metric
        .metadata
        .get("window_seconds")
        .and_then(|value| value.parse::<i64>().ok())
    {
        Some(14_400..=21_600) => Some(AntigravityQuotaPeriod::FiveHour),
        Some(518_400..=691_200) => Some(AntigravityQuotaPeriod::Weekly),
        _ => None,
    }
}

fn select_antigravity_quota_metric(
    metrics: &[UsageMetric],
    group: AntigravityQuotaGroup,
    period: AntigravityQuotaPeriod,
) -> Option<&UsageMetric> {
    let candidates = metrics
        .iter()
        .filter(|metric| {
            is_antigravity_summary_metric(metric)
                && antigravity_quota_group(metric) == Some(group)
                && antigravity_quota_period(metric) == Some(period)
        })
        .collect::<Vec<_>>();
    let known = candidates
        .iter()
        .copied()
        .filter(|metric| metric.remaining_percent().is_some_and(f64::is_finite))
        .collect::<Vec<_>>();

    known
        .into_iter()
        .min_by(|left, right| {
            left.remaining_percent()
                .unwrap_or_default()
                .total_cmp(&right.remaining_percent().unwrap_or_default())
        })
        .or_else(|| candidates.first().copied())
}

fn select_antigravity_model_quota_metric(
    metrics: &[UsageMetric],
    group: AntigravityQuotaGroup,
) -> Option<&UsageMetric> {
    let candidates = metrics
        .iter()
        .filter(|metric| antigravity_model_quota_group(metric) == Some(group))
        .collect::<Vec<_>>();
    let known = candidates
        .iter()
        .copied()
        .filter(|metric| metric.remaining_percent().is_some_and(f64::is_finite));

    known
        .min_by(|left, right| {
            left.remaining_percent()
                .unwrap_or_default()
                .total_cmp(&right.remaining_percent().unwrap_or_default())
        })
        .or_else(|| candidates.first().copied())
}

fn select_antigravity_model_quota_fallback<'a>(
    metrics: &'a [UsageMetric],
    group: AntigravityQuotaGroup,
    weekly: Option<&'a UsageMetric>,
    five_hour: Option<&'a UsageMetric>,
) -> Option<&'a UsageMetric> {
    if weekly.is_some() || five_hour.is_some() {
        return None;
    }

    select_antigravity_model_quota_metric(metrics, group)
}

fn append_antigravity_quota_groups(
    rows: &mut Vec<Element<'static, Message>>,
    metrics: &[UsageMetric],
    data_confidence: &str,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let model_quota_is_authoritative = data_confidence.eq_ignore_ascii_case("authoritative");
    let mut displayed_any_group = false;

    for (group, title) in [
        (AntigravityQuotaGroup::Gemini, Text::GeminiModels),
        (AntigravityQuotaGroup::ClaudeGpt, Text::ClaudeGptModels),
    ] {
        if group == AntigravityQuotaGroup::ClaudeGpt && antigravity_claude_gpt_hidden() {
            continue;
        }
        let weekly =
            select_antigravity_quota_metric(metrics, group, AntigravityQuotaPeriod::Weekly);
        let five_hour =
            select_antigravity_quota_metric(metrics, group, AntigravityQuotaPeriod::FiveHour);
        // Prefer provider-supplied grouped windows. When they are absent, show
        // the actual most-constrained per-model quota with its reset time; the
        // model catalogue does not identify it as a shared 5-hour or weekly
        // window, so never label this fallback as one.
        let model_quota =
            select_antigravity_model_quota_fallback(metrics, group, weekly, five_hour);
        if weekly.is_none() && five_hour.is_none() && model_quota.is_none() {
            continue;
        }

        if displayed_any_group {
            rows.push(space().height(Length::Fixed(8.0)).into());
        }
        rows.push(
            text(locale::text(language, title))
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .into(),
        );
        // Put the shorter reset window first, followed by the weekly limit.
        if let Some(metric) = five_hour {
            rows.push(antigravity_quota_row(
                metric,
                Text::FiveHourLimit,
                theme,
                language,
            ));
        }
        if let Some(metric) = weekly {
            rows.push(antigravity_quota_row(metric, Text::Weekly, theme, language));
        }
        if let Some(metric) = model_quota {
            let confidence_note = if model_quota_is_authoritative {
                ""
            } else {
                match language {
                    Language::English => " · unverified model quota",
                    Language::Arabic => " · حصة نموذج غير مؤكدة",
                }
            };
            let label = format!(
                "{} · {}{}",
                locale::text(
                    language,
                    match crate::percent_display::current() {
                        crate::percent_display::PercentDisplay::Remaining => Text::QuotaRemaining,
                        crate::percent_display::PercentDisplay::Used => Text::QuotaUsed,
                    }
                ),
                metric.name,
                confidence_note
            );
            rows.push(antigravity_quota_row_with_label(
                metric, label, theme, language,
            ));
        }
        displayed_any_group = true;
    }

    if !displayed_any_group {
        rows.push(
            text(locale::text(language, Text::GroupedQuotasUnavailable))
                .size(typography::METADATA_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
}

fn antigravity_quota_row(
    metric: &UsageMetric,
    period_label: Text,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    antigravity_quota_row_with_label(
        metric,
        locale::text(language, period_label).to_owned(),
        theme,
        language,
    )
}

fn antigravity_quota_row_with_label(
    metric: &UsageMetric,
    label: String,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let remaining = metric
        .remaining_percent()
        .filter(|remaining| remaining.is_finite());
    let mut children = if let Some(remaining) = remaining {
        vec![percent_line(&label, remaining, theme, language)]
    } else {
        vec![info_line(
            &label,
            locale::text(language, Text::Unavailable),
            theme,
        )]
    };
    if let Some(reset_at) = metric.reset_at_utc {
        children.push(reset_time_label(reset_at, Utc::now(), theme, language));
    }
    column(children).spacing(1).width(Fill).into()
}

fn append_spend_summary(
    rows: &mut Vec<Element<'static, Message>>,
    spend: Option<&SpendSnapshot>,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let Some(spend) = spend else {
        return;
    };

    let value = match (spend.monthly_usage, spend.monthly_limit) {
        (Some(usage), Some(limit)) => format!(
            "{} / {}",
            format_amount(usage, None, spend.currency_code.as_deref(), language),
            format_amount(limit, None, spend.currency_code.as_deref(), language),
        ),
        (Some(usage), None) => format_amount(usage, None, spend.currency_code.as_deref(), language),
        _ => String::new(),
    };
    if let Some(remaining) = displayable_spend_percent(spend.remaining_percent()) {
        rows.push(percent_line(&value, remaining, theme, language));
    } else if !value.is_empty() {
        rows.push(compact_values_line(&[value], theme));
    }
}

fn append_reset_credit_inventory(
    rows: &mut Vec<Element<'static, Message>>,
    inventory: &UsageCreditInventory,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) {
    let mut available_credits = available_reset_credits(inventory);
    available_credits.sort_by_key(|credit| credit.expires_at_utc);

    for credit in &available_credits {
        let label = reset_credit_label(credit, language);
        let expiration = credit.expires_at_utc.map_or_else(
            || locale::text(language, Text::NoExpiryDate).to_owned(),
            |expires_at| credit_expiration_label(expires_at, Utc::now(), language),
        );
        rows.push(reset_credit_info_line(&label, &expiration, theme, language));
    }

    if available_credits.len() < inventory.available_count as usize {
        rows.push(
            text(locale::text(language, Text::ResetExpiryUnavailable))
                .size(typography::METADATA_SIZE)
                .color(muted_text(theme))
                .into(),
        );
    }
}

fn available_reset_credits(inventory: &UsageCreditInventory) -> Vec<&UsageCreditRecord> {
    if inventory.available_count == 0 {
        return Vec::new();
    }

    inventory
        .credits
        .iter()
        .filter(|credit| {
            credit
                .status
                .as_deref()
                .is_none_or(|status| status.eq_ignore_ascii_case("available"))
        })
        .collect::<Vec<_>>()
}

fn is_weekly_usage_window(
    window: &RateLimitWindow,
    is_primary: bool,
    primary_window_kind: Option<UsagePrimaryWindowKind>,
) -> bool {
    if is_primary && primary_window_kind == Some(UsagePrimaryWindowKind::Weekly) {
        return true;
    }

    if window.kind != UsageWindowKind::Additional && window.limit_window_seconds >= 6 * 24 * 60 * 60
    {
        return true;
    }

    is_weekly_usage_name(&window.name)
}

fn is_additional_weekly_window(window: &RateLimitWindow) -> bool {
    window.kind == UsageWindowKind::Additional
        && (window.limit_window_seconds >= 6 * 24 * 60 * 60
            || window.name.to_ascii_lowercase().contains("weekly"))
}

fn is_weekly_usage_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "weekly" | "week" | "7-day" | "7 days" | "secondary"
    )
}

fn reset_credit_label(credit: &UsageCreditRecord, language: Language) -> String {
    credit
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            credit
                .reset_type
                .as_deref()
                .filter(|reset_type| !reset_type.trim().is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| locale::text(language, Text::ResetCredit).to_owned())
}

fn credit_expiration_label(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
    language: Language,
) -> String {
    let local_expiration = format_local_reset(expires_at.with_timezone(&Local), language);
    let seconds = (expires_at - now).num_seconds();
    if seconds <= 0 {
        return format!(
            "{} {local_expiration}",
            locale::text(language, Text::Expired)
        );
    }

    if seconds > 24 * 60 * 60 {
        let days = seconds / (24 * 60 * 60);
        let hours = (seconds % (24 * 60 * 60)) / (60 * 60);
        return match language {
            Language::English => format!(
                "{} {local_expiration} · in {days}d {hours}h",
                locale::text(language, Text::Expires)
            ),
            Language::Arabic => format!(
                "{} {local_expiration} · بعد {} و{}",
                locale::text(language, Text::Expires),
                arabic_day_count(days),
                arabic_hour_count(hours)
            ),
        };
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    match language {
        Language::English => format!(
            "{} {local_expiration} · in {hours}h {}m",
            locale::text(language, Text::Expires),
            minutes.max(1)
        ),
        Language::Arabic => format!(
            "{} {local_expiration} · بعد {hours} س و{} د",
            locale::text(language, Text::Expires),
            minutes.max(1)
        ),
    }
}

fn duplicates_rate_window(metric: &UsageMetric, windows: &[&RateLimitWindow]) -> bool {
    windows.iter().any(|window| {
        let same_name = metric.name.trim().eq_ignore_ascii_case(window.name.trim());
        let same_percent = metric
            .used_percent
            .is_some_and(|used| (used - window.used_percent).abs() < 0.01);
        let same_reset = metric.reset_at_utc.as_ref() == window.reset_at_utc.as_ref();
        same_name && same_percent && same_reset
    })
}

fn providers_match(account_provider: &str, snapshot_provider: &str) -> bool {
    account_provider == snapshot_provider
        || matches!(
            (account_provider, snapshot_provider),
            ("codex", "openai") | ("openai", "codex")
        )
}

fn status_badge(
    status: AccountStatus,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Option<Element<'static, Message>> {
    let (label, color) = match status {
        AccountStatus::Active => return None,
        AccountStatus::NeedsReauthentication => (
            locale::text(language, Text::NeedsSignIn),
            Color::from_rgb8(230, 116, 101),
        ),
        AccountStatus::Paused => (
            locale::text(language, Text::Paused),
            Color::from_rgb8(224, 182, 92),
        ),
        AccountStatus::Disabled => (
            locale::text(language, Text::Disabled),
            Color::from_rgb8(163, 169, 180),
        ),
    };

    Some(
        container(
            text(label)
                .size(typography::METADATA_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
        )
        .padding([4, 7])
        .style(move |_| container::Style {
            background: Some(Background::Color(color.scale_alpha(0.20))),
            border: Border {
                color: color.scale_alpha(0.38),
                width: 1.0,
                radius: 7.0.into(),
            },
            ..Default::default()
        })
        .into(),
    )
}

fn compact_values_line(
    values: &[String],
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    text(values.join(" · "))
        .size(typography::METADATA_SIZE)
        .color(muted_text(theme))
        .width(Fill)
        .into()
}

fn info_line(
    label: &str,
    value: &str,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    row![
        text(label.to_owned())
            .size(typography::METADATA_SIZE)
            .color(muted_text(theme)),
        space().width(Fill),
        text(value.to_owned())
            .size(typography::VALUE_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .width(Fill)
    .into()
}

fn reset_credit_info_line(
    label: &str,
    value: &str,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let value: Element<'static, Message> =
        if let Some((prefix, countdown)) = countdown_label_parts(value, language) {
            rich_text::<(), Message, iced::Theme, iced::Renderer>([
                span::<(), iced::Font>(prefix.to_owned()).color(muted_text(theme)),
                span::<(), iced::Font>(countdown.to_owned())
                    .font(typography::EMPHASIS)
                    .color(reset_time_accent(theme)),
            ])
            .size(typography::COMPACT_SIZE)
            .font(typography::EMPHASIS)
            .into()
        } else {
            text(value.to_owned())
                .size(typography::COMPACT_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text())
                .into()
        };

    row![
        text(label.to_owned())
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text().scale_alpha(0.92)),
        space().width(Fill),
        value,
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .width(Fill)
    .into()
}

fn rate_window_row(
    window: &RateLimitWindow,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let remaining = window.remaining_percent();
    let mut children: Vec<Element<'static, Message>> = vec![percent_line(
        &display_window_name(&window.name, language),
        remaining,
        theme,
        language,
    )];
    if let Some(reset_at) = window.reset_at_utc {
        children.push(reset_time_label(reset_at, Utc::now(), theme, language));
    }
    column(children).spacing(1).width(Fill).into()
}

fn metric_row(
    metric: &UsageMetric,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    if let Some(remaining) = metric.remaining_percent() {
        let mut children = vec![percent_line(&metric.name, remaining, theme, language)];
        if let Some(reset_at) = metric.reset_at_utc {
            children.push(reset_time_label(reset_at, Utc::now(), theme, language));
        }
        return column(children).spacing(1).width(Fill).into();
    }

    let value = if let Some(remaining) = metric.remaining_amount {
        Some(format_amount(
            remaining,
            metric.unit.as_deref(),
            None,
            language,
        ))
    } else if let Some(used) = metric.used_amount {
        Some(format_amount(used, metric.unit.as_deref(), None, language))
    } else {
        metric
            .reset_at_utc
            .map(|reset| format_local_reset(reset.with_timezone(&Local), language))
    };

    if let Some(value) = value {
        info_line(&metric.name, &value, theme)
    } else {
        text(metric.name.clone())
            .size(typography::METADATA_SIZE)
            .color(muted_text(theme))
            .into()
    }
}

fn percent_line(
    label: &str,
    remaining: f64,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let Some(remaining) = valid_percent(remaining) else {
        return info_line(
            label,
            locale::text(language, Text::InvalidPercentage),
            theme,
        );
    };
    let accent = usage_color(remaining, theme);
    let shown = crate::percent_display::current().displayed(remaining);
    column![
        row![
            text(label.to_owned())
                .size(typography::LABEL_SIZE)
                .font(typography::EMPHASIS)
                .color(theme.colors.text()),
            space().width(Fill),
            text(format!("{shown:.0}%"))
                .size(typography::PERCENTAGE_SIZE)
                .font(typography::STRONG)
                .color(accent),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .width(Fill),
        progress_bar(0.0..=100.0, shown as f32)
            .girth(5)
            .style(move |_| progress_bar::Style {
                background: Background::Color(theme.colors.border(0.16)),
                bar: Background::Color(accent),
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
            }),
    ]
    .spacing(4)
    .width(Fill)
    .into()
}

fn warning_line(
    message: &str,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    let (text_color, background_color, border_color) = if theme.colors.is_light {
        (
            Color::from_rgb8(133, 82, 0),
            Color::from_rgb8(255, 248, 230),
            Color::from_rgb8(224, 190, 127),
        )
    } else {
        (
            Color::from_rgb8(235, 190, 114),
            Color::from_rgba(0.62, 0.36, 0.10, 0.18),
            Color::from_rgba(0.83, 0.57, 0.24, 0.25),
        )
    };
    container(
        text(message.to_owned())
            .size(typography::BODY_SIZE)
            .color(text_color),
    )
    .width(Fill)
    .padding([5, 7])
    .style(move |_| container::Style {
        background: Some(Background::Color(background_color)),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn centered_note(
    message: &str,
    theme: &'static crate::theme::ThemeDefinition,
) -> Element<'static, Message> {
    container(
        text(message.to_owned())
            .size(typography::BODY_SIZE)
            .color(muted_text(theme)),
    )
    .width(Fill)
    .height(Fill)
    .center(Fill)
    .style(move |_| container::Style {
        text_color: Some(theme.colors.text()),
        ..Default::default()
    })
    .into()
}

fn muted_text(theme: &'static crate::theme::ThemeDefinition) -> Color {
    theme.colors.muted_text()
}

fn valid_percent(value: f64) -> Option<f64> {
    if value.is_finite() {
        Some(value.clamp(0.0, 100.0))
    } else {
        None
    }
}

fn displayable_spend_percent(remaining: Option<f64>) -> Option<f64> {
    remaining.filter(|value| !value.is_finite() || format!("{value:.0}") != "0")
}

fn usage_color(remaining: f64, theme: &'static crate::theme::ThemeDefinition) -> Color {
    if theme.colors.is_light {
        if remaining <= 15.0 {
            Color::from_rgb8(176, 48, 43)
        } else if remaining <= 40.0 {
            Color::from_rgb8(143, 86, 0)
        } else {
            Color::from_rgb8(27, 116, 69)
        }
    } else if remaining <= 15.0 {
        Color::from_rgb8(237, 119, 105)
    } else if remaining <= 40.0 {
        Color::from_rgb8(229, 190, 102)
    } else {
        Color::from_rgb8(139, 205, 164)
    }
}

fn format_amount(
    amount: f64,
    unit: Option<&str>,
    currency: Option<&str>,
    language: Language,
) -> String {
    if !amount.is_finite() {
        return locale::text(language, Text::Unavailable).to_owned();
    }
    let amount = if amount.abs() >= 100.0 {
        format!("{amount:.0}")
    } else {
        format!("{amount:.2}")
    };

    if let Some(currency) = currency {
        let symbol = match currency.to_ascii_uppercase().as_str() {
            "USD" => "$",
            "EUR" => "€",
            "GBP" => "£",
            "JPY" => "¥",
            _ => return format!("{amount} {currency}"),
        };
        return format!("{symbol}{amount}");
    }

    match unit {
        Some("tokens") => format!("{amount} token"),
        Some(unit) => format!("{amount} {unit}"),
        None => amount,
    }
}

fn display_window_name(name: &str, language: Language) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "primary" | "session" | "5-hour" | "5 hours" | "five-hour" => {
            locale::text(language, Text::FiveHourLimit).to_owned()
        }
        "secondary" | "weekly" | "7-day" | "7 days" | "week" => {
            locale::text(language, Text::Weekly).to_owned()
        }
        "daily" | "day" => locale::text(language, Text::Daily).to_owned(),
        "monthly" | "month" => locale::text(language, Text::Monthly).to_owned(),
        _ => name.to_owned(),
    }
}

fn format_local_reset(reset_at: DateTime<Local>, language: Language) -> String {
    match language {
        Language::English => reset_at.format("%b %-d, %Y at %-I:%M %p").to_string(),
        Language::Arabic => reset_at.format("%Y-%m-%d %H:%M").to_string(),
    }
}

fn reset_time_label(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    theme: &'static crate::theme::ThemeDefinition,
    language: Language,
) -> Element<'static, Message> {
    let label = reset_label(reset_at, now, language);
    let Some((prefix, countdown)) = countdown_label_parts(&label, language) else {
        return text(label)
            .size(typography::RESET_TIME_SIZE)
            .font(typography::STRONG)
            .color(muted_text(theme))
            .into();
    };

    rich_text::<(), Message, iced::Theme, iced::Renderer>([
        span::<(), iced::Font>(prefix.to_owned()).color(muted_text(theme)),
        span::<(), iced::Font>(countdown.to_owned())
            .font(typography::STRONG)
            .color(reset_time_accent(theme)),
    ])
    .size(typography::RESET_TIME_SIZE)
    .font(typography::BODY)
    .width(Fill)
    .into()
}

fn countdown_label_parts(label: &str, language: Language) -> Option<(&str, &str)> {
    let countdown_word = match language {
        Language::English => "in ",
        Language::Arabic => "بعد ",
    };

    if let Some((_, countdown)) = label.split_once(" · ") {
        if countdown.starts_with(countdown_word) {
            let prefix_end = label.len() - countdown.len();
            return Some((&label[..prefix_end], &label[prefix_end..]));
        }
    }

    let immediate_reset_prefix = match language {
        Language::English => "Resets ",
        Language::Arabic => "يتجدد ",
    };
    let countdown = label.strip_prefix(immediate_reset_prefix)?;
    countdown
        .starts_with(countdown_word)
        .then_some((immediate_reset_prefix, countdown))
}

fn reset_time_accent(theme: &'static crate::theme::ThemeDefinition) -> Color {
    if theme.colors.is_light {
        Color::from_rgb8(149, 91, 0)
    } else {
        Color::from_rgb8(246, 183, 83)
    }
}

fn reset_label(reset_at: DateTime<Utc>, now: DateTime<Utc>, language: Language) -> String {
    let seconds = (reset_at - now).num_seconds();
    if seconds <= 0 {
        return locale::text(language, Text::ResetReached).to_owned();
    }

    let local_reset = format_local_reset(reset_at.with_timezone(&Local), language);
    if seconds > 24 * 60 * 60 {
        let days = seconds / (24 * 60 * 60);
        let hours = (seconds % (24 * 60 * 60)) / (60 * 60);
        return match language {
            Language::English => format!("Resets on {local_reset} · in {days}d {hours}h"),
            Language::Arabic => format!(
                "يتجدد في {local_reset} · بعد {} و{}",
                arabic_day_count(days),
                arabic_hour_count(hours)
            ),
        };
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    match language {
        Language::English if hours > 0 => format!("Resets in {hours}h {minutes}m"),
        Language::English => format!("Resets in {}m", minutes.max(1)),
        Language::Arabic if hours > 0 => format!("يتجدد بعد {hours} س و{minutes} د"),
        Language::Arabic => format!("يتجدد بعد {} د", minutes.max(1)),
    }
}

fn arabic_day_count(days: i64) -> String {
    match days {
        1 => "يوم".to_owned(),
        2 => "يومين".to_owned(),
        3..=10 => format!("{days} أيام"),
        _ => format!("{days} يوم"),
    }
}

fn arabic_hour_count(hours: i64) -> String {
    match hours {
        1 => "ساعة".to_owned(),
        2 => "ساعتين".to_owned(),
        3..=10 => format!("{hours} ساعات"),
        _ => format!("{hours} ساعة"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use codex_usage_core::usage::{RateLimitWindow, UsageWindowKind};

    fn model_metric(
        model_id: &str,
        name: &str,
        remaining: f64,
        reset_in_hours: i64,
    ) -> UsageMetric {
        UsageMetric {
            key: model_id.to_owned(),
            name: name.to_owned(),
            used_percent: Some(100.0 - remaining),
            used_amount: Some(100.0 - remaining),
            limit_amount: Some(100.0),
            remaining_amount: Some(remaining),
            unit: Some("percent".to_owned()),
            reset_at_utc: Some(Utc::now() + Duration::hours(reset_in_hours)),
            reset_label: None,
            metadata: [
                ("source".to_owned(), "model-quota".to_owned()),
                ("model_id".to_owned(), model_id.to_owned()),
            ]
            .into(),
        }
    }

    fn grouped_metric(
        key: &str,
        group: &str,
        bucket_id: &str,
        raw_bucket: &str,
        window_seconds: i64,
        remaining: Option<f64>,
    ) -> UsageMetric {
        let used_percent = remaining.map(|value| 100.0 - value);
        UsageMetric {
            key: key.to_owned(),
            name: format!("{group} {raw_bucket}"),
            used_percent,
            used_amount: used_percent,
            limit_amount: remaining.map(|_| 100.0),
            remaining_amount: remaining,
            unit: remaining.map(|_| "percent".to_owned()),
            reset_at_utc: None,
            reset_label: None,
            metadata: [
                ("source".to_owned(), "local-quota-summary".to_owned()),
                ("group".to_owned(), group.to_owned()),
                ("bucket_id".to_owned(), bucket_id.to_owned()),
                ("raw_bucket".to_owned(), raw_bucket.to_owned()),
                ("window_seconds".to_owned(), window_seconds.to_string()),
            ]
            .into(),
        }
    }

    #[test]
    fn antigravity_group_mode_selects_real_pool_and_window_summary_metrics() {
        let metrics = vec![
            grouped_metric(
                "gemini-weekly",
                "Gemini",
                "gemini-weekly",
                "Weekly Limit Remaining",
                604_800,
                Some(71.0),
            ),
            grouped_metric(
                "gemini-weekly-constrained",
                "Gemini",
                "gemini-weekly",
                "Weekly Limit Remaining",
                604_800,
                Some(65.0),
            ),
            grouped_metric(
                "gemini-session",
                "Gemini",
                "gemini-5h",
                "Five Hour Limit Remaining",
                18_000,
                Some(39.0),
            ),
            grouped_metric(
                "3p-weekly",
                "Claude/GPT",
                "3p-weekly",
                "Weekly Limit Remaining",
                604_800,
                Some(100.0),
            ),
            grouped_metric(
                "3p-session",
                "Claude/GPT",
                "3p-5h",
                "Five Hour Limit Remaining",
                18_000,
                Some(100.0),
            ),
            model_metric("gemini-3.7-flash", "Gemini 3.7 Flash", 20.0, 5),
        ];

        let gemini_weekly = select_antigravity_quota_metric(
            &metrics,
            AntigravityQuotaGroup::Gemini,
            AntigravityQuotaPeriod::Weekly,
        )
        .unwrap();
        assert_eq!(gemini_weekly.key, "gemini-weekly-constrained");
        assert_eq!(gemini_weekly.remaining_percent(), Some(65.0));
        assert_eq!(
            select_antigravity_quota_metric(
                &metrics,
                AntigravityQuotaGroup::Gemini,
                AntigravityQuotaPeriod::FiveHour,
            )
            .unwrap()
            .remaining_percent(),
            Some(39.0)
        );
        assert_eq!(
            select_antigravity_quota_metric(
                &metrics,
                AntigravityQuotaGroup::ClaudeGpt,
                AntigravityQuotaPeriod::Weekly,
            )
            .unwrap()
            .remaining_percent(),
            Some(100.0)
        );
        assert!(
            select_antigravity_quota_metric(
                &[model_metric(
                    "gemini-3.7-flash",
                    "Gemini 3.7 Flash",
                    20.0,
                    5
                )],
                AntigravityQuotaGroup::Gemini,
                AntigravityQuotaPeriod::FiveHour,
            )
            .is_none()
        );
    }

    #[test]
    fn antigravity_group_fallback_selects_the_lowest_real_model_quota() {
        let metrics = vec![
            model_metric("gemini-3.7-flash", "Gemini 3.7 Flash", 82.0, 120),
            model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 43.0, 168),
            model_metric("claude-sonnet-4-6", "Claude Sonnet 4.6", 72.0, 168),
            model_metric("claude-opus-4-6", "Claude Opus 4.6", 57.0, 168),
            model_metric("gpt-oss-120b", "GPT OSS 120B", 88.0, 168),
            model_metric("chat_20706", "Unknown model", 1.0, 168),
        ];

        let gemini =
            select_antigravity_model_quota_metric(&metrics, AntigravityQuotaGroup::Gemini).unwrap();
        assert_eq!(gemini.key, "gemini-2.5-pro");
        assert_eq!(gemini.remaining_percent(), Some(43.0));
        assert_eq!(
            gemini.reset_at_utc, metrics[1].reset_at_utc,
            "the fallback must preserve the selected model's actual reset time"
        );

        let claude_gpt =
            select_antigravity_model_quota_metric(&metrics, AntigravityQuotaGroup::ClaudeGpt)
                .unwrap();
        assert_eq!(claude_gpt.key, "claude-opus-4-6");
        assert_eq!(claude_gpt.remaining_percent(), Some(57.0));

        assert!(
            select_antigravity_model_quota_metric(
                &[metrics[5].clone()],
                AntigravityQuotaGroup::Gemini,
            )
            .is_none()
        );
        assert!(antigravity_quota_period(&metrics[0]).is_none());
    }

    #[test]
    fn antigravity_model_fallback_is_used_only_when_group_windows_are_absent() {
        let metrics = vec![
            grouped_metric(
                "gemini-weekly",
                "Gemini",
                "gemini-weekly",
                "Weekly Limit Remaining",
                604_800,
                Some(65.0),
            ),
            model_metric("gemini-3.7-flash", "Gemini 3.7 Flash", 20.0, 5),
        ];

        let grouped_weekly = select_antigravity_quota_metric(
            &metrics,
            AntigravityQuotaGroup::Gemini,
            AntigravityQuotaPeriod::Weekly,
        );
        assert!(
            select_antigravity_model_quota_fallback(
                &metrics,
                AntigravityQuotaGroup::Gemini,
                grouped_weekly,
                None,
            )
            .is_none()
        );

        assert_eq!(
            select_antigravity_model_quota_fallback(
                &metrics,
                AntigravityQuotaGroup::Gemini,
                None,
                None,
            )
            .map(|metric| metric.key.as_str()),
            Some("gemini-3.7-flash")
        );
    }

    #[test]
    fn returning_to_all_or_pinned_view_closes_antigravity_group_mode() {
        let mut state = DashboardState::loading();
        state.set_show_antigravity_quota_groups(true);

        state.set_show_all_model_quotas(false);

        assert!(!state.show_antigravity_quota_groups);
        assert!(!state.show_all_model_quotas);
    }

    #[test]
    fn antigravity_group_mode_is_the_default() {
        let state = DashboardState::loading();

        assert!(state.show_antigravity_quota_groups);
        assert!(!state.show_all_model_quotas);
    }

    #[test]
    fn incremental_account_usage_replaces_only_the_completed_account() {
        let mut state = DashboardState::loading();
        let first = AccountRecord::create(
            "First Codex account",
            "first@example.com",
            None,
            "openai",
            None,
        )
        .unwrap();
        let second = AccountRecord::create(
            "Second Codex account",
            "second@example.com",
            None,
            "openai",
            None,
        )
        .unwrap();
        let first_id = first.id;
        let second_id = second.id;
        state.set_accounts(vec![
            AccountUsageEntry {
                account: first,
                snapshot: None,
            },
            AccountUsageEntry {
                account: second,
                snapshot: None,
            },
        ]);

        let mut refreshed_first = state
            .account_entries()
            .iter()
            .find(|entry| entry.account.id == first_id)
            .unwrap()
            .clone();
        refreshed_first.account.label = "Personal Codex".to_owned();
        state.update_account_usage(refreshed_first);

        assert_eq!(state.account_entries().len(), 2);
        assert_eq!(
            state
                .account_entries()
                .iter()
                .find(|entry| entry.account.id == first_id)
                .unwrap()
                .account
                .label,
            "Personal Codex"
        );
        assert!(
            state
                .account_entries()
                .iter()
                .any(|entry| entry.account.id == second_id)
        );
    }

    fn flattened_models(models: &[UsageMetric]) -> &[UsageMetric] {
        models
    }

    fn account_entry_with_usage(used_percent: f64) -> AccountUsageEntry {
        let account =
            AccountRecord::create("Codex test", "codex-test@example.com", None, "openai", None)
                .unwrap();
        let snapshot = UsageSnapshot {
            account_id: account.id,
            observed_at_utc: Utc::now(),
            response_account_id: None,
            plan_type: Some("pro".to_owned()),
            primary: Some(RateLimitWindow {
                kind: UsageWindowKind::Primary,
                name: "Primary".to_owned(),
                used_percent,
                reset_at_utc: None,
                limit_window_seconds: 5 * 60 * 60,
            }),
            primary_window_kind: Some(UsagePrimaryWindowKind::Session),
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: Vec::new(),
            source_diagnostics: Vec::new(),
            provider_id: "openai".to_owned(),
            source: Some("api".to_owned()),
            data_confidence: "authoritative".to_owned(),
        };

        AccountUsageEntry {
            account,
            snapshot: Some(snapshot),
        }
    }

    #[test]
    fn changed_usage_starts_a_transition_from_the_previous_visible_value() {
        let previous = account_entry_with_usage(20.0);
        let mut next = previous.clone();
        next.snapshot
            .as_mut()
            .unwrap()
            .primary
            .as_mut()
            .unwrap()
            .used_percent = 42.0;

        let now = Instant::now();
        let mut animation = UsageAnimationState::default();
        animation.update(
            std::slice::from_ref(&previous),
            std::slice::from_ref(&next),
            now,
        );

        let key = UsagePercentKey {
            account_id: next.account.id,
            field: "window:primary".to_owned(),
        };
        let transition = animation.transitions.get(&key).unwrap();
        assert_eq!(transition.from_remaining, 80.0);
        assert_eq!(transition.to_remaining, 58.0);
        assert!(animation.is_active());
    }

    #[test]
    fn usage_percent_transition_eases_to_the_new_value() {
        let started_at = Instant::now();
        let transition = UsagePercentTransition {
            from_remaining: 82.0,
            to_remaining: 67.0,
            started_at,
        };

        assert_eq!(transition.value_at(started_at), 82.0);
        let midpoint = transition.value_at(started_at + USAGE_CHANGE_ANIMATION_DURATION / 2);
        assert!(midpoint < 82.0 && midpoint > 67.0);
        assert_eq!(
            transition.value_at(started_at + USAGE_CHANGE_ANIMATION_DURATION),
            67.0
        );
        assert!(!transition.is_active(started_at + USAGE_CHANGE_ANIMATION_DURATION));
    }

    #[test]
    fn reset_label_uses_date_and_day_hour_count_after_24_hours() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 0).unwrap();
        let reset_at = now + Duration::hours(51) + Duration::minutes(20);

        let label = reset_label(reset_at, now, Language::English);
        let formatted_reset = format_local_reset(reset_at.with_timezone(&Local), Language::English);

        assert!(label.starts_with("Resets on "));
        assert!(label.contains(&formatted_reset));
        assert!(label.contains(" at "));
        assert!(label.contains("· in 2d 3h"));
        assert!(!label.contains("51h"));
    }

    #[test]
    fn moving_an_account_reorders_only_its_own_tab() {
        let make = |provider: &str, email: &str| AccountUsageEntry {
            account: AccountRecord::create(email, email, None, provider, None).unwrap(),
            snapshot: None,
        };
        let entries = vec![
            make(codex_usage_core::accounts::OPENAI, "a@example.com"),
            make(codex_usage_core::accounts::CLAUDE, "c@example.com"),
            make(codex_usage_core::accounts::OPENAI, "b@example.com"),
        ];
        let ids = entries
            .iter()
            .map(|entry| entry.account.id)
            .collect::<Vec<_>>();
        let mut state = DashboardState::loading();
        state.entries = entries;
        state.account_order = Vec::new();

        let codex_order = |state: &DashboardState| {
            state
                .ordered_entries(UsageProvider::Codex)
                .iter()
                .map(|entry| entry.account.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(codex_order(&state), vec![ids[0], ids[2]]);

        // Moving the first account up does nothing.
        let _ = state.move_account(ids[0], -1);
        assert_eq!(codex_order(&state), vec![ids[0], ids[2]]);

        let _ = state.move_account(ids[2], -1);
        assert_eq!(codex_order(&state), vec![ids[2], ids[0]]);
        assert_eq!(
            state.account_order,
            vec![ids[2], ids[1], ids[0]],
            "the Claude account keeps its slot"
        );
    }

    #[test]
    fn reset_label_keeps_hour_format_at_exactly_24_hours() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 0).unwrap();
        let label = reset_label(now + Duration::hours(24), now, Language::English);

        assert_eq!(label, "Resets in 24h 0m");
    }

    #[test]
    fn countdown_label_parts_highlight_only_relative_time_in_both_languages() {
        let english = "Resets on Sep 29, 2026 at 10:20 PM · in 4d 11h";
        assert_eq!(
            countdown_label_parts(english, Language::English),
            Some(("Resets on Sep 29, 2026 at 10:20 PM · ", "in 4d 11h"))
        );

        let arabic = "يتجدد في 2026-09-29 22:20 · بعد ٤ أيام و١١ ساعة";
        assert_eq!(
            countdown_label_parts(arabic, Language::Arabic),
            Some(("يتجدد في 2026-09-29 22:20 · ", "بعد ٤ أيام و١١ ساعة"))
        );

        assert_eq!(
            countdown_label_parts("Resets in 24h 0m", Language::English),
            Some(("Resets ", "in 24h 0m"))
        );
        assert_eq!(
            countdown_label_parts("يتجدد بعد 4 س و15 د", Language::Arabic),
            Some(("يتجدد ", "بعد 4 س و15 د"))
        );
        assert_eq!(
            countdown_label_parts(
                "Expires Sep 29, 2026 at 10:20 PM · in 4d 11h",
                Language::English
            ),
            Some(("Expires Sep 29, 2026 at 10:20 PM · ", "in 4d 11h"))
        );
        assert_eq!(
            countdown_label_parts("Reset time reached", Language::English),
            None
        );
    }

    #[test]
    fn reset_credit_expiry_shows_local_date_and_remaining_days_or_hours() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 0).unwrap();
        let expiry = now + Duration::hours(51) + Duration::minutes(20);
        let label = credit_expiration_label(expiry, now, Language::English);
        let local_date = format_local_reset(expiry.with_timezone(&Local), Language::English);

        assert!(label.starts_with("Expires "));
        assert!(label.contains(&local_date));
        assert!(label.contains("· in 2d 3h"));
        assert!(!label.contains("51h"));

        let under_one_day = credit_expiration_label(
            now + Duration::hours(4) + Duration::minutes(15),
            now,
            Language::English,
        );
        assert!(under_one_day.contains("· in 4h 15m"));

        let expired = credit_expiration_label(now - Duration::minutes(1), now, Language::English);
        assert!(expired.starts_with("Expired "));
    }

    #[test]
    fn spend_summary_hides_percentages_that_render_as_zero() {
        assert_eq!(displayable_spend_percent(Some(0.0)), None);
        assert_eq!(displayable_spend_percent(Some(0.49)), None);
        assert_eq!(displayable_spend_percent(Some(1.0)), Some(1.0));
        assert_eq!(displayable_spend_percent(None), None);
    }

    #[test]
    fn session_window_uses_the_official_five_hour_limit_label() {
        assert_eq!(
            display_window_name("session", Language::English),
            "5 hours limit"
        );
        assert_eq!(
            display_window_name("5-hour", Language::Arabic),
            "حد 5 ساعات"
        );
    }

    #[test]
    fn account_alias_can_be_reset_to_original_or_saved_trimmed() {
        assert_eq!(normalized_alias("   ", "Provider name"), None);
        assert_eq!(normalized_alias(" Provider name ", "Provider name"), None);
        assert_eq!(
            normalized_alias("  Work account  ", "Provider name"),
            Some("Work account".to_owned())
        );
    }

    #[test]
    fn plan_name_starts_with_an_uppercase_letter() {
        assert_eq!(capitalize_first("plus"), "Plus");
        assert_eq!(capitalize_first(""), "");
    }

    #[test]
    fn reset_credit_expiry_lists_only_credits_still_counted_as_available() {
        let record = |id: &str, status: Option<&str>| UsageCreditRecord {
            id: Some(id.to_owned()),
            reset_type: None,
            status: status.map(str::to_owned),
            granted_at_utc: None,
            expires_at_utc: None,
            redeem_started_at_utc: None,
            redeemed_at_utc: None,
            title: None,
            description: None,
        };
        let inventory = UsageCreditInventory {
            available_count: 2,
            credits: vec![
                record("available", Some("available")),
                record("legacy-without-status", None),
                record("redeemed", Some("redeemed")),
            ],
        };

        let listed = available_reset_credits(&inventory);
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|credit| {
            credit
                .status
                .as_deref()
                .is_none_or(|status| status.eq_ignore_ascii_case("available"))
        }));

        let empty_inventory = UsageCreditInventory {
            available_count: 0,
            credits: vec![record("stale", None)],
        };
        assert!(available_reset_credits(&empty_inventory).is_empty());
    }

    #[test]
    fn weekly_reset_inventory_anchor_recognizes_semantic_and_named_weekly_windows() {
        let primary = RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "primary".to_owned(),
            used_percent: 25.0,
            reset_at_utc: None,
            limit_window_seconds: 5 * 60 * 60,
        };
        assert!(is_weekly_usage_window(
            &primary,
            true,
            Some(UsagePrimaryWindowKind::Weekly)
        ));
        assert!(!is_weekly_usage_window(
            &primary,
            false,
            Some(UsagePrimaryWindowKind::Weekly)
        ));
        assert!(is_weekly_usage_name("Weekly"));
        assert!(is_weekly_usage_name("7-day"));
    }

    #[test]
    fn pinned_mode_defaults_to_curated_families_and_all_mode_shows_every_family() {
        let metrics = [
            model_metric("gemini-3.1-pro-low", "Gemini 3.1 Pro Low", 80.0, 6),
            model_metric("gemini-3.1-pro-high", "Gemini 3.1 Pro High", 60.0, 6),
            model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 91.0, 6),
        ];
        let default_visibility = ModelVisibilityPreferences::default();
        let input = metrics.iter().collect::<Vec<_>>();
        let pinned = visible_model_quota_metrics(&input, &default_visibility, false);
        assert_eq!(flattened_models(&pinned).len(), 1);
        assert_eq!(pinned[0].name, "Gemini 3.1 Pro");
        assert_eq!(pinned[0].remaining_percent(), Some(60.0));

        let mut visibility = ModelVisibilityPreferences::default();
        visibility.set_visible("gemini-3.1-pro", false);
        let pinned = visible_model_quota_metrics(&input, &visibility, false);
        assert!(pinned.is_empty());

        let all = visible_model_quota_metrics(&input, &visibility, true);
        assert_eq!(flattened_models(&all).len(), 2);
        assert_eq!(all[0].name, "Gemini 2.5 Pro");
    }

    #[test]
    fn default_pins_match_the_compact_antigravity_selection() {
        let metrics = [
            model_metric("gemini-3.1-pro-high", "Gemini 3.1 Pro High", 67.0, 4),
            model_metric("gemini-3.1-flash-image", "Gemini 3.1 Flash Image", 67.0, 4),
            model_metric("gemini-3-flash", "Gemini 3 Flash", 67.0, 4),
            model_metric(
                "claude-opus-4-6-thinking",
                "Claude Opus 4.6 Thinking",
                67.0,
                4,
            ),
            model_metric("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite", 67.0, 4),
        ];
        let pinned = visible_model_quota_metrics(
            &metrics.iter().collect::<Vec<_>>(),
            &ModelVisibilityPreferences::default(),
            false,
        );
        assert_eq!(
            pinned
                .iter()
                .map(|metric| metric.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Claude Opus 4.6",
                "Gemini 3 Flash",
                "Gemini 3.1 Flash Image",
                "Gemini 3.1 Pro",
            ]
        );
    }

    #[test]
    fn model_visibility_preferences_keep_legacy_lines_and_allow_visible_overrides() {
        let preferences = parse_model_visibility_preferences(
            "gemini-3.1-pro-high\nvisible:Models/gemini-2.5-pro\n",
        );

        assert!(!preferences.is_visible("gemini-3.1-pro-high"));
        assert!(!preferences.is_visible("gemini-2.5-flash"));
        assert!(preferences.is_visible("gemini-2.5-pro"));
        assert!(preferences.is_visible("gemini-3.1-pro-low"));
    }

    #[test]
    fn hidden_model_indicator_matches_the_available_model_families() {
        let metrics = [
            model_metric("gemini-3.1-pro-high", "Gemini 3.1 Pro High", 82.0, 12),
            model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 70.0, 8),
        ];
        let mut visibility = ModelVisibilityPreferences::default();

        assert!(has_hidden_model_quota(&metrics, &visibility));

        visibility.set_visible("gemini-2.5-pro", true);
        assert!(!has_hidden_model_quota(&metrics, &visibility));
    }

    #[test]
    fn thinking_variants_render_once_without_thinking_names_and_use_conservative_quota() {
        let metrics = [
            model_metric(
                "gemini-3.7-flash-medium",
                "Gemini 3.7 Flash Medium",
                82.0,
                12,
            ),
            model_metric("gemini-3.7-flash-high", "Gemini 3.7 Flash High", 61.0, 8),
            model_metric("claude-sonnet-4-5", "Claude Sonnet 4.5", 75.0, 6),
            model_metric(
                "claude-sonnet-4-5-thinking",
                "Claude Sonnet 4.5 Thinking",
                24.0,
                3,
            ),
        ];
        let models = visible_model_quota_metrics(
            &metrics.iter().collect::<Vec<_>>(),
            &ModelVisibilityPreferences::default(),
            true,
        );
        let flash = models
            .iter()
            .find(|metric| metric.name == "Gemini 3.7 Flash")
            .unwrap();
        assert_eq!(flash.remaining_percent(), Some(61.0));
        assert!(
            !models
                .iter()
                .any(|metric| metric.name == "High" || metric.name == "Medium")
        );

        let sonnet = models
            .iter()
            .find(|metric| metric.name == "Claude Sonnet 4.5")
            .unwrap();
        assert_eq!(sonnet.remaining_percent(), Some(24.0));
        assert!(sonnet.reset_at_utc.unwrap() <= Utc::now() + Duration::hours(3));
    }

    #[test]
    fn thinking_variants_and_base_model_collapse_into_one_named_row() {
        let metrics = [
            model_metric("gemini-3.8-flash", "Gemini 3.8 Flash", 91.0, 12),
            model_metric("gemini-3.8-flash-high", "Gemini 3.8 Flash High", 61.0, 8),
            model_metric(
                "gemini-3.8-flash-medium",
                "Gemini 3.8 Flash Medium",
                82.0,
                10,
            ),
        ];
        let models = visible_model_quota_metrics(
            &metrics.iter().collect::<Vec<_>>(),
            &ModelVisibilityPreferences::default(),
            true,
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "Gemini 3.8 Flash");
        assert_eq!(models[0].remaining_percent(), Some(61.0));
    }

    #[test]
    fn distinct_base_models_remain_separate_while_thinking_levels_merge() {
        let metrics = [
            model_metric("gemini-3-flash", "Gemini 3 Flash", 90.0, 8),
            model_metric("gemini-3.5-flash-high", "Gemini 3.5 Flash High", 80.0, 8),
            model_metric(
                "gemini-3.5-flash-medium",
                "Gemini 3.5 Flash Medium",
                70.0,
                8,
            ),
        ];
        let models = visible_model_quota_metrics(
            &metrics.iter().collect::<Vec<_>>(),
            &ModelVisibilityPreferences::default(),
            true,
        );
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|metric| metric.name == "Gemini 3 Flash"));
        let flash_35 = models
            .iter()
            .find(|metric| metric.name == "Gemini 3.5 Flash")
            .unwrap();
        assert_eq!(flash_35.remaining_percent(), Some(70.0));
    }

    #[test]
    fn a_single_thinking_variant_displays_its_base_model_name() {
        let metrics = [
            model_metric(
                "gemini-3.7-flash-medium",
                "Gemini 3.7 Flash Medium",
                82.0,
                12,
            ),
            model_metric("gemini-3.7-flash-high", "Gemini 3.7 Flash High", 61.0, 8),
        ];
        let mut visibility = ModelVisibilityPreferences::default();
        visibility.set_visible("gemini-3.7-flash", true);
        let models =
            visible_model_quota_metrics(&metrics.iter().collect::<Vec<_>>(), &visibility, true);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "Gemini 3.7 Flash");
    }

    #[test]
    fn unpinned_models_are_hidden_until_selected_or_all_models_is_enabled() {
        let metrics = [
            model_metric("gemini-1.5-pro", "Gemini 1.5 Pro", 96.0, 4),
            model_metric("gemini-2.5-pro", "Gemini 2.5 Pro", 70.0, 4),
            model_metric("gemini-2.5-flash", "Gemini 2.5 Flash", 95.0, 4),
            model_metric("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite", 67.0, 4),
        ];
        let mut visibility = ModelVisibilityPreferences::default();
        assert!(!visibility.is_visible("gemini-1.5-pro"));
        assert!(!visibility.is_visible("gemini-2.5-pro"));

        let input = metrics.iter().collect::<Vec<_>>();
        assert!(visible_model_quota_metrics(&input, &visibility, false).is_empty());

        let all = visible_model_quota_metrics(&input, &visibility, true);
        assert_eq!(all.len(), 4);
        assert!(
            all.iter()
                .any(|metric| metric.name == "Gemini 3.1 Flash Lite")
        );

        visibility.set_visible("gemini-2.5-pro", true);
        assert!(visibility.is_visible("gemini-2.5-pro"));
        let models = visible_model_quota_metrics(&input, &visibility, false);
        assert!(models.iter().any(|metric| metric.name == "Gemini 2.5 Pro"));
    }

    #[test]
    fn duplicate_window_metrics_are_hidden_without_hiding_provider_specific_metrics() {
        let window = RateLimitWindow {
            kind: UsageWindowKind::Primary,
            name: "Session".to_owned(),
            used_percent: 25.0,
            reset_at_utc: None,
            limit_window_seconds: 3600,
        };
        let duplicate = UsageMetric {
            key: "primary".to_owned(),
            name: "Session".to_owned(),
            used_percent: Some(25.0),
            used_amount: None,
            limit_amount: None,
            remaining_amount: None,
            unit: None,
            reset_at_utc: None,
            reset_label: None,
            metadata: Default::default(),
        };
        let specific = UsageMetric {
            name: "Model A".to_owned(),
            ..duplicate.clone()
        };

        assert!(duplicates_rate_window(&duplicate, &[&window]));
        assert!(!duplicates_rate_window(&specific, &[&window]));
    }

    #[test]
    fn openrouter_activity_is_hidden_without_hiding_other_provider_data() {
        let activity = UsageMetric {
            key: "activity.summary".to_owned(),
            name: "Activity summary".to_owned(),
            used_percent: None,
            used_amount: Some(12.0),
            limit_amount: None,
            remaining_amount: None,
            unit: Some("USD".to_owned()),
            reset_at_utc: None,
            reset_label: None,
            metadata: Default::default(),
        };
        let credits = UsageMetric {
            key: "credits.balance".to_owned(),
            name: "Credits".to_owned(),
            ..activity.clone()
        };

        assert!(is_openrouter_activity_metric("openrouter", &activity));
        assert!(!is_openrouter_activity_metric("openrouter", &credits));
        assert!(!is_openrouter_activity_metric(
            "another-provider",
            &activity
        ));
        assert!(is_openrouter_activity_source("openrouter", "activity"));
        assert!(!is_openrouter_activity_source("openrouter", "credits"));
        assert!(!is_openrouter_activity_source(
            "another-provider",
            "activity"
        ));
    }

    #[test]
    fn remaining_percentage_is_clamped_and_non_finite_values_do_not_reach_ui() {
        assert_eq!(valid_percent(-10.0), Some(0.0));
        assert_eq!(valid_percent(140.0), Some(100.0));
        assert_eq!(valid_percent(f64::NAN), None);
    }
}
