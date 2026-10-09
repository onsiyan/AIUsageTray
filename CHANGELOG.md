# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.4.0] - 2026-10-09

### Added

- Hide emails, in the palette menu: every email shows as dots, for
  screenshots.
- Dusk, a dark plum theme with a teal accent, now the theme a new install
  starts with. The themes are listed White, Dusk, Dark, then the picture
  themes.

### Fixed

- "In the middle of the screen" under Opens took two lines and ran into
  the next choice; it reads "Screen center".

## [0.3.0] - 2026-10-09

### Added

- Where the popup opens, in the palette menu: above the tray icon (as
  before), in the middle of the screen, or where you left it.

### Changed

- The app draws with Vulkan instead of Direct3D 12, whose drivers held
  about 100 MB more, and hands its memory back to Windows while the popup
  is hidden: about 1 to 15 MB in Task Manager instead of 70 to 210 MB, and
  the popup still opens at once.

### Fixed

- On a screen large enough to enlarge the popup, it grew again every time
  it opened after the first.

### Removed

- The Memory saver option, which closed the hidden popup to save memory
  at the cost of a slower open; the app now saves more without the wait.

## [0.2.0] - 2026-10-09

### Added

- API keys page, opened from the key button at the top left: keep any
  service's API key and copy it in one click. The service is picked from a
  grid of 60 AI companies' logos, named on hover (OpenAI, Claude, Gemini,
  xAI, DeepSeek, Mistral, NVIDIA, Azure OpenAI, Amazon Bedrock, and more),
  or named freely, and a pasted key's service is picked when its start
  shows it. The name is optional: a key saved without one takes its
  service's. The keys of API-key accounts already added are listed too.
  Keys are kept in Windows Credential Manager; the page is quick access
  only and reads no usage from them.
- A log of what went wrong, for bug reports, in
  `%LOCALAPPDATA%\UsageMonitor\logs` (Open log folder in the tray menu).
  It holds no keys, tokens, or email addresses.
- The app looks for a newer release on GitHub at start and once a day, and
  offers it in the tray menu and on a line in the popup. Nothing is
  downloaded on its own.
- Uninstalling asks whether to remove the saved accounts, keys, and
  settings too; `ai-usage-tray-cli reset --yes` does the same. Backups of
  the Codex and Antigravity sign-ins the app replaced are kept.

### Changed

- A usage request gives up after 15 seconds instead of 45, so one provider
  that stalls no longer holds the whole refresh.
- Antigravity starts with the server that answered last time instead of
  always trying the test server first.
- Syncing another machine sends only the half hours that changed since the
  last sync, over a compressed connection: about 100 bytes a sync instead
  of the machine's whole history.

## [0.1.0] - 2026-10-08

### Added

- A setup wizard (Inno Setup) that installs for the current Windows user
  without an administrator prompt: choose the folder, start with Windows,
  add a desktop shortcut, and open the app at the end. It closes a running
  copy before updating it, replaces the earlier setup's install in place,
  and is removed from Windows' installed apps list like any other app.
- The app icon on the taskbar button and its thumbnail when the popup is
  set to show in the taskbar.
- A right-click menu on the tray icon: open the popup or the Cost page,
  refresh all accounts, turn Start with Windows or Memory saver on and off,
  and quit. It follows the dark or light Windows app mode.
- MIT license.
- An app icon, shown in the notification area, on the executable, and in
  setup.
- Tray desktop app showing usage for Codex, Claude, Antigravity, OpenCode Go,
  and OpenRouter accounts, with reset times, reset credits, and plan.
- Direct Claude OAuth sign-in, Claude plan detection, and Claude usage-limit
  reset credits.
- "Use in Codex" and "Use in Antigravity" to switch those desktop apps to a
  saved account.
- Choice to show usage as remaining or used percentage; each bar's number
  says which (Left or Used), and each tab can read its own way from the tab
  manager.
- Account reordering, cancelling an in-progress sign-in, and hiding the
  Antigravity Claude/GPT group.
- An account whose saved sign-in the provider refused says "Sign-in
  expired" with the provider's reason (such as `refresh_token_reused`) and
  when its last reading was taken, with a "Sign in again" button.
- Image themes put a deeper shade over the whole window, so the figures stand
  off the picture.
- The Cost page keeps every day it has seen, even after Codex or Claude
  Code delete their old sessions, on this PC and on each SSH machine. Its
  period list adds 90 days and All time; long periods chart by week or
  month.
- The Cost page counts other machines too: add a Linux, macOS or Windows
  machine reached over SSH (with your keys; no password is asked or kept)
  and its Codex and Claude Code logs are read every minute while the page
  is open and every 15 minutes otherwise. A small script is sent each time
  (python3, or Windows PowerShell); nothing is installed there. A picker
  beside the period, or a click on a machine, shows one machine alone.
