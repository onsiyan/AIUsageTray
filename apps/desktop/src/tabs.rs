//! The tab bar the user arranges: which tabs show, in what order, and the
//! custom tabs that gather several providers' accounts under one name.

use std::{collections::HashMap, fs, io};

use usage_monitor_core::accounts::AccountId;

use crate::{DashboardTab, PROVIDER_TABS, UsageProvider, theme::preference_directory};

const LAYOUT_FILE: &str = "tabs.txt";
/// Long names would be clipped in the narrow tab buttons.
pub const MAX_CUSTOM_TAB_NAME: usize = 12;

/// A set of providers, small enough to keep [`DashboardTab`] `Copy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ProviderSet(u16);

impl ProviderSet {
    fn bit(provider: UsageProvider) -> u16 {
        let index = PROVIDER_TABS
            .iter()
            .position(|tab| tab.provider == provider)
            .expect("every provider has a tab");
        1 << index
    }

    pub fn contains(self, provider: UsageProvider) -> bool {
        self.0 & Self::bit(provider) != 0
    }

    pub fn toggle(&mut self, provider: UsageProvider) {
        self.0 ^= Self::bit(provider);
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The providers in tab-bar order.
    pub fn providers(self) -> impl Iterator<Item = UsageProvider> {
        PROVIDER_TABS
            .iter()
            .map(|tab| tab.provider)
            .filter(move |provider| self.contains(*provider))
    }

    pub fn from_providers(providers: impl IntoIterator<Item = UsageProvider>) -> Self {
        let mut set = Self::default();
        for provider in providers {
            if !set.contains(provider) {
                set.toggle(provider);
            }
        }
        set
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomTab {
    pub id: u32,
    pub name: String,
    pub providers: ProviderSet,
    /// Accounts picked one by one, beyond the whole providers above.
    pub accounts: Vec<AccountId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabKind {
    Provider(UsageProvider),
    Favorites,
    Cost,
    Custom(CustomTab),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabEntry {
    pub kind: TabKind,
    pub visible: bool,
}

impl TabEntry {
    pub fn dashboard_tab(&self) -> DashboardTab {
        match &self.kind {
            TabKind::Provider(provider) => DashboardTab::Provider(*provider),
            TabKind::Favorites => DashboardTab::Favorites,
            TabKind::Cost => DashboardTab::Cost,
            TabKind::Custom(custom) => DashboardTab::Custom {
                id: custom.id,
                providers: custom.providers,
            },
        }
    }
}

/// Every tab in bar order, shown or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabLayout {
    entries: Vec<TabEntry>,
}

impl Default for TabLayout {
    /// Every provider in its usual order, with Favorites in the middle and
    /// Cost at the end.
    fn default() -> Self {
        let mut entries = PROVIDER_TABS
            .iter()
            .map(|tab| TabEntry {
                kind: TabKind::Provider(tab.provider),
                visible: true,
            })
            .collect::<Vec<_>>();
        entries.insert(
            entries.len().div_ceil(2),
            TabEntry {
                kind: TabKind::Favorites,
                visible: true,
            },
        );
        entries.push(TabEntry {
            kind: TabKind::Cost,
            visible: true,
        });
        Self { entries }
    }
}

impl TabLayout {
    pub fn entries(&self) -> &[TabEntry] {
        &self.entries
    }

    pub fn visible_tabs(&self) -> impl Iterator<Item = &TabEntry> {
        self.entries.iter().filter(|entry| entry.visible)
    }

    /// The tab to show when `tab` is hidden or gone: the first shown tab.
    pub fn resolve(&self, tab: DashboardTab) -> DashboardTab {
        self.visible_tabs()
            .map(TabEntry::dashboard_tab)
            .find(|visible| visible.same_tab(tab))
            .or_else(|| self.visible_tabs().next().map(TabEntry::dashboard_tab))
            .unwrap_or(DashboardTab::Favorites)
    }

    /// A shown tab that lists `provider`'s accounts, preferring its own tab.
    pub fn tab_showing(&self, provider: UsageProvider) -> Option<DashboardTab> {
        let own = DashboardTab::Provider(provider);
        self.visible_tabs()
            .map(TabEntry::dashboard_tab)
            .find(|tab| tab.same_tab(own))
            .or_else(|| {
                self.visible_tabs()
                    .map(TabEntry::dashboard_tab)
                    .find(|tab| tab.includes_provider(provider))
            })
    }

    /// Shows or hides a tab. The last shown tab cannot be hidden, so the
    /// bar is never empty. Returns whether anything changed.
    pub fn toggle_visible(&mut self, index: usize) -> bool {
        let visible_count = self.visible_tabs().count();
        let Some(entry) = self.entries.get_mut(index) else {
            return false;
        };
        if entry.visible && visible_count == 1 {
            return false;
        }
        entry.visible = !entry.visible;
        true
    }

    /// Makes `provider`'s own tab shown, e.g. after adding an account no
    /// shown tab would list.
    pub fn show_provider(&mut self, provider: UsageProvider) {
        for entry in &mut self.entries {
            if entry.kind == TabKind::Provider(provider) {
                entry.visible = true;
            }
        }
    }

    /// Shows the tabs of `providers` and hides the other providers' own
    /// tabs, as the welcome leaves them. Favorites and custom tabs stay.
    pub fn show_only_providers(&mut self, providers: &[UsageProvider]) {
        if providers.is_empty() {
            return;
        }
        for entry in &mut self.entries {
            if let TabKind::Provider(provider) = entry.kind {
                entry.visible = providers.contains(&provider);
            }
        }
    }

    /// Moves a tab one place earlier (`-1`) or later (`1`) in the bar.
    pub fn move_tab(&mut self, index: usize, offset: isize) -> bool {
        let Some(target) = index
            .checked_add_signed(offset)
            .filter(|target| *target < self.entries.len())
        else {
            return false;
        };
        if index >= self.entries.len() {
            return false;
        }
        self.entries.swap(index, target);
        true
    }

    pub fn custom_tab(&self, id: u32) -> Option<&CustomTab> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            TabKind::Custom(custom) if custom.id == id => Some(custom),
            _ => None,
        })
    }

    /// Adds a new shown custom tab at the end of the bar and returns it.
    pub fn add_custom(
        &mut self,
        name: &str,
        providers: ProviderSet,
        accounts: Vec<AccountId>,
    ) -> DashboardTab {
        let id = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                TabKind::Custom(custom) => Some(custom.id),
                _ => None,
            })
            .max()
            .map_or(1, |id| id + 1);
        let entry = TabEntry {
            kind: TabKind::Custom(CustomTab {
                id,
                name: clean_name(name),
                providers,
                accounts,
            }),
            visible: true,
        };
        let tab = entry.dashboard_tab();
        self.entries.push(entry);
        tab
    }

    pub fn update_custom(
        &mut self,
        id: u32,
        name: &str,
        providers: ProviderSet,
        accounts: Vec<AccountId>,
    ) {
        for entry in &mut self.entries {
            if let TabKind::Custom(custom) = &mut entry.kind
                && custom.id == id
            {
                custom.name = clean_name(name);
                custom.providers = providers;
                custom.accounts.clone_from(&accounts);
            }
        }
    }

    /// The accounts each custom tab picked one by one, by tab id.
    pub fn custom_accounts(&self) -> HashMap<u32, Vec<AccountId>> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                TabKind::Custom(custom) => Some((custom.id, custom.accounts.clone())),
                _ => None,
            })
            .collect()
    }

    /// Deletes a custom tab, keeping at least one tab shown.
    pub fn remove_custom(&mut self, id: u32) {
        self.entries
            .retain(|entry| !matches!(&entry.kind, TabKind::Custom(custom) if custom.id == id));
        if self.visible_tabs().next().is_none()
            && let Some(first) = self.entries.first_mut()
        {
            first.visible = true;
        }
    }

    /// One line per tab: kind, key, shown, providers, name, accounts;
    /// tab-separated.
    fn to_text(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                let visible = if entry.visible { "1" } else { "0" };
                match &entry.kind {
                    TabKind::Provider(provider) => {
                        format!("provider\t{}\t{visible}", provider.cli_name())
                    }
                    TabKind::Favorites => format!("favorites\t-\t{visible}"),
                    TabKind::Cost => format!("cost\t-\t{visible}"),
                    TabKind::Custom(custom) => format!(
                        "custom\t{}\t{visible}\t{}\t{}\t{}",
                        custom.id,
                        custom
                            .providers
                            .providers()
                            .map(UsageProvider::cli_name)
                            .collect::<Vec<_>>()
                            .join(","),
                        custom.name,
                        custom
                            .accounts
                            .iter()
                            .map(AccountId::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Reads a saved layout. Providers added since it was saved join at the
    /// end; anything unreadable is skipped rather than failing the bar.
    fn from_text(text: &str) -> Self {
        let mut entries = Vec::new();
        for line in text.lines() {
            let fields = line.split('\t').collect::<Vec<_>>();
            let visible = fields.get(2) != Some(&"0");
            let kind = match fields.as_slice() {
                ["provider", key, ..] => provider_from_cli_name(key).map(TabKind::Provider),
                ["favorites", ..] => Some(TabKind::Favorites),
                ["cost", ..] => Some(TabKind::Cost),
                ["custom", id, _, providers, name, rest @ ..] => id.parse().ok().map(|id| {
                    TabKind::Custom(CustomTab {
                        id,
                        name: clean_name(name),
                        providers: ProviderSet::from_providers(
                            providers.split(',').filter_map(provider_from_cli_name),
                        ),
                        accounts: rest
                            .first()
                            .into_iter()
                            .flat_map(|accounts| accounts.split(','))
                            .filter_map(|account| account.parse().ok())
                            .collect(),
                    })
                }),
                _ => None,
            };
            let Some(kind) = kind else { continue };
            let duplicate = entries
                .iter()
                .any(|entry: &TabEntry| match (&entry.kind, &kind) {
                    (TabKind::Custom(a), TabKind::Custom(b)) => a.id == b.id,
                    (a, b) => a == b,
                });
            if !duplicate {
                entries.push(TabEntry { kind, visible });
            }
        }
        for tab in PROVIDER_TABS {
            if !entries
                .iter()
                .any(|entry| entry.kind == TabKind::Provider(tab.provider))
            {
                entries.push(TabEntry {
                    kind: TabKind::Provider(tab.provider),
                    visible: true,
                });
            }
        }
        // Tabs added since the layout was saved join at the end.
        for kind in [TabKind::Favorites, TabKind::Cost] {
            if !entries.iter().any(|entry| entry.kind == kind) {
                entries.push(TabEntry {
                    kind,
                    visible: true,
                });
            }
        }
        let mut layout = Self { entries };
        if layout.visible_tabs().next().is_none() {
            layout.entries[0].visible = true;
        }
        layout
    }
}

fn provider_from_cli_name(name: &str) -> Option<UsageProvider> {
    PROVIDER_TABS
        .iter()
        .map(|tab| tab.provider)
        .find(|provider| provider.cli_name() == name)
}

/// A name that fits a tab button and the one-line file format.
pub fn clean_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_CUSTOM_TAB_NAME)
        .collect()
}

pub fn load_saved() -> TabLayout {
    preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(LAYOUT_FILE)).ok())
        .map(|text| TabLayout::from_text(&text))
        .unwrap_or_default()
}

