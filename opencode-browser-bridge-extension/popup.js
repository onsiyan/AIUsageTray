const DEFAULT_ENDPOINT = "http://127.0.0.1:0/v1/browser-bridge";
const PROVIDER_ID = "opencodego";
const ALLOWED_HOSTS = new Set(["opencode.ai", "app.opencode.ai"]);
const MAX_COOKIES = 128;

const endpointInput = document.getElementById("endpoint");
const pairingInput = document.getElementById("pairing");
const connectButton = document.getElementById("connect");
const statusElement = document.getElementById("status");

function setStatus(message, isError = false) {
  statusElement.textContent = message;
  statusElement.style.color = isError ? "#b91c1c" : "#166534";
}

function getCookies(url) {
  return new Promise((resolve, reject) => {
    chrome.cookies.getAll({ url }, (cookies) => {
      const error = chrome.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
        return;
      }
      resolve(cookies);
    });
  });
}

function selectCookies(cookies) {
  const selected = [];
  const seen = new Set();
  for (const cookie of cookies) {
    const domain = String(cookie.domain || "").replace(/^\./, "").toLowerCase();
    if (!ALLOWED_HOSTS.has(domain) && ![...ALLOWED_HOSTS].some((host) => domain.endsWith(`.${host}`))) {
      continue;
    }
    const key = `${cookie.name.toLowerCase()}\u0000${domain}\u0000${cookie.path || "/"}`;
    if (seen.has(key) || !cookie.name || !cookie.value) {
      continue;
    }
    seen.add(key);
    selected.push({
      name: cookie.name,
      value: cookie.value,
      domain: cookie.domain,
      path: cookie.path || "/"
    });
    if (selected.length >= MAX_COOKIES) {
      break;
    }
  }
  return selected;
}

async function connect() {
  connectButton.disabled = true;
  setStatus("يتم جمع كوكيز OpenCode وإرسالها محليًا...");
  try {
    const endpoint = endpointInput.value.trim().replace(/\/$/, "");
    const pairing = pairingInput.value.trim();
    if (!endpoint || endpoint.includes(":0/") || !pairing) {
      throw new Error("أدخل Bridge endpoint ورمز الاقتران من البرنامج أولًا.");
    }
    const cookies = selectCookies([
      ...(await getCookies("https://opencode.ai/")),
      ...(await getCookies("https://app.opencode.ai/"))
    ]);
    if (!cookies.some((cookie) => ["auth", "__host-auth", "__host-console_session"].includes(cookie.name.toLowerCase()))) {
      throw new Error("لم نجد جلسة OpenCode. أكمل تسجيل الدخول ثم اضغط Connect مرة أخرى.");
    }
    const response = await fetch(endpoint, {
      method: "POST",
      headers: {
        "Authorization": `Bearer ${pairing}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify({
        provider_id: PROVIDER_ID,
        cookies,
        user_agent: navigator.userAgent,
        browser: "chromium"
      })
    });
    if (!response.ok) {
      throw new Error(`رفض البرنامج الطلب (${response.status}). تأكد من الرمز وأن جلسة الإضافة ما زالت مفتوحة.`);
    }
    setStatus("تم إرسال الجلسة. يمكنك إغلاق هذه النافذة.");
    pairingInput.value = "";
    await chrome.storage.local.remove("pairing");
  } catch (error) {
    setStatus(error instanceof Error ? error.message : String(error), true);
  } finally {
    connectButton.disabled = false;
  }
}

async function loadSettings() {
  const settings = await chrome.storage.local.get({ endpoint: DEFAULT_ENDPOINT, pairing: "" });
  endpointInput.value = settings.endpoint;
  pairingInput.value = settings.pairing;
}

endpointInput.addEventListener("change", () => {
  chrome.storage.local.set({ endpoint: endpointInput.value.trim() });
});
connectButton.addEventListener("click", connect);
loadSettings().catch((error) => setStatus(String(error), true));