- Cost page, opened from the $ button at the top left: what Codex and
  Claude Code use on this PC would cost at API list
  prices, read from their local session logs, weighed against what the
  saved accounts' plans cost for the same days ("22× your plans' worth").
  Plan prices start at the list price and can be changed in the tab. Also:
  today, 7 or 30 days, each tool's share, the days,
  cache reads and fresh input, thinking, what the cache saved, and where the
  use went by model. Logs are read once and then only
  where they grew; prices come from models.dev, updated daily, with a
  built-in table for offline use. `ai-usage-tray-cli cost` prints the same.
- Favorites tab, third in the tab bar, gathering accounts starred from any
  provider. The star sits with the rename and move controls on each account;
  favorites keep their own order and are refreshed first while shown.
- "Save memory in tray" option in the palette menu: closes the popup window
  while it is hidden, freeing about 100 MB of GPU memory, at the cost of a
  slower next open. Off by default.
- DeepSeek accounts, added from an API key (desktop dialog, or
  `usage account add deepseek` with `DEEPSEEK_API_KEY` or stdin). The tab
  shows the prepaid balance from DeepSeek's official `/user/balance`
  endpoint, with paid and granted funds broken out when there are granted
  funds, and a notice when the balance is empty or cannot be used.
- Money amounts in usage rows read as `$7.25` or `¥8.50` instead of
  `7.25 USD`.
- Palette menu options to hide the email and plan line on account cards,
  and to list all stored reset credits, only those expiring within 5 days,
  or none.
- Tabs button in the title bar: show or hide each tab, reorder them, and
  create named tabs that gather several providers (for example a "Prepaid"
  tab for OpenRouter and DeepSeek). The layout is saved in `tabs.txt`; a
  hidden provider's tab comes back when an account is added for it.
- Optional PNG or JPEG image for a custom tab, shown in the tab bar in place
  of its name.
- Custom tabs can pick single accounts as well as whole providers; leaving
  one account out of a whole provider keeps its other accounts picked.
- GitHub Copilot accounts, signed in with GitHub's device code (the app shows
  and copies the code, then waits for it to be entered on GitHub; or
  `usage account add copilot`). The tab shows premium requests and chat
  quotas with their monthly reset, credits used, unlimited and billed-by-usage
  plans, and a note when a quota is exceeded. Accounts are matched by GitHub
  user, so signing in again refreshes the existing one.
- Cursor accounts, taken from the Cursor app signed in on this computer, or
  from a `WorkosCursorSessionToken` cookie pasted from cursor.com (or
  `usage account add cursor`). The tab shows the included plan's total,
  Auto + Composer and API usage for the billing cycle, the weekly Grok Bot
  allowance, request quotas on legacy plans, and on-demand spend against its
  budget. While the Cursor app stays signed in to the account, each refresh
  uses its newer session.
- Kimi Code accounts, added from a Kimi Code API key (desktop dialog, or
  `usage account add kimi` with `KIMI_CODE_API_KEY` or stdin). The tab shows
  the 5-hour and weekly quotas and the monthly total usage pool, with the
  membership plan. Older count-based responses are read too.
  Keys from kimi.com (China) and kimi.ai (International) both work: the
  region that accepts the key is found when the account is added.
- z.ai (GLM Coding Plan) accounts, added from an API key (desktop dialog, or
  `usage account add zai` with `Z_AI_API_KEY` or stdin). The key's region
  (api.z.ai or open.bigmodel.cn) is found when the account is added. The tab
  shows the 5-hour and weekly token quotas and the monthly MCP quota, and
  credit plans note whether peak hours apply.
- xAI API accounts, added from a Management API key and team ID (desktop
  dialog, or `usage account add xai` with `XAI_MANAGEMENT_API_KEY` and
  `XAI_TEAM_ID`, or both on stdin). The tab shows the prepaid credit balance
  with today's and the last 30 days' spend; the balance still shows when the
  spend history is unavailable.
- MiniMax Coding Plan accounts, added from a Coding Plan API key (desktop
  dialog, or `usage account add minimax` with `MINIMAX_CODING_API_KEY` or
  stdin). The key's region (api.minimax.io or api.minimaxi.com) is found when
  the account is added. The tab shows the 5-hour and weekly text quotas, the
  other model quotas the plan includes, and the points balance.
- Xiaomi MiMo provider support (balance and monthly token plan, read with a
  pasted platform.xiaomimimo.com console cookie). Hidden for now: the desktop
  app does not offer it and the CLI does not list it.
- Custom theme in the palette menu ("Customize…"): a dark or light base, a
  background and an accent color (presets or a hex code), and an optional
  background picture of your own, cropped to the window and dimmed lightly,
  medium, or strongly so text stays readable. Changes apply as you make them.
- An empty provider tab has an "Add account" button; an empty custom tab
  opens the list of providers.
- The palette menu is laid out in two columns across the window.
- Palette menu options to show the window in the taskbar and to turn off the
  shade behind reset times on image themes.
- First-run welcome: pick the services you use (each says how its account is
  added; Cursor only from the Cursor app signed in on this computer), then add
  accounts to each in turn or skip it. Only the chosen services' tabs show
  afterwards. Shown once, and never to people who already have accounts.
