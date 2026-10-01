(() => {
  const POLL_INTERVAL_MS = 2000;
  const MAX_WAIT_MS = 5 * 60 * 1000;
  const startedAt = Date.now();
  let stopped = false;
  let timer;

  function getPairingState() {
    return new Promise((resolve) => {
      chrome.storage.local.get(
        ["providerId", "pairing", "pairingExpiresAt", "bridgeStatus"],
        resolve
      );
    });
  }

  function sendReadyMessage(email) {
    return new Promise((resolve) => {
      chrome.runtime.sendMessage(
        {
          type: "codex-session-ready",
          providerId: "openai",
          email,
          userAgent: navigator.userAgent
        },
        (response) => {
          if (chrome.runtime.lastError) {
            resolve({ pending: true });
            return;
          }
          resolve(response || { pending: true });
        }
      );
    });
  }

  function notifyPairingExpired() {
    chrome.runtime.sendMessage({ type: "codex-pairing-expired" }, () => {
      void chrome.runtime.lastError;
    });
  }

  async function checkForSignedInSession() {
    const settings = await getPairingState();
    if (
      settings.providerId !== "openai" ||
      !settings.pairing ||
      settings.bridgeStatus === "session-sent" ||
      settings.bridgeStatus === "expired"
    ) {
      stopped = true;
      return;
    }
    if (Date.now() - startedAt >= MAX_WAIT_MS || Date.now() >= settings.pairingExpiresAt) {
      notifyPairingExpired();
      stopped = true;
      return;
    }

    try {
      const response = await fetch("/api/auth/session", {
        method: "GET",
        credentials: "include",
        cache: "no-store",
        headers: { Accept: "application/json" }
      });
      if (!response.ok) {
        return;
      }
      const session = await response.json();
      const email = session?.user?.email;
      if (typeof email !== "string" || !email.includes("@")) {
        return;
      }

      const result = await sendReadyMessage(email);
      if (result?.accepted || result?.expired || result?.pending === false) {
        stopped = true;
      }
    } catch {
      // Login redirects and transient API failures are expected while the user
      // is still completing sign-in; retry without exposing any page data.
    }
  }

  async function tick() {
    await checkForSignedInSession();
    if (!stopped) {
      timer = setTimeout(tick, POLL_INTERVAL_MS);
    } else {
      clearTimeout(timer);
    }
  }

  tick();
})();
