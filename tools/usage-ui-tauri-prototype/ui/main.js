// The only native bridge action in this static preview is hiding its own popup.
document.getElementById("close").addEventListener("click", () => {
  window.__TAURI__.core.invoke("hide_popup");
});
