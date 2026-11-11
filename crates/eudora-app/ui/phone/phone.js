// Eudora on a phone (iOS and Android): its own screens over the same core and commands as the
// desktop window. Choices: the bottom tab bar is theirs to set (Settings; starts as Mail,
// Calendar, To Do, More); swiping while reading moves between messages (left = next, right =
// previous) and list rows have no swipe actions, so nothing is deleted by a stray thumb; the
// first version does mail fully, To Do and Views, with calendar and contacts read-only.
"use strict";

const invoke = window.__TAURI__.core.invoke;

// ------------------------------------------------------------------ small helpers

const $ = (id) => document.getElementById(id);

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function icon(name, small) {
  const i = el("span", small ? "icon small" : "icon");
  i.style.setProperty("--src", `url("../icons/${window.ICON_SET || "free"}/${name}.svg")`);
  return i;
}

/** The phone's own choices (the tab bar…), kept on this phone only. */
const store = {
  get(key, fallback) {
    try {
      const v = localStorage.getItem(`eudora.phone.${key}`);
      return v == null ? fallback : JSON.parse(v);
    } catch {
      return fallback;
    }
  },
  set(key, value) {
    try { localStorage.setItem(`eudora.phone.${key}`, JSON.stringify(value)); } catch { /* private storage off: not kept */ }
  },
};

const number = new Intl.NumberFormat();
const UNREAD = 1;
const FLAGGED = 1 << 11;

/** "9:41 AM" today, "Yesterday", "Tuesday" this week, else "Oct 3" (or "Oct 3, 2019"). */
function when(seconds) {
  if (!seconds) return "";
  const d = new Date(seconds * 1000);
  const now = new Date();
  const day = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((day(now) - day(d)) / 86400000);
  if (days === 0) return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (days === 1) return "Yesterday";
  if (days > 1 && days < 7) return d.toLocaleDateString([], { weekday: "long" });
  return d.toLocaleDateString([], d.getFullYear() === now.getFullYear() ? { month: "short", day: "numeric" } : { year: "numeric", month: "short", day: "numeric" });
}

function longDate(seconds) {
  return new Date(seconds * 1000).toLocaleString([], { weekday: "short", month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit" });
}

function size(bytes) {
  if (bytes < 1024) return `${bytes} bytes`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** The name shown for a person ("Ann Smith <ann@x.com>" → "Ann Smith"). */
function nameOf(from) {
  const m = /^\s*"?([^"<]*?)"?\s*<([^>]+)>\s*$/.exec(from || "");
  if (m) return m[1] || m[2];
  return (from || "").trim() || "(no sender)";
}

/** A colored circle with someone's initial (the same person, the same color). */
function avatar(name) {
  const a = el("div", "avatar");
  const clean = (name || "?").replace(/[^\p{L}\p{N}]/gu, " ").trim();
  a.textContent = (clean[0] || "?").toUpperCase();
  let h = 0;
  for (const c of name || "") h = (h * 31 + c.charCodeAt(0)) >>> 0;
  a.style.background = `hsl(${h % 360}, 45%, 48%)`;
  return a;
}

/** A short message at the bottom, with an optional action ("Undo"). */
let toastTimer;
function toast(text, action) {
  const t = $("toast");
  t.replaceChildren(el("span", "text", text));
  if (action) {
    const b = el("button", "", action.label);
    b.addEventListener("click", () => { t.hidden = true; action.run(); });
    t.append(b);
  }
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, action ? 6000 : 3500);
}

function failed(what, err) {
  console.error(what, err);
  toast(`${what}: ${err}`);
}

// ------------------------------------------------------------------ the sheet (choices from the bottom)

/** Shows a sheet; `fill(box, close)` puts its content in. Returns a promise of what `close` got. */
function sheet(fill) {
  return new Promise((resolve) => {
    const box = $("sheet");
    const scrim = $("scrim");
    const close = (value) => {
      box.hidden = scrim.hidden = true;
      box.replaceChildren();
      resolve(value);
    };
    box.replaceChildren(el("div", "grip"));
    fill(box, close);
    scrim.onclick = () => close(undefined);
    box.hidden = scrim.hidden = false;
  });
}

/** A list of choices: [{label, sub, icon, value, danger, indent}]. Resolves to the value chosen. */
function choose(title, choices, opts = {}) {
  return sheet((box, close) => {
    if (title) box.append(el("h2", "", title));
    if (opts.text) box.append(el("p", "", opts.text));
    let list;
    if (opts.filter) {
      const f = el("div", "search filter");
      f.append(icon("magnifying-glass", true));
      const input = el("input");
      input.type = "search";
      input.placeholder = opts.filter;
      f.append(input);
      box.append(f);
      input.addEventListener("input", () => {
        const q = input.value.trim().toLowerCase();
        for (const b of list.children) b.hidden = q && !b.dataset.find.includes(q);
      });
    }
    list = el("div", "choices");
    for (const c of choices) {
      const b = el("button", `choice${c.danger ? " danger" : ""}${c.primary ? " primary" : ""}${c.indent ? ` indent${Math.min(c.indent, 3)}` : ""}`);
      if (c.icon) b.append(icon(c.icon));
      const label = el("span", "", c.label);
      if (c.sub) label.append(el("span", "sub", c.sub));
      b.append(label);
      b.dataset.find = `${c.label} ${c.sub || ""}`.toLowerCase();
      b.addEventListener("click", () => close(c.value));
      list.append(b);
    }
    const cancel = el("button", "choice primary", "Cancel");
    cancel.addEventListener("click", () => close(undefined));
    list.append(cancel);
    box.append(list);
  });
}

/** A yes / no question; the buttons say what each does. */
async function ask(title, text, yes, { danger } = {}) {
  return (await choose(title, [{ label: yes, value: true, danger, primary: !danger }], { text })) === true;
}

/** Asks for a line of text (null when canceled); `opts.password` hides what's typed, `opts.text`
 *  explains. */
function askText(title, placeholder, value = "", ok = "OK", opts = {}) {
  return sheet((box, close) => {
    box.append(el("h2", "", title));
    if (opts.text) box.append(el("p", "", opts.text));
    const form = el("form", "form");
    const input = el("input");
    if (opts.password) {
      input.type = "password";
      input.autocomplete = "new-password";
    }
    input.placeholder = placeholder;
    input.value = value;
    const go = el("button", "send-btn", ok);
    go.type = "submit";
    form.append(input, go);
    form.addEventListener("submit", (e) => { e.preventDefault(); close(input.value.trim() || null); });
    box.append(form);
    setTimeout(() => input.focus(), 50);
  });
}

// ------------------------------------------------------------------ tabs and screens

/** Every section a tab can show; More is always last and holds the rest. */
const SECTIONS = {
  mail: { title: "Mail", icon: "envelope", root: () => listScreen(INBOX, { root: true }) },
  calendar: { title: "Calendar", icon: "calendar-days", root: () => calendarScreen() },
  todo: { title: "To Do", icon: "list-check", root: () => todoScreen() },
  contacts: { title: "Contacts", icon: "address-book", root: () => contactsScreen() },
  views: { title: "Views", icon: "layer-group", root: () => viewsScreen() },
  search: { title: "Search", icon: "magnifying-glass", root: () => listScreen(INBOX, { root: true, searching: true, title: "Search" }) },
  mailboxes: { title: "Mailboxes", icon: "folder", root: () => mailboxesScreen({ root: true }) },
  more: { title: "More", icon: "ellipsis", root: () => moreScreen() },
};
const DEFAULT_TABS = ["mail", "calendar", "todo"];
const MAX_TABS = 4; // (plus More)

const state = {
  tab: null,
  stacks: new Map(),   // tab → { el, screens: [] }
  folders: [],         // every folder: {id, path, messages, account, account_id}
  folderById: new Map(),
  accounts: [],
};

function chosenTabs() {
  const tabs = store.get("tabs", DEFAULT_TABS).filter((t) => SECTIONS[t] && t !== "more");
  return [...(tabs.length ? tabs : DEFAULT_TABS).slice(0, MAX_TABS), "more"];
}

function drawTabbar() {
  const bar = $("tabbar");
  bar.replaceChildren();
  for (const key of chosenTabs()) {
    const s = SECTIONS[key];
    const b = el("button", key === state.tab ? "current" : "");
    b.dataset.tab = key;
    b.append(icon(s.icon), el("span", "", s.title));
    b.addEventListener("click", () => (key === state.tab ? popToRoot() : showTab(key)));
    bar.append(b);
  }
}

function showTab(key) {
  if (!chosenTabs().includes(key)) key = chosenTabs()[0];
  state.tab = key;
  store.set("tab", key);
  if (!state.stacks.has(key)) {
    const stackEl = el("div", "stack");
    $("tabs").append(stackEl);
    const stack = { el: stackEl, screens: [] };
    state.stacks.set(key, stack);
    const root = SECTIONS[key].root();
    stack.screens.push(root);
    stackEl.append(root.el);
    root.shown?.();
  }
  for (const [k, s] of state.stacks) s.el.classList.toggle("current", k === key);
  drawTabbar();
  const top = currentScreen();
  top?.shown?.();
}

function currentStack() {
  return state.stacks.get(state.tab);
}

function currentScreen() {
  const s = currentStack();
  return s?.screens[s.screens.length - 1];
}

/** Opens a screen over the current one (it slides in from the right). */
function push(screen) {
  const stack = currentStack();
  const below = stack.screens[stack.screens.length - 1];
  screen.el.classList.add("entering");
  stack.el.append(screen.el);
  stack.screens.push(screen);
  requestAnimationFrame(() => requestAnimationFrame(() => {
    screen.el.classList.remove("entering");
    below?.el.classList.add("covered");
  }));
  setTimeout(() => { if (below && stack.screens.includes(screen)) below.el.hidden = true; }, 300);
  screen.shown?.();
  document.body.classList.toggle("no-tabbar", !!screen.full);
}

/** Back to the screen below (it slides away to the right). */
function pop() {
  const stack = currentStack();
  if (stack.screens.length < 2) return;
  const top = stack.screens.pop();
  const below = stack.screens[stack.screens.length - 1];
  below.el.hidden = false;
  requestAnimationFrame(() => {
    below.el.classList.remove("covered");
    top.el.classList.add("leaving");
  });
  setTimeout(() => top.el.remove(), 300);
  top.closed?.();
  below.shown?.();
  document.body.classList.toggle("no-tabbar", !!below.full);
}

function popToRoot() {
  while (currentStack().screens.length > 1) pop();
}

/**
 * A screen: the bar at the top (back, title, buttons; a big title on a section's first screen),
 * then its scrolling body. Returns {el, top, body, ...} for the caller to fill.
 */
function screen({ title, big, back = true, left, right, search, grouped, full }) {
  const s = { el: el("section", `screen${grouped ? " grouped" : ""}`), full };
  const top = el("header", "top");
  const row = el("div", "top-row");
  const l = el("div", "side");
  if (left) l.append(left);
  else if (back) {
    const b = el("button", "bar-btn back");
    b.append(icon("chevron-left"), el("span", "", typeof back === "string" ? back : "Back"));
    b.addEventListener("click", pop);
    l.append(b);
  }
  const t = el("div", "title", big ? "" : title || "");
  const r = el("div", "side right");
  for (const x of [].concat(right || [])) r.append(x);
  row.append(l, t, r);
  top.append(row);
  if (big) {
    s.bigTitle = el("div", "big-title", title);
    top.append(s.bigTitle);
  }
  s.titleEl = t;
  if (search) {
    const f = el("form", "search");
    f.append(icon("magnifying-glass", true));
    const input = el("input");
    input.type = "search";
    input.placeholder = search;
    input.enterKeyHint = "search";
    input.autocomplete = "off";
    input.setAttribute("autocapitalize", "off");
    const cancel = el("button", "cancel", "Cancel");
    cancel.type = "button";
    cancel.hidden = true;
    f.append(input, cancel);
    top.append(f);
    s.searchForm = f;
    s.searchInput = input;
    s.searchCancel = cancel;
  }
  s.top = top;
  s.body = el("div", "body");
  s.el.append(top, s.body);
  return s;
}

