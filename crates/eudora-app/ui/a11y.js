// Keyboard and screen-reader access for the main window. Loaded after app.js, calendar.js and
// todo.js:
//
//  - the Mailboxes pane as a tree (WAI-ARIA tree pattern): ↑ ↓ move, → opens or goes in, ← closes
//    or goes up, Home / End, Enter or Space opens, letters jump to a name, Shift+F10 / the Menu key
//    its right-click menu;
//  - every right-click menu (.popup-menu) as a menu: opened from the keyboard, ↑ ↓ Home End, Enter,
//    Esc back to where the keyboard was;
//  - F6 / Shift+F6 from pane to pane (classic Eudora's keys): Mailboxes, Messages, the message, the
//    toolbar, the search box;
//  - Shift+F10 / the Menu key on a message opens its right-click menu.

"use strict";

(() => {
  const $id = (id) => document.getElementById(id);

  // ---------------------------------------------------------------- the Mailboxes tree

  const tree = $id("folders");
  tree.setAttribute("role", "tree");
  let active = null; // the key of the row the keyboard is on
  let marking = false;

  const rows = () => [...tree.querySelectorAll(".folder")].filter((r) => r.offsetParent !== null || r.getClientRects().length);

  /** Roles, levels, open / closed, chosen, and a spoken name for every row, after each redraw. */
  function markTree() {
    marking = true;
    let n = 0;
    for (const r of tree.querySelectorAll(".folder")) {
      r.setAttribute("role", "treeitem");
      if (!r.id) r.id = `tree-${++n}-${Math.random().toString(36).slice(2, 7)}`;
      const depth = Number(r.style.getPropertyValue("--depth")) || 0;
      r.setAttribute("aria-level", String(depth + 1));
      const twist = r.querySelector(".twist");
      const hasChildren = twist && twist.childElementCount > 0;
      if (hasChildren) r.setAttribute("aria-expanded", String(typeof state !== "undefined" && state.open?.has(r.dataset.key)));
      else r.removeAttribute("aria-expanded");
      r.setAttribute("aria-selected", String(r.classList.contains("on")));
      const name = r.querySelector(".f-name")?.textContent || r.textContent.trim();
      const total = r.querySelector(".f-total")?.dataset.full;
      const unread = r.querySelector(".f-unread")?.dataset.full?.replace(/[()]/g, "");
      const left = r.classList.contains("search-off") || r.querySelector(".search-off") ? ", left out of search" : "";
      r.setAttribute("aria-label", [name, total && `${total} messages`, unread && `${unread} unread`].filter(Boolean).join(", ") + left);
      for (const icon of r.querySelectorAll(".icon, .twist")) icon.setAttribute("aria-hidden", "true");
      r.classList.toggle("kb", r.dataset.key === active);
    }
    const current = rows().find((r) => r.dataset.key === active);
    if (current) tree.setAttribute("aria-activedescendant", current.id);
    else tree.removeAttribute("aria-activedescendant");
    marking = false;
  }
  let markSoon = 0;
  new MutationObserver(() => {
    if (marking || markSoon) return;
    markSoon = requestAnimationFrame(() => { markSoon = 0; markTree(); });
  }).observe(tree, { childList: true, subtree: true, attributes: true, attributeFilter: ["class"] });

  function goTo(r) {
    if (!r) return;
    active = r.dataset.key;
    markTree();
    r.scrollIntoView({ block: "nearest" });
  }

  tree.addEventListener("focus", () => {
    const all = rows();
    if (!all.some((r) => r.dataset.key === active)) goTo(all.find((r) => r.classList.contains("on")) || all[0]);
  });
  tree.addEventListener("click", (e) => {
    const r = e.target.closest(".folder");
    if (r) { active = r.dataset.key; markTree(); }
  });

  const tree_typed = { text: "", at: 0 };
  tree.addEventListener("keydown", (e) => {
    const all = rows();
    const i = all.findIndex((r) => r.dataset.key === active);
    const cur = all[i];
    const level = (r) => Number(r?.getAttribute("aria-level")) || 1;
    const key = e.key;
    if (key === "ArrowDown") goTo(all[Math.min(all.length - 1, i + 1)]);
    else if (key === "ArrowUp") goTo(all[Math.max(0, i - 1)]);
    else if (key === "Home") goTo(all[0]);
    else if (key === "End") goTo(all[all.length - 1]);
    else if (key === "ArrowRight" && cur) {
      if (cur.getAttribute("aria-expanded") === "false") cur.querySelector(".twist")?.click();
      else if (cur.getAttribute("aria-expanded") === "true" && level(all[i + 1]) > level(cur)) goTo(all[i + 1]);
    } else if (key === "ArrowLeft" && cur) {
      if (cur.getAttribute("aria-expanded") === "true") cur.querySelector(".twist")?.click();
      else {
        for (let j = i - 1; j >= 0; j--) if (level(all[j]) < level(cur)) { goTo(all[j]); break; }
      }
    } else if ((key === "Enter" || key === " ") && cur) {
      cur.click();
    } else if ((key === "F10" && e.shiftKey) || key === "ContextMenu") {
      if (cur) openContextMenu(cur);
    } else if (key.length === 1 && /\S/.test(key) && !e.ctrlKey && !e.altKey && !e.metaKey) {
      // type-ahead: the next name starting with what was typed (a second's pause starts over)
      const now = Date.now();
      tree_typed.text = now - tree_typed.at > 1000 ? key : tree_typed.text + key;
      tree_typed.at = now;
      const want = tree_typed.text.toLowerCase();
      const name = (r) => (r.querySelector(".f-name")?.textContent || "").toLowerCase();
      const from = tree_typed.text.length === 1 ? i + 1 : i;
      for (let n = 0; n < all.length; n++) {
        const r = all[(Math.max(0, from) + n) % all.length];
        if (name(r).startsWith(want)) { goTo(r); break; }
      }
    } else return;
    e.preventDefault();
  });

  // ---------------------------------------------------------------- right-click menus

  /** Opens an element's right-click menu from the keyboard, at the element. */
  function openContextMenu(target) {
    const box = target.getBoundingClientRect();
    target.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: box.left + 24, clientY: box.top + box.height / 2 }));
  }
  window.openContextMenu = openContextMenu;

  // the message list: Shift+F10 / the Menu key on the message with the keyboard
  $id("list").addEventListener("keydown", (e) => {
    if (!((e.key === "F10" && e.shiftKey) || e.key === "ContextMenu")) return;
    const row = document.querySelector("#list .row.on") || document.querySelector("#list .row");
    if (row) { e.preventDefault(); e.stopPropagation(); openContextMenu(row); }
  }, true);

  /** Where the keyboard was before a menu opened (it goes back there when the menu closes). */
  let before = null;
  const items = (menu) => [...menu.querySelectorAll("button, [role=menuitem]")].filter((b) => !b.disabled && b.offsetParent !== null);

  function menuShown(menu) {
    menu.setAttribute("role", "menu");
    for (const hr of menu.querySelectorAll("hr")) hr.setAttribute("role", "separator");
    const list = items(menu);
    for (const b of list) { b.setAttribute("role", "menuitem"); b.tabIndex = -1; }
    if (!menu.contains(document.activeElement)) before = document.activeElement;
    list[0]?.focus();
  }
  function menuHidden(menu) {
    if (before && (menu.contains(document.activeElement) || document.activeElement === document.body)) {
      try { before.focus(); } catch { /* (gone) */ }
    }
    before = null;
  }
  const visible = (m) => !m.hidden && m.isConnected && m.offsetParent !== null;
  const seen = new WeakMap(); // menu → shown last time it was looked at
  function lookAtMenus() {
    for (const m of document.querySelectorAll(".popup-menu")) {
      const now = visible(m);
      if (now && !seen.get(m)) menuShown(m);
      if (!now && seen.get(m)) menuHidden(m);
      seen.set(m, now);
    }
  }
  let menuSoon = 0;
  new MutationObserver((changes) => {
    // (only changes to menus themselves: being shown, hidden, added, or filled)
    if (!changes.some((c) => c.target.closest?.(".popup-menu") || [...c.addedNodes, ...c.removedNodes].some((n) => n.nodeType === 1 && (n.matches(".popup-menu") || n.querySelector?.(".popup-menu"))))) return;
    if (!menuSoon) menuSoon = requestAnimationFrame(() => { menuSoon = 0; lookAtMenus(); });
  }).observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden", "style", "class"] });

  document.addEventListener("keydown", (e) => {
    const menu = document.activeElement?.closest?.(".popup-menu");
    if (!menu) return;
    const list = items(menu);
    const i = list.indexOf(document.activeElement);
    if (e.key === "ArrowDown") list[(i + 1) % list.length]?.focus();
    else if (e.key === "ArrowUp") list[(i - 1 + list.length) % list.length]?.focus();
    else if (e.key === "Home") list[0]?.focus();
    else if (e.key === "End") list[list.length - 1]?.focus();
    else if (e.key === "Escape" || e.key === "Tab") {
      menu.hidden = true;
      if (menu.style) menu.style.display = menu.style.display === "block" ? "none" : menu.style.display;
      lookAtMenus();
    } else if (e.key.length === 1 && /\S/.test(e.key)) {
      // a letter: the next item starting with it
      const want = e.key.toLowerCase();
      for (let n = 1; n <= list.length; n++) {
        const b = list[(i + n) % list.length];
        if (b.textContent.trim().toLowerCase().startsWith(want)) { b.focus(); break; }
      }
    } else return; // (Enter and Space press the button)
    e.preventDefault();
    e.stopPropagation();
  }, true);

  // ---------------------------------------------------------------- F6: pane to pane

  /** The panes in order, those shown now. */
  function panes() {
    const shown = (el) => el && el.offsetParent !== null && !el.closest("[hidden]");
    const message = $id("m-body")?.querySelector("iframe") || $id("message");
    const right = document.querySelector("#calendar:not([hidden]) [tabindex], #todo:not([hidden]) [tabindex], #contacts:not([hidden]) #ab-list");
    return [tree, right || $id("list"), right ? null : message, document.querySelector("#toolbar .actions button:not([hidden])"), $id("query")].filter(shown);
  }
  document.addEventListener("keydown", (e) => {
    if (e.key !== "F6" || e.ctrlKey || e.altKey) return;
    const all = panes();
    if (!all.length) return;
    const now = all.findIndex((p) => p === document.activeElement || p.contains(document.activeElement));
    const next = all[(now + (e.shiftKey ? -1 : 1) + all.length) % all.length];
    e.preventDefault();
    next.focus();
    try { if (next.tagName === "IFRAME") next.contentDocument?.body?.focus(); } catch { /* (its own) */ }
    const names = { folders: "Mailboxes", list: "Messages", query: "Search" };
    window.announce?.(names[next.id] || (next.tagName === "IFRAME" ? "Message" : next.closest("#toolbar") ? "Toolbar" : next.getAttribute("aria-label") || ""));
  });


  // ---------------------------------------------------------------- tabs

  /** A row of buttons shown as tabs: each a tab, the one on selected and the only one Tab
   *  stops at; ← → Home End move along and choose. Kept in step as the page changes `.on`. */
  function makeTabs(list) {
    const tabs = () => [...list.querySelectorAll("button")];
    const sync = () => {
      for (const b of tabs()) {
        const on = b.classList.contains("on");
        b.setAttribute("role", "tab");
        b.setAttribute("aria-selected", String(on));
        b.tabIndex = on ? 0 : -1;
      }
    };
    sync();
    new MutationObserver(sync).observe(list, { subtree: true, attributes: true, attributeFilter: ["class"], childList: true });
    list.addEventListener("keydown", (e) => {
      const all = tabs();
      const i = all.indexOf(document.activeElement);
      const to = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: all.length - 1 }[e.key];
      if (to === undefined || i < 0) return;
      e.preventDefault();
      const b = all[(to + all.length) % all.length];
      b.focus();
      b.click();
    });
  }
  window.makeTabs = makeTabs;
  for (const list of document.querySelectorAll("[role=tablist]")) {
    if (!list.getAttribute("aria-label")) list.setAttribute("aria-label", "Show the message as");
    makeTabs(list);
  }

  markTree();
})();