pub fn save(layout: &TabLayout) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(LAYOUT_FILE), layout.to_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bar_has_every_provider_and_favorites_in_the_middle() {
        let layout = TabLayout::default();
        let tabs = layout
            .visible_tabs()
            .map(TabEntry::dashboard_tab)
            .collect::<Vec<_>>();
        assert_eq!(tabs.len(), PROVIDER_TABS.len() + 2);
        assert_eq!(tabs.last(), Some(&DashboardTab::Cost));
        assert_eq!(
            tabs[PROVIDER_TABS.len().div_ceil(2)],
            DashboardTab::Favorites
        );
    }

    #[test]
    fn a_layout_round_trips_through_its_file() {
        let mut layout = TabLayout::default();
        let prepaid =
            ProviderSet::from_providers([UsageProvider::DeepSeek, UsageProvider::OpenRouter]);
        let picked = AccountId::new();
        layout.add_custom("  Prepaid   balance ", prepaid, vec![picked]);
        assert!(layout.toggle_visible(0));
        assert!(layout.move_tab(1, 1));

        let restored = TabLayout::from_text(&layout.to_text());
        assert_eq!(restored, layout);
        let custom = restored.custom_tab(1).unwrap();
        assert_eq!(custom.name, "Prepaid bala");
        assert!(custom.providers.contains(UsageProvider::DeepSeek));
        assert!(!custom.providers.contains(UsageProvider::Codex));
        assert_eq!(custom.accounts, vec![picked]);
    }

    #[test]
    fn providers_added_after_saving_join_the_bar_and_junk_is_skipped() {
        let layout = TabLayout::from_text("provider\tclaude\t1\nnonsense\nprovider\tunknown\t1\n");
        assert_eq!(
            layout.entries()[0].kind,
            TabKind::Provider(UsageProvider::Claude)
        );
        assert_eq!(layout.entries().len(), PROVIDER_TABS.len() + 2);
    }

    #[test]
    fn the_last_shown_tab_stays_shown() {
        let mut layout = TabLayout::default();
        let count = layout.entries().len();
        for index in 0..count - 1 {
            assert!(layout.toggle_visible(index));
        }
        assert!(!layout.toggle_visible(count - 1));
        assert_eq!(layout.visible_tabs().count(), 1);
    }

    #[test]
    fn a_hidden_selection_falls_back_and_providers_are_found_in_custom_tabs() {
        let mut layout = TabLayout::default();
        let codex = DashboardTab::Provider(UsageProvider::Codex);
        assert!(layout.toggle_visible(0));
        assert_ne!(layout.resolve(codex), codex);
        assert_eq!(layout.tab_showing(UsageProvider::Codex), None);

        let mine = layout.add_custom(
            "Mine",
            ProviderSet::from_providers([UsageProvider::Codex]),
            Vec::new(),
        );
        assert_eq!(layout.tab_showing(UsageProvider::Codex), Some(mine));

        let DashboardTab::Custom { id, .. } = mine else {
            unreachable!()
        };
        layout.remove_custom(id);
        layout.show_provider(UsageProvider::Codex);
        assert_eq!(layout.tab_showing(UsageProvider::Codex), Some(codex));
    }
}
