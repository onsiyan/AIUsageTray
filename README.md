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
  platform-windows/     Credential Manager, browser sessions, and desktop-app integration
extensions/
  browser-bridge/       Browser extension used by the OpenCode Go console sign-in
docs/                   Research notes
```

`usage-monitor-core` holds everything that is not Windows-specific:
account/usage contracts, the HTTP transport, OAuth with PKCE and loopback
callbacks, SQLite storage, the provider adapters, and the refresh coordinator
(coalesced per-account flights, bounded concurrency, adaptive cadence,
reset-boundary refreshes, and last-good snapshot retention).

## Getting started

Requires Windows and the Rust toolchain pinned in `rust-toolchain.toml`.

```powershell
cargo build --workspace --release
.\target\release\usage-monitor.exe
```

The desktop app, CLI, and sign-in helper must sit in the same directory; the
app runs the CLI to add accounts, and the CLI runs the sign-in helper. To build
an installer, run `apps/desktop/packaging/windows/build-installer.ps1`.

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
snapshot is returned marked stale. `usage watch` keeps refreshing on the adaptive
cadence until stopped. `account remove` deletes local data and saved
credentials but does not revoke provider access.

## Accounts and sign-in

| Provider | Sign-in | Usage source |
|---|---|---|
| Codex | OpenAI OAuth + PKCE, localhost callback (port 1455, fallback 1457) | WHAM usage API with the account's workspace id |
| Claude | Claude OAuth + PKCE, localhost callback | OAuth usage API, profile for identity and plan |
| Antigravity | Google OAuth + PKCE | Local language server when it is signed in to the same account, otherwise Cloud Code APIs |
| OpenCode Go | Console sign-in through `extensions/browser-bridge`, or API key | Console, local history estimate, or Zen Go API |
| OpenRouter | API key (optional management key) via environment or stdin | `/key`, `/credits`, `/activity` |

Every credential is scoped to one local account. Environment keys, local
provider files, and CLI sessions are never applied to a different account than
the one they identify; a generic `OPENAI_API_KEY` or Claude API key is not used
as a usage credential.

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
| API keys and imported sessions | Credential Manager `UsageMonitor/Auth/<account-id>` |
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
