use std::{env, fs, io, path::PathBuf};

use iced::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeId {
    Dark,
    GreySpace,
    LanternStreet,
    White,
}

impl ThemeId {
    pub const fn as_key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::GreySpace => "grey-space",
            Self::LanternStreet => "lantern-street",
            Self::White => "white",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "dark" => Some(Self::Dark),
            "grey-space" => Some(Self::GreySpace),
            "lantern-street" => Some(Self::LanternStreet),
            "white" => Some(Self::White),
            _ => None,
        }
    }

    pub fn definition(self) -> &'static ThemeDefinition {
        THEME_MANIFEST
            .iter()
            .find(|theme| theme.id == self)
            .expect("every ThemeId must have a manifest entry")
    }
}

#[derive(Clone, Copy)]
pub struct ThemeColors {
    pub window_surface: [u8; 3],
    pub text: [u8; 3],
    pub muted_text: [u8; 3],
    pub control_surface: [u8; 3],
    pub border: [u8; 3],
    pub is_light: bool,
    pub hover: [u8; 3],
    pub hover_opacity: f32,
    pub danger_hover: [u8; 3],
}

impl ThemeColors {
    pub fn window_surface(self) -> Color {
        Color::from_rgb8(
            self.window_surface[0],
            self.window_surface[1],
            self.window_surface[2],
        )
    }

    pub fn text(self) -> Color {
        Color::from_rgb8(self.text[0], self.text[1], self.text[2])
    }

    pub fn muted_text(self) -> Color {
        Color::from_rgb8(self.muted_text[0], self.muted_text[1], self.muted_text[2])
    }

    pub fn control_surface(self) -> Color {
        Color::from_rgb8(
            self.control_surface[0],
            self.control_surface[1],
            self.control_surface[2],
        )
    }

    pub fn border(self, opacity: f32) -> Color {
        Color::from_rgba(
            f32::from(self.border[0]) / 255.0,
            f32::from(self.border[1]) / 255.0,
            f32::from(self.border[2]) / 255.0,
            opacity,
        )
    }

    pub fn hover(self) -> Color {
        Color::from_rgba(
            f32::from(self.hover[0]) / 255.0,
            f32::from(self.hover[1]) / 255.0,
            f32::from(self.hover[2]) / 255.0,
            self.hover_opacity,
        )
    }

    pub fn danger_hover(self) -> Color {
        Color::from_rgb8(
            self.danger_hover[0],
            self.danger_hover[1],
            self.danger_hover[2],
        )
    }
}

#[derive(Clone, Copy)]
pub struct ThemeBackdrop {
    pub image_bytes: &'static [u8],
    pub image_opacity: f32,
    pub image_scale: f32,
    pub shade_opacity: f32,
}

#[derive(Clone, Copy)]
pub struct ThemeDefinition {
    pub id: ThemeId,
    pub label: &'static str,
    pub swatch: [u8; 3],
    pub accent: [u8; 3],
    pub colors: ThemeColors,
    pub backdrop: Option<ThemeBackdrop>,
}

impl ThemeDefinition {
    pub fn swatch_color(self) -> Color {
        Color::from_rgb8(self.swatch[0], self.swatch[1], self.swatch[2])
    }

    pub fn accent_color(self) -> Color {
        Color::from_rgb8(self.accent[0], self.accent[1], self.accent[2])
    }
}

pub const DEFAULT_THEME_ID: ThemeId = ThemeId::GreySpace;

