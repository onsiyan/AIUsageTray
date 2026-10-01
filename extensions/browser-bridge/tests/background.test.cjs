const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const backgroundSource = fs.readFileSync(
  path.join(__dirname, "..", "background.js"),
  "utf8"
);

function createBridgeHarness({ settings, cookiesByDomain, fetchImpl }) {
  let messageListener;
  const state = { ...settings };
  const requests = [];
  const chrome = {
    runtime: {
      lastError: undefined,
      onMessage: {
        addListener(listener) {
          messageListener = listener;
        }
      }
    },
    storage: {
      local: {
        get(keys, callback) {
          callback(Object.fromEntries(keys.filter((key) => key in state).map((key) => [key, state[key]])));
        },
        set(values, callback) {
          Object.assign(state, values);
          callback();
        },
        remove(keys, callback) {
          for (const key of keys) delete state[key];
          callback();
        }
      }
    },
    cookies: {
      getAll({ domain }, callback) {
        callback(cookiesByDomain[domain] || []);
      }
    }
  };
  const context = {
    chrome,
    fetch: async (url, options) => {
      requests.push({ url, options });
      return fetchImpl(url, options);
    },
    URL,
    TextEncoder,
    Promise,
    Date,
    String,
    Set,
    Error
  };
  vm.runInNewContext(backgroundSource, context, { filename: "background.js" });

  return {
    state,
    requests,
    dispatch(message, sender) {
      return new Promise((resolve, reject) => {
        try {
          const keepChannelOpen = messageListener(message, sender, resolve);
          if (!keepChannelOpen) reject(new Error("message was not handled"));
        } catch (error) {
          reject(error);
        }
      });
    }
  };
}

test("pending Codex add transfers only provider cookies to the paired loopback listener", async () => {
  const harness = createBridgeHarness({
    settings: {
      endpoint: "http://127.0.0.1:43921/v1/browser-bridge",
      pairing: "p".repeat(43),
      providerId: "openai",
      pairingExpiresAt: Date.now() + 60_000,
      bridgeStatus: "waiting-for-chatgpt"
    },
    cookiesByDomain: {
      "chatgpt.com": [
        { name: "session", value: "chatgpt-secret", domain: ".chatgpt.com", path: "/" },
        { name: "unrelated", value: "discard-me", domain: ".evil.example", path: "/" }
      ],
      "openai.com": [
        { name: "device", value: "openai-device", domain: ".openai.com", path: "/" }
      ]
    },
    fetchImpl: async () => ({ ok: true, status: 200 })
  });

  const result = await harness.dispatch(
    {
      type: "codex-session-ready",
      providerId: "openai",
      email: "person@example.com",
      userAgent: "Browser UA"
    },
    { url: "https://chatgpt.com/" }
  );

  assert.equal(result.accepted, true);
  assert.equal(harness.requests.length, 1);
  assert.equal(harness.requests[0].url, "http://127.0.0.1:43921/v1/browser-bridge");
  assert.equal(harness.requests[0].options.credentials, "omit");
  const payload = JSON.parse(harness.requests[0].options.body);
  assert.deepEqual(payload.cookies.map((cookie) => cookie.name).sort(), ["device", "session"]);
  assert.equal(payload.provider_id, "openai");
  assert.equal("pairing" in harness.state, false);
  assert.equal(harness.state.providerId, "openai");
  assert.equal(harness.state.bridgeStatus, "session-sent");
});

test("Codex cookies are not read or sent without a pending pairing", async () => {
  const harness = createBridgeHarness({
    settings: { providerId: "openai" },
    cookiesByDomain: {},
    fetchImpl: async () => {
      throw new Error("fetch should not run without a pairing");
    }
  });

  const result = await harness.dispatch(
    {
      type: "codex-session-ready",
      providerId: "openai",
      email: "person@example.com"
    },
    { url: "https://chatgpt.com/" }
  );

  assert.equal(result.pending, false);
  assert.equal(harness.requests.length, 0);
});