// Settings ▸ General ▸ Text size and buttons (zoom.js does the work)
(() => {
  const sel = document.getElementById("p-text-size");
  const big = document.getElementById("p-big-buttons");
  if (!sel || !big) return;
  const read = (key, fallback) => { try { const v = localStorage.getItem(`eudora.${key}`); return v === null ? fallback : JSON.parse(v); } catch { return fallback; } };
  const show = () => {
    const s = Number(read("textSize", 1)) || 1;
    sel.value = String(s);
    if (sel.value !== String(s)) sel.value = "1";
    big.checked = !!read("bigButtons", false);
  };
  sel.addEventListener("change", () => window.setTextSize?.(Number(sel.value)));
  const roomy = document.getElementById("p-roomy");
  if (roomy) {
    roomy.checked = !!read("roomy", false);
    roomy.addEventListener("change", () => {
      try { localStorage.setItem("eudora.roomy", JSON.stringify(roomy.checked)); } catch { /* (not kept) */ }
      document.documentElement.classList.toggle("roomy", roomy.checked);
      // (the list reads its row height from the look: drawn again)
      if (typeof applySkin === "function") applySkin();
    });
  }
  big.addEventListener("change", () => {
    try { localStorage.setItem("eudora.bigButtons", JSON.stringify(big.checked)); } catch { /* (not kept) */ }
    document.documentElement.classList.toggle("big-buttons", big.checked);
    window.fitToolbar?.();
  });
  // (kept in step when the size changes by the keys)
  document.addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && ["=", "+", "-", "_", "0"].includes(e.key)) setTimeout(show, 0); });
  show();
})();

