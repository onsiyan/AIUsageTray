//! What each account card shows, for people who want a denser popup: the
//! email and plan line, team budgets, and which stored reset credits are
//! listed.

use std::{
    fs, io,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use chrono::{DateTime, Duration, Utc};

use crate::theme::preference_directory;

const ACCOUNT_DETAILS_FILE: &str = "account_details.txt";
const RESET_CREDITS_FILE: &str = "reset_credits.txt";
const TEAM_BUDGETS_FILE: &str = "team_budgets.txt";

/// A reset credit expiring within this many days stays listed under
/// [`ResetCreditVisibility::ExpiringSoon`].
pub const SOON_DAYS: i64 = 5;

static SHOW_ACCOUNT_DETAILS: AtomicBool = AtomicBool::new(true);
static RESET_CREDITS: AtomicU8 = AtomicU8::new(0);
/// Team workspace amounts (Codex workspace balance and monthly limit,
/// Cursor member budget) are hidden until asked for.
static SHOW_TEAM_BUDGETS: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCreditVisibility {
    All,
    /// Only credits that expire within [`SOON_DAYS`]; later ones and those
    /// without an expiry date are hidden.
    ExpiringSoon,
    None,
}

impl ResetCreditVisibility {
    pub const ALL: [Self; 3] = [Self::All, Self::ExpiringSoon, Self::None];

    fn as_key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ExpiringSoon => "soon",
            Self::None => "none",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_key() == key)
    }

    fn index(self) -> u8 {
        match self {
            Self::All => 0,
            Self::ExpiringSoon => 1,
            Self::None => 2,
        }
    }

    /// Whether a credit with this expiry is listed.
    pub fn shows(self, expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        match self {
            Self::All => true,
            Self::ExpiringSoon => {
                expires_at.is_some_and(|expires_at| expires_at - now <= Duration::days(SOON_DAYS))
            }
            Self::None => false,
        }
    }
}

pub fn show_account_details() -> bool {
    SHOW_ACCOUNT_DETAILS.load(Ordering::Relaxed)
}

pub fn set_show_account_details(shown: bool) {
    SHOW_ACCOUNT_DETAILS.store(shown, Ordering::Relaxed);
}

pub fn show_team_budgets() -> bool {
    SHOW_TEAM_BUDGETS.load(Ordering::Relaxed)
}

pub fn set_show_team_budgets(shown: bool) {
    SHOW_TEAM_BUDGETS.store(shown, Ordering::Relaxed);
}

/// Whether a metric, or a diagnostic's source, is one of the team amounts
/// behind [`show_team_budgets`].
pub fn is_team_budget_key(key: &str) -> bool {
    key.starts_with("team.")
}

pub fn reset_credits() -> ResetCreditVisibility {
    match RESET_CREDITS.load(Ordering::Relaxed) {
        1 => ResetCreditVisibility::ExpiringSoon,
        2 => ResetCreditVisibility::None,
        _ => ResetCreditVisibility::All,
    }
}

pub fn set_reset_credits(mode: ResetCreditVisibility) {
    RESET_CREDITS.store(mode.index(), Ordering::Relaxed);
}

/// Applies the saved choices; called once at start.
pub fn load_saved() {
    let read = |file: &str| {
        preference_directory()
            .ok()
            .and_then(|directory| fs::read_to_string(directory.join(file)).ok())
    };
    set_show_account_details(
        read(ACCOUNT_DETAILS_FILE).is_none_or(|value| value.trim() != "hidden"),
    );
    set_show_team_budgets(read(TEAM_BUDGETS_FILE).is_some_and(|value| value.trim() == "shown"));
    set_reset_credits(
        read(RESET_CREDITS_FILE)
            .and_then(|value| ResetCreditVisibility::from_key(value.trim()))
            .unwrap_or(ResetCreditVisibility::All),
    );
}

pub fn save_show_account_details(shown: bool) -> io::Result<()> {
    write(ACCOUNT_DETAILS_FILE, if shown { "shown" } else { "hidden" })
}

pub fn save_show_team_budgets(shown: bool) -> io::Result<()> {
    write(TEAM_BUDGETS_FILE, if shown { "shown" } else { "hidden" })
}

pub fn save_reset_credits(mode: ResetCreditVisibility) -> io::Result<()> {
    write(RESET_CREDITS_FILE, mode.as_key())
}

fn write(file: &str, value: &str) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(file), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_credit_modes_round_trip_and_filter_by_expiry() {
        for mode in ResetCreditVisibility::ALL {
            assert_eq!(ResetCreditVisibility::from_key(mode.as_key()), Some(mode));
        }
        assert_eq!(ResetCreditVisibility::from_key("other"), None);

        let now = Utc::now();
        let soon = Some(now + Duration::days(4));
        let edge = Some(now + Duration::days(SOON_DAYS));
        let later = Some(now + Duration::days(16));
        let all = ResetCreditVisibility::All;
        let expiring = ResetCreditVisibility::ExpiringSoon;
        let none = ResetCreditVisibility::None;

        assert!(all.shows(later, now) && all.shows(None, now));
        assert!(expiring.shows(soon, now) && expiring.shows(edge, now));
        assert!(!expiring.shows(later, now) && !expiring.shows(None, now));
        assert!(!none.shows(soon, now));
    }

    #[test]
    fn team_budgets_are_the_team_metrics() {
        assert!(is_team_budget_key("team.workspace_balance"));
        assert!(is_team_budget_key("team.monthly-usage"));
        assert!(!is_team_budget_key("on_demand.team"));
    }
}
