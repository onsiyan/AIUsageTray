# CodexUsageMonitor Rust

This is the active backend migration workspace. The original .NET tree remains
in the parent directory as a read-only migration reference; Rust work belongs
under this `rust/` workspace.

## Current crates

- `codex-usage-core`: account and usage contracts, HTTP transport, OAuth/PKCE,
  loopback callback validation, SQLite metadata/snapshot storage, and provider
  adapters. Its `refresh` module is the provider-independent coordinator:
  coalesced account flights, bounded batch concurrency, adaptive/fixed cadence,
  reset-boundary refreshes, and last-known snapshot retention on transient
  failures. `providers::registry` constructs and aliases the first-party
  adapters, while `runtime::UsageRuntime` composes that registry with SQLite,
  shared transport, injected auth, and the coordinator.
- `codex-usage-windows-auth`: Windows Credential Manager and default-browser
  integration. OAuth refresh credentials use the isolated target namespace
  `CodexUsageMonitor-Rust/OAuth/<account-id>`; imported browser cookies and
  provider keys use the separate
  `CodexUsageMonitor-Rust/Auth/<account-id>` namespace.
- `codex-usage-oauth-probe`: real Antigravity OAuth and usage probe.
- `codex-usage-claude-probe`: Claude Web account registration and usage probe.
  It first reuses an existing `sessionKey`; only when the profile is readable
  and no session exists does it open `claude.ai/login` in the user's default
  browser. A locked live profile is reported as busy instead of being treated
  as a missing session and opening an unnecessary login page.
- `codex-usage-openrouter-probe`: account-scoped OpenRouter API-key probe. It
  reads the primary key from a process environment variable or stdin, stores
  it in Windows Credential Manager, and never writes it to SQLite or prints it.

The durable SQLite database contains account metadata and snapshots only; it
never contains provider secrets. OAuth access tokens are memory-only during a
normal refresh, while refresh credentials and deliberately imported browser
tokens/cookies are kept in Windows Credential Manager. `auth::CompositeAuthMaterialProvider` is account-scoped and merges
sources in priority order without allowing one account's environment or local
file credential to leak into another account. The supported compatibility
sources are:

- secure per-account material written by an explicit provider login or refresh,
- Antigravity OAuth refresh credentials,
- provider-owned Antigravity `.gemini/oauth_creds.json` and OpenCode Go
  `auth.json` files,
- provider-owned Codex `$CODEX_HOME/auth.json` OAuth material (read-only),
- provider-owned Claude `~/.claude/.credentials.json` OAuth/session material,
- explicitly account-bound environment variables (`OPENROUTER_API_KEY`,
  `OPENROUTER_MANAGEMENT_API_KEY`, `OPENCODE_API_KEY`, or
  `CLAUDE_SESSION_KEY` / `CLAUDE_OAUTH_TOKEN` / `ANTHROPIC_ADMIN_KEY`).

Claude OAuth tokens use Anthropic's `/api/oauth/usage` contract. Claude Web
login is a user-driven browser bridge: the host opens the normal browser, reads
only a private copy of the Chromium cookie database, and waits for the
provider's `sessionKey`. It never receives a password, embeds a WebView, or
keeps a browser process alive during polling. The session is written to the
account-scoped Windows Credential Manager target and is then used by the Web
adapter for usage, reset times, optional extra usage, and prepaid credits.

The CLI cooldown is persisted as a timestamp only (no credentials or usage
payloads) in `%LOCALAPPDATA%\CodexUsageMonitor\claude-cli-state.json` by
default. Set `CODEX_USAGE_CLAUDE_STATE_FILE` when an isolated state location
is required for tests or a separate installation.

Claude OAuth files may carry an expiring access token and refresh token. The
optional `claude_oauth::ClaudeOAuthRefreshingAuthMaterialProvider` performs a
per-account refresh through `https://platform.claude.com/v1/oauth/token` and
stores the rotated material securely; invalid grants become an explicit
reauthentication state.

