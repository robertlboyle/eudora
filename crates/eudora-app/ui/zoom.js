// Text size and large buttons in every Eudora window. The size is kept in the shared storage
// ("eudora.textSize": 1 = usual, up to 3), applied to each window as it opens; Ctrl+plus /
// Ctrl+minus / Ctrl+0 change it in every window at once (Settings ▸ General ▸ Text size too).
// Pages inside the main window (an inner message window) are drawn at its size.

"use strict";

(() => {
  if (window.parent !== window) return; // (drawn at the window's own size)
  const invoke = window.__TAURI__?.core?.invoke;
  if (!invoke) return;
  const STEPS = [0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3];
  const read = (key, fallback) => { try { const v = localStorage.getItem(`eudora.${key}`); return v === null ? fallback : JSON.parse(v); } catch { return fallback; } };
  const size = () => Number(read("textSize", 1)) || 1;

  function apply() {
    invoke("zoom_this", { scale: size() }).catch(() => {});
    document.documentElement.classList.toggle("big-buttons", !!read("bigButtons", false));
    document.documentElement.classList.toggle("roomy", !!read("roomy", false));
  }

  /** A new size for every window (and remembered). */
  window.setTextSize = (scale) => {
    const s = Math.min(3, Math.max(0.5, Number(scale) || 1));
    try { localStorage.setItem("eudora.textSize", JSON.stringify(s)); } catch { /* (not kept) */ }
    invoke("zoom_all", { scale: s }).catch(() => {});
    window.announce?.(`Text size ${Math.round(s * 100)} percent`);
    return s;
  };

  document.addEventListener("keydown", (e) => {
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
    const now = size();
    let next = null;
    if (e.key === "=" || e.key === "+") next = STEPS.find((s) => s > now + 0.001) ?? 3;
    else if (e.key === "-" || e.key === "_") next = [...STEPS].reverse().find((s) => s < now - 0.001) ?? STEPS[0];
    else if (e.key === "0") next = 1;
    if (next == null) return;
    e.preventDefault();
    window.setTextSize(next);
  });

  // another window changed it (the storage event), or large buttons were switched
  window.addEventListener("storage", (e) => {
    if (e.key === "eudora.bigButtons" || e.key === "eudora.roomy") apply();
  });
  apply();
})();