function barButton(iconName, label, run) {
  const b = el("button", "bar-btn");
  if (iconName) b.append(icon(iconName));
  if (label) b.append(el("span", "", label));
  b.setAttribute("aria-label", label || iconName);
  b.addEventListener("click", run);
  return b;
}

/** A grouped list (Settings, More, Mailboxes): items [{label, sub, icon, value, run, red, indent, plain}]. */
function group(title, items, foot) {
  const g = el("div", "group");
  if (title) g.append(el("h3", "", title));
  const box = el("div", "items");
  for (const it of items) {
    const b = el(it.run ? "button" : "div", `item${it.red ? " red" : ""}${it.danger ? " danger" : ""}${it.indent ? ` indent${Math.min(it.indent, 3)}` : ""}${it.icon ? "" : " plain"}`);
    if (it.icon) b.append(icon(it.icon));
    const label = el("span", "label", it.label);
    if (it.sub) label.append(el("span", "sub", it.sub));
    b.append(label);
    if (it.value != null) b.append(el("span", "value", it.value));
    if (it.control) b.append(it.control);
    if (it.run) {
      if (it.chevron !== false) b.append(icon("chevron-right", true));
      b.lastChild.classList?.add("chev");
      b.addEventListener("click", it.run);
    }
    box.append(b);
  }
  g.append(box);
  if (foot) g.append(el("div", "foot", foot));
  return g;
}

function toggle(checked, onChange) {
  const label = el("label", "switch");
  const input = el("input");
  input.type = "checkbox";
  input.checked = checked;
  input.addEventListener("change", () => onChange(input.checked, input));
  label.append(input, el("span"));
  return label;
}

/** Back by swiping from the left edge, as phones do. */
function edgeBack(target) {
  let start = null;
  target.addEventListener("touchstart", (e) => {
    const t = e.touches[0];
    start = t.clientX < 24 && currentStack()?.screens.length > 1 ? { x: t.clientX, y: t.clientY } : null;
  }, { passive: true });
  target.addEventListener("touchend", (e) => {
    if (!start) return;
    const t = e.changedTouches[0];
    if (t.clientX - start.x > 80 && Math.abs(t.clientY - start.y) < 60) pop();
    start = null;
  }, { passive: true });
}

// ------------------------------------------------------------------ folders and accounts

async function loadFolders() {
  try {
    const [folders, accounts] = await Promise.all([invoke("folders", {}), invoke("accounts", { counts: false })]);
    state.folders = folders;
    state.folderById = new Map(folders.map((f) => [f.id, f]));
    state.accounts = accounts.accounts;
  } catch (err) {
    failed("Reading the mailboxes", err);
  }
}

/** A folder as shown: the Online Archive's folders join the mailbox's (as on the desktop). */
function shownPath(path) {
  return path.startsWith("Online Archive/") ? path.slice("Online Archive/".length) : path === "Online Archive" ? "" : path;
}

/** One account's folders as a tree, each shown path once (with every folder id under it). */
function folderTree(accountId) {
  const byPath = new Map();
  for (const f of state.folders.filter((x) => x.account_id === accountId)) {
    const p = shownPath(f.path);
    if (!p) continue;
    const entry = byPath.get(p) || { path: p, ids: [], messages: 0 };
    entry.ids.push(f.id);
    entry.messages += f.messages;
    byPath.set(p, entry);
  }
  const order = ["inbox", "drafts", "sent", "sent items", "out", "archive", "deleted items", "trash", "junk email", "spam"];
  const rank = (p) => { const i = order.indexOf(p.split("/")[0].toLowerCase()); return i < 0 ? order.length : i; };
  return [...byPath.values()].sort((a, b) => rank(a.path) - rank(b.path) || a.path.localeCompare(b.path, undefined, { sensitivity: "base" }));
}

function folderIcon(path) {
  const top = path.split("/")[0].toLowerCase();
  if (top === "inbox") return "inbox";
  if (top.startsWith("sent") || top === "out") return "paper-plane";
  if (top.startsWith("deleted") || top === "trash") return "trash-can";
  if (top.startsWith("junk") || top === "spam") return "ban";
  if (top === "archive") return "box-archive";
  if (top === "drafts") return "pen-to-square";
  return "folder";
}

// ------------------------------------------------------------------ lists of messages

/** What a list shows: `folders` (ids), or `filtered` (the Inbox across every account), `all`
 *  (All Mail without what the user sent), a Synthetic View (`view`), or a search (`query`). */
const INBOX = { title: "Inbox", filtered: true, folders: [] };

function listArgs(list) {
  return {
    folders: list.folders || [],
    hideSent: !!list.all,
    filtered: !!list.filtered,
    view: list.view || null,
    accounts: null,
  };
}

const PAGE = 50;

/**
 * A list of messages (the Inbox, a folder, All Mail, a View, search results). Tap opens one;
 * a long press starts choosing several (then the actions at the bottom); pull down checks for
 * new mail. No swipe actions on the rows (the user's choice).
 */
function listScreen(list, opts = {}) {
  const compose = barButton("pen-to-square", "", () => openCompose("new"));
  compose.setAttribute("aria-label", "New Message");
  let left;
  if (opts.root && !opts.searching) {
    left = el("button", "bar-btn back");
    left.append(icon("chevron-left"), el("span", "", "Mailboxes"));
    left.addEventListener("click", () => push(mailboxesScreen()));
  }
  const s = screen({ title: opts.title || list.title, big: true, back: !opts.root, left, right: compose, search: "Search all mail" });
  s.list = list;
  s.rows = [];
  s.page = 0;
  s.done = false;
  s.selected = new Set();
  s.status = el("div", "small");
  s.status.style.cssText = "padding: 0 16px 6px";
  s.top.append(s.status);

  const pull = el("div", "pull");
  pull.append(icon("arrows-rotate"));
  const ul = el("ul", "rows");
  const more = el("div", "more-rows");
  s.body.append(pull, ul, more);
  s.ul = ul;

  // ---- loading
  async function load(reset) {
    if (s.loading) return;
    s.loading = true;
    try {
      if (reset) {
        s.page = 0;
        s.done = false;
      }
      let rows;
      if (s.query) {
        const found = await invoke("search", { query: s.query, limit: 300, order: "newest" });
        rows = found.rows;
        s.done = true;
        s.total = found.total;
        const notes = [...(found.notes || []), ...(found.suggestions || []).map((x) => `Try: ${x.query}`)];
        s.status.textContent = `${number.format(found.total)} found${found.total > rows.length ? ` · the newest ${rows.length} shown` : ""}${notes.length ? ` · ${notes.join(" · ")}` : ""}`;
      } else {
        rows = await invoke("page", { ...listArgs(list), page: s.page, size: PAGE });
        if (rows.length < PAGE) s.done = true;
        s.page++;
      }
      if (reset) {
        s.rows = rows;
        ul.replaceChildren();
      } else {
        s.rows.push(...rows);
      }
      for (const r of rows) ul.append(rowEl(r));
      more.textContent = s.rows.length === 0 ? "" : s.done ? (s.query ? "" : `${number.format(s.rows.length)} messages`) : "Loading more…";
      if (s.rows.length === 0) {
        ul.append(Object.assign(el("div", "note"), { innerHTML: "" }));
        const n = ul.lastChild;
        n.append(el("strong", "", s.query ? "No results" : "No messages"));
        n.append(document.createTextNode(s.query ? "Try other words, or from:, subject:, has:attachment." : "Pull down to check for new mail."));
      }
      if (!s.query) countMessages();
    } catch (err) {
      failed("Reading the messages", err);
    } finally {
      s.loading = false;
    }
  }
  s.load = load;

  async function countMessages() {
    try {
      const n = await invoke("count", listArgs(list));
      s.total = n;
      if (!s.query && !s.selecting) {
        const big = s.bigTitle;
        big.replaceChildren(document.createTextNode(opts.title || list.title));
        if (n) big.append(el("span", "count", number.format(n)));
      }
    } catch { /* the number can wait */ }
  }

  // ---- a row
  function rowEl(r) {
    const li = el("li", `row${r.flags & UNREAD ? " unread" : ""}`);
    li.dataset.id = r.id;
    if (r.flags & UNREAD) li.append(el("span", "dot"));
    const check = el("div", "check");
    check.append(icon("check", true));
    const main = el("div", "main");
    const line1 = el("div", "line1");
    const sentList = /^(sent|out|drafts)/i.test(list.title || "");
    line1.append(el("span", "who", sentList ? `To: ${nameOf(r.to.split(",")[0])}` : nameOf(r.from)));
    const whenEl = el("span", "when");
    const marks = el("span", "marks");
    if (r.flags & FLAGGED) { const f = icon("flag", true); f.classList.add("flag"); marks.append(f); }
    if (r.attachments) marks.append(icon("paperclip", true));
    whenEl.append(marks, document.createTextNode(when(r.date)));
    line1.append(whenEl);
    main.append(line1, el("div", "subject", r.subject || "(no subject)"), el("div", "preview", r.snippet || ""));
    li.append(check, main);
    return li;
  }

  function rowOf(id) {
    return s.rows.find((r) => r.id === id);
  }

  function redraw(id) {
    const r = rowOf(id);
    const old = ul.querySelector(`[data-id="${id}"]`);
    if (r && old) {
      const fresh = rowEl(r);
      fresh.classList.toggle("selected", s.selected.has(id));
      old.replaceWith(fresh);
    }
  }
  s.redraw = redraw;
  s.rowEl = rowEl;

  // ---- tapping, long-pressing
  let press = null;
  ul.addEventListener("touchstart", (e) => {
    const li = e.target.closest(".row");
    if (!li) return;
    const t = e.touches[0];
    press = { li, x: t.clientX, y: t.clientY, long: false };
    press.timer = setTimeout(() => {
      press.long = true;
      li.classList.remove("pressed");
      navigator.vibrate?.(12);
      startSelecting(Number(li.dataset.id));
    }, 450);
    li.classList.add("pressed");
  }, { passive: true });
  ul.addEventListener("touchmove", (e) => {
    if (!press) return;
    const t = e.touches[0];
    if (Math.abs(t.clientX - press.x) > 10 || Math.abs(t.clientY - press.y) > 10) {
      clearTimeout(press.timer);
      press.li.classList.remove("pressed");
    }
  }, { passive: true });
  ul.addEventListener("touchend", () => {
    if (!press) return;
    clearTimeout(press.timer);
    press.li.classList.remove("pressed");
    setTimeout(() => { press = null; }, 0);
  }, { passive: true });
  ul.addEventListener("click", (e) => {
    const li = e.target.closest(".row");
    if (!li || press?.long) return;
    const id = Number(li.dataset.id);
    if (s.selecting) {
      toggleSelected(id);
      return;
    }
    const index = s.rows.findIndex((r) => r.id === id);
    push(readerScreen(s, index));
  });
  // (a long press with a mouse, in the Simulator or a desktop browser)
  ul.addEventListener("contextmenu", (e) => {
    const li = e.target.closest(".row");
    if (!li) return;
    e.preventDefault();
    startSelecting(Number(li.dataset.id));
  });

  // ---- choosing several
  const actions = el("nav", "actions");
  actions.hidden = true;
  s.el.append(actions);

  function startSelecting(id) {
    s.selecting = true;
    s.el.classList.add("selecting");
    s.selected = new Set([id]);
    ul.querySelector(`[data-id="${id}"]`)?.classList.add("selected");
    drawActions();
    actions.hidden = false;
    document.body.classList.add("no-tabbar");
    const done = barButton(null, "Done", stopSelecting);
    const all = barButton(null, "Select All", () => {
      for (const r of s.rows) s.selected.add(r.id);
      for (const li of ul.querySelectorAll(".row")) li.classList.add("selected");
      drawActions();
    });
    s.saveBar = [...s.top.querySelector(".top-row").children].map((c) => [...c.childNodes]);
    const [l, , r] = s.top.querySelector(".top-row").children;
    l.replaceChildren(all);
    r.replaceChildren(done);
  }

  function stopSelecting() {
    if (!s.selecting) return;
    s.selecting = false;
    s.el.classList.remove("selecting");
    s.selected.clear();
    for (const li of ul.querySelectorAll(".row.selected")) li.classList.remove("selected");
    actions.hidden = true;
    document.body.classList.remove("no-tabbar");
    const [l, t, r] = s.top.querySelector(".top-row").children;
    l.replaceChildren(...s.saveBar[0]);
    t.replaceChildren(...s.saveBar[1]);
    r.replaceChildren(...s.saveBar[2]);
    countMessages();
  }
  s.stopSelecting = stopSelecting;

  function toggleSelected(id) {
    if (s.selected.has(id)) s.selected.delete(id);
    else s.selected.add(id);
    ul.querySelector(`[data-id="${id}"]`)?.classList.toggle("selected", s.selected.has(id));
    if (!s.selected.size) stopSelecting();
    else drawActions();
  }

  function drawActions() {
    const n = s.selected.size;
    s.top.querySelector(".title").textContent = `${n} selected`;
    const chosen = [...s.selected].map(rowOf).filter(Boolean);
    const anyUnread = chosen.some((r) => r.flags & UNREAD);
    const allFlagged = chosen.length && chosen.every((r) => r.flags & FLAGGED);
    const button = (name, label, run, cls) => {
      const b = el("button", cls || "");
      b.append(icon(name), el("span", "", label));
      b.addEventListener("click", run);
      return b;
    };
    actions.replaceChildren(
      button(anyUnread ? "envelope-open" : "envelope", anyUnread ? "Read" : "Unread", () => markRead(s, chosen, anyUnread)),
      button("flag", allFlagged ? "Unflag" : "Flag", () => flag(s, chosen, !allFlagged)),
      button("folder-open", "Move", () => moveTo(s, chosen)),
      button("ban", "Spam", () => spam(s, chosen, true)),
      button("trash-can", "Delete", () => remove(s, chosen), "danger"),
    );
  }

  // ---- scrolling for more, pulling for new mail
  s.body.addEventListener("scroll", () => {
    if (!s.done && s.body.scrollTop + s.body.clientHeight > s.body.scrollHeight - 600) load(false);
  }, { passive: true });
  pullToRefresh(s.body, pull, async () => {
    try { await invoke("check_mail"); } catch (err) { failed("Checking mail", err); }
    await loadFolders();
    await load(true);
    showStatus(s);
  });

  // ---- searching (the same words as the desktop: from:, subject:, has:attachment, received:…)
  s.searchForm.addEventListener("submit", (e) => {
    e.preventDefault();
    s.searchInput.blur();
    const q = s.searchInput.value.trim();
    s.query = q || null;
    s.bigTitle.textContent = q ? "Results" : opts.title || list.title;
    load(true);
  });
  s.searchInput.addEventListener("focus", () => { s.searchCancel.hidden = false; });
  s.searchCancel.addEventListener("click", () => {
    s.searchInput.value = "";
    s.searchCancel.hidden = true;
    s.searchInput.blur();
    if (s.query) {
      s.query = null;
      s.bigTitle.textContent = opts.title || list.title;
      load(true);
    }
    s.status.textContent = "";
    showStatus(s);
  });

  // ---- shown / hidden: new mail looked for every 30 s while it's in front
  let timer;
  s.shown = () => {
    if (!s.rows.length && !s.loading) load(true);
    else if (!s.query) refreshQuietly(s);
    showStatus(s);
    clearInterval(timer);
    timer = setInterval(() => { if (currentScreen() === s && !s.query && !s.selecting) refreshQuietly(s); }, 30000);
    if (opts.searching) setTimeout(() => s.searchInput.focus(), 350);
  };
  s.closed = () => clearInterval(timer);
  return s;
}

