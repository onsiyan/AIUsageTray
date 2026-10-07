//! The user's own theme: a dark or light base, a background color, an
//! accent color, and optionally a background image of their choosing,
//! dimmed so text stays readable. Saved as `custom_theme.txt`, with the
//! image prepared to the window's size as `custom-backdrop.jpg`.

use std::{
    fs, io,
    path::PathBuf,
    sync::{Mutex, RwLock},
};

use crate::{
    BACKDROP_PIXEL_SCALE, WINDOW_HEIGHT, WINDOW_WIDTH,
    theme::{ThemeBackdrop, ThemeColors, ThemeDefinition, ThemeId, preference_directory},
};

const SETTINGS_FILE: &str = "custom_theme.txt";
const IMAGE_FILE: &str = "custom-backdrop.jpg";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Light,
    Medium,
    Strong,
}

impl Dim {
    pub const ALL: [Self; 3] = [Self::Light, Self::Medium, Self::Strong];

    fn as_key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Medium => "medium",
            Self::Strong => "strong",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|dim| dim.as_key() == key)
    }

    /// How much the image is covered. A light base needs more, since its
    /// dark text sits on a pale veil rather than a dark one.
    fn shade(self, light: bool) -> f32 {
        match (self, light) {
            (Self::Light, false) => 0.15,
            (Self::Medium, false) => 0.38,
            (Self::Strong, false) => 0.6,
            (Self::Light, true) => 0.4,
            (Self::Medium, true) => 0.58,
            (Self::Strong, true) => 0.76,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomTheme {
    pub light: bool,
    pub background: [u8; 3],
    pub accent: [u8; 3],
    pub image: bool,
    pub dim: Dim,
}

impl Default for CustomTheme {
    fn default() -> Self {
        Self {
            light: false,
            background: [16, 20, 28],
            accent: [74, 112, 165],
            image: false,
            dim: Dim::Medium,
        }
    }
}

/// Ready-made colors offered beside the hex field.
pub const BACKGROUND_PRESETS: [[u8; 3]; 8] = [
    [0, 0, 0],
    [16, 20, 28],
    [24, 31, 46],
    [33, 24, 44],
    [18, 38, 32],
    [44, 28, 24],
    [241, 244, 248],
    [255, 255, 255],
];
pub const ACCENT_PRESETS: [[u8; 3]; 8] = [
    [74, 112, 165],
    [49, 100, 165],
    [111, 116, 94],
    [46, 139, 87],
    [196, 120, 40],
    [190, 70, 90],
    [130, 90, 190],
    [90, 160, 170],
];

static SETTINGS: Mutex<Option<CustomTheme>> = Mutex::new(None);
/// The definition built from the settings. A new one is made, and the old
/// one kept, each time the settings change: views hold `&'static` themes,
/// and a few hundred bytes per change is a fair price for that.
static DEFINITION: RwLock<Option<&'static ThemeDefinition>> = RwLock::new(None);

pub fn settings() -> CustomTheme {
    let mut settings = SETTINGS.lock().unwrap_or_else(|error| error.into_inner());
    *settings.get_or_insert_with(load_saved)
}

pub fn definition() -> &'static ThemeDefinition {
    if let Some(definition) = *DEFINITION.read().unwrap_or_else(|error| error.into_inner()) {
        return definition;
    }
    apply(settings())
}

/// Uses `theme` from now on and returns its definition.
pub fn apply(theme: CustomTheme) -> &'static ThemeDefinition {
    *SETTINGS.lock().unwrap_or_else(|error| error.into_inner()) = Some(theme);
    let definition: &'static ThemeDefinition = Box::leak(Box::new(build(theme)));
    *DEFINITION
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(definition);
    definition
}

fn build(theme: CustomTheme) -> ThemeDefinition {
    let CustomTheme {
        light,
        background,
        accent,
        image,
        dim,
    } = theme;
    let colors = if light {
        ThemeColors {
            window_surface: background,
            text: [17, 24, 39],
            muted_text: [55, 65, 81],
            control_surface: mix(background, [0, 0, 0], 0.05),
            border: [100, 112, 128],
            is_light: true,
            hover: accent,
            hover_opacity: 0.12,
            danger_hover: [196, 54, 47],
        }
    } else {
        ThemeColors {
            window_surface: background,
            text: [242, 244, 248],
            muted_text: [185, 190, 200],
            control_surface: if image {
                background
            } else {
                mix(background, [255, 255, 255], 0.06)
            },
            border: [255, 255, 255],
            is_light: false,
            hover: accent,
            hover_opacity: 0.2,
            danger_hover: [219, 82, 69],
        }
    };
    ThemeDefinition {
        id: ThemeId::Custom,
        label: "Custom",
        swatch: if image { accent } else { background },
        accent,
        colors,
        // The image itself is read from the preference directory.
        backdrop: image.then(|| ThemeBackdrop {
            image_bytes: &[],
            image_opacity: 1.0,
            image_scale: 1.0,
            shade_opacity: dim.shade(light),
        }),
    }
}

