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
- `usage-monitor-cli` for people and agents, with stable account references
  and JSON output.

### Changed

- Repository restructured into `apps/` and `crates/`; the five provider login
  helpers are merged into `usage-monitor-login`.
- Data, preferences, and credentials moved to the `UsageMonitor` names; data
  from the previous `CodexUsageMonitor-Rust` and `UsageMonitorPreview` names is
  migrated automatically.
- Faster refreshes: one long-lived refresh context with cached OAuth access
  tokens, concurrent Antigravity requests, and a cached Cloud Code project
  lookup.

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

- One tray click always brings the popup forward.
- Credentials larger than Credential Manager's 2,560-byte limit are split
  across entries instead of failing to save.
