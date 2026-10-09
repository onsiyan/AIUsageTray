//! Whether usage percentages read as what is left or what has been used.
//! The theme menu sets the default; a tab may read the other way, set from
//! the tab manager and saved as `tab_percent.txt`.

use std::{
    cell::Cell,
    collections::HashMap,
    fs, io,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::theme::preference_directory;

const PREFERENCE_FILE: &str = "percent_display.txt";
const TAB_PREFERENCE_FILE: &str = "tab_percent.txt";

static SHOW_USED: AtomicBool = AtomicBool::new(false);
/// Tabs that read differently from the default, by tab key.
static TAB_MODES: Mutex<Option<HashMap<String, PercentDisplay>>> = Mutex::new(None);

thread_local! {
    /// The mode of the tab being drawn; set by the view before it draws.
    static DRAWING: Cell<Option<PercentDisplay>> = const { Cell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PercentDisplay {
    Remaining,
    Used,
}

impl PercentDisplay {
    pub const ALL: [PercentDisplay; 2] = [PercentDisplay::Remaining, PercentDisplay::Used];

    fn as_key(self) -> &'static str {
        match self {
            PercentDisplay::Remaining => "remaining",
            PercentDisplay::Used => "used",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_key() == key)
    }

    pub fn other(self) -> Self {
        match self {
            PercentDisplay::Remaining => PercentDisplay::Used,
            PercentDisplay::Used => PercentDisplay::Remaining,
        }
    }

    /// Converts a remaining percentage into the value this mode displays.
    pub fn displayed(self, remaining: f64) -> f64 {
        match self {
            PercentDisplay::Remaining => remaining,
            PercentDisplay::Used => 100.0 - remaining,
        }
    }
}

/// The mode usage rows render with: the drawn tab's, else the default.
pub fn current() -> PercentDisplay {
    DRAWING.get().unwrap_or_else(default_mode)
}

/// The mode tabs read with unless set otherwise.
pub fn default_mode() -> PercentDisplay {
    if SHOW_USED.load(Ordering::Relaxed) {
        PercentDisplay::Used
    } else {
        PercentDisplay::Remaining
    }
}

/// Changes the default. Tabs set to the new default no longer differ from
/// it, so they follow it from now on.
pub fn set_default(mode: PercentDisplay) {
    SHOW_USED.store(mode == PercentDisplay::Used, Ordering::Relaxed);
    let changed = with_tab_modes(|modes| {
        let before = modes.len();
        modes.retain(|_, tab_mode| *tab_mode != mode);
        modes.len() != before
    });
    if changed {
        save_tab_modes();
    }
}

/// Makes the tab with `key` the one being drawn.
pub fn begin_drawing(key: &str) {
    DRAWING.set(Some(for_tab(key)));
}

pub fn for_tab(key: &str) -> PercentDisplay {
    with_tab_modes(|modes| modes.get(key).copied()).unwrap_or_else(default_mode)
}

/// Sets how one tab reads, and saves it.
pub fn set_for_tab(key: &str, mode: PercentDisplay) {
    let default = default_mode();
    with_tab_modes(|modes| {
        if mode == default {
            modes.remove(key);
        } else {
            modes.insert(key.to_owned(), mode);
        }
    });
    save_tab_modes();
}

/// Drops the setting of a tab that no longer exists.
pub fn forget_tab(key: &str) {
    if with_tab_modes(|modes| modes.remove(key).is_some()) {
        save_tab_modes();
    }
}

fn with_tab_modes<T>(action: impl FnOnce(&mut HashMap<String, PercentDisplay>) -> T) -> T {
    let mut modes = TAB_MODES.lock().unwrap_or_else(|error| error.into_inner());
    action(modes.get_or_insert_with(|| {
        preference_directory()
            .ok()
            .and_then(|directory| fs::read_to_string(directory.join(TAB_PREFERENCE_FILE)).ok())
            .map(|text| parse_tab_modes(&text))
            .unwrap_or_default()
    }))
}

fn parse_tab_modes(text: &str) -> HashMap<String, PercentDisplay> {
    text.lines()
        .filter_map(|line| {
            let (key, mode) = line.split_once('\t')?;
            Some((key.to_owned(), PercentDisplay::from_key(mode.trim())?))
        })
        .collect()
}

fn format_tab_modes(modes: &HashMap<String, PercentDisplay>) -> String {
    let mut lines = modes
        .iter()
        .map(|(key, mode)| format!("{key}\t{}\n", mode.as_key()))
        .collect::<Vec<_>>();
    lines.sort();
    lines.concat()
}

fn save_tab_modes() {
    let text = with_tab_modes(|modes| format_tab_modes(modes));
    let result = preference_directory().and_then(|directory| {
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(TAB_PREFERENCE_FILE), text)
    });
    if let Err(error) = result {
        crate::app_log::write(format!("tab percent preference save failed: {error}"));
    }
}

pub fn load_saved() -> PercentDisplay {
    preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(PREFERENCE_FILE)).ok())
        .and_then(|key| PercentDisplay::from_key(key.trim()))
        .unwrap_or(PercentDisplay::Remaining)
}

pub fn save(mode: PercentDisplay) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(PREFERENCE_FILE), mode.as_key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_and_flip_the_percentage() {
        for mode in PercentDisplay::ALL {
            assert_eq!(PercentDisplay::from_key(mode.as_key()), Some(mode));
            assert_eq!(mode.other().other(), mode);
        }
        assert_eq!(PercentDisplay::from_key("other"), None);
        assert_eq!(PercentDisplay::Remaining.displayed(78.0), 78.0);
        assert_eq!(PercentDisplay::Used.displayed(78.0), 22.0);
    }

    #[test]
    fn tab_modes_round_trip_and_junk_is_skipped() {
        let modes = HashMap::from([
            ("codex".to_owned(), PercentDisplay::Used),
            ("custom-3".to_owned(), PercentDisplay::Remaining),
        ]);
        let text = format_tab_modes(&modes);
        assert_eq!(text, "codex\tused\ncustom-3\tremaining\n");
        assert_eq!(parse_tab_modes(&text), modes);
        assert!(parse_tab_modes("claude\tsideways\nno tab here\n").is_empty());
    }
}
