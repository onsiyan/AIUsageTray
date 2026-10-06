# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Tray desktop app showing usage for Codex, Claude, Antigravity, OpenCode Go,
  and OpenRouter accounts, with reset times, reset credits, and plan.
- Direct Claude OAuth sign-in, Claude plan detection, and Claude usage-limit
  reset credits.
- "Use in Codex" and "Use in Antigravity" to switch those desktop apps to a
  saved account.
- Choice to show usage as remaining or used percentage.
- Account reordering, cancelling an in-progress sign-in, and hiding the
  Antigravity Claude/GPT group.
- Favorites tab, third in the tab bar, gathering accounts starred from any
  provider. The star sits with the rename and move controls on each account;
  favorites keep their own order and are refreshed first while shown.
- "Save memory in tray" option in the palette menu: closes the popup window
  while it is hidden, freeing about 100 MB of GPU memory, at the cost of a
  slower next open. Off by default.
- `usage-monitor-cli` for people and agents, with stable account references
  and JSON output.

### Changed

- Resets within a day show their local clock time as well as the countdown
  ("Resets at 7:25 PM · in 4h 41m", or "tomorrow at ...").
- Emails and reset times use IBM Plex Sans Medium, and semibold text uses the
  real SemiBold face instead of falling back to Bold.
- The desktop app draws with the GPU, falling back to CPU drawing when no
  suitable graphics adapter is available. Scrolling is much smoother.
- Continuous wheel input (precision touchpads, free-spinning wheels) scrolls
  directly instead of restarting the smooth-scroll animation on every event,
  and mouse moves no longer rebuild the whole window.
- Smooth mouse-wheel scrolling in account lists, account management, and model
  menus, with immediate direction reversal and direct precision-touchpad input.
- Repository restructured into `apps/` and `crates/`; the five provider login
  helpers are merged into `usage-monitor-login`.
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
