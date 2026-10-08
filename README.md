# Usage Monitor

Usage Monitor tracks AI subscription usage across several accounts and
providers from one Windows tray popup: Codex (ChatGPT), Claude, Antigravity,
OpenCode Go, and OpenRouter. It shows each account's session, weekly, and
model limits with their reset times, reset credits, and plan, and can switch
the Codex and Antigravity desktop apps to any saved account.

## Repository layout

```
apps/
  desktop/              Tray app (iced) — the product users run
  cli/                  usage-monitor-cli: account and usage commands for people and agents
  login/                usage-monitor-login: per-provider sign-in flows run by the CLI
crates/
  core/                 Provider-neutral accounts, OAuth, storage, refresh, and provider adapters
  platform-windows/     Credential Manager and desktop-app integration
```

`usage-monitor-core` holds everything that is not Windows-specific:
account/usage contracts, the HTTP transport, OAuth with PKCE and loopback
callbacks, SQLite storage, the provider adapters, and the refresh coordinator
(coalesced per-account flights, bounded concurrency, a 30-minute cadence,
reset-boundary refreshes, and last-good snapshot retention).

## Getting started

Requires Windows and the Rust toolchain pinned in `rust-toolchain.toml`.

```powershell
cargo build --workspace --release
.\target\release\usage-monitor.exe
```

The desktop app, CLI, and sign-in helper must sit in the same directory; the
app runs the CLI to add accounts, and the CLI runs the sign-in helper. To build
the setup, install [Inno Setup 6](https://jrsoftware.org/isinfo.php)
(`winget install JRSoftware.InnoSetup`) and run
`apps/desktop/packaging/windows/build-installer.ps1`; it writes
`target/dist/UsageMonitor-<version>-Setup.exe`.

## Command line

```powershell
.\target\release\usage-monitor-cli.exe account add codex --alias "Personal"
.\target\release\usage-monitor-cli.exe account add claude
.\target\release\usage-monitor-cli.exe account add antigravity
.\target\release\usage-monitor-cli.exe account add opencode-go
.\target\release\usage-monitor-cli.exe account add openrouter --api-key-stdin
.\target\release\usage-monitor-cli.exe account list
.\target\release\usage-monitor-cli.exe usage get ch1 --json
.\target\release\usage-monitor-cli.exe usage refresh --all --provider codex --json
.\target\release\usage-monitor-cli.exe usage watch
.\target\release\usage-monitor-cli.exe account remove ch1 --yes
```

Accounts have stable references: `chN` (Codex), `ccN` (Claude), `agN`
(Antigravity), `ocN` (OpenCode Go), and `orN` (OpenRouter). They survive
restarts and renames and are never reused. An exact alias, label, or email also
selects an account; `--provider` or `--workspace` disambiguates.

`--json` writes one object with `schema_version: 1` to stdout and sends progress
to stderr. Exit codes: 2 invalid arguments, 3 no matching account, 4 ambiguous
selector, 5 removal not confirmed, 6 failed or partial refresh, 1 other errors.
`usage get` refreshes before returning; if the provider fails, the last good
snapshot is returned marked stale. `usage watch` refreshes every account every
30 minutes, and again right after each known window reset, until stopped.
`account remove` deletes local data and saved credentials but does not revoke
provider access.

## Accounts and sign-in

| Provider | Sign-in | Usage source |
|---|---|---|
| Codex | OpenAI OAuth + PKCE, localhost callback (port 1455, fallback 1457) | WHAM usage API with the account's workspace id |
| Claude | Claude OAuth + PKCE, localhost callback | OAuth usage API, profile for identity and plan |
| Antigravity | Google OAuth + PKCE | Cloud Code APIs (models, grouped quota summary, project and tier) |
| OpenCode Go | OpenCode Console device authorization | Console (the Zen Go API for an OpenCode API key) |
| OpenRouter | API key (optional management key) via environment or stdin | `/key`, `/credits`, `/activity` |

Every credential is scoped to one local account and is only read from that
account's Credential Manager entry (or Codex's `auth.json` while that account
is linked to Codex, below); environment variables, CLI sessions, and browser
cookies are never used as usage credentials.

### Switching the desktop apps

The desktop app can sign the Codex or Antigravity desktop app in with a saved
account and restart it:

- **Codex** reads its sign-in from `%USERPROFILE%\.codex\auth.json`. OpenAI
  refresh tokens are single use, so the account is *linked* rather than
  copied: while linked, `auth.json` is the source of truth, the monitor picks
  up rotations Codex made and writes its own back, and it uses Codex's valid
  access token instead of refreshing. Signing in to another account inside
  Codex ends the link. Switching is refused when Codex keeps its sign-in in the
  OS keyring.
- **Antigravity** keeps its sign-in in the Credential Manager entry
  `gemini:antigravity`. Saved accounts use the same Google OAuth client and
  Google refresh tokens do not rotate, so the token is simply written there.

An existing sign-in that does not belong to a saved account is backed up before
it is replaced.

## Data and credentials

| What | Where |
|---|---|
| Accounts and usage history (no secrets) | `%LOCALAPPDATA%\UsageMonitor\accounts.db` |
| OAuth refresh credentials | Credential Manager `UsageMonitor/OAuth/<account-id>` |
| API keys and console sessions | Credential Manager `UsageMonitor/Auth/<account-id>` |
| Codex link and `auth.json` backups | `%LOCALAPPDATA%\UsageMonitor\` |
| Desktop preferences | `%APPDATA%\UsageMonitor\` |

Credentials larger than Credential Manager's 2,560-byte limit are split across
`#partN` entries. Access tokens live only in memory. Data written under the
earlier `CodexUsageMonitor-Rust` and `UsageMonitorPreview` names is moved to the
locations above the first time it is read. Pass `--database PATH` to any CLI
command to use another database.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Continuous integration runs the same checks on Windows for every push and pull
request (`.github/workflows/ci.yml`). See `SECURITY.md` for how credentials are
handled and how to report a vulnerability, and `CHANGELOG.md` for changes.

To measure delivery of desktop refresh results on Windows, explicitly run:

```powershell
cargo test --workspace --release live_refresh_latency -- --ignored --nocapture
```

This diagnostic contacts the providers for the locally saved accounts and
updates their stored snapshots. It reports cold and warm delivery times using
account references, without printing credentials, and is excluded from normal tests.