fn mix(color: [u8; 3], other: [u8; 3], amount: f32) -> [u8; 3] {
    let channel = |index: usize| {
        (f32::from(color[index]) * (1.0 - amount) + f32::from(other[index]) * amount).round() as u8
    };
    [channel(0), channel(1), channel(2)]
}

/// `#1a2b3c` or `1a2b3c`.
pub fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |index: usize| u8::from_str_radix(&hex[index..index + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

pub fn to_hex(color: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2])
}

fn load_saved() -> CustomTheme {
    let Some(text) = preference_directory()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(SETTINGS_FILE)).ok())
    else {
        return CustomTheme::default();
    };
    let mut theme = CustomTheme::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "base" => theme.light = value == "light",
            "background" => theme.background = parse_hex(value).unwrap_or(theme.background),
            "accent" => theme.accent = parse_hex(value).unwrap_or(theme.accent),
            "image" => theme.image = value == "yes",
            "dim" => theme.dim = Dim::from_key(value).unwrap_or(theme.dim),
            _ => {}
        }
    }
    // A saved image that went missing leaves a plain background.
    theme.image &= image_path().is_ok_and(|path| path.exists());
    theme
}

pub fn save(theme: CustomTheme) -> io::Result<()> {
    let directory = preference_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join(SETTINGS_FILE),
        format!(
            "base={}\nbackground={}\naccent={}\nimage={}\ndim={}\n",
            if theme.light { "light" } else { "dark" },
            to_hex(theme.background),
            to_hex(theme.accent),
            if theme.image { "yes" } else { "no" },
            theme.dim.as_key(),
        ),
    )
}

fn image_path() -> io::Result<PathBuf> {
    Ok(preference_directory()?.join(IMAGE_FILE))
}

/// The saved background image, if any.
pub fn image_bytes() -> Option<Vec<u8>> {
    fs::read(image_path().ok()?).ok()
}

/// Crops the picture to the window's shape at the size it is drawn and
/// encodes it as JPEG, so the original file can move or go.
pub fn prepare_image(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let decoded = ::image::load_from_memory(bytes)
        .map_err(|error| format!("could not decode image: {error}"))?;
    let filled = decoded
        .resize_to_fill(
            WINDOW_WIDTH as u32 * BACKDROP_PIXEL_SCALE,
            WINDOW_HEIGHT as u32 * BACKDROP_PIXEL_SCALE,
            ::image::imageops::FilterType::Lanczos3,
        )
        .to_rgb8();
    let mut jpeg = io::Cursor::new(Vec::new());
    ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&filled)
        .map_err(|error| format!("could not encode image: {error}"))?;
    Ok(jpeg.into_inner())
}

/// Asks for a picture and saves it as the background; `Ok(false)` when
/// cancelled. Blocks while the file dialog is open.
pub fn choose_image() -> Result<bool, String> {
    let Some(path) = crate::tab_icons::pick_image_file() else {
        return Ok(false);
    };
    let bytes = fs::read(&path).map_err(|error| format!("could not read image: {error}"))?;
    let prepared = prepare_image(&bytes)?;
    let target = image_path().map_err(|error| error.to_string())?;
    if let Some(directory) = target.parent() {
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    fs::write(target, prepared).map_err(|error| format!("could not save image: {error}"))?;
    Ok(true)
}

pub fn remove_image() -> io::Result<()> {
    match fs::remove_file(image_path()?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_round_trip_and_bad_ones_are_refused() {
        assert_eq!(parse_hex("#4A70a5"), Some([74, 112, 165]));
        assert_eq!(parse_hex("4a70a5"), Some([74, 112, 165]));
        assert_eq!(to_hex([74, 112, 165]), "#4a70a5");
        assert_eq!(parse_hex("#4a70a"), None);
        assert_eq!(parse_hex("#4a70zz"), None);
    }

    #[test]
    fn the_base_sets_readable_text_and_an_image_brings_a_veil() {
        let dark = build(CustomTheme::default());
        assert!(!dark.colors.is_light && dark.backdrop.is_none());
        assert_eq!(dark.colors.text, [242, 244, 248]);

        let light = build(CustomTheme {
            light: true,
            background: [255, 255, 255],
            image: true,
            dim: Dim::Strong,
            ..CustomTheme::default()
        });
        assert!(light.colors.is_light);
        assert_eq!(light.colors.text, [17, 24, 39]);
        assert_eq!(light.backdrop.unwrap().shade_opacity, 0.76);
    }

    #[test]
    fn pictures_are_cropped_to_the_window_shape() {
        let wide = ::image::RgbImage::from_pixel(1000, 200, ::image::Rgb([200, 100, 50]));
        let mut png = io::Cursor::new(Vec::new());
        wide.write_to(&mut png, ::image::ImageFormat::Png).unwrap();
        let prepared = prepare_image(png.get_ref()).unwrap();
        let stored = ::image::load_from_memory(&prepared).unwrap();
        assert_eq!(
            (stored.width(), stored.height()),
            (
                WINDOW_WIDTH as u32 * BACKDROP_PIXEL_SCALE,
                WINDOW_HEIGHT as u32 * BACKDROP_PIXEL_SCALE
            )
        );
    }
}