pub const THEME_MANIFEST: &[ThemeDefinition] = &[
    ThemeDefinition {
        id: ThemeId::GreySpace,
        label: "Grey Space",
        swatch: [111, 116, 94],
        accent: [111, 116, 94],
        colors: ThemeColors {
            window_surface: [10, 12, 10],
            text: [242, 243, 233],
            muted_text: [190, 192, 180],
            control_surface: [10, 12, 10],
            border: [255, 255, 255],
            is_light: false,
            hover: [171, 179, 151],
            hover_opacity: 0.16,
            danger_hover: [219, 97, 82],
        },
        backdrop: Some(ThemeBackdrop {
            image_bytes: include_bytes!("../assets/themes/grey-space.jpg"),
            image_opacity: 1.0,
            image_scale: 1.035,
            shade_opacity: 0.10,
        }),
    },
    ThemeDefinition {
        id: ThemeId::Dark,
        label: "Dark",
        swatch: [55, 59, 69],
        accent: [55, 59, 69],
        colors: ThemeColors {
            window_surface: [0, 0, 0],
            text: [242, 244, 248],
            muted_text: [185, 190, 200],
            control_surface: [18, 20, 27],
            border: [255, 255, 255],
            is_light: false,
            hover: [24, 24, 24],
            hover_opacity: 1.0,
            danger_hover: [213, 59, 46],
        },
        backdrop: None,
    },
    ThemeDefinition {
        id: ThemeId::LanternStreet,
        label: "Lantern Street",
        swatch: [74, 112, 165],
        accent: [74, 112, 165],
        colors: ThemeColors {
            window_surface: [10, 17, 30],
            text: [241, 245, 250],
            muted_text: [181, 191, 205],
            control_surface: [10, 17, 30],
            border: [255, 255, 255],
            is_light: false,
            hover: [116, 157, 207],
            hover_opacity: 0.18,
            danger_hover: [219, 82, 69],
        },
        backdrop: Some(ThemeBackdrop {
            image_bytes: include_bytes!("../assets/themes/lantern-street.jpg"),
            image_opacity: 1.0,
            image_scale: 1.035,
            shade_opacity: 0.38,
        }),
    },
    ThemeDefinition {
        id: ThemeId::White,
        label: "White",
        swatch: [255, 255, 255],
        accent: [49, 100, 165],
        colors: ThemeColors {
            window_surface: [255, 255, 255],
            // Near-black text and a dark slate for secondary lines: the
            // lighter grey was hard to read on white.
            text: [17, 24, 39],
            muted_text: [55, 65, 81],
            control_surface: [241, 244, 248],
            border: [100, 112, 128],
            is_light: true,
            hover: [66, 101, 145],
            hover_opacity: 0.11,
            danger_hover: [196, 54, 47],
        },
        backdrop: None,
    },
];

pub fn load_saved_theme() -> ThemeId {
    preference_directory()
        .map(|directory| directory.join("theme.txt"))
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|key| ThemeId::from_key(key.trim()))
        .unwrap_or(DEFAULT_THEME_ID)
}

pub fn save_theme(theme_id: ThemeId) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("theme.txt"), theme_id.as_key())
}

pub fn preference_directory() -> io::Result<PathBuf> {
    let config_root = env::var_os("APPDATA")
        .or_else(|| env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        })
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "user config directory unavailable")
        })?;

    let directory = config_root.join("UsageMonitor");
    // Preferences were kept under the preview name before the rename; move
    // them over once so the theme, order, and display choices are kept.
    static MIGRATION: std::sync::Once = std::sync::Once::new();
    MIGRATION.call_once(|| {
        let legacy = config_root.join("UsageMonitorPreview");
        if !directory.exists() && legacy.is_dir() {
            let _ = fs::rename(&legacy, &directory);
        }
    });
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_keys_round_trip_and_unknown_keys_are_rejected() {
        for theme in [
            ThemeId::Dark,
            ThemeId::GreySpace,
            ThemeId::LanternStreet,
            ThemeId::White,
        ] {
            assert_eq!(ThemeId::from_key(theme.as_key()), Some(theme));
            assert!(!theme.definition().label.is_empty());
        }

        assert_eq!(ThemeId::from_key("not-a-theme"), None);
    }

    #[test]
    fn default_theme_has_a_registered_definition_and_backdrop() {
        let definition = DEFAULT_THEME_ID.definition();

        assert_eq!(definition.id, DEFAULT_THEME_ID);
        assert!(definition.backdrop.is_some());
    }

    #[test]
    fn dark_theme_uses_black_as_its_window_background() {
        assert_eq!(ThemeId::Dark.definition().colors.window_surface, [0, 0, 0]);
    }

    #[test]
    fn lantern_street_theme_has_its_own_backdrop() {
        let backdrop = ThemeId::LanternStreet
            .definition()
            .backdrop
            .expect("Lantern Street must render its background image");

        assert!(!backdrop.image_bytes.is_empty());
        assert_eq!(backdrop.image_opacity, 1.0);
    }

    #[test]
    fn white_theme_uses_a_pure_white_surface_and_dark_readable_text() {
        let theme = ThemeId::White.definition();

        assert_eq!(theme.colors.window_surface, [255, 255, 255]);
        assert!(theme.colors.is_light);
        assert!(theme.backdrop.is_none());
        assert!(theme.colors.text[0] < 64);
        assert!(theme.colors.muted_text[0] < 128);
    }
}
