# Security

AI Usage Tray signs in to AI providers on the user's behalf and keeps their
credentials, so it treats credential handling as a security boundary.

## How credentials are handled

- **Secrets never touch the database.** `accounts.db` holds account metadata
  and usage snapshots only.
- **Refresh credentials, API keys, and imported sessions** are stored per
  account in Windows Credential Manager (`UsageMonitor/OAuth/<account-id>` and
  `UsageMonitor/Auth/<account-id>`), readable only by the signed-in Windows
  user.
- **Access tokens** are kept in memory and are never written to disk by the
  monitor itself.
- **Sign-in** uses each provider's OAuth authorization-code flow with PKCE, a
  random `state`, and a short-lived loopback listener that rejects requests
  without the expected state. OpenRouter keys are read from stdin or the
  environment and are never accepted as command-line arguments.
- **Account isolation.** A credential is only used for the account it
  identifies, read from that account's Credential Manager entry (or, while a
  Codex account is linked, from Codex's own `auth.json`). Environment keys,
  CLI sessions, and browser cookies are never used as usage credentials.
- **Logs and output** never include tokens, keys, or cookies; errors returned by
  sign-in helpers are redacted before they are shown.

## Desktop app switching

"Use in Codex" and "Use in Antigravity" deliberately write an account's tokens
where those apps read their sign-in (`%USERPROFILE%\.codex\auth.json` and the
Credential Manager entry `gemini:antigravity`). A sign-in found there that does
not belong to a saved account is backed up first: Codex's file to
`%LOCALAPPDATA%\UsageMonitor\codex-auth-backups\`, Antigravity's entry to
`UsageMonitor/Backup/gemini-antigravity`. Removing an account from the monitor
does not revoke it at the provider.

## Reporting a vulnerability

Please report vulnerabilities privately through the repository's
**Security → Report a vulnerability** page (GitHub private vulnerability
reporting) rather than in a public issue. Include the affected version, steps
to reproduce, and the impact. You will receive an acknowledgement, and fixes
are released as soon as they are verified.