/** New mail at the top without moving what's on screen: the first page read again, rows not
 *  shown yet put in above, and marks changed elsewhere (read on another device) drawn again. */
async function refreshQuietly(s) {
  try {
    const rows = await invoke("page", { ...listArgs(s.list), page: 0, size: PAGE });
    const known = new Set(s.rows.map((r) => r.id));
    for (const r of rows) {
      const old = s.rows.find((x) => x.id === r.id);
      if (old && old.flags !== r.flags) { old.flags = r.flags; s.redraw(r.id); }
    }
    const fresh = rows.filter((r) => !known.has(r.id));
    if (!fresh.length) return;
    s.ul.querySelector(".note")?.remove();
    const first = s.ul.firstChild;
    for (const r of fresh) s.ul.insertBefore(s.rowEl(r), first);
    s.rows.unshift(...fresh);
  } catch { /* quietly: the next look tries again */ }
}

/** The line under the title: how the download stands ("Updated 9:41 AM", "Downloading 120 of 3,000"). */
async function showStatus(s) {
  if (s.query) return;
  try {
    const d = await invoke("download_status");
    if (!d) { s.status.textContent = ""; return; }
    const st = d.status || d;
    // (the download speaks of the desktop's Preferences; on the phone that's More ▸ Accounts)
    const line = (st.line || "").replace("Preferences ▸ Accounts", "More ▸ Accounts");
    s.status.textContent = line.length > 90 ? `${line.slice(0, 88)}…` : line;
  } catch { s.status.textContent = ""; }
}

/** Pull down at the top of a list: `run` when let go far enough. */
function pullToRefresh(body, pull, run) {
  let startY = null;
  let busy = false;
  body.addEventListener("touchstart", (e) => {
    startY = body.scrollTop <= 0 && !busy ? e.touches[0].clientY : null;
  }, { passive: true });
  body.addEventListener("touchmove", (e) => {
    if (startY == null) return;
    const dy = e.touches[0].clientY - startY;
    if (dy <= 0) { pull.style.height = "0px"; return; }
    const h = Math.min(dy * 0.5, 80);
    pull.style.transition = "none";
    pull.style.height = `${h}px`;
    pull.classList.toggle("ready", h >= 60);
  }, { passive: true });
  body.addEventListener("touchend", async () => {
    if (startY == null) return;
    startY = null;
    pull.style.transition = "";
    if (!pull.classList.contains("ready")) { pull.style.height = "0px"; return; }
    pull.classList.remove("ready");
    pull.classList.add("busy");
    pull.style.height = "48px";
    busy = true;
    try { await run(); } finally {
      busy = false;
      pull.classList.remove("busy");
      pull.style.height = "0px";
    }
  }, { passive: true });
}

// ------------------------------------------------------------------ acting on messages

/** The folder a message is acted on in: the list's own folder, else the first it's in. */
function fromFolder(s, r) {
  if (s?.list?.folders?.length) {
    const own = (r.labels || []).find(([f]) => s.list.folders.includes(f));
    return own ? own[0] : s.list.folders[0];
  }
  return r.labels?.[0]?.[0] ?? r.folder_ids?.[0];
}

/** Messages grouped by the folder each is acted on in. */
function byFolder(s, rows) {
  const groups = new Map();
  for (const r of rows) {
    const f = fromFolder(s, r);
    if (f == null) continue;
    if (!groups.has(f)) groups.set(f, []);
    groups.get(f).push(r.id);
  }
  return groups;
}

/** Takes rows out of a list on screen (moved, deleted). */
function dropRows(s, ids) {
  if (!s?.rows) return;
  const gone = new Set(ids);
  s.rows = s.rows.filter((r) => !gone.has(r.id));
  for (const id of ids) s.ul?.querySelector(`[data-id="${id}"]`)?.remove();
}

async function markRead(s, rows, read) {
  const ids = rows.map((r) => r.id);
  try {
    await invoke("set_status", { ids, status: read ? "read" : "unread" });
    for (const r of rows) r.flags = read ? r.flags & ~UNREAD : r.flags | UNREAD;
    for (const r of rows) s?.redraw?.(r.id);
  } catch (err) { return failed("Marking", err); }
  s?.stopSelecting?.();
}

async function flag(s, rows, on) {
  try {
    await invoke("todo_flag", { ids: rows.map((r) => r.id), on });
    for (const r of rows) r.flags = on ? r.flags | FLAGGED : r.flags & ~FLAGGED;
    for (const r of rows) s?.redraw?.(r.id);
    toast(on ? (rows.length > 1 ? `${rows.length} added to To Do` : "Added to To Do") : "Taken off To Do");
  } catch (err) { return failed("Flagging", err); }
  s?.stopSelecting?.();
}

async function remove(s, rows) {
  const groups = byFolder(s, rows);
  const done = [];
  try {
    for (const [from, ids] of groups) {
      const trash = await invoke("delete_messages", { ids, from });
      done.push({ ids, from, trash });
    }
  } catch (err) { return failed("Deleting", err); }
  dropRows(s, rows.map((r) => r.id));
  s?.stopSelecting?.();
  toast(rows.length > 1 ? `${rows.length} deleted` : "Deleted", {
    label: "Undo",
    run: async () => {
      try {
        for (const d of done) await invoke("move_messages", { ids: d.ids, from: d.trash, to: d.from });
        s?.load?.(true);
      } catch (err) { failed("Undoing", err); }
    },
  });
  return true;
}

async function spam(s, rows, isSpam) {
  const items = [];
  for (const r of rows) {
    const f = fromFolder(s, r);
    if (f != null) items.push([r.id, f]);
  }
  try {
    const [notes] = await invoke("mark_spam", { items, spam: isSpam });
    dropRows(s, rows.map((r) => r.id));
    s?.stopSelecting?.();
    toast(isSpam ? (rows.length > 1 ? `${rows.length} marked as spam` : "Marked as spam: mail like it goes to Spam") : (notes?.[0] || "Not spam: back in the Inbox"));
    return true;
  } catch (err) { failed(isSpam ? "Marking as spam" : "Marking as not spam", err); }
  return false;
}

