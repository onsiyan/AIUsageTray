//! Whether the popup window is closed, rather than hidden, when it goes back
//! to the tray. Closing releases the GPU memory (about 100 MB) at the cost of
//! a slower next open, so it is off unless the user turns it on.

use std::{fs, io};

use crate::theme::preference_directory;

const PREFERENCE_FILE: &str = "memory_saver.txt";
const ENABLED: &str = "on";

pub fn load_saved() -> bool {
    preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(PREFERENCE_FILE)).ok())
        .is_some_and(|value| value.trim() == ENABLED)
}

pub fn save(enabled: bool) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join(PREFERENCE_FILE),
        if enabled { ENABLED } else { "off" },
    )
}
