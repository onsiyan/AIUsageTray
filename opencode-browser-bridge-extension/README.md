# OpenCode browser bridge

This is the explicit user-action part of the Windows account-add flow. It is
deliberately a plain Manifest V3 unpacked extension; it does not run a
background export, send data to the internet, or log cookie values.

## Load once in the default Chromium browser

1. Open the browser's extensions page (`brave://extensions`, `chrome://extensions`,
   or `edge://extensions`).
2. Enable **Developer mode**.
3. Choose **Load unpacked** and select this directory.

The extension needs its `cookies` permission and host permissions only for
`opencode.ai`, `app.opencode.ai`, and loopback HTTP. The browser shows these
permissions before enabling it.

## Connect an account

1. Run `codex-usage-opencode-go-probe` (or the eventual tray **Add account**
   action). The program opens a local bootstrap page in the Windows default
   browser, then redirects to OpenCode and pre-fills the extension's one-time
   endpoint and pairing code.
2. Finish login at `https://opencode.ai/auth`.
3. Open this extension and press **Connect this account**. If the browser did
   not pre-fill the fields, paste the endpoint and pairing code printed by the
   program.
4. The program accepts one payload, validates the provider domains and required
   authentication cookie, stores the result in Windows Credential Manager, and
   closes the loopback session.

The pairing code is single-use and expires when the add-account timeout ends.
The extension sends only cookies whose domains are `opencode.ai` or
`app.opencode.ai`; it never sends passwords or browser database files.
