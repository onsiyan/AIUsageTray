# Isolated Iced UI prototype

This prototype is outside the backend workspace and contains only illustrative
provider data. It does not read account storage, call providers, or modify the
backend.

Run it from the Rust workspace root:

```powershell
cargo run --release --manifest-path tools/usage-ui-iced-prototype/Cargo.toml --target-dir target
```

It starts hidden in the notification area. Left-click the icon to show or
restore a standard, decorated, resizable window near the actual tray rectangle.
The window is not kept above other windows and appears in the taskbar. Its
Close button hides it back to the tray; its Minimize button behaves normally.
Set `USAGE_UI_PREVIEW_OPEN_ON_START=1` to open a preview at the icon's current
rectangle after startup.

Iced is MIT-licensed. This evaluation enables its tiny-skia software renderer
and advanced text shaping instead of its default wgpu renderer. The tray uses
`tray-icon`, licensed MIT OR Apache-2.0.
