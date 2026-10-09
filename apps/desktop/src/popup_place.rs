//! Where the popup opens: above the tray icon, in the middle of the screen,
//! or where it was last left, which is kept in screen pixels.

use std::fs;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::theme::preference_directory;

const PLACE_FILE: &str = "popup-place.txt";
const LAST_POSITION_FILE: &str = "popup-last-position.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Tray,
    Center,
    Last,
}

impl Place {
    pub(crate) const ALL: [Self; 3] = [Self::Tray, Self::Center, Self::Last];

    fn key(self) -> &'static str {
        match self {
            Self::Tray => "tray",
            Self::Center => "center",
            Self::Last => "last",
        }
    }

    fn from_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|place| place.key() == key.trim())
            .unwrap_or(Self::Tray)
    }

    fn index(self) -> u8 {
        match self {
            Self::Tray => 0,
            Self::Center => 1,
            Self::Last => 2,
        }
    }
}

static PLACE: AtomicU8 = AtomicU8::new(0);
/// The popup's top-left corner when it last hid, in screen pixels.
static LAST_POSITION: Mutex<Option<(f32, f32)>> = Mutex::new(None);

pub(crate) fn place() -> Place {
    Place::ALL[usize::from(PLACE.load(Ordering::Relaxed)).min(2)]
}

pub(crate) fn set_place(place: Place) {
    PLACE.store(place.index(), Ordering::Relaxed);
}

pub(crate) fn last_position() -> Option<(f32, f32)> {
    LAST_POSITION.lock().ok().and_then(|position| *position)
}

/// Applies the saved choice and position; called once at start.
pub(crate) fn load_saved() {
    let read = |file: &str| {
        preference_directory()
            .ok()
            .and_then(|directory| fs::read_to_string(directory.join(file)).ok())
    };
    set_place(read(PLACE_FILE).map_or(Place::Tray, |key| Place::from_key(&key)));
    if let Ok(mut last) = LAST_POSITION.lock() {
        *last = read(LAST_POSITION_FILE).and_then(|text| parse_position(&text));
    }
}

pub(crate) fn save_place(place: Place) -> io::Result<()> {
    write(PLACE_FILE, place.key())
}

/// Keeps where the popup was, unless it is where it already was.
pub(crate) fn keep_last_position(x: f32, y: f32) -> io::Result<()> {
    let position = (x.round(), y.round());
    if let Ok(mut last) = LAST_POSITION.lock() {
        if *last == Some(position) {
            return Ok(());
        }
        *last = Some(position);
    }
    write(
        LAST_POSITION_FILE,
        &format!("{} {}", position.0, position.1),
    )
}

fn write(file: &str, value: &str) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(file), value)
}

fn parse_position(text: &str) -> Option<(f32, f32)> {
    let mut parts = text.split_whitespace().map(str::parse::<f32>);
    let (Some(Ok(x)), Some(Ok(y)), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_choices_and_positions_are_read_back() {
        assert_eq!(Place::from_key("center\n"), Place::Center);
        assert_eq!(Place::from_key("last"), Place::Last);
        assert_eq!(Place::from_key("anything else"), Place::Tray);
        assert_eq!(parse_position("-1200 340"), Some((-1200.0, 340.0)));
        assert_eq!(parse_position("12"), None);
        assert_eq!(parse_position("1 2 3"), None);
        assert_eq!(parse_position("NaN 4"), None);
    }
}
