//! Whether usage percentages read as what is left or what has been used.

use std::{
    fs, io,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::theme::preference_directory;

const PREFERENCE_FILE: &str = "percent_display.txt";

static SHOW_USED: AtomicBool = AtomicBool::new(false);

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

    /// Converts a remaining percentage into the value this mode displays.
    pub fn displayed(self, remaining: f64) -> f64 {
        match self {
            PercentDisplay::Remaining => remaining,
            PercentDisplay::Used => 100.0 - remaining,
        }
    }
}

/// The mode every usage row renders with.
pub fn current() -> PercentDisplay {
    if SHOW_USED.load(Ordering::Relaxed) {
        PercentDisplay::Used
    } else {
        PercentDisplay::Remaining
    }
}

pub fn set_current(mode: PercentDisplay) {
    SHOW_USED.store(mode == PercentDisplay::Used, Ordering::Relaxed);
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
        }
        assert_eq!(PercentDisplay::from_key("other"), None);
        assert_eq!(PercentDisplay::Remaining.displayed(78.0), 78.0);
        assert_eq!(PercentDisplay::Used.displayed(78.0), 22.0);
    }
}