- "Team budgets" option under Account cards in the palette menu, off by
  default. When on, Codex team and business accounts show the workspace's
  remaining credit balance and the member's monthly credit limit, and Cursor
  team accounts show the member's spend against their per-user budget. These
  are read only when the plan has them, and a failure never hides the usage.
- `ai-usage-tray-cli` for people and agents, with stable account references
  and JSON output.

### Changed

- The app is now called AI Usage Tray (`ai-usage-tray.exe`,
  `ai-usage-tray-cli`). Setup replaces a copy installed as Usage Monitor,
  with its shortcuts and Start with Windows entry; accounts and settings
  stay where they were.
- Codex, Claude, and Antigravity account cards are named by their email
  (unless renamed), and the email is not repeated under the name.
- The Favorites tab sits in the middle of the tab bar.
- Accounts added from an API key (DeepSeek, OpenRouter) no longer show their
  placeholder email address.
- The White theme uses near-black text and darker secondary text, and the
  empty part of usage bars stays visible on white.
- Menus, dialogs, tooltips, and warnings use heavier, larger type; the
  palette menu follows the active theme instead of always being black, and
  the API-key dialog dims the popup behind it.
- The popup scales with the screen it opens on: on large screens it grows to
  about 72% of the usable height (at most double size), while laptop screens
  keep the designed size.
- Resets within a day show their local clock time as well as the countdown
  ("Resets at 7:25 PM · in 4h 41m", or "tomorrow at ...").
- Emails, reset times, and reset-credit expiry times use IBM Plex Sans Medium,
  with the same bold countdown, and semibold text uses the
  real SemiBold face instead of falling back to Bold.
- The desktop app draws with the GPU, falling back to CPU drawing when no
  suitable graphics adapter is available. Scrolling is much smoother.
- Continuous wheel input (precision touchpads, free-spinning wheels) scrolls
  directly instead of restarting the smooth-scroll animation on every event,
  and mouse moves no longer rebuild the whole window.
- Smooth mouse-wheel scrolling in account lists, account management, and model
  menus, with immediate direction reversal and direct precision-touchpad input.
- Repository restructured into `apps/` and `crates/`; the five provider login
  helpers are merged into `ai-usage-tray-login`.
- Data, preferences, and credentials moved to the `UsageMonitor` names; data
  from the previous `CodexUsageMonitor-Rust` and `UsageMonitorPreview` names is
  migrated automatically.
- Faster refreshes: one long-lived refresh context with cached OAuth access
  tokens, concurrent Antigravity requests, and a cached Cloud Code project
  lookup.
- Desktop refreshes use one bounded queue across providers, prioritize the
  visible provider, and share remaining slots between the others. Usage
  transitions finish in 150ms instead of 420ms.
- CLI batch refreshes run concurrently with a shared limit of four accounts;
  Claude usage and profile requests overlap while retaining identity checks.

### Removed

- Sources and options no flow could reach: the Claude web-session, Admin
  API, and Claude Code CLI sources; the Antigravity local language-server
  source; the OpenCode Go automatic mode and local history estimate; the
  OpenAI workspace spend and balance requests; browser cookie import and
  the browser bridge extension; environment and local-file credential
  sources; and the sign-in helper's manual-only options.
- Adaptive refresh signals no host sent; automatic refresh is now every
  30 minutes plus right after each known reset.

### Fixed

- Cancelling an account add now stops the sign-in helper too; before, it
  could keep waiting in the background and hold the callback port.
- Adding a Codex account no longer fails with "could not bind OAuth callback
  (os error 10013)" on computers where Windows reserves OpenAI's sign-in port
  (Hyper-V, WSL, Docker): the app then signs in with a code entered on
  OpenAI's page, as `codex login --device-auth` does.
- Opening the popup repeatedly no longer gets accounts rate limited (HTTP 429,
  seen with Claude): automatic refreshes skip accounts read in the last 45
  seconds, and an account answered with 429 is left alone for at least 3
  minutes, or as long as the provider asks.
- A reading that could not be updated says so and shows when it was taken,
  instead of only "This usage reading is stale".
- Desktop tests no longer overwrite the saved account order.
- Scrolling stuttered while a refresh ran: the spinning refresh icon rebuilt
  the whole window 20 times a second. The icon now spins by redrawing only
  itself.
- Two Codex accounts in the same ChatGPT Team could show the same usage after
  "Use in Codex": the link to Codex's `auth.json` matched only the shared
  workspace id, so a teammate's sign-in was adopted as the linked account's.
  The link now also records the user, and a token issued to another user is
  reported as an account mismatch instead of being shown.
- An early Codex weekly reset is shown in the same refresh, after one
  re-read about 20 seconds later; it no longer needs unused reset credits and
  no longer leaves the old usage on screen until the original reset time.
- A window whose reset time has passed shows as unused until the provider's
  next reading, and an open popup refreshes when a shown reset passes.
- One tray click always brings the popup forward.
- Credentials larger than Credential Manager's 2,560-byte limit are split
  across entries instead of failing to save.