/** Move: a sheet of the account's folders, then moved (Undo in the toast). */
async function moveTo(s, rows) {
  const groups = byFolder(s, rows);
  const accounts = new Set([...groups.keys()].map((f) => state.folderById.get(f)?.account_id));
  if (accounts.size !== 1) return toast("Choose messages from one account to move them together");
  const account = [...accounts][0];
  if (!state.folders.length) await loadFolders();
  const tree = folderTree(account);
  const choices = tree.map((f) => ({ label: f.path.split("/").pop(), sub: f.path.includes("/") ? f.path : null, icon: folderIcon(f.path), value: f, indent: f.path.split("/").length - 1 }));
  const to = await choose("Move to", choices, { filter: "Find a folder" });
  if (!to) return false;
  const target = to.ids.find((id) => !state.folderById.get(id)?.path.startsWith("Online Archive")) ?? to.ids[0];
  const done = [];
  try {
    for (const [from, ids] of groups) {
      if (from === target) continue;
      await invoke("move_messages", { ids, from, to: target });
      done.push({ ids, from });
    }
  } catch (err) { return failed("Moving", err); }
  dropRows(s, rows.map((r) => r.id));
  s?.stopSelecting?.();
  toast(`Moved to ${to.path.split("/").pop()}`, {
    label: "Undo",
    run: async () => {
      try {
        for (const d of done) await invoke("move_messages", { ids: d.ids, from: target, to: d.from });
        s?.load?.(true);
      } catch (err) { failed("Undoing", err); }
    },
  });
  return true;
}

// ------------------------------------------------------------------ reading a message

/**
 * One message, from a list: swipe left for the next, right for the previous (the user's
 * choice), the actions at the bottom. The message's own HTML is shown in a sandbox with no
 * scripts and nothing loaded from the network, as on the desktop.
 */
function readerScreen(list, index) {
  const up = barButton("chevron-up", "", () => step(-1));
  const down = barButton("chevron-down", "", () => step(1));
  up.setAttribute("aria-label", "Previous message");
  down.setAttribute("aria-label", "Next message");
  const s = screen({ title: "", back: list.list?.title || "Back", right: [up, down] });
  const hintL = el("div", "swipe-hint left");
  hintL.append(icon("chevron-left"));
  const hintR = el("div", "swipe-hint right");
  hintR.append(icon("chevron-right"));
  s.el.append(hintL, hintR);
  const actions = el("nav", "actions");
  s.el.append(actions);
  s.full = true; // (the actions take the tab bar's place)
  let row = list.rows[index];
  let message = null;

  function step(by) {
    const next = index + by;
    if (next < 0 || next >= list.rows.length) {
      toast(by > 0 ? "That's the last message here" : "That's the first message here");
      return;
    }
    index = next;
    row = list.rows[index];
    const dir = by > 0 ? -1 : 1;
    s.body.style.transition = "transform 0.18s ease-in, opacity 0.18s";
    s.body.style.transform = `translateX(${dir * 40}%)`;
    s.body.style.opacity = "0";
    setTimeout(() => {
      s.body.style.transition = "none";
      s.body.style.transform = `translateX(${-dir * 40}%)`;
      show();
      requestAnimationFrame(() => {
        s.body.style.transition = "transform 0.2s ease-out, opacity 0.2s";
        s.body.style.transform = "";
        s.body.style.opacity = "";
      });
    }, 170);
  }

  // swiping (also inside the message's own frame, which keeps its touches to itself). The
  // screen only scrolls up and down (touch-action: pan-y), so a sideways move stays the page's;
  // a touch the system cancels is decided from where the finger last was.
  function swipeable(target, styled) {
    styled.style.touchAction = "pan-y";
    let start = null;
    let last = null;
    const finish = () => {
      hintL.style.opacity = hintR.style.opacity = 0;
      if (!start || !last) { start = null; return; }
      const dx = last.x - start.x;
      const dy = last.y - start.y;
      start = last = null;
      if (Math.abs(dx) > 80 && Math.abs(dx) > Math.abs(dy) * 1.8) step(dx < 0 ? 1 : -1);
    };
    target.addEventListener("touchstart", (e) => {
      const t = e.touches[0];
      start = t.clientX > 24 && e.touches.length === 1 ? { x: t.clientX, y: t.clientY } : null;
      last = start;
    }, { passive: true });
    target.addEventListener("touchmove", (e) => {
      if (!start) return;
      const t = e.touches[0];
      last = { x: t.clientX, y: t.clientY };
      const dx = last.x - start.x;
      const dy = last.y - start.y;
      const sideways = Math.abs(dx) > 30 && Math.abs(dx) > Math.abs(dy) * 1.5;
      hintL.style.opacity = sideways && dx > 0 ? Math.min(dx / 120, 1) : 0;
      hintR.style.opacity = sideways && dx < 0 ? Math.min(-dx / 120, 1) : 0;
    }, { passive: true });
    target.addEventListener("touchend", (e) => {
      const t = e.changedTouches[0];
      if (start) last = { x: t.clientX, y: t.clientY };
      finish();
    }, { passive: true });
    target.addEventListener("touchcancel", finish, { passive: true });
  }
  swipeable(s.body, s.body);

  async function show() {
    s.body.scrollTop = 0;
    const box = el("div", "reader");
    s.body.replaceChildren(box);
    up.disabled = index <= 0;
    down.disabled = index >= list.rows.length - 1;
    drawActions();
    try {
      message = await invoke("message", { id: row.id });
    } catch (err) {
      box.append(el("div", "note", `This message couldn't be read: ${err}`));
      return;
    }
    if (row.id !== message.id) return; // (moved on meanwhile)
    if (!row.labels?.length) row.labels = (message.folder_ids || []).map((f) => [f, ""]);
    if (message.warning) box.append(el("div", "warning", message.warning));
    box.append(el("h1", "subject", message.subject || "(no subject)"));
    const who = el("div", "who");
    const from = message.from[0] || { name: "", address: "" };
    who.append(avatar(from.name || from.address));
    const names = el("div", "names");
    names.append(el("div", "from", from.name || from.address));
    if (from.name) names.append(el("div", "addr", from.address));
    const to = message.to.map((p) => p.name || p.address).join(", ");
    if (to) names.append(el("div", "to", `To: ${to}${message.cc.length ? `, Cc: ${message.cc.map((p) => p.name || p.address).join(", ")}` : ""}`));
    who.append(names, el("div", "date", when(message.date)));
    who.title = longDate(message.date);
    box.append(who);
    const labels = (message.folders || []).map(shownPath).filter(Boolean);
    if (labels.length) {
      const l = el("div", "labels");
      for (const f of labels.slice(0, 4)) l.append(el("span", "", f));
      box.append(l);
    }
    const files = (message.attachments || []).filter((a) => !a.inline);
    if (files.length) {
      const a = el("div", "attachments");
      for (const f of files) {
        const chip = el("button", "file");
        chip.append(icon("paperclip", true), el("span", "", f.name || "attachment"), el("small", "small", size(f.size)));
        // (iOS: Apple's Quick Look viewer, with its Share button; Android: the phone's own app for it)
        chip.addEventListener("click", async () => {
          const how = await choose(f.name || "Attachment", [
            { label: "Open", sub: "Look at it here", value: "open", primary: true },
            { label: "Share or Save…", sub: "Save to Files, open in another app, AirDrop, send it on", value: "share" },
          ]);
          if (!how) return;
          invoke(how === "share" ? "attachment_share" : "attachment_open", { id: row.id, part: f.part })
            .catch((err) => failed(how === "share" ? "Sharing the attachment" : "Opening the attachment", err));
        });
        a.append(chip);
      }
      box.append(a);
    }
    if (message.html) {
      const frame = el("iframe", "mail-frame");
      frame.setAttribute("sandbox", "allow-same-origin");
      frame.srcdoc = sandboxed(message.html);
      frame.addEventListener("load", () => fitFrame(frame));
      box.append(frame);
    } else {
      box.append(el("div", "mail-text", message.text || ""));
    }
    // read after a moment (as on the desktop)
    if (row.flags & UNREAD) {
      const id = row.id;
      setTimeout(() => {
        if (row.id === id && row.flags & UNREAD) markRead(list, [row], true).then(drawActions);
      }, 1000);
    }
  }

  /** The frame as tall as its message; a message wider than the phone is zoomed out to fit
   *  (newsletters written for 600-pixel screens), and links open in the browser. */
  function fitFrame(frame) {
    const doc = frame.contentDocument;
    if (!doc) return;
    swipeable(doc, doc.documentElement);
    const fit = () => {
      const width = frame.clientWidth;
      const wide = doc.documentElement.scrollWidth;
      if (wide > width + 4) doc.body.style.zoom = String(width / wide);
      frame.style.height = `${Math.ceil(doc.documentElement.scrollHeight * (Number(doc.body.style.zoom) || 1)) + 4}px`;
    };
    fit();
    for (const img of doc.images) img.addEventListener("load", fit);
    doc.addEventListener("click", (e) => {
      const a = e.target.closest?.("a[href]");
      if (!a) return;
      e.preventDefault();
      openMessageLink(a.getAttribute("href") || "");
    });
  }

  function drawActions() {
    const b = (name, label, run, cls) => {
      const x = el("button", cls || "");
      x.append(icon(name), el("span", "", label));
      x.addEventListener("click", run);
      return x;
    };
    const flagged = row.flags & FLAGGED;
    actions.replaceChildren(
      b("trash-can", "Delete", async () => { if (await remove(list, [row])) afterGone(); }, "danger"),
      b("folder-open", "Move", async () => { if (await moveTo(list, [row])) afterGone(); }),
      b("flag", flagged ? "Unflag" : "To Do", async () => { await flag(list, [row], !flagged); drawActions(); }),
      b("reply", "Reply", replyChoices),
      b("ellipsis", "More", moreChoices),
    );
  }

  /** After a delete or move: the next message, else back to the list. */
  function afterGone() {
    if (!list.rows.length) return pop();
    index = Math.min(index, list.rows.length - 1);
    row = list.rows[index];
    show();
  }

  async function replyChoices() {
    const mode = await choose(null, [
      { label: "Reply", icon: "reply", value: "reply" },
      { label: "Reply All", icon: "reply-all", value: "replyAll" },
      { label: "Forward", icon: "share", value: "forward" },
    ]);
    if (mode) openCompose(mode, row.id);
  }

  async function moreChoices() {
    const unread = row.flags & UNREAD;
    const inSpam = (message?.folders || []).some((f) => /^(spam|junk)/i.test(shownPath(f)));
    const what = await choose(null, [
      { label: unread ? "Mark as Read" : "Mark as Unread", icon: unread ? "envelope-open" : "envelope", value: "read" },
      inSpam ? { label: "Not Spam", icon: "circle-check", value: "notspam" } : { label: "Spam", icon: "ban", value: "spam" },
      { label: "Show the original text", icon: "file-lines", value: "text" },
    ]);
    if (what === "read") { await markRead(list, [row], !!unread); drawActions(); }
    if (what === "spam" || what === "notspam") { if (await spam(list, [row], what === "spam")) afterGone(); }
    if (what === "text") {
      const t = screen({ title: "Original text" });
      t.full = true;
      const pre = el("div", "mail-text reader", message?.text || "");
      t.body.append(pre);
      push(t);
    }
  }

  s.shown = () => { if (!message) show(); };
  return s;
}

/** HTML mail in a sandbox: no scripts, nothing from the network (pictures from the web stay
 *  blank), as the desktop window shows it. A light page even in dark mode: email is written
 *  for one. */
function sandboxed(html) {
  const policy = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:";
  const head = `<meta http-equiv="Content-Security-Policy" content="${policy}"><meta name="viewport" content="width=device-width">` +
    `<style>html{background:#fff}body{font:16px/1.45 -apple-system,system-ui,Roboto,sans-serif;margin:0;padding:12px;color:#1d1d1f;background:#fff;word-wrap:break-word;border-radius:10px}img{max-width:100%;height:auto}table{max-width:100%}</style>`;
  return `<!doctype html><html><head>${head}</head><body>${html}</body></html>`;
}

// ------------------------------------------------------------------ writing

/**
 * Writing a message, full screen: New, Reply, Reply All or Forward. Sent through the Out
 * mailbox as on the desktop (it goes at once, in the background).
 */
