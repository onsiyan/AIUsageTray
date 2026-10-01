# Browser session bridge

This unpacked Manifest V3 extension supports the Windows Codex and OpenCode Go
account-add flows. It does not run a general background export, send session
data to a remote service, or log cookie values. Codex transfer is enabled only
while a one-shot local Add Account request is pending and ChatGPT reports a
signed-in email.

## Load once in the default Chromium browser

1. Open the browser's extensions page (`brave://extensions`, `chrome://extensions`,
   or `edge://extensions`).
2. Enable **Developer mode**.
3. Choose **Load unpacked** and select this directory.

The extension needs the `cookies` permission and host permissions for ChatGPT
and OpenAI cookie domains, OpenCode domains, and loopback HTTP. The browser
shows these permissions when the extension is loaded or updated. If the
extension was already installed, reload it once after updating these files so
the new Codex permissions and service worker take effect.

## Add a Codex account

1. Start Add Account in the monitor. It opens a one-shot local bootstrap page
   in the default browser, stores a short-lived pairing code in extension
   storage, and redirects to ChatGPT.
2. Sign in to the desired ChatGPT account. While the local add request remains
   pending, the extension checks ChatGPT's own session endpoint. Once that
   endpoint returns a signed-in email, the extension automatically transfers
   only ChatGPT/OpenAI cookies to the paired loopback listener. No extension
   button click is needed.
3. The monitor verifies the email through `/api/auth/session`, saves the
   account-scoped credentials in Windows Credential Manager, and then queries
   WHAM. The loopback listener accepts one payload and closes.

The extension never receives the password or reads browser database files. It
does not transfer cookies when there is no pending local pairing.

## Add an OpenCode Go account

1. Run `codex-usage-opencode-go-probe` (or the eventual tray **Add account**
   action). The program opens a local bootstrap page in the default browser,
   then redirects to OpenCode and stores the one-time endpoint and pairing code.
2. Finish login at `https://opencode.ai/auth`.
3. Open this extension and press **Connect this account**. If the browser did
   not pre-fill the fields, paste the endpoint and pairing code printed by the
   program.
4. The program validates the provider domains and required authentication
   cookie, stores the result in Windows Credential Manager, and closes the
   loopback session.

The OpenCode pairing code is single-use and expires when the add-account
timeout ends. Its manual **Connect this account** flow is unchanged.
