# Isolated Iced UI prototype

This prototype is outside the backend workspace and provides a native Iced
window for the usage monitor. Provider tabs show one card per locally saved
account, with its latest persisted usage snapshot. The refresh control updates
provider usage. The top-bar Add account button uses the existing `codex-usage`
account-add flow and provider-specific login helpers; OpenRouter keys are
entered in masked fields and passed to the CLI through standard input.

Run it from the Rust workspace root:

```powershell
cargo run --release --manifest-path tools/usage-ui-iced-prototype/Cargo.toml --target-dir target
```

Before using Add account, build the CLI and provider login helpers into the
same target directory so they sit next to the UI executable:

```powershell
cargo build --workspace --release --offline -j 2
cargo build --release --manifest-path tools/usage-ui-iced-prototype/Cargo.toml --target-dir target --offline -j 2
```

It starts hidden in the notification area. Left-click the icon to show or
restore a fixed-size window near the actual tray rectangle. The window is not
kept above other windows and appears in the taskbar. Its in-window title bar
has a drag area, account-add and refresh controls, a theme selector, and a close button; Close hides it back to the
tray. The theme catalog currently contains `Grey Space` (with its bundled Dao
backdrop) and `Dark`. The selected theme is stored in
`%APPDATA%\UsageMonitorPreview\theme.txt`. Set
`USAGE_UI_PREVIEW_OPEN_ON_START=1` to open a preview at the icon's current
rectangle after startup.

Iced is MIT-licensed. This evaluation enables its tiny-skia software renderer,
JPEG image decoding, and advanced text shaping instead of its default wgpu renderer. The tray uses
`tray-icon`, licensed MIT OR Apache-2.0. The close icon uses the Lucide icon
font through the `lucide-icons` crate (MIT AND ISC).