`OPENAI_API_KEY` is intentionally not accepted for WHAM usage, and Claude API
keys are not treated as web session cookies. This prevents a generic API key or
another account's ambient session from being mislabeled as the selected
account.

Codex usage is fetched from the account-scoped WHAM contract. The normalized
snapshot includes primary/weekly/model-specific windows, reset-credit inventory,
workspace spend/balance enrichments, and absolute reset timestamps. PAT material
is resolved through the provider `whoami` contract first, then the returned
account id is sent with the usage request; a generic `OPENAI_API_KEY` is never
used. When the OAuth source is unavailable or unauthorized, the registry can
fall back to the short-lived read-only `codex app-server` JSON-RPC path
(`initialize`, `account/rateLimits/read`, `account/read`) without opening the
interactive TUI.

Managed Codex accounts persist an optional per-account `CODEX_HOME` through the
SQLite migration. The read-only `auth.json` loader, account-scoped base URL, and
native CLI/app-server probe use that home, so native sessions cannot cross
between accounts. A shared launch gate suppresses repeated CLI launches for 30
minutes after a launch failure. The planner is wired into the live Wham adapter
and registry: App automatic mode is `PAT -> OAuth -> CLI`, explicit OAuth keeps
native CLI recovery, and managed workspaces suppress unscoped CLI fallback.
When one auth file contains both credentials, PAT is tried first and OAuth is
retained for a controlled unauthorized retry. Individual/monthly limits are
represented separately from a successfully-read balance, including remaining
values and reset timestamps; malformed lanes lower `data_confidence` to
`unknown` while valid sibling lanes remain visible. The Web dashboard remains a
separate source until its cookie-owned endpoint strategy is verified; no
endpoint is guessed and no WebView is kept alive.

The refresh coordinator follows the source-observed scheduling rules used by
the reference implementation: timestamps are UTC, countdowns are derived from
absolute reset timestamps, and a refresh never runs once per countdown second.
The default cadence is adaptive; callers can select `Manual` or a fixed
`Duration` for tests or host policy.