async function openCompose(mode, id = null, preset = {}) {
  let start;
  try {
    start = await invoke("compose_seed", { mode, id, ids: null });
  } catch (err) {
    return failed("Starting the message", err);
  }
  const seed = start.seed;
  const send = el("button", "send-btn", "Send");
  const cancel = barButton(null, "Cancel", () => close());
  const titles = { new: "New Message", reply: "Reply", replyAll: "Reply All", forward: "Forward" };
  const s = screen({ title: titles[mode] || "Message", left: cancel, right: send });
  s.full = true;
  const form = el("div", "compose");
  s.body.append(form);

  const field = (label, input, extra) => {
    const f = el("div", "field");
    f.append(el("label", "", label), input);
    if (extra) f.append(extra);
    form.append(f);
    return f;
  };
  const text = (value, kind) => {
    const i = el("input");
    i.type = "text";
    if (kind === "email") i.inputMode = "email";
    i.value = value || "";
    i.autocomplete = "off";
    i.setAttribute("autocapitalize", "off");
    i.spellcheck = false;
    return i;
  };
  const to = text(preset.to || seed.to, "email");
  const ccToggle = el("button", "expand", "Cc/Bcc");
  ccToggle.type = "button";
  field("To:", to, ccToggle);
  const cc = text(seed.cc, "email");
  const bcc = text("", "email");
  const ccField = field("Cc:", cc);
  const bccField = field("Bcc:", bcc);
  ccField.hidden = bccField.hidden = !seed.cc;
  ccToggle.hidden = !!seed.cc;
  ccToggle.addEventListener("click", () => { ccField.hidden = bccField.hidden = false; ccToggle.hidden = true; cc.focus(); });

  const from = el("select");
  const senders = start.identities.filter((i) => i.can_send);
  for (const i of senders) {
    const o = el("option", "", i.address);
    o.value = i.address;
    o.dataset.name = i.name;
    from.append(o);
  }
  if (seed.to_me && senders.some((i) => i.address.toLowerCase() === seed.to_me.toLowerCase())) from.value = senders.find((i) => i.address.toLowerCase() === seed.to_me.toLowerCase()).address;
  const fromField = field("From:", from);
  fromField.hidden = senders.length < 2;
  const subject = text(seed.subject);
  subject.setAttribute("autocapitalize", "sentences");
  subject.spellcheck = true;
  field("Subject:", subject);

  const editor = el("div", "editor");
  editor.contentEditable = "true";
  editor.innerHTML = `<div><br></div>${seed.html || ""}`;
  form.append(editor);
  if (seed.forwarded?.length) {
    const a = el("div", "attachments");
    a.style.padding = "0 16px 16px";
    for (const f of seed.forwarded) {
      const chip = el("span", "file");
      chip.append(icon("paperclip", true), el("span", "", f.name), el("small", "small", size(f.size)));
      a.append(chip);
    }
    form.append(a);
  }

  // files attached (the phone's own picker: Photos, Files, the camera), each read in whole
  const attached = [];
  const files = el("div", "attachments");
  files.style.padding = "0 16px 16px";
  const picker = el("input");
  picker.type = "file";
  picker.multiple = true;
  picker.hidden = true;
  const attach = el("button", "file add-file");
  attach.type = "button";
  attach.append(icon("paperclip", true), el("span", "", "Attach"));
  attach.addEventListener("click", () => picker.click());
  const drawFiles = () => {
    files.replaceChildren(attach);
    attached.forEach((f, n) => {
      const chip = el("span", "file");
      chip.append(icon("paperclip", true), el("span", "", f.name), el("small", "small", size(Math.floor((f.data.length * 3) / 4))));
      const remove = el("button", "remove", "×");
      remove.type = "button";
      remove.setAttribute("aria-label", `Remove ${f.name}`);
      remove.addEventListener("click", () => { attached.splice(n, 1); dirty = true; drawFiles(); });
      chip.append(remove);
      files.insertBefore(chip, attach);
    });
  };
  picker.addEventListener("change", async () => {
    for (const file of picker.files) {
      try {
        const data = await new Promise((resolve, reject) => {
          const reader = new FileReader();
          reader.onload = () => resolve(String(reader.result).split(",")[1] || "");
          reader.onerror = () => reject(reader.error);
          reader.readAsDataURL(file);
        });
        attached.push({ name: file.name || "attachment", mime: file.type || "application/octet-stream", data });
        dirty = true;
      } catch (err) { failed(`Not attached: ${file.name}`, err); }
    }
    picker.value = "";
    drawFiles();
  });
  drawFiles();
  form.append(files, picker);

  // address completion: people from the mail and the address book
  for (const input of [to, cc, bcc]) completion(input);

  let dirty = false;
  form.addEventListener("input", () => { dirty = true; });

  function draft() {
    const o = from.selectedOptions[0];
    return {
      from: o?.value || "",
      from_name: o?.dataset.name || "",
      to: to.value,
      cc: cc.value,
      bcc: bcc.value,
      subject: subject.value,
      html: editor.innerHTML,
      text: editor.innerText,
      plain: false,
      in_reply_to: seed.in_reply_to,
      references: seed.references,
      attachments: attached,
      forwarded: seed.forwarded || [],
      priority: null,
      sender: null,
      origin: id != null && mode !== "new" ? { message: id, kind: mode === "forward" ? "forward" : "reply" } : null,
      redirect_of: null,
      calendar: null,
    };
  }

  send.addEventListener("click", async () => {
    if (!to.value.trim() && !cc.value.trim() && !bcc.value.trim()) return toast("Add someone to send it to");
    if (!senders.length) return toast("No account can send yet: add one in More ▸ Accounts");
    send.disabled = true;
    try {
      await invoke("send_message", { draft: draft(), draftId: null });
      dirty = false;
      pop();
      toast("Sending…");
    } catch (err) {
      send.disabled = false;
      failed("Not sent", err);
    }
  });

  async function close() {
    if (dirty) {
      const what = await choose(null, [
        { label: "Save as Draft", icon: "file-lines", value: "save" },
        { label: "Delete", icon: "trash-can", value: "delete", danger: true },
      ]);
      if (!what) return;
      if (what === "save") {
        try { await invoke("save_draft", { id: null, draft: draft() }); toast("Saved in Drafts"); } catch (err) { return failed("Saving the draft", err); }
      }
    }
    pop();
  }

  push(s);
  setTimeout(() => (to.value ? editor : to).focus(), 350);
}

/** Suggestions under an address field as you type (after two letters). */
function completion(input) {
  const box = el("div", "suggestions");
  box.hidden = true;
  input.parentElement.append(box);
  let asked = 0;
  input.addEventListener("input", async () => {
    const typed = input.value.split(/[,;]/).pop().trim();
    if (typed.length < 2) { box.hidden = true; return; }
    const ask = ++asked;
    let found = [];
    try { found = await invoke("complete_address", { typed }); } catch { return; }
    if (ask !== asked) return;
    box.replaceChildren();
    for (const p of found.slice(0, 6)) {
      const b = el("button", "", p.name || p.address);
      if (p.name) b.append(el("span", "sub", p.address));
      b.type = "button";
      b.addEventListener("click", () => {
        const parts = input.value.split(/[,;]/);
        parts[parts.length - 1] = ` ${p.name ? `${p.name} <${p.address}>` : p.address}`;
        input.value = `${parts.join(",").replace(/^\s+/, "")}, `;
        box.hidden = true;
        input.focus();
      });
      box.append(b);
    }
    box.hidden = !found.length;
  });
  input.addEventListener("blur", () => setTimeout(() => { box.hidden = true; }, 200));
}

// ------------------------------------------------------------------ Mailboxes

/** The Inbox and every other place mail is: All Mail, the Views, each account's folders. */
function mailboxesScreen({ root } = {}) {
  const s = screen({ title: "Mailboxes", big: true, back: root ? false : "Inbox", grouped: true, right: barButton("pen-to-square", "", () => openCompose("new")) });
  async function draw() {
    await loadFolders();
    let unread = new Map();
    try { unread = new Map(await invoke("unread_counts")); } catch { /* counts can wait */ }
    let views = [];
    try { views = await invoke("views_list", { accounts: null }); } catch { /* none */ }
    s.body.replaceChildren();
    const open = (list, title) => push(listScreen(list, { title }));
    s.body.append(group(null, [
      { label: "Inbox", sub: "Every account, sorted", icon: "inbox", run: () => open(INBOX, "Inbox") },
      { label: "All Mail", icon: "layer-group", run: () => open({ title: "All Mail", all: true, folders: [] }, "All Mail") },
    ]));
    const shownViews = views.filter((v) => v.messages > 0);
    if (shownViews.length) {
      s.body.append(group("Views", shownViews.map((v) => ({
        label: v.name,
        icon: v.key === "red-alert" ? "circle-exclamation" : "tag",
        red: v.key === "red-alert",
        value: number.format(v.messages),
        run: () => open({ title: v.name, view: v.key, folders: [] }, v.name),
      }))));
    }
    for (const a of state.accounts) {
      const tree = folderTree(a.id);
      if (!tree.length) continue;
      const top = tree.filter((f) => !f.path.includes("/")).slice(0, 8);
      const items = top.map((f) => {
        const n = f.ids.reduce((sum, id) => sum + (unread.get(id) || 0), 0);
        return { label: f.path, icon: folderIcon(f.path), value: n ? number.format(n) : null, run: () => open({ title: f.path, folders: f.ids }, f.path) };
      });
      if (tree.length > top.length) items.push({ label: "All folders", icon: "folder-open", value: number.format(tree.length), run: () => push(accountScreen(a)) });
      s.body.append(group(a.label || a.address || a.name, items));
    }
    if (!state.accounts.length) {
      const n = el("div", "note");
      n.append(el("strong", "", "No accounts yet"));
      n.append(document.createTextNode("Add your mail in More ▸ Accounts."));
      s.body.append(n);
    }
  }
  s.shown = () => draw();
  return s;
}

/** Every folder of one account, as a tree. */
function accountScreen(account) {
  const s = screen({ title: account.label || account.address || account.name, grouped: true, search: "Find a folder" });
  const draw = (q) => {
    const tree = folderTree(account.id).filter((f) => !q || f.path.toLowerCase().includes(q));
    s.body.replaceChildren(group(null, tree.map((f) => ({
      label: f.path.split("/").pop(),
      icon: folderIcon(f.path),
      indent: q ? 0 : f.path.split("/").length - 1,
      value: f.messages ? number.format(f.messages) : null,
      run: () => push(listScreen({ title: f.path.split("/").pop(), folders: f.ids }, { title: f.path.split("/").pop() })),
    }))));
  };
  s.searchInput.addEventListener("input", () => draw(s.searchInput.value.trim().toLowerCase()));
  s.searchForm.addEventListener("submit", (e) => { e.preventDefault(); s.searchInput.blur(); });
  draw("");
  return s;
}

/** The Views (Receipts, Travel, Red Alert…) as a section of their own. */
function viewsScreen() {
  const s = screen({ title: "Views", big: true, back: false, grouped: true });
  s.shown = async () => {
    let views = [];
    try { views = await invoke("views_list", { accounts: null }); } catch (err) { failed("Reading the Views", err); }
    s.body.replaceChildren(group(null, views.map((v) => ({
      label: v.name,
      icon: v.key === "red-alert" ? "circle-exclamation" : "tag",
      red: v.key === "red-alert",
      value: number.format(v.messages),
      run: () => push(listScreen({ title: v.name, view: v.key, folders: [] }, { title: v.name })),
    })), "Views gather mail by what it is (receipts, travel, sign-in codes…) wherever it's filed."));
  };
  return s;
}

