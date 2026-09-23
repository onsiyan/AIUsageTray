const OPENAI_PROVIDER_ID = "openai";
const BRIDGE_PATH = "/v1/browser-bridge";
const MAX_COOKIE_COUNT = 128;
const MAX_COOKIE_BYTES = 400 * 1024;

let captureInFlight = false;

function getSettings() {
  return new Promise((resolve, reject) => {
    chrome.storage.local.get(
      ["endpoint", "pairing", "providerId", "pairingExpiresAt"],
      (settings) => {
        const error = chrome.runtime.lastError;
        if (error) {
          reject(new Error(error.message));
          return;
        }
        resolve(settings);
      }
    );
  });
}

function setBridgeStatus(status) {
  return new Promise((resolve) => {
    chrome.storage.local.set({ bridgeStatus: status }, resolve);
  });
}

function removePairing() {
  return new Promise((resolve) => {
    chrome.storage.local.remove(
      ["pairing", "pairingExpiresAt"],
      resolve
    );
  });
}

function getCookies(domain) {
  return new Promise((resolve, reject) => {
    chrome.cookies.getAll({ domain }, (cookies) => {
      const error = chrome.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
        return;
      }
      resolve(cookies || []);
    });
  });
}

function isAllowedCookieDomain(value) {
  const domain = String(value || "").replace(/^\./, "").toLowerCase();
  return ["chatgpt.com", "openai.com"].some(
    (allowed) => domain === allowed || domain.endsWith(`.${allowed}`)
  );
}

function selectCookies(cookies) {
  const selected = [];
  const seen = new Set();
  let totalBytes = 0;

  for (const cookie of cookies) {
    if (!isAllowedCookieDomain(cookie.domain) || !cookie.name || !cookie.value) {
      continue;
    }
    const key = `${cookie.name.toLowerCase()}\u0000${cookie.domain}\u0000${cookie.path || "/"}`;
    if (seen.has(key)) {
      continue;
    }
    const value = {
      name: cookie.name,
      value: cookie.value,
      domain: cookie.domain,
      path: cookie.path || "/"
    };
    totalBytes += new TextEncoder().encode(JSON.stringify(value)).byteLength;
    if (selected.length >= MAX_COOKIE_COUNT || totalBytes > MAX_COOKIE_BYTES) {
      throw new Error("حجم كوكيز ChatGPT تجاوز حد النقل المحلي.");
    }
    seen.add(key);
    selected.push(value);
  }
  return selected;
}

function validateBridgeEndpoint(value) {
  const endpoint = new URL(value);
  if (
    endpoint.protocol !== "http:" ||
    !["127.0.0.1", "localhost"].includes(endpoint.hostname) ||
    endpoint.pathname !== BRIDGE_PATH ||
    endpoint.username ||
    endpoint.password ||
    endpoint.search ||
    endpoint.hash
  ) {
    throw new Error("رفضت الإضافة عنوان الجسر لأنه ليس عنوان loopback متوقعًا.");
  }
  return endpoint.href;
}

async function captureCodexSession(sender, message) {
  const senderUrl = new URL(sender.url || "about:blank");
  if (senderUrl.protocol !== "https:" || senderUrl.hostname !== "chatgpt.com") {
    throw new Error("مصدر طلب نقل الجلسة ليس ChatGPT.");
  }
  if (
    message?.type !== "codex-session-ready" ||
    message.providerId !== OPENAI_PROVIDER_ID ||
    typeof message.email !== "string" ||
    !message.email.includes("@")
  ) {
    return { pending: true };
  }

  const settings = await getSettings();
  if (
    settings.providerId !== OPENAI_PROVIDER_ID ||
    !/^[A-Za-z0-9_-]{40,64}$/.test(settings.pairing || "")
  ) {
    return { pending: false };
  }
  if (!Number.isFinite(settings.pairingExpiresAt) || Date.now() >= settings.pairingExpiresAt) {
    await removePairing();
    await setBridgeStatus("expired");
    return { pending: false, expired: true };
  }
  if (captureInFlight) {
    return { pending: true };
  }

  captureInFlight = true;
  try {
    const [chatgptCookies, openaiCookies] = await Promise.all([
      getCookies("chatgpt.com"),
      getCookies("openai.com")
    ]);
    const cookies = selectCookies([...chatgptCookies, ...openaiCookies]);
    if (cookies.length === 0) {
      return { pending: true };
    }

    const body = JSON.stringify({
      provider_id: OPENAI_PROVIDER_ID,
      cookies,
      user_agent: String(message.userAgent || "").slice(0, 1024)
    });
    if (new TextEncoder().encode(body).byteLength > MAX_COOKIE_BYTES) {
      throw new Error("حجم كوكيز ChatGPT تجاوز حد النقل المحلي.");
    }
    const response = await fetch(validateBridgeEndpoint(settings.endpoint), {
      method: "POST",
      headers: {
        Authorization: `Bearer ${settings.pairing}`,
        "Content-Type": "application/json"
      },
      body,
      cache: "no-store",
      credentials: "omit"
    });
    if (!response.ok) {
      if ([401, 404].includes(response.status)) {
        await removePairing();
        await setBridgeStatus("expired");
        return { pending: false, expired: true };
      }
      await setBridgeStatus("waiting-for-chatgpt");
      return { pending: true, rejected: true };
    }

    await removePairing();
    await setBridgeStatus("session-sent");
    return { pending: false, accepted: true };
  } finally {
    captureInFlight = false;
  }
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message?.type === "codex-pairing-expired") {
    const senderUrl = new URL(sender.url || "about:blank");
    if (senderUrl.protocol !== "https:" || senderUrl.hostname !== "chatgpt.com") {
      return false;
    }
    getSettings()
      .then(async (settings) => {
        if (
          settings.providerId === OPENAI_PROVIDER_ID &&
          Number.isFinite(settings.pairingExpiresAt) &&
          Date.now() >= settings.pairingExpiresAt
        ) {
          await removePairing();
          await setBridgeStatus("expired");
        }
        sendResponse({ pending: false, expired: true });
      })
      .catch((error) => sendResponse({ pending: false, error: String(error) }));
    return true;
  }
  if (message?.type !== "codex-session-ready") {
    return false;
  }
  captureCodexSession(sender, message)
    .then(sendResponse)
    .catch(async (error) => {
      await setBridgeStatus("capture-error");
      sendResponse({ pending: true, error: String(error) });
    });
  return true;
});
