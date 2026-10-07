//! Optional images the user picks for custom tabs, kept as small PNGs in
//! the preference directory (`tab-icons/custom-<id>.png`).

use std::{collections::HashMap, fs, io, path::PathBuf, sync::Arc};

use iced::widget::image;

use crate::{tabs, theme::preference_directory};

const ICON_DIRECTORY: &str = "tab-icons";
/// Twice the largest size the icon is drawn at, so it stays sharp.
const ICON_PIXELS: u32 = 64;

/// A prepared tab image: the handle to draw and the PNG to store.
#[derive(Debug, Clone)]
pub struct TabIcon {
    pub handle: image::Handle,
    png: Arc<Vec<u8>>,
}

/// Decodes a PNG or JPEG and shrinks it to fit the tab icon size.
pub fn prepare(bytes: &[u8]) -> Result<TabIcon, String> {
    let decoded = ::image::load_from_memory(bytes)
        .map_err(|error| format!("could not decode image: {error}"))?
        .thumbnail(ICON_PIXELS, ICON_PIXELS)
        .to_rgba8();
    let mut png = io::Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, ::image::ImageFormat::Png)
        .map_err(|error| format!("could not encode image: {error}"))?;
    Ok(TabIcon {
        handle: image::Handle::from_rgba(decoded.width(), decoded.height(), decoded.into_raw()),
        png: Arc::new(png.into_inner()),
    })
}

/// Asks for an image file and prepares it; `Ok(None)` when cancelled.
/// Blocks while the file dialog is open.
pub fn choose() -> Result<Option<TabIcon>, String> {
    let Some(path) = pick_image_file() else {
        return Ok(None);
    };
    let bytes = fs::read(&path).map_err(|error| format!("could not read image: {error}"))?;
    prepare(&bytes).map(Some)
}

/// The saved images of the layout's custom tabs.
pub fn load_saved(layout: &tabs::TabLayout) -> HashMap<u32, image::Handle> {
    layout
        .entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            tabs::TabKind::Custom(custom) => Some(custom.id),
            _ => None,
        })
        .filter_map(|id| {
            let bytes = fs::read(icon_path(id).ok()?).ok()?;
            Some((id, prepare(&bytes).ok()?.handle))
        })
        .collect()
}

pub fn save(id: u32, icon: &TabIcon) -> io::Result<()> {
    let path = icon_path(id)?;
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, icon.png.as_slice())
}

pub fn remove(id: u32) -> io::Result<()> {
    match fs::remove_file(icon_path(id)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn icon_path(id: u32) -> io::Result<PathBuf> {
    Ok(preference_directory()?
        .join(ICON_DIRECTORY)
        .join(format!("custom-{id}.png")))
}

#[cfg(windows)]
pub(crate) fn pick_image_file() -> Option<PathBuf> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };

    let filter = "Images (*.png, *.jpg, *.jpeg)\0*.png;*.jpg;*.jpeg\0"
        .encode_utf16()
        .chain([0])
        .collect::<Vec<u16>>();
    let mut file = vec![0u16; 1024];
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: file.as_mut_ptr(),
        nMaxFile: file.len() as u32,
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: the filter and file buffers outlive the call, and `nMaxFile`
    // matches the file buffer's length.
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        return None;
    }
    let length = file
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(file.len());
    Some(PathBuf::from(OsString::from_wide(&file[..length])))
}

#[cfg(not(windows))]
pub(crate) fn pick_image_file() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_images_shrink_to_the_icon_size_keeping_their_shape() {
        let wide = ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255]));
        let mut bytes = io::Cursor::new(Vec::new());
        wide.write_to(&mut bytes, ::image::ImageFormat::Png)
            .unwrap();

        let icon = prepare(bytes.get_ref()).unwrap();
        let stored = ::image::load_from_memory(&icon.png).unwrap();
        assert_eq!((stored.width(), stored.height()), (64, 32));
    }

    #[test]
    fn files_that_are_not_images_are_refused() {
        assert!(prepare(b"not an image").is_err());
    }
}