// ------------------------------------------------------------------ To Do

function todoScreen() {
  const s = screen({ title: "To Do", big: true, back: false });
  const add = el("form", "add-todo");
  const input = el("input");
  input.placeholder = "Add a task";
  input.enterKeyHint = "done";
  add.append(input);
  add.addEventListener("submit", async (e) => {
    e.preventDefault();
    const title = input.value.trim();
    if (!title) return;
    try {
      await invoke("todo_add", { title, notes: null, due: null, message: null });
      input.value = "";
      draw();
    } catch (err) { failed("Adding the task", err); }
  });
  s.top.append(add);
  const pull = el("div", "pull");
  pull.append(icon("arrows-rotate"));
  const list = el("div");
  s.body.append(pull, list);
  pullToRefresh(s.body, pull, () => draw());

  async function draw() {
    let todos = [];
    try { todos = await invoke("todos_list"); } catch (err) { return failed("Reading To Do", err); }
    list.replaceChildren();
    const today = new Date();
    today.setHours(0, 0, 0, 0);
    const dayOf = (t) => { const d = new Date(t * 1000); return new Date(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()); };
    const open = todos.filter((t) => !t.done).sort((a, b) => (a.due ?? Infinity) - (b.due ?? Infinity) || b.created - a.created);
    const done = todos.filter((t) => t.done).sort((a, b) => b.created - a.created).slice(0, 20);
    if (!open.length && !done.length) {
      const n = el("div", "note");
      n.append(el("strong", "", "Nothing to do"));
      n.append(document.createTextNode("Add a task above, or flag a message as To Do while reading it."));
      list.append(n);
    }
    for (const t of [...open, ...done]) {
      const row = el("div", `todo${t.done ? " done" : ""}`);
      const tick = el("button", "tick");
      tick.setAttribute("aria-label", t.done ? "Not done" : "Done");
      if (t.done) tick.append(icon("check", true));
      tick.addEventListener("click", async () => {
        try { await invoke("todo_update", { id: t.id, title: null, notes: null, due: null, clearDue: null, done: !t.done, important: null }); draw(); } catch (err) { failed("Changing the task", err); }
      });
      const text = el("div", "text");
      text.append(el("div", "title", t.title));
      if (t.due) {
        const d = dayOf(t.due);
        const late = !t.done && d < today;
        text.append(el("div", `due${late ? " late" : ""}`, `${late ? "Was due " : "Due "}${d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" })}`));
      }
      if (t.message) {
        const m = el("button", "small", "✉ Open the message");
        m.style.color = "var(--accent)";
        m.addEventListener("click", () => push(readerScreen({ rows: [{ id: t.message, flags: 0, labels: [] }], list: { title: "To Do" } }, 0)));
        text.append(m);
      }
      row.append(tick, text);
      if (t.important) { const st = icon("star", true); st.classList.add("star"); row.append(st); }
      list.append(row);
    }
    if (done.length && open.length) list.insertBefore(el("div", "day-head", "Done"), list.querySelector(".todo.done"));
  }
  s.shown = () => draw();
  return s;
}

// ------------------------------------------------------------------ Calendar (read-only for now)

function calendarScreen() {
  const s = screen({ title: "Calendar", big: true, back: false });
  s.shown = async () => {
    const from = Math.floor(Date.now() / 1000) - 12 * 3600;
    const to = from + 60 * 86400;
    let events = [];
    try { events = await invoke("calendar_events", { from, to }); } catch (err) { return failed("Reading the calendar", err); }
    s.body.replaceChildren();
    if (!events.length) {
      const n = el("div", "note");
      n.append(el("strong", "", "Nothing in the next two months"));
      n.append(document.createTextNode("Meetings and appointments from your Microsoft 365 and Google calendars show here."));
      s.body.append(n);
      return;
    }
    events.sort((a, b) => a.start - b.start);
    let lastDay = "";
    const todayKey = new Date().toDateString();
    for (const e of events) {
      const start = e.all_day ? new Date(new Date(e.start * 1000).toISOString().slice(0, 10) + "T00:00") : new Date(e.start * 1000);
      const key = start.toDateString();
      if (key !== lastDay) {
        lastDay = key;
        s.body.append(el("div", `day-head${key === todayKey ? " today" : ""}`, key === todayKey ? `Today · ${start.toLocaleDateString([], { month: "long", day: "numeric" })}` : start.toLocaleDateString([], { weekday: "long", month: "long", day: "numeric" })));
      }
      const row = el("div", `event${e.show_as === "free" ? " free" : ""}${e.cancelled ? " cancelled" : ""}`);
      const time = el("div", "time", e.all_day ? "all day" : start.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }));
      const what = el("div", "what");
      what.append(el("div", "", e.subject || "(no title)"));
      if (e.location) what.append(el("div", "place", e.location));
      row.append(time, el("div", "bar"), what);
      if (e.join_url) {
        const join = el("button", "send-btn", "Join");
        join.addEventListener("click", () => invoke("open_link", { url: e.join_url }).catch((err) => failed("Opening the meeting", err)));
        row.append(join);
      }
      s.body.append(row);
    }
  };
  return s;
}

// ------------------------------------------------------------------ Contacts (read-only for now)

function contactsScreen() {
  const s = screen({ title: "Contacts", big: true, back: false, search: "Find someone" });
  const list = el("div");
  s.body.append(list);
  let asked = 0;
  async function draw(query) {
    const ask = ++asked;
    let cards = [];
    try { cards = await invoke("cards_list", { query, account: null }); } catch (err) { return failed("Reading contacts", err); }
    if (ask !== asked) return;
    list.replaceChildren();
    for (const c of cards.slice(0, 300)) {
      const row = el("div", "contact");
      const card = c.card || {};
      const email = (card.emails || [])[0];
      const address = typeof email === "string" ? email : email?.address;
      row.append(avatar(c.shown));
      const text = el("div", "text");
      text.append(el("div", "", c.shown));
      const sub = [card.company, address].filter(Boolean).join(" · ");
      if (sub) text.append(el("div", "sub", sub));
      row.append(text);
      if (address) {
        const write = barButton("pen-to-square", "", () => openCompose("new", null, { to: c.shown ? `${c.shown} <${address}>` : address }));
        write.setAttribute("aria-label", `Write to ${c.shown}`);
        row.append(write);
      }
      list.append(row);
    }
    if (!cards.length) list.append(el("div", "note", query ? "No one by that name" : "No contacts yet"));
  }
  s.searchInput.addEventListener("input", () => draw(s.searchInput.value.trim()));
  s.searchForm.addEventListener("submit", (e) => { e.preventDefault(); s.searchInput.blur(); });
  s.shown = () => { if (!list.childElementCount) draw(""); };
  return s;
}

// ------------------------------------------------------------------ More, Accounts, Settings

function moreScreen() {
  const s = screen({ title: "More", big: true, back: false, grouped: true });
  s.shown = () => {
    const tabs = chosenTabs();
    const sections = Object.entries(SECTIONS).filter(([k]) => k !== "more" && !tabs.includes(k));
    s.body.replaceChildren();
    if (sections.length) {
      s.body.append(group(null, sections.map(([k, x]) => ({ label: x.title, icon: x.icon, run: () => push(withBack(x.root())) }))));
    }
    s.body.append(group(null, [
      { label: "Check Mail Now", icon: "arrows-rotate", run: async () => { try { await invoke("check_mail"); toast("Checking for new mail…"); } catch (err) { failed("Checking mail", err); } } },
      { label: "Accounts", icon: "users", run: () => push(accountsScreen()) },
      { label: "Back Up & Restore", icon: "box-archive", run: () => push(backupScreen()) },
      { label: "Settings", icon: "sliders", run: () => push(settingsScreen()) },
    ]));
    s.body.append(group(null, [{ label: "About Eudora", icon: "envelope", run: () => push(aboutScreen()) }]));
  };
  return s;
}

/** A section's first screen opened from More: with a way back. */
function withBack(sc) {
  const row = sc.top.querySelector(".top-row");
  const b = el("button", "bar-btn back");
  b.append(icon("chevron-left"), el("span", "", "More"));
  b.addEventListener("click", pop);
  row.firstChild.replaceChildren(b);
  return sc;
}

function accountsScreen() {
  const s = screen({ title: "Accounts", back: "More", grouped: true });
  async function draw() {
    await loadFolders();
    let missing = [];
    try { missing = await invoke("missing_sign_ins"); } catch { /* fine */ }
    let google = false;
    try { google = await invoke("google_available"); } catch { /* fine */ }
    const needs = new Map(missing.map((m) => [m.address.toLowerCase(), m]));
    s.body.replaceChildren();
    s.body.append(group("Your accounts", state.accounts.length ? state.accounts.map((a) => {
      const m = a.address && needs.get(a.address.toLowerCase());
      return {
        label: a.label || a.address || a.name,
        sub: m ? `${a.kind_name} · tap to sign in again` : a.kind_name,
        icon: a.kind.startsWith("graph") ? "envelope" : a.kind === "gmail" ? "envelope" : "inbox",
        red: !!m,
        run: m ? () => signIn(m.kind, a.address) : undefined,
      };
    }) : [{ label: "No accounts yet", icon: "inbox" }]));
    const add = [{ label: "Microsoft 365 or Outlook.com", sub: "Sign in with Microsoft", icon: "envelope", run: () => signIn("microsoft") }];
    if (google) add.push({ label: "Gmail or Google Workspace", sub: "Sign in with Google", icon: "envelope", run: () => signIn("google") });
    add.push({ label: "Another mail account", sub: "IMAP: iCloud, Yahoo, your provider or company", icon: "inbox", run: () => push(imapScreen(draw)) });
    s.body.append(group("Add an account", add, "Restoring your computer’s backup: More ▸ Back Up & Restore."));
  }
  async function signIn(kind, address) {
    // (a new account: how much mail to download is asked first, once)
    if (!address && !(await periodChosen()) && !(await choosePeriod())) return;
    toast(kind === "google" ? "Finish signing in on Google’s page…" : "Finish signing in on Microsoft’s page…");
    try {
      const who = await invoke(kind === "google" ? "google_sign_in" : "microsoft_sign_in", { address: address || null });
      toast(`Signed in: ${who}. Its mail starts coming in.`);
      invoke("check_mail").catch(() => {});
      draw();
    } catch (err) { failed("Not signed in", err); }
  }
  s.shown = () => draw();
  return s;
}

// ------------------------------------------------------------------ how much mail the phone downloads

/** A size in words: 1500 → "1.5 GB", 800 → "800 MB". */
function sizeText(mb) {
  return mb >= 1000 ? `${(mb / 1000).toLocaleString(undefined, { maximumFractionDigits: 1 })} GB` : `${mb} MB`;
}

/** The choice in words: 30 days → "Last 30 days", 365 → "Last year", a size → "Up to 5 GB",
 *  nothing → "Everything". */
function periodText(days, mb) {
  if (mb) return `Up to ${sizeText(mb)}`;
  if (days == null) return "Everything";
  const unit = (n, one) => (n === 1 ? `Last ${one}` : `Last ${n} ${one}s`);
  if (days % 365 === 0) return unit(days / 365, "year");
  if (days % 30 === 0 && days >= 60) return unit(days / 30, "month");
  if (days % 7 === 0 && days >= 14 && days !== 30) return unit(days / 7, "week");
  return unit(days, "day");
}

