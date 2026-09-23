(() => {
  const params = new URLSearchParams(location.hash.replace(/^#/, ""));
  const pairing = params.get("pairing");
  const providerId = params.get("provider_id");
  if (!pairing || !/^[A-Za-z0-9_-]{40,64}$/.test(pairing)) {
    return;
  }
  if (!["openai", "opencodego"].includes(providerId)) {
    return;
  }
  chrome.storage.local.set({
    endpoint: `${location.origin}/v1/browser-bridge`,
    pairing,
    providerId,
    pairingExpiresAt: Date.now() + 5 * 60 * 1000,
    bridgeStatus: providerId === "openai" ? "waiting-for-chatgpt" : "ready-for-opencode"
  });
})();
