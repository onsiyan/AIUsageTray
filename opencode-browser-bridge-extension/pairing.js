(() => {
  const pairing = new URLSearchParams(location.hash.replace(/^#/, "")).get("pairing");
  if (!pairing || !/^[A-Za-z0-9_-]{40,64}$/.test(pairing)) {
    return;
  }
  chrome.storage.local.set({
    endpoint: `${location.origin}/v1/browser-bridge`,
    pairing
  });
})();