/** Any number of days, weeks, months, or years (undefined when canceled). */
function askOtherPeriod() {
  return sheet((box, close) => {
    box.append(el("h2", "", "How much mail?"));
    const form = el("form", "form period");
    const n = el("input");
    n.type = "number";
    n.inputMode = "numeric";
    n.min = "1";
    n.value = "6";
    const unit = el("select");
    for (const [label, days] of [["days", 1], ["weeks", 7], ["months", 30], ["years", 365]]) {
      const o = el("option", "", label);
      o.value = days;
      unit.append(o);
    }
    unit.value = "30";
    const go = el("button", "send-btn", "OK");
    go.type = "submit";
    form.append(el("span", "", "The last"), n, unit, go);
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      const count = Math.floor(Number(n.value));
      if (!(count >= 1)) return toast("A number of at least 1");
      close(count * Number(unit.value));
    });
    box.append(form);
    setTimeout(() => n.focus(), 50);
  });
}

/** Up to how much space (resolves to {mb}, undefined when canceled). */
async function askSpace() {
  let used = 0;
  try { used = (await invoke("download_period_get")).used_mb || 0; } catch { /* fine */ }
  const picked = await choose("Up to how much space?", [
    { label: "Up to 1 GB", value: 1000 },
    { label: "Up to 5 GB", value: 5000 },
    { label: "Up to 20 GB", value: 20000 },
    { label: "Up to 50 GB", value: 50000 },
    { label: "Another size…", value: "other" },
  ], { text: `The newest mail comes down until Eudora takes up this much. As new mail comes in, the oldest leaves the phone (it stays on the server; the last week always stays). Eudora takes up ${sizeText(used)} now.` });
  if (picked === undefined) return undefined;
  if (picked !== "other") return { mb: picked };
  const typed = await askText("Up to how many GB?", "For example 10", "", "OK");
  if (typed == null) return undefined;
  const gb = Number(typed.replace(",", "."));
  if (!(gb >= 0.1)) { toast("At least 0.1 GB"); return undefined; }
  return { mb: Math.round(gb * 1000) };
}

/** Asks how much mail this phone downloads: days, null = everything, {mb} = a space,
 *  undefined = canceled. */
async function askPeriod() {
  const picked = await choose("How much mail should this phone download?", [
    { label: "Last 30 days", value: 30 },
    { label: "Last 90 days", value: 90 },
    { label: "Last year", value: 365 },
    { label: "Everything", sub: "All of it, back to the oldest", value: "all" },
    { label: "Another period…", sub: "Any number of days, weeks, months, or years", value: "other" },
    { label: "Up to a size…", sub: "As much as fits in the space you choose, newest first", value: "space" },
  ], { text: "The newest comes first. Older mail stays on the server; you can change this in Settings." });
  if (picked === undefined) return undefined;
  if (picked === "all") return null;
  if (picked === "space") return askSpace();
  if (picked === "other") return askOtherPeriod();
  return picked;
}

/** Asks and keeps the answer (false when canceled). */
async function choosePeriod() {
  const answer = await askPeriod();
  if (answer === undefined) return false;
  const [days, mb] = answer && typeof answer === "object" ? [null, answer.mb] : [answer, null];
  await invoke("download_period_set", { days, mb });
  toast(`Mail on this phone: ${periodText(days, mb).toLowerCase()}`);
  return true;
}

/** Whether the period was ever chosen here. */
async function periodChosen() {
  try { return (await invoke("download_period_get")).chosen; } catch { return true; }
}

// ------------------------------------------------------------------ back up and restore

/** Where the phone's backups are, in words the person sees. */
const BACKUP_PLACE = /iPhone|iPad/.test(navigator.userAgent) ? "the Files app ▸ On My iPhone ▸ Eudora" : "Eudora’s own folder (with the phone plugged into a computer: Internal storage ▸ Android ▸ data ▸ net.planet.eudora ▸ files ▸ Documents)";

/** Android: backups are picked with Android's file picker, and copies saved out of Eudora's folder. */
const ANDROID = /Android/.test(navigator.userAgent);

/** Follows the backup or restore running (a line a second) until it ends: its final state. */
async function followJob(show) {
  for (;;) {
    let j = null;
    try { j = await invoke("job_state"); } catch { /* fine */ }
    if (!j) return null;
    if (!j.running) return j;
    const counted = j.total ? ` (${Math.round((100 * j.done) / j.total)}%)` : "";
    show(`${j.line || "Working…"}${counted}`);
    await new Promise((r) => setTimeout(r, 1000));
  }
}

/** A phone backup's download limit, in words ("the last 90 days"), from its header. */
function limitText(stored) {
  try {
    const v = JSON.parse(stored || "null");
    if (!v) return "";
    return periodText(v.days ?? null, v.mb ?? null).toLowerCase();
  } catch { return ""; }
}

