# Usage Monitor desktop app

The tray app users run. It starts hidden in the notification area; one click
on the icon shows the popup near the tray (or brings it forward), and Close
hides it again. Provider tabs show a card per saved account with its usage,
reset times, reset credits, and plan.

From the popup you can:

- add accounts (the app runs `usage-monitor-cli account add`, which runs the
  provider sign-in in the default browser; OpenRouter keys are typed into
  masked fields and passed through standard input) and cancel a sign-in;
- refresh all providers, starting with the selected tab;
- rename and reorder accounts, and delete them;
- switch the Codex or Antigravity desktop app to an account;
- choose the theme, whether percentages show what is left or what is used,
  which model quotas are visible, and whether Antigravity shows its Claude and
  GPT group.

Preferences are stored in `%APPDATA%\UsageMonitor\`. Set
`USAGE_UI_PREVIEW_OPEN_ON_START=1` to open the popup at startup and write a
diagnostic log to the temporary directory.

## Run

From the repository root, build the whole workspace so the CLI and sign-in
helper sit next to the app:

```powershell
cargo build --workspace --release
.\target\release\usage-monitor.exe
```

`packaging/windows/build-installer.ps1` tests and builds the workspace and
produces a per-user installer.

## Third-party components

Iced (MIT) with its tiny-skia software renderer, `tray-icon` (MIT OR
Apache-2.0), Lucide icons via `lucide-icons` (MIT AND ISC), and IBM Plex Sans
(see `assets/fonts/ibm-plex-sans/license.txt`). Provider logos are listed in
`assets/providers/ATTRIBUTION.md`.