// BossWatch's spoken recipients, asked by a message window (compose.js): said here, with the
// speech chosen in Settings; and its choice in Settings ▸ General ▸ Speech
(() => {
  window.__TAURI__?.event?.listen("speak-request", (e) => { if (typeof e.payload === "string") window.speak?.(e.payload); });
  const when = document.getElementById("p-speak-recipients");
  const over = document.getElementById("p-speak-over");
  if (!when || !over) return;
  const read = (key, fallback) => { try { const v = localStorage.getItem(`eudora.${key}`); return v === null ? fallback : JSON.parse(v); } catch { return fallback; } };
  const write = (key, v) => { try { localStorage.setItem(`eudora.${key}`, JSON.stringify(v)); } catch { /* (not kept) */ } };
  when.value = read("speakRecipients", "off");
  over.value = String(read("speakRecipientsOver", 5));
  const show = () => { over.disabled = !["many", "replyAllOrMany"].includes(when.value); };
  show();
  when.addEventListener("change", () => { write("speakRecipients", when.value); show(); });
  over.addEventListener("change", () => write("speakRecipientsOver", Math.max(1, Number(over.value) || 5)));
})();

// Add account: "standard: 995" beside a port changed by hand puts the standard port back (a
// button for the keyboard and screen readers too)
(() => {
  for (const [note, box, typed] of [["a-port-default", "a-port", "incoming"], ["a-smtp-port-default", "a-smtp-port", "outgoing"]]) {
    const n = document.getElementById(note);
    if (!n) continue;
    const take = () => {
      const m = n.textContent.match(/standard: (\d+)/);
      if (!m) return;
      const b = document.getElementById(box);
      b.value = m[1];
      b.dispatchEvent(new Event("input", { bubbles: true }));
      // (back to following the security choice, as an untouched port does)
      if (typed === "incoming") portTouched = false; else smtpPortTouched = false;
      n.textContent = "standard";
      window.announce?.(`Port ${m[1]}, the standard one`);
    };
    n.addEventListener("click", take);
    n.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); take(); } });
    new MutationObserver(() => {
      const offStandard = /standard: \d+/.test(n.textContent);
      n.tabIndex = offStandard ? 0 : -1;
      n.setAttribute("role", offStandard ? "button" : "note");
      n.style.cursor = offStandard ? "pointer" : "";
      n.style.textDecoration = offStandard ? "underline" : "";
      n.title = offStandard ? "Use the standard port" : "";
    }).observe(n, { childList: true, characterData: true, subtree: true });
  }
})();