## Verification

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
```

To run the real Windows OAuth probe:

```powershell
cargo run -p codex-usage-oauth-probe
```

To add a Claude account through the official browser login flow and probe its
OAuth usage (the `claude` CLI must already be installed):

```powershell
cargo run -p codex-usage-claude-probe
```

The command opens the default browser itself. Complete the login there; the
probe then calls `/api/oauth/profile`, registers the verified account, and runs
the normal account-scoped runtime. To probe an existing Claude CLI login
without opening the browser again, pass `--probe-existing`.

The probe uses `%LOCALAPPDATA%\CodexUsageMonitor-Rust\accounts.db` and does
not reuse or overwrite the original application's credential namespace.

The probe's usage pass now exercises the same runtime composition path used by
the future tray host; it is not a separate provider-specific shortcut.

## OpenCode Go source model

OpenCode Go keeps the provider sources separate and account-scoped:

- `Automatic` uses the local read-only `opencode.db` history as a device-local
  estimate. For an unscoped API account it overlays that history with the Zen
  Go API (`/zen/go/v1/usage`); for a browser/workspace account it tries the
  signed-in console first, then local history, then the API.
- Browser sessions use the current console endpoints (`/console/api/orgs`,
  `/console/api/go/status`, and optional `/console/api/billing/status`) and
  retain the dashboard and `_server` paths as compatibility fallbacks. The
  parser keeps five-hour, weekly, and monthly meters, absolute reset times,
  plan/email/workspace identity, and Zen balance when supplied.
- `OPENCODE_GO_WORKSPACE_ID` can pin the browser account to one workspace when
  discovery returns more than one organization.
- `Api` and `Web` modes are strict and do not silently substitute another
  source. Optional failures are recorded in `source_diagnostics` while a
  valid quota snapshot remains usable.

On Windows, the OpenCode Go account-add flow uses the user's normal default
browser plus the unpacked extension in
`opencode-browser-bridge-extension`. This is required for Chromium app-bound
(`v20`) cookies: the desktop process never decrypts or weakens the browser's
profile protection. The extension sends only OpenCode-domain cookies after an
explicit **Connect** click to a one-shot loopback endpoint protected by a
pairing code; the validated material is then stored in Windows Credential
Manager and the endpoint closes.

The local estimate is explicitly marked `data_confidence = "estimated"`; it
is never presented as account truth when an API or web snapshot is available.

## OpenRouter API-key live test

OpenRouter has no browser session to import. Set the primary key only for the
current PowerShell process, and optionally set a management key for the
30-day Activity endpoint:

```powershell
$env:OPENROUTER_API_KEY = "sk-or-v1-..."
$env:OPENROUTER_MANAGEMENT_API_KEY = "sk-or-v1-..."
# Optional Activity scoping:
# $env:OPENROUTER_ACTIVITY_WORKSPACE_ID = "workspace-..."
# $env:OPENROUTER_ACTIVITY_GROUP_BY_WORKSPACE = "true"
cargo run -p codex-usage-openrouter-probe -- --label "OpenRouter primary"
```

The keys are saved in the per-account Windows Credential Manager target. The
primary key is used for `/key` and `/credits`; the management key is used only
for `/activity`. `--api-key-stdin` can be used instead of setting the primary
environment variable. For a one-off validation that must not save either key,
add `--ephemeral`; this keeps credentials in memory for the process only. Use
`--credentials-stdin` when both lines should be read from stdin (primary key
first, optional management key second). Use `--new` for another labeled key
account and `--no-activity` or `--no-credits` to disable optional enrichment
requests.

The normalized snapshot contains the server-reported key cap and remaining
amount, daily/weekly/monthly spend, free-model daily request limits, reset
timestamps, optional prepaid balance, and management-key Activity rows. The
primary percentage uses `limit_remaining` first, then the current reset-period
spend, and only then cumulative spend; a missing cap is not converted into a
fake percentage. Activity also includes a compact 30-day UTC summary with
requests, input/output/reasoning token totals, and distinct models. Optional
credits and Activity calls have bounded four-second deadlines; a timeout,
HTTP rejection, or invalid response is retained as a source diagnostic without
discarding valid `/key` data. Workspace filters are opt-in through the
environment variables above.

## Codex two-account live test

Keep each native Codex session in its own `CODEX_HOME`. This leaves the normal
`%USERPROFILE%\\.codex` installation untouched and prevents one account from
being silently reused for the other:

```powershell
$probeRoot = Join-Path $env:LOCALAPPDATA "CodexUsageMonitor-Rust\\codex-accounts"
$account1Home = Join-Path $probeRoot "account-1"
$account2Home = Join-Path $probeRoot "account-2"
New-Item -ItemType Directory -Force $account1Home, $account2Home | Out-Null

$env:CODEX_HOME = $account1Home
codex.cmd login
codex.cmd login status

$env:CODEX_HOME = $account2Home
codex.cmd login
codex.cmd login status
```

When the browser opens for the second account, choose **Use another account**
if the browser already has the first account selected. Do not run `codex.cmd
logout` without `CODEX_HOME` set to one of these isolated directories.

For a new account, the probe can own the login step as well. It opens the
native browser login with `CODEX_HOME` scoped only to the child process, then
probes both sessions automatically:

```powershell
cd "<path-to>\\CodexUsageMonitor-Rust\\rust"
cargo run -p codex-usage-codex-probe -- `
  --login `
  --home $account2Home `
  --home (Join-Path $env:USERPROFILE ".codex")
```

The probe prints the plan, source actually used (`oauth`, `pat`, or native
CLI), every quota window, absolute reset time, and credit information. It never
prints access/refresh tokens. It uses the same account-scoped runtime that the
future tray host will use; no browser or WebView remains running after login.