/** Back up this phone, and restore a backup (one from a computer is the one to restore). */
function backupScreen() {
  const s = screen({ title: "Back Up & Restore", back: "More", grouped: true });
  let busy = null; // the line shown while a job runs
  async function draw() {
    let found = { backups: [] };
    try { found = await invoke("phone_backups"); } catch (err) { failed("Backups", err); }
    s.body.replaceChildren();
    if (busy != null) {
      s.body.append(group(null, [{ label: busy, icon: "arrows-rotate" }, { label: "Stop", icon: "xmark", red: true, run: () => invoke("job_cancel").catch(() => {}) }]));
      return;
    }
    const day = (t) => new Date(t * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
    const listed = found.backups.map((b) => ({ label: b.name.replace(/(\.part001)?\.eudora-backup$/, ""), sub: `${sizeText(Math.max(1, Math.round(b.size / 1e6)))} · ${day(b.modified)}`, icon: "download", run: () => restore(b) }));
    if (ANDROID) {
      s.body.append(group("Restore a backup", [{ label: "Choose a Backup File…", icon: "folder-open", run: pickAndRestore }, ...listed],
        "Your computer’s backup (File ▸ Back Up Eudora on a Mac, PC, or Linux computer) can be anywhere the phone reaches: Downloads, Google Drive, a USB stick… Choose every part of a backup made in parts."));
    } else {
      s.body.append(group("Restore a backup", listed.length ? listed : [{ label: "No backups here yet", icon: "download" }],
        `To restore your computer’s backup (File ▸ Back Up Eudora on a Mac, PC, or Linux computer), copy it into ${BACKUP_PLACE}, then come back here.`));
    }
    const backupItems = [{ label: "Back Up Now", icon: "box-archive", run: () => backUp(found.suggested) }];
    if (ANDROID && found.backups.length) backupItems.push({ label: "Save a Copy…", sub: "To Downloads, Google Drive, or anywhere else", icon: "share", run: () => saveCopy(found.backups) });
    s.body.append(group("Back up this phone", backupItems,
      `Saved in ${BACKUP_PLACE}.${ANDROID ? " Uninstalling Eudora deletes the backups kept there: Save a Copy puts one somewhere safe." : ""} A phone’s backup isn’t complete (rules live on your computers): restore a computer’s backup whenever you can.`));
  }
  async function run(kind, start) {
    busy = "Starting…";
    draw();
    try {
      await start();
      const done = await followJob((line) => { busy = line; draw(); });
      busy = null;
      draw();
      return done;
    } catch (err) {
      busy = null;
      draw();
      failed(kind === "backup" ? "Not backed up" : kind === "copy" ? "Not copied" : "Not restored", err);
      return null;
    }
  }
  async function backUp(dest) {
    const how = await choose("Back up this phone", [
      { label: "With a password", sub: "Recommended: the backup is sealed, and your mail server passwords go in it", value: "password", primary: true },
      { label: "Without a password", sub: "Anyone with the file can read the mail in it; no passwords go in it", value: "none" },
    ], { text: "Microsoft and Google sign-ins never go in a backup: you sign in again after restoring." });
    if (how === undefined) return;
    let password = null;
    if (how === "password") {
      password = await askText("A password for the backup", "Password", "", "Next", { password: true, text: "You need it to restore. Eudora can’t recover a forgotten one." });
      if (!password) return;
      const again = await askText("Type it again", "Password", "", "Back Up", { password: true });
      if (again == null) return;
      if (again !== password) return toast("The two passwords differ: nothing was backed up");
    }
    const done = await run("backup", () => invoke("backup_start", { dest, password, indexes: false, passwords: !!password, agreed: "", partGb: null }));
    if (done) toast(done.failed ? done.result : `Backed up. ${done.result}`);
    draw();
    if (ANDROID && done && !done.failed) {
      const elsewhere = await ask("Save a copy somewhere else?",
        "The backup is in Eudora’s own folder, which uninstalling Eudora deletes. A copy in Downloads or Google Drive stays.", "Save a Copy…");
      if (elsewhere) saveCopy([{ path: dest }]);
    }
  }
  /** Android: a backup copied to where the person picks (out of Eudora's folder). */
  async function saveCopy(backups) {
    let path = backups[0].path;
    if (backups.length > 1) {
      path = await choose("Which backup?", backups.map((b) => ({ label: b.name.replace(/(\.part001)?\.eudora-backup$/, ""), value: b.path })));
      if (path === undefined) return;
    }
    // (the picker shows first; the copy is a job of its own, followed like a backup)
    let started;
    try { started = await invoke("backup_save_copy", { path }); } catch (err) { return failed("Not copied", err); }
    if (!started) return;
    const done = await run("copy", async () => {});
    if (done) toast(done.result);
  }
  /** Android: a backup chosen in Android's file picker, copied into Eudora, then restored. */
  async function pickAndRestore() {
    let started;
    try { started = await invoke("backup_pick"); } catch (err) { return failed("Can’t open that file", err); }
    if (!started) return;
    const done = await run("copy", async () => {});
    if (!done) return;
    if (done.failed) return toast(done.result);
    try { await restore({ path: done.result }); } finally { invoke("backup_picked_clear").catch(() => {}); }
  }
  async function restore(b) {
    let d;
    try { d = await invoke("restore_describe", { path: b.path }); } catch (err) { return failed("Can’t read that backup", err); }
    if (d.device === "phone") {
      const limit = limitText(d.limit);
      const ok = await ask("This backup is from a phone",
        `It isn’t complete: your rules aren’t in it (they run on your computers)${limit ? `, and it holds only the mail that phone kept (${limit})` : ""}. Use it only when no backup from a Mac, PC, or Linux computer is available.`,
        "Restore it anyway", { danger: true });
      if (!ok) return;
    }
    let password = null;
    if (d.encrypted) {
      password = await askText("This backup has a password", "Password", "", "Next", { password: true });
      if (!password) return;
    }
    const total = Object.values(d.sizes || {}).reduce((a, n) => a + (Number(n) || 0), 0);
    const sure = await ask("Replace the mail on this phone?",
      `The backup (${sizeText(Math.max(1, Math.round(total / 1e6)))}) is restored beside what’s on the phone now and takes its place when Eudora is opened again; the phone needs room for both until then.`,
      "Restore");
    if (!sure) return;
    let dest = "";
    try { dest = await invoke("restore_suggested"); } catch { /* the usual place */ }
    const done = await run("restore", () => invoke("restore_start", { path: b.path, dest, password, replace: false }));
    if (!done) return;
    if (done.failed) return toast(done.result);
    await choose("Restored", [{ label: "OK", value: true, primary: true }], {
      text: "Close Eudora (swipe it away in the app switcher) and open it again to use the restored mail. Then sign in again to your Microsoft and Google accounts (More ▸ Accounts).",
    });
  }
  s.shown = () => draw();
  return s;
}

// ------------------------------------------------------------------ an IMAP account

/** The usual servers for an address's domain (the desktop's presets). */
const IMAP_PRESETS = {
  "gmail.com": { imap: "imap.gmail.com", smtp: "smtp.gmail.com", hint: "Gmail: use an app password (Google Account ▸ Security ▸ 2-Step Verification ▸ App passwords), not your Google password." },
  "googlemail.com": { imap: "imap.gmail.com", smtp: "smtp.gmail.com", hint: "Gmail: use an app password, not your Google password." },
  "yahoo.com": { imap: "imap.mail.yahoo.com", smtp: "smtp.mail.yahoo.com", hint: "Yahoo: use an app password (Account security ▸ Generate app password)." },
  "icloud.com": { imap: "imap.mail.me.com", smtp: "smtp.mail.me.com", hint: "iCloud: use an app-specific password (account.apple.com ▸ Sign-In and Security)." },
  "me.com": { imap: "imap.mail.me.com", smtp: "smtp.mail.me.com", hint: "iCloud: use an app-specific password." },
  "mac.com": { imap: "imap.mail.me.com", smtp: "smtp.mail.me.com", hint: "iCloud: use an app-specific password." },
  "aol.com": { imap: "imap.aol.com", smtp: "smtp.aol.com", hint: "AOL: use an app password." },
};

/** Adds an IMAP account (with its SMTP server for sending): the address and password, the
 *  servers filled in from the address and changeable. `done` redraws Accounts. */
function imapScreen(done) {
  const s = screen({ title: "Mail Account", back: "Accounts", grouped: true });
  const input = (placeholder, type = "text", mode) => {
    const i = el("input");
    i.type = type;
    i.placeholder = placeholder;
    if (mode) i.inputMode = mode;
    i.autocomplete = "off";
    i.setAttribute("autocapitalize", "off");
    i.spellcheck = false;
    return i;
  };
  const address = input("you@example.com", "email", "email");
  const password = input("Password", "password");
  const name = input("Your name (shown to people you write to)");
  name.setAttribute("autocapitalize", "words");
  const server = input("imap.example.com", "text", "url");
  const port = input("993", "text", "numeric");
  const smtp = input("smtp.example.com", "text", "url");
  const smtpPort = input("465", "text", "numeric");
  const user = input("User name (usually the address)", "email", "email");
  const hint = el("div", "foot", "");
  const result = el("div", "foot", "");
  const row = (label, control) => {
    const r = el("label", "form-row");
    r.append(el("span", "", label), control);
    return r;
  };
  const box = (title, rows) => {
    const g = el("div", "group form-group");
    if (title) g.append(el("h3", "", title));
    const items = el("div", "items");
    items.append(...rows);
    g.append(items);
    return g;
  };
  let touched = false;
  for (const i of [server, smtp, user]) i.addEventListener("input", () => { touched = true; });
  const guess = async () => {
    const a = address.value.trim().toLowerCase();
    const domain = a.split("@")[1] || "";
    if (!domain) return;
    let host = null;
    try { host = await invoke("mail_host", { address: a }); } catch { /* fine */ }
    if (host === "microsoft") {
      hint.textContent = "This is a Microsoft address: Microsoft no longer takes passwords from mail programs. Go back and choose Microsoft 365 or Outlook.com.";
      return;
    }
    const preset = IMAP_PRESETS[domain] || (host === "google" ? IMAP_PRESETS["gmail.com"] : null);
    if (!touched) {
      server.value = preset ? preset.imap : `imap.${domain}`;
      smtp.value = preset ? preset.smtp : `smtp.${domain}`;
      user.value = a;
    }
    hint.textContent = preset?.hint || "Your password is kept in this phone’s keychain, never in the mail library.";
  };
  address.addEventListener("change", guess);
  const config = () => ({
    address: address.value.trim(),
    name: name.value.trim(),
    protocol: "imap",
    server: server.value.trim(),
    port: Number(port.value) || 993,
    security: "tls",
    tls_level: "high",
    username: user.value.trim() || address.value.trim(),
    smtp_server: smtp.value.trim(),
    smtp_port: Number(smtpPort.value) || 465,
    smtp_security: Number(smtpPort.value) === 587 ? "starttls" : "tls",
    smtp_tls_level: "high",
    smtp_username: "",
    smtp_auth: true,
    server_writes: true,
    pop_delete_after_days: null,
    pop_skip_over_mb: null,
  });
  const go = el("button", "send-btn wide", "Sign In");
  go.type = "button";
  go.addEventListener("click", async () => {
    const c = config();
    if (!c.address.includes("@")) return toast("Type the account’s email address");
    if (!password.value) return toast("Type its password");
    if (!c.server) await guess();
    // (how much mail to download is asked first, once)
    if (!(await periodChosen()) && !(await choosePeriod())) return;
    go.disabled = true;
    result.textContent = `Signing in to ${c.server} and ${c.smtp_server}…`;
    try {
      const r = await invoke("imap_test", { config: config(), password: password.value, smtpPassword: null });
      result.textContent = `Signed in (${r.security}): ${r.folders.length} folders, ${r.messages.toLocaleString()} messages.`;
      await invoke("imap_save", { config: config(), password: password.value, smtpPassword: null });
      invoke("check_mail").catch(() => {});
      toast(`Added ${c.address}. Its mail starts coming in.`);
      pop();
      done?.();
    } catch (err) {
      result.textContent = String(err);
    } finally {
      go.disabled = false;
    }
  });
  s.body.append(
    box("Account", [row("Address", address), row("Password", password), row("Name", name)]),
    hint,
    box("Incoming mail (IMAP, encrypted)", [row("Server", server), row("Port", port), row("User name", user)]),
    box("Sending (SMTP, encrypted)", [row("Server", smtp), row("Port", smtpPort)]),
    el("div", "foot", "Port 465 is encrypted from the start; 587 starts plain and switches to encryption (STARTTLS)."),
    go,
    result,
  );
  setTimeout(() => address.focus(), 300);
  return s;
}

/** Settings on this phone: what the tab bar holds (up to four, More always last). */
function settingsScreen() {
  const s = screen({ title: "Settings", back: "More", grouped: true });
  async function draw() {
    const tabs = chosenTabs().filter((t) => t !== "more");
    let period = { chosen: false, days: null };
    try { period = await invoke("download_period_get"); } catch { /* fine */ }
    s.body.replaceChildren();
    s.body.append(group("Mail on this phone", [{
      label: "Download",
      value: period.chosen ? periodText(period.days, period.mb) : "Not chosen",
      icon: "rotate-left",
      run: async () => { if (await choosePeriod()) draw(); },
    }], "Newest first. Older mail leaves the phone as time goes by (hourly), and stays on the server. Flagged messages stay."));
    // settings sync: Views, labels, signatures, and the like from your computers (rules run there)
    let sync = null;
    try { sync = await invoke("sync_state"); } catch { /* fine */ }
    s.body.append(group("Sync with your computers", [sync?.key
      ? { label: "Syncing", sub: `Through ${sync.home || "your home account"}`, icon: "arrows-rotate", value: "Sync Now", run: async () => { try { await invoke("sync_now", { repair: false }); toast("Syncing with your computers…"); } catch (err) { failed("Not synced", err); } } }
      : { label: "Join with the recovery phrase", sub: "From a computer: Preferences ▸ Sync ▸ Show recovery phrase", icon: "arrows-rotate", run: async () => {
          const phrase = await askText("Join sync", "The recovery phrase’s words", "", "Join", { text: "Type the words your computer shows. They stay on this phone (its keychain), and never leave it in a message." });
          if (!phrase) return;
          try {
            await invoke("sync_join", { phrase });
            await invoke("sync_now", { repair: false }).catch(() => {});
            toast("Joined: your Views, labels, and signatures come over at the next check");
            draw();
          } catch (err) { failed("Not joined", err); }
        } }],
      "Rules and the spam filter run on your computers; their filing reaches this phone through the server."));
    const items = Object.entries(SECTIONS).filter(([k]) => k !== "more").map(([k, x]) => ({
      label: x.title,
      icon: x.icon,
      control: toggle(tabs.includes(k), (on, input) => {
        let now = chosenTabs().filter((t) => t !== "more");
        if (on) {
          if (now.length >= MAX_TABS) { input.checked = false; return toast(`The tab bar holds ${MAX_TABS} (and More): turn one off first`); }
          now.push(k);
        } else {
          if (now.length <= 1) { input.checked = true; return toast("Keep at least one tab"); }
          now = now.filter((t) => t !== k);
        }
        // in the order the sections are listed
        const order = Object.keys(SECTIONS);
        store.set("tabs", now.sort((a, b) => order.indexOf(a) - order.indexOf(b)));
        drawTabbar();
      }),
    }));
    s.body.append(group("Tab bar", items, `Up to ${MAX_TABS} sections, plus More (which holds the rest).`));
    s.body.append(group("Reading", [{ label: "Swiping a message", value: "Next / previous", icon: "envelope-open" }], "Messages in a list have no swipe actions, so nothing is deleted by a stray thumb: long-press a message to choose several."));
  }
  s.shown = () => draw();
  return s;
}

function aboutScreen() {
  const s = screen({ title: "About Eudora", back: "More", grouped: true });
  s.shown = async () => {
    let info = {};
    try { info = await invoke("info"); } catch { /* fine */ }
    s.body.replaceChildren(group(null, [
      { label: "Eudora", sub: "Free and open: no ads, nothing collected about you", icon: "envelope" },
      { label: "Library", sub: info.path || "", icon: "folder" },
    ], "Feedback and bug reports: software@planet.net"));
  };
  return s;
}

// ------------------------------------------------------------------ start

/** A link in a message: mailto: writes a new message, web links open in the browser. */
function openMessageLink(href) {
  if (!href || href.startsWith("#")) return;
  if (/^mailto:/i.test(href)) return openCompose("new", null, { to: decodeURIComponent(href.slice(7).split("?")[0]) });
  invoke("open_link", { url: href }).catch((err) => failed("Opening the link", err));
}

// WebKit (iOS) runs no click listener inside the message's sandboxed frame: the frame goes to
// the link instead, and the app stops that and hands the link back here (links_go_to_eudora)
window.__TAURI__?.webviewWindow?.getCurrentWebviewWindow().listen("link-clicked", (e) => openMessageLink(String(e.payload || "")));

// a script error shows (a phone has no console to look in)
window.addEventListener("error", (e) => toast(`Something went wrong: ${e.message}`));
window.addEventListener("unhandledrejection", (e) => toast(`Something went wrong: ${e.reason}`));

edgeBack($("tabs"));
loadFolders().then(async () => {
  showTab(store.get("tab", "mail"));
  // accounts that download, but how much was never asked (signed in before the question existed)
  const servers = state.accounts.some((a) => /^(graph|gmail|imap|pop3)/.test(a.kind || ""));
  if (servers && !(await periodChosen())) choosePeriod().catch((e) => failed("Not changed", e));
  // after a restore: the accounts whose sign-ins didn't come with it
  try {
    const missing = await invoke("missing_sign_ins");
    if (missing.length) toast(`Sign in again to ${missing.map((m) => m.address).join(", ")}: More ▸ Accounts`);
  } catch { /* fine */ }
});
// the people offered while writing (from the mail: whom the user writes to, who writes to them),
// as the desktop: shortly after starting, then every 6 hours
const people = () => invoke("rebuild_contacts").catch(() => {});
setTimeout(people, 5000);
setInterval(people, 6 * 3600 * 1000);
