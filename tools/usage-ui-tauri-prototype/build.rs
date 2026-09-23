use std::{env, fs, path::PathBuf};

fn main() {
    // Tauri's Windows resource compiler and context generator both require an
    // ICO. Keep this placeholder in the ignored package-local target folder;
    // the actual notification-area icon is drawn by the Rust app.
    let package_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the manifest path"));
    let icon_dir = package_dir.join("target");
    fs::create_dir_all(&icon_dir).expect("create generated icon directory");
    let icon_path = icon_dir.join("preview.ico");
    fs::write(&icon_path, placeholder_ico()).expect("write preview icon");

    tauri_build::build()
}

fn placeholder_ico() -> [u8; 70] {
    let mut bytes = [0_u8; 70];

    // ICO directory: one 1x1, 32-bit icon image at byte offset 22.
    bytes[2] = 1;
    bytes[4] = 1;
    bytes[6] = 1;
    bytes[10] = 1;
    bytes[12] = 32;
    bytes[14] = 48;
    bytes[18] = 22;

    // BITMAPINFOHEADER. Icon bitmaps store XOR and AND planes, hence height 2.
    bytes[22] = 40;
    bytes[26] = 1;
    bytes[30] = 2;
    bytes[34] = 1;
    bytes[36] = 32;
    bytes[42] = 4;

    // Opaque violet pixel (BGRA); the remaining bytes are the empty AND mask.
    bytes[62..66].copy_from_slice(&[245, 111, 120, 255]);
    bytes
}
