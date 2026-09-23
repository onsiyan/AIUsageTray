# Isolated Tauri UI prototype

Run this isolated prototype from the Rust workspace root:

```powershell
cargo run --release --manifest-path tools/usage-ui-tauri-prototype/Cargo.toml --target-dir target
```

The hidden window is opened from its notification-area icon. The Tauri
positioner receives the tray event and places the window relative to the icon.
The tray menu can open the preview or quit the process; clicking elsewhere
hides the popup. The HTML/CSS screen is static demonstration data and does not
read accounts, call providers, or access the Rust backend.

For a UI-only preview on launch, set `USAGE_UI_PREVIEW_OPEN_ON_START=1`; normal
startup remains hidden and tray-first.

This prototype uses Tauri 2 and its positioner plugin, both under MIT or
Apache-2.0 licensing. On Windows, Tauri renders the frontend with the installed
Microsoft Edge WebView2 runtime. It is deliberately kept outside the backend
workspace while the UI option is being evaluated.
