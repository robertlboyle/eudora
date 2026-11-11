// Eudora's front end: the sidebar (All Mail, then Accounts with their folders, or Labels),
// the message list (virtual: only the rows on screen exist), and the message. Everything comes
// from the Rust commands in src/lib.rs. Every look and both ways of organizing work on the
// same library, whatever the accounts are (Microsoft 365, Gmail, IMAP, POP3, old archives).
"use strict";

const invoke = window.__TAURI__.core.invoke;
const PAGE = 200;           // rows asked for at a time
const SEARCH_LIMIT = 2000;  // search results listed (the total is always shown)
const LABELS_SHOWN = 3;     // labels shown on a message before "+N"
const $ = (id) => document.getElementById(id);

// ------------------------------------------------------------------ small helpers

/**
 * The window's choices. Those that travel to the user's other computers (the look, how mail is
 * organized, columns, hiding sent mail: `syncedChoice`) are also kept in the library as
 * settings "ui:<name>", which settings sync carries; window sizes, open folders and the last
 * folder shown stay on this computer.
 */
const store = {
  get(key, fallback) {
    try { const v = localStorage.getItem("eudora." + key); return v === null ? fallback : JSON.parse(v); } catch { return fallback; }
  },
  set(key, value) {
    try { localStorage.setItem("eudora." + key, JSON.stringify(value)); } catch { /* private window: not remembered */ }
    if (syncedChoice(key)) window.__TAURI__?.core.invoke("set_setting", { key: "ui:" + key, value: JSON.stringify(value) }).catch(() => {});
  },
};

function syncedChoice(key) {
  return ["skin", "organize", "hideSent"].includes(key) || key.startsWith("columns:");
}

/**
 * Takes in choices made on the user's other computers (settings sync brings them into the
 * library), and puts this computer's into the library the first time. Returns whether any
 * changed here.
 */
async function pullChoices() {
  let changed = false;
  try {
    const kept = new Map((await invoke("settings_with_prefix", { prefix: "ui:" })).map(([k, value]) => [k.slice(3), value]));
    for (const [key, value] of kept) {
      if (localStorage.getItem("eudora." + key) !== value) {
        localStorage.setItem("eudora." + key, value);
        changed = true;
      }
    }
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i).replace(/^eudora\./, "");
      if (syncedChoice(key) && !kept.has(key)) invoke("set_setting", { key: "ui:" + key, value: localStorage.getItem("eudora." + key) }).catch(() => {});
    }
  } catch { /* the library isn't open: this computer's choices stand */ }
  return changed;
}

/** After choices arrive from another computer: the look, the organization, the columns. */
function applyChoices() {
  state.skin = store.get("skin", state.skin);
  state.organize = store.get("organize", state.organize);
  state.hideSent = store.get("hideSent", true) !== false;
  applySkin();
  renderSidebar();
}

function iconUrl(name) {
  return `icons/${window.ICON_SET || "free"}/${name}.svg`;
}

/** An icon element (Font Awesome, colored by the text color). */
function icon(name) {
  const span = document.createElement("span");
  span.className = "icon";
  span.style.setProperty("--icon", `url("${iconUrl(name)}")`);
  return span;
}

function fillIcons(root = document) {
  for (const el of root.querySelectorAll("[data-icon]")) {
    el.classList.add("icon");
    el.style.setProperty("--icon", `url("${iconUrl(el.dataset.icon)}")`);
  }
}

function el(tag, className, text) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

const number = new Intl.NumberFormat("en-US");
const dayFormat = new Intl.DateTimeFormat("en-US", { month: "numeric", day: "numeric", year: "numeric" });
const monthDayFormat = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric" });
const timeFormat = new Intl.DateTimeFormat("en-US", { hour: "numeric", minute: "2-digit" });
const weekdayFormat = new Intl.DateTimeFormat("en-US", { weekday: "short" });
const fullFormat = new Intl.DateTimeFormat("en-US", { weekday: "short", month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short" });

function shortDate(seconds) {
  if (!seconds) return "";
  const d = new Date(seconds * 1000);
  const today = new Date();
  const s = skin();
  // today's mail says so, beside its time; the Eudora look puts the time first, as it does for
  // other days
  if (d.toDateString() === today.toDateString()) {
    return (s.css || state.skin) === "eudora" && !s.stacked && !s.line ? `${timeFormat.format(d)} Today` : `Today ${timeFormat.format(d)}`;
  }
  if (s.stacked) {
    // Lookout (as Outlook): the weekday within the last week, else the date
    const days = (today - d) / 86_400_000;
    if (days >= 0 && days < 6) return `${weekdayFormat.format(d)} ${timeFormat.format(d)}`;
    return dayFormat.format(d);
  }
  if (s.line) return d.getFullYear() === today.getFullYear() ? monthDayFormat.format(d) : dayFormat.format(d);
  return (skin().css || state.skin) === "eudora" ? `${timeFormat.format(d)} ${dayFormat.format(d)}` : dayFormat.format(d);
}

function kilobytes(bytes) {
  return number.format(Math.max(1, Math.round(bytes / 1024)));
}

// The periods the slider for "Mail kept on this computer" stops at (Settings ▸ Library and the
// Welcome window): days, and how they're said; null = everything
const KEEP_STEPS = [
  [30, "1 month"], [61, "2 months"], [91, "3 months"], [183, "6 months"], [365, "1 year"], [730, "2 years"],
  [1096, "3 years"], [1826, "5 years"], [3652, "10 years"], [7305, "20 years"], [null, "Everything"],
];

function sizeText(bytes) {
  if (bytes < 1024) return `${bytes} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function personText(p) {
  if (p.name && p.address) return `${p.name} <${p.address}>`;
  return p.name || p.address;
}

function escapeHtml(text) {
  return text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

function status(left, right) {
  if (left !== undefined) $("status-left").textContent = left;
  if (right !== undefined) $("status-right").textContent = right;
}

function showError(what, err) {
  status(`${what}: ${err}`);
  console.error(what, err);
}

// ------------------------------------------------------------------ state

/** This page showing one message only (`index.html?message=ID`): a message maximized into a
 *  window inside Eudora (`&frame=…`) or popped out into a window of its own. */
const READER = Number(new URLSearchParams(location.search).get("message")) || null;
const READER_FRAME = new URLSearchParams(location.search).get("frame");

const state = {
  skin: store.get("skin", "eudora"),
  /** "folders": All Mail, then Accounts and their folder trees; "labels": All Mail and one
   * flat list of labels (a label = a folder name, across every account). */
  organize: store.get("organize", "folders"),
  folders: [],
  accounts: { accounts: [], total: 0 },
  /** The list shown: { kind: "all" | "folder" | "label" | "search", key, folders: [ids],
   * title, total, rows: Map(index → row), pending: Set(page), stale } */
  list: null,
  /** What each sidebar item opens (key → view), rebuilt with the sidebar. */
  views: new Map(),
  selected: -1,
  messageId: null,
  view: "message",
  open: new Set(store.get("openFolders", ["#accounts"])),
};

// ------------------------------------------------------------------ looks

/** The looks. Every one shows the same views and runs the same commands; they differ in
 * arrangement and feel.
 * `columns`: [key, title, width (CSS grid track)] for a column list;
 * `stacked`: lines per message for the Lookout styles (sender + date, subject, preview);
 * `line`: one line per message (sender, subject - preview, date), as Gmail and Yahoo;
 * `reading`: where the message shows: "bottom", "right", or "replace" (in place of the list,
 * with a way back). */
const SKINS = {
  eudora: {
    sidebar: "Mailboxes",
    reading: "bottom",
    columns: [
      ["status", "•", "22px"],
      ["priority", "!", "28px"],
      ["attach", "", "22px"],
      ["who", "Who", "minmax(120px, 22%)"],
      ["date", "Date", "150px"],
      ["size", "K", "52px"],
      ["subject", "Subject", "1fr"],
    ],
  },
  thunderbird: {
    sidebar: "Folders",
    reading: "bottom",
    columns: [
      ["attach", "", "24px"],
      ["subject", "Subject", "1fr"],
      ["who", "Correspondents", "minmax(140px, 26%)"],
      ["date", "Date", "110px"],
    ],
  },
  "lookout-classic": { sidebar: "Folders", reading: "right", stacked: 2 },
  "lookout-new": { sidebar: "Folders", reading: "right", stacked: 3 },
  gmail: { sidebar: "Mail", reading: "replace", line: true },
  yahoo: { sidebar: "Folders", reading: "replace", line: true },
};

// Eudora Classic: the
// Eudora look in Classic mode (classic Eudora 7.1's menus, shortcuts and icons); every other look
// is Modern. Its stylesheet is the Eudora one ("css"); body.classic switches the classic parts in.
SKINS["eudora-classic"] = { ...SKINS.eudora, css: "eudora", classic: true };

function skin() {
  return SKINS[state.skin] || SKINS.eudora;
}

// ------------------------------------------------------------------ columns

/** Every column a list can show. `text(row)` for plain columns; header columns ("h:Name")
 * show a header's value, read for the rows on screen. */
const COLUMN_CATALOG = {
  attach: { title: "Attachment (paperclip)", head: "", width: "22px" },
  status: { title: "Status (• unread, R replied, F forwarded, D redirected)", head: "•", width: "22px", text: (r) => (r.flags & UNREAD ? "•" : r.flags & REDIRECTED ? "D" : r.flags & FORWARDED ? "F" : r.flags & REPLIED ? "R" : "") },
  priority: { title: "Priority set by hand", head: "!", width: "28px", text: (r) => ["", "!!", "!", "", "↓", "↓↓"][priorityOf(r.flags)] || "" },
  who: { title: "Who", width: "minmax(120px, 22%)", text: (r) => whoOf(r) },
  from: { title: "From", width: "minmax(120px, 22%)", text: (r) => r.from },
  to: { title: "To", width: "minmax(120px, 18%)", text: (r) => r.to },
  cc: { title: "Cc", width: "minmax(100px, 14%)", text: (r) => r.cc },
  date: { title: "Date", width: "150px", text: (r) => shortDate(r.date) },
  added: { title: "Added to Eudora", width: "150px", text: (r) => shortDate(r.added) },
  size: { title: "Size (K)", head: "K", width: "52px", text: (r) => kilobytes(r.size) },
  attachments: { title: "Number of attachments", head: "Att.", width: "44px", text: (r) => (r.attachments ? String(r.attachments) : "") },
  subject: { title: "Subject", width: "1fr" },
  preview: { title: "Preview (first words)", width: "minmax(120px, 30%)", text: (r) => r.snippet },
  folders: { title: "Folders", width: "minmax(100px, 16%)", text: (r) => [...new Set((r.labels || []).map((l) => l[1]))].join(", ") },
  account: { title: "Account", width: "minmax(100px, 14%)", text: (r) => [...new Set((r.labels || []).map((l) => state.folders.find((f) => f.id === l[0])?.account).filter(Boolean))].join(", ") },
  list: { title: "Mailing list", width: "minmax(100px, 14%)", text: (r) => r.list_id || "" },
  msgid: { title: "Message-ID", width: "minmax(120px, 18%)", text: (r) => r.message_id },
};

/** The columns shown in this look: [key, width], the user's own if they changed them. */
function listColumns() {
  const own = store.get(`columns:${state.skin}`, null);
  if (Array.isArray(own) && own.length) return own;
  return (skin().columns || []).map(([key, , width]) => [key, width]);
}

function saveColumns(columns) {
  store.set(`columns:${state.skin}`, columns);
  headerCache.clear();
  applySkin();
}

function columnTitle(key) {
  if (key.startsWith("h:")) return key.slice(2);
  const own = (skin().columns || []).find(([k]) => k === key);
  return own ? own[1] : (COLUMN_CATALOG[key]?.head ?? COLUMN_CATALOG[key]?.title ?? key);
}

// header columns' values, read for the rows on screen: id → Map(name → value)
const headerCache = new Map();
let headerWanted = new Set();
let headerTimer = null;

function headerNames() {
  return listColumns().filter(([k]) => k.startsWith("h:")).map(([k]) => k.slice(2));
}

function wantHeaders(id) {
  headerWanted.add(id);
  clearTimeout(headerTimer);
  headerTimer = setTimeout(async () => {
    const ids = [...headerWanted];
    headerWanted = new Set();
    const names = headerNames();
    if (!ids.length || !names.length) return;
    try {
      const values = await invoke("header_values", { ids, names });
      ids.forEach((id, i) => headerCache.set(id, new Map(names.map((n, j) => [n, values[i][j]]))));
      clearRows();
      renderList();
    } catch (err) {
      console.warn("header values", err);
    }
  }, 60);
}

const colMenu = $("col-menu");

/** The column menu (right-click on the column headings, or Mailbox ▸ Columns…). */
function showColumnMenu(x, y, clicked) {
  if (!skin().columns) {
    status("Columns are for the Eudora and Thunderbird-style looks (this look shows each message its own way).");
    return;
  }
  const columns = listColumns();
  const shown = new Set(columns.map(([k]) => k));
  const button = (text, action, checked) => {
    const b = el("button", checked === undefined ? "" : checked ? "checked" : "unchecked", text);
    b.addEventListener("click", () => { colMenu.hidden = true; action(); });
    return b;
  };
  const items = [el("div", "menu-title", "Columns")];
  for (const [key, c] of Object.entries(COLUMN_CATALOG)) {
    items.push(button(c.title, () => {
      if (shown.has(key)) saveColumns(columns.filter(([k]) => k !== key));
      else saveColumns([...columns, [key, c.width]]);
    }, shown.has(key)));
  }
  const headers = columns.filter(([k]) => k.startsWith("h:"));
  if (headers.length) items.push(el("div", "menu-title", "Header columns"));
  for (const [key] of headers) items.push(button(key.slice(2), () => saveColumns(columns.filter(([k]) => k !== key)), true));
  items.push(el("hr"));
  items.push(button("Add a header column…", () => { $("col-header-name").value = ""; $("col-header-box").showModal(); }));
  if (clicked) {
    const i = columns.findIndex(([k]) => k === clicked);
    if (i > 0) items.push(button(`Move “${columnTitle(clicked) || "Attachment"}” left`, () => { const c = [...columns]; [c[i - 1], c[i]] = [c[i], c[i - 1]]; saveColumns(c); }));
    if (i >= 0 && i < columns.length - 1) items.push(button(`Move “${columnTitle(clicked) || "Attachment"}” right`, () => { const c = [...columns]; [c[i + 1], c[i]] = [c[i], c[i + 1]]; saveColumns(c); }));
  }
  items.push(button(`Reset to the ${$("skin").selectedOptions[0]?.textContent || "look"}'s columns`, () => { store.set(`columns:${state.skin}`, null); headerCache.clear(); applySkin(); }));
  colMenu.replaceChildren(...items);
  colMenu.hidden = false;
  colMenu.style.left = `${Math.min(x, innerWidth - colMenu.offsetWidth - 8)}px`;
  colMenu.style.top = `${Math.min(y, innerHeight - colMenu.offsetHeight - 8)}px`;
}

$("list-head").addEventListener("contextmenu", (e) => {
  e.preventDefault();
  showColumnMenu(e.clientX, e.clientY, e.target.closest(".cell")?.dataset.key);
});
document.addEventListener("click", (e) => { if (!colMenu.contains(e.target)) colMenu.hidden = true; });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") colMenu.hidden = true; });

function addHeaderColumn() {
  const name = $("col-header-name").value.trim().replace(/:$/, "");
  if (!/^[A-Za-z0-9_-]+$/.test(name)) { $("col-header-name").focus(); return; }
  $("col-header-box").close();
  const columns = listColumns();
  if (!columns.some(([k]) => k.toLowerCase() === `h:${name}`.toLowerCase())) saveColumns([...columns, [`h:${name}`, "minmax(100px, 16%)"]]);
}
$("col-header-add").addEventListener("click", addHeaderColumn);
$("col-header-name").addEventListener("keydown", (e) => { if (e.key === "Enter") addHeaderColumn(); });
$("col-header-cancel").addEventListener("click", () => $("col-header-box").close());

/** Column headings: drag one onto another to move it there; drag a heading's right edge to
 *  make the column wider or narrower. */
function setupHeadingDrag(head) {
  let resizing = null;
  head.addEventListener("pointerdown", (e) => {
    const cell = e.target.closest(".cell");
    if (!cell || e.button !== 0) return;
    const r = cell.getBoundingClientRect();
    if (e.clientX < r.right - 6) return;
    e.preventDefault();
    resizing = { key: cell.dataset.key, start: e.clientX, width: r.width };
    head.setPointerCapture(e.pointerId);
  });
  head.addEventListener("pointermove", (e) => {
    const cell = e.target.closest(".cell");
    if (cell && !resizing) {
      const r = cell.getBoundingClientRect();
      cell.style.cursor = e.clientX >= r.right - 6 ? "col-resize" : "";
    }
    if (!resizing) return;
    const width = Math.max(24, Math.round(resizing.width + e.clientX - resizing.start));
    const columns = listColumns().map(([k, w]) => [k, k === resizing.key ? `${width}px` : w]);
    document.documentElement.style.setProperty("--columns", columns.map((c) => c[1]).join(" "));
    resizing.columns = columns;
  });
  let resized = 0;
  head.addEventListener("pointerup", () => {
    if (resizing?.columns) {
      saveColumns(resizing.columns);
      resized = Date.now();
    }
    resizing = null;
  });
  // a click on a heading sorts by it (not the click that ends sizing it)
  head.addEventListener("click", (e) => {
    const cell = e.target.closest(".cell");
    if (!cell?.dataset.key || Date.now() - resized < 400) return;
    const r = cell.getBoundingClientRect();
    if (e.clientX >= r.right - 6) return;
    sortBy(cell.dataset.key);
  });
  head.addEventListener("dragstart", (e) => {
    const cell = e.target.closest(".cell");
    if (!cell) return;
    e.dataTransfer.setData("text/x-column", cell.dataset.key);
    e.dataTransfer.effectAllowed = "move";
  });
  head.addEventListener("dragover", (e) => {
    if (e.dataTransfer.types.includes("text/x-column")) { e.preventDefault(); e.dataTransfer.dropEffect = "move"; }
  });
  head.addEventListener("drop", (e) => {
    const from = e.dataTransfer.getData("text/x-column");
    const to = e.target.closest(".cell")?.dataset.key;
    if (!from || !to || from === to) return;
    e.preventDefault();
    const columns = listColumns();
    const moving = columns.find(([k]) => k === from);
    const rest = columns.filter(([k]) => k !== from);
    rest.splice(rest.findIndex(([k]) => k === to), 0, moving);
    saveColumns(rest);
  });
}
setupHeadingDrag($("list-head"));

function applySkin() {
  if (!SKINS[state.skin]) state.skin = "eudora";
  const s = skin();
  document.body.className = `skin-${s.css || state.skin}${s.classic ? " classic" : ""} reading-${s.reading}${s.stacked ? " stacked" : ""}${s.line ? " line" : ""}${document.body.classList.contains("offline") ? " offline" : ""}`;
  $("skin").value = state.skin;
  document.querySelector("#folders-pane .pane-title").textContent = state.organize === "labels" ? "Labels" : s.sidebar;
  const head = $("list-head");
  if (s.columns) {
    const columns = listColumns();
    document.documentElement.style.setProperty("--columns", columns.map((c) => c[1]).join(" "));
    head.replaceChildren(...columns.map(([key]) => {
      const cell = el("div", `cell c-${key.startsWith("h:") ? "header" : key}`);
      cell.dataset.key = key;
      cell.draggable = true;
      if (key === "attach") cell.append(icon("paperclip"));
      else cell.textContent = columnTitle(key);
      const sortable = key === "who" || SORT_KEYS[key];
      cell.title = `${key.startsWith("h:") ? `The ${key.slice(2)} header` : COLUMN_CATALOG[key]?.title || key}.${sortable ? " Click to sort by it, again for the other way." : ""} Drag to move, drag the right edge to size, right-click for more columns.`;
      return cell;
    }));
  } else {
    document.documentElement.style.setProperty("--columns", "1fr");
    head.replaceChildren(el("div", "cell stacked-head", "By date · newest on top"));
  }
  showSortMark();
  rowHeight = parseFloat(getComputedStyle(document.body).getPropertyValue("--row")) || 20;
  applySizes();
  clearRows();
  renderList();
}

function setSkin(key) {
  state.skin = key;
  store.set("skin", key);
  applySkin();
  applyMode();
  // every message window takes the new look at once (inside this window and on their own)
  window.__TAURI__.event.emit("skin", key).catch(() => {});
  for (const c of composers.values()) c.frame.contentWindow?.applySkin?.(key);
}

$("skin").addEventListener("change", () => setSkin($("skin").value));

// Light or dark: every look has its dark version for the system's dark mode; the button sets what
// every window takes the system's to be (`theme_set`), kept for next time. The moon shows while
// light (a click: dark), the sun while dark; a right-click follows Windows again.
const darkNow = matchMedia("(prefers-color-scheme: dark)");
// (drawn here: Font Awesome's sun looked like the gear beside it; its sun-bright isn't in Free)
const SUN_SVG = `<svg class="sun" viewBox="0 0 24 24" width="1.15em" height="1.15em" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="4.2" fill="currentColor" stroke="none"/><path d="M12 1.8v2.6M12 19.6v2.6M1.8 12h2.6M19.6 12h2.6M4.8 4.8l1.85 1.85M17.35 17.35l1.85 1.85M4.8 19.2l1.85-1.85M17.35 6.65l1.85-1.85"/></svg>`;
function showThemeButton() {
  const b = $("theme-toggle");
  if (darkNow.matches) b.innerHTML = SUN_SVG;
  else b.replaceChildren(icon("moon"));
  b.title = `${darkNow.matches ? "Light mode" : "Dark mode"} (right-click: follow Windows again)`;
}
function setTheme(choice) {
  store.set("theme", choice);
  invoke("theme_set", { theme: choice }).catch((err) => showError("Switching light and dark", err));
}
$("theme-toggle").addEventListener("click", () => setTheme(darkNow.matches ? "light" : "dark"));
$("theme-toggle").addEventListener("contextmenu", (e) => {
  e.preventDefault();
  setTheme(null);
  status("Light or dark: following Windows again.");
});
// (a message shown is drawn again in the new colors)
darkNow.addEventListener("change", () => { showThemeButton(); if (current) renderBody(); });
showThemeButton();
if (!READER && store.get("theme", null)) invoke("theme_set", { theme: store.get("theme", null) }).catch(() => {});

// ------------------------------------------------------------------ sidebar

function folderIcon(name, open) {
  const n = name.toLowerCase().replace(/\.(mbx|fol)$/, "");
  if (n === "in" || n === "inbox") return "inbox";
  if (n === "out" || n.startsWith("sent") || n.startsWith("old sent") || n.startsWith("oldout")) return "paper-plane";
  if (n === "trash" || n.startsWith("deleted")) return "trash-can";
  if (n === "junk" || n.includes("spam")) return "ban";
  if (n.includes("archive")) return "box-archive";
  return open ? "folder-open" : "folder";
}

/**
 * How a folder's path is shown: the Online Archive is part of the mailbox, so
 * "Online Archive/Inbox" shows as "Inbox" (one folder with the mailbox's own Inbox) and
 * "Online Archive/Inbox_2021" as "Inbox_2021". "" = the archive's top, which holds no mail.
 */
function shownPath(path) {
  if (path === "Online Archive") return "";
  return path.startsWith("Online Archive/") ? path.slice("Online Archive/".length) : path;
}

/** A folder's key (also for remembering the last one open): the account, then its path as shown. */
function folderKey(f) {
  return `#${f.account_id}/${shownPath(f.path)}`;
}

/**
 * How many messages a folder view holds: one folder's own count, else counted by the library
 * (a message both in a folder and in its archive twin counts once there, as the list shows it).
 */
async function folderTotal(ids, folders = state.folders) {
  if (ids.length === 1) return folders.find((f) => f.id === ids[0])?.messages ?? 0;
  return invoke("count", { folders: ids });
}

function folderName(f) {
  return f.path.split("/").pop();
}

function accountTip(a) {
  const lines = [`${a.address || a.name} · ${a.kind_name}`];
  if (a.label) lines.push(`Shown as “${a.label}” (Settings)`);
  if (a.source) lines.push(`From ${a.source}`);
  lines.push(`${number.format(a.messages)} messages`);
  return lines.join("\n");
}

/**
 * One account's folders as a tree, the Online Archive's merged in (a folder and its archive
 * twin are one node: `folders` holds both ids); folders that only exist as parents get none.
 */
function folderTree(account) {
  const root = { children: new Map() };
  for (const f of state.folders.filter((f) => f.account_id === account.id)) {
    const parts = shownPath(f.path).split("/").filter(Boolean);
    if (!parts.length) continue;
    let node = root;
    parts.forEach((part, i) => {
      if (!node.children.has(part)) {
        node.children.set(part, { name: part, path: parts.slice(0, i + 1).join("/"), key: `#${account.id}/${parts.slice(0, i + 1).join("/")}`, children: new Map(), folders: [], messages: 0, total: 0, unread: 0, unreadTotal: 0 });
      }
      node = node.children.get(part);
    });
    node.folders.push(f.id);
    node.messages += f.messages;
    node.unread += state.unread.get(f.id) || 0;
  }
  (function sum(node) {
    node.total = node.messages || 0;
    node.unreadTotal = node.unread || 0;
    for (const child of node.children.values()) {
      node.total += sum(child);
      node.unreadTotal += child.unreadTotal;
    }
    return node.total;
  })(root);
  return root;
}

/** What a list query needs: its folders, or its Synthetic View and mailboxes. */
function listArgs(list) {
  return {
    folders: list.folders || [],
    hideSent: list.kind === "all" && state.hideSent,
    filtered: list.kind === "filtered",
    recent: list.kind === "recent" ? recentSince() : null,
    view: list.kind === "view" ? list.view : null,
    accounts: list.kind === "view" ? list.accounts ?? null : null,
  };
}

/** Synthetic Views' counts: "combined" (the mailboxes chosen) or an account id → [{key, name, messages}]. */
state.viewCounts = new Map();
/** A view's senders, by its sidebar key. */
state.viewGroups = new Map();

async function loadViewCounts(scope) {
  try {
    const list = await invoke("views_list", { accounts: scope === "combined" ? null : [scope] });
    state.viewCounts.set(scope, list);
    renderSidebar();
  } catch (err) {
    status(`Couldn't count the views (${err}).`);
  }
}

/** The views under a sidebar line: `scope` "combined" or an account id. */
function addViews(scope, depth, titlePrefix) {
  const list = state.viewCounts.get(scope);
  if (!list) {
    loadViewCounts(scope);
    sideItem({ key: `views:${scope}:counting`, depth, name: "…", iconName: "rectangle-list" });
    return;
  }
  const accounts = scope === "combined" ? null : [scope];
  for (const v of list) {
    const key = `view:${scope}:${v.key}`;
    const open = sideItem({
      key, depth, name: v.name, iconName: "rectangle-list", count: v.messages, children: v.messages > 0,
      tip: scope === "combined" ? `${v.name} in the mailboxes chosen for Views (Choose mailboxes…)` : `${v.name} in ${titlePrefix}`,
      view: { kind: "view", view: v.key, accounts, folders: [], title: scope === "combined" ? v.name : `${titlePrefix}: ${v.name}` },
    });
    if (!open) continue;
    // its senders (Receipts ▸ Amazon, eBay…), the biggest first
    const groups = state.viewGroups.get(key);
    if (!groups) {
      invoke("view_groups", { view: v.key, accounts }).then((g) => { state.viewGroups.set(key, g); renderSidebar(); }).catch(() => {});
      sideItem({ key: `${key}:counting`, depth: depth + 1, name: "…", iconName: "rectangle-list" });
      continue;
    }
    for (const [name, count] of groups.slice(0, 60)) {
      sideItem({
        key: `${key}/${name}`, depth: depth + 1, name, iconName: "folder", count,
        tip: `${v.name} from ${name}`,
        view: { kind: "view", view: `${v.key}/${name}`, accounts, folders: [], title: `${scope === "combined" ? "" : titlePrefix + ": "}${v.name} ▸ ${name}` },
      });
    }
    if (groups.length > 60) sideItem({ key: `${key}:more`, depth: depth + 1, name: `${groups.length - 60} more senders (search to find them)`, iconName: "folder" });
  }
  if (scope === "combined") {
    const key = "views:choose";
    sideItem({ key, depth, name: "Choose mailboxes…", iconName: "gear", cls: "f-action", tip: "Which mailboxes the combined Views show (shared mailboxes are off at first)" });
  }
}

/** Labels: the folder names across every account ("Inbox" = every account's inbox). */
function labelList() {
  const labels = new Map();
  for (const f of state.folders) {
    const name = folderName(f);
    const key = name.toLowerCase();
    if (!labels.has(key)) labels.set(key, { name, folders: [], messages: 0 });
    const label = labels.get(key);
    label.folders.push(f.id);
    label.messages += f.messages;
  }
  const first = ["inbox", "in", "sent items", "sent", "out", "drafts", "archive"];
  return [...labels.values()].sort((a, b) => {
    const ra = first.indexOf(a.name.toLowerCase()), rb = first.indexOf(b.name.toLowerCase());
    if (ra !== rb) return (ra < 0 ? 99 : ra) - (rb < 0 ? 99 : rb);
    return a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });
  });
}

/** One sidebar line. `view`: what a click opens (none: the line only opens and closes). */
function sideItem({ key, depth, name, iconName, count, unread, tip, children, view, cls, scope }) {
  const open = state.open.has(key);
  const item = el("div", `folder${cls ? " " + cls : ""}`);
  item.style.setProperty("--depth", depth);
  item.dataset.key = key;
  if (view) {
    state.views.set(key, view);
    item.dataset.view = "1";
  }
  // what's showing on the right is what's selected on the left: the Calendar, To Do, Contacts or
  // the mailbox
  const page = document.body.classList.contains("calendar-mode") ? "cal:all" : document.body.classList.contains("todo-mode") ? "todo:all" : state.contactsOn ? "contacts:all" : null;
  if (page ? key === page : state.list?.key === key) item.classList.add("on");
  const twist = el("span", "twist");
  if (children) twist.append(icon(open ? "chevron-down" : "chevron-right"));
  // its count, and its unread in bold: "4,593 (6)"
  const count_ = el("span", "f-count");
  const total = el("span", "f-total", count ? number.format(count) : "");
  if (count) {
    total.dataset.full = number.format(count);
    total.dataset.short = compactNumber.format(count);
  }
  count_.append(total);
  if (unread) {
    const u = el("b", "f-unread", `(${number.format(unread)})`);
    u.dataset.full = `(${number.format(unread)})`;
    u.dataset.short = `(${compactNumber.format(unread)})`;
    count_.append(u);
    item.classList.add("has-unread");
  }
  item.append(twist, icon(iconName));
  if (scope) item.append(searchToggle(scope, name));
  item.append(el("span", "f-name", name), count_);
  item.title = tip || name;
  $("folders").append(item);
  return open;
}

/** The sidebar's counts give way to the names: each row whose name doesn't fit shows its
 *  count short, then none. Done after every redraw and whenever the pane's width changes. */
const compactNumber = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
function fitCounts() {
  const rows = [...$("folders").querySelectorAll(".folder")]
    .map((f) => [f.querySelector(".f-name"), f.querySelector(".f-total"), f.querySelector(".f-unread")])
    .filter(([name, total, unread]) => name && (total?.dataset.full || unread));
  // (written, then read, all rows at once: a few layouts in all, however many rows); the
  // total gives way first, the unread last
  for (const [, total, unread] of rows) {
    for (const part of [total, unread]) {
      if (part?.dataset.full) {
        part.textContent = part.dataset.full;
        part.hidden = false;
      }
    }
  }
  const cut = rows.filter(([name]) => name.scrollWidth > name.clientWidth);
  for (const [, total, unread] of cut) {
    if (total?.dataset.full) total.textContent = total.dataset.short;
    if (unread) unread.textContent = unread.dataset.short;
  }
  const still = cut.filter(([name]) => name.scrollWidth > name.clientWidth);
  for (const [, total] of still) if (total) total.hidden = true;
  for (const [, , unread] of still.filter(([name]) => name.scrollWidth > name.clientWidth)) if (unread) unread.hidden = true;
}
let fitting = 0;
function fitCountsSoon() {
  if (!fitting) fitting = requestAnimationFrame(() => { fitting = 0; fitCounts(); });
}
new ResizeObserver(fitCountsSoon).observe($("folders"));
new MutationObserver(fitCountsSoon).observe($("folders"), { childList: true });

// ------------------------------------------------------------------ which mailboxes search looks in

/** Search on or off per mailbox: a magnifying glass
 *  on Accounts, Shared Mailboxes, each account and each folder; struck through = left out of
 *  search. Everything stays indexed; searches just leave those messages out (search_scope.rs),
 *  unless Include all mailboxes is on beside the search box. */
state.searchScope = { switches: {}, excluded: [] };
state.searchExcluded = new Set();
state.searchEverywhere = false;

const GLASS_SVG = (off, bang) => `<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true"><circle cx="6.6" cy="6.6" r="4.6"/><path d="M10.1 10.1l4.3 4.3"/>${off ? '<path d="M1.6 14.4L14.4 1.6" stroke="#d32f2f" stroke-width="1.9"/>' : ""}${bang ? '<path d="M6.6 4.1v3" stroke-width="1.6"/><circle cx="6.6" cy="9.2" r=".55" fill="currentColor" stroke="none"/>' : ""}</svg>`;

function sectionSearched(section) {
  return state.searchScope.switches[section] ?? true;
}
function accountSearched(a) {
  return state.searchScope.switches[`account:${a.id}`] ?? sectionSearched(a.kind === "graph-shared" ? "shared" : "accounts");
}

/** A row's magnifying glass: a click turns search on or off there (and for everything under it
 *  that has no switch of its own). */
function searchToggle(scope, name) {
  const b = el("button", `f-search${scope.searched ? "" : " off"}`);
  b.type = "button";
  b.innerHTML = GLASS_SVG(!scope.searched, false);
  b.title = scope.searched
    ? `${name} is searched. Click to leave it out of search (and what's under it, unless you switch those back on).`
    : `${name} is left out of search: its mail is still kept and indexed, but searches don't show it. Click to search it again.`;
  b.setAttribute("aria-label", `${name}: ${scope.searched ? "searched" : "left out of search"}`);
  b.setAttribute("aria-pressed", String(!scope.searched));
  for (const ev of ["mousedown", "dblclick"]) b.addEventListener(ev, (e) => e.stopPropagation());
  b.addEventListener("click", (e) => {
    e.stopPropagation();
    setSearched(scope.keys, !scope.searched, name);
  });
  return b;
}

async function setSearched(keys, searched, name) {
  try {
    for (const key of keys) state.searchScope = await invoke("search_scope_set", { key, searched });
  } catch (err) { showError("Changing what search looks in", err); return; }
  state.searchExcluded = new Set(state.searchScope.excluded);
  renderSidebar();
  showEverywhereButton();
  if (state.searchExcluded.size) invoke("search_warm").catch(() => {});
  status(searched ? `${name} is searched again.` : `${name} is left out of search (its mail is still kept). The magnifying glass beside the search box includes it in one search.`);
  if (state.list?.kind === "search") runSearch();
}

async function loadSearchScope() {
  try { state.searchScope = await invoke("search_scope_get"); } catch { return; }
  state.searchExcluded = new Set(state.searchScope.excluded);
  showEverywhereButton();
  renderSidebar();
  // (the search's connection works out what to leave out now, not at the first search)
  if (state.searchExcluded.size) invoke("search_warm").catch(() => {});
}

/** Beside the search box (only while some mailbox is left out): a magnifying glass with "!",
 *  gray while searches leave those mailboxes out, bold and colored while they include them. */
function showEverywhereButton() {
  const b = $("search-all");
  const left = state.searchExcluded.size;
  b.hidden = !left;
  if (!left) state.searchEverywhere = false;
  b.classList.toggle("on", state.searchEverywhere);
  b.innerHTML = GLASS_SVG(false, true);
  b.setAttribute("aria-pressed", String(state.searchEverywhere));
  b.title = state.searchEverywhere
    ? "Include all mailboxes: on. Searches look in every mailbox, also the ones you left out. Click to leave those out again."
    : `Include all mailboxes: off. ${number.format(left)} ${left === 1 ? "folder is" : "folders are"} left out of search (the struck-through magnifying glasses on the left). Click to search them too.`;
}
$("search-all").addEventListener("click", () => {
  state.searchEverywhere = !state.searchEverywhere;
  showEverywhereButton();
  if ($("query").value.trim()) runSearch();
});

function renderSidebar() {
  const box = $("folders");
  box.replaceChildren();
  state.views = new Map();
  const total = state.accounts.total;
  // contacts: at the top, above the mail, each account's address books under it;
  // a click opens the Address Book on them
  const books = state.cardCounts || [];
  if (books.length) {
    const total = books.reduce((n, b) => n + b[3], 0);
    const contactsOpen = sideItem({
      key: "contacts:all", depth: 0, name: "Contacts", iconName: "address-book", count: total, children: true, cls: "accounts",
      tip: `${number.format(total)} contacts in ${books.length} address books (click: the Address Book, Ctrl+L)`,
    });
    if (state.contactsOn) $("folders").lastElementChild.classList.add("on");
    if (contactsOpen) {
      for (const [account, address, book, count] of books) {
        sideItem({ key: `contacts:${account}:${book}`, depth: 1, name: books.filter((b) => b[0] === account).length > 1 ? `${address} › ${book}` : address, iconName: "address-book", count, tip: `${address} › ${book}: ${number.format(count)} contacts` });
      }
    }
  }
  // the calendars (calendar.js), under Contacts
  window.calendarSidebar?.();
  // To Do (todo.js), under the calendar
  window.todoSidebar?.();
  // what was sorted away lately: at the top, above the filtered Inbox
  sideItem({
    key: "recent", depth: 0, name: "Recent Mail", iconName: "inbox", count: state.recentTotal ?? 0, cls: "all",
    tip: `What the spam filter and your rules sorted out of the Inbox in the last ${recentHours()} hours (marketing, notifications, mailing lists), without spam. Settings ▸ General sets the hours.`,
    view: { kind: "recent", folders: [], title: "Recent Mail" },
  });
  // the default view: what the filter leaves for the Inbox, in every account
  sideItem({
    key: "filtered", depth: 0, name: "All Mail - Filtered", iconName: "inbox", count: state.filteredTotal ?? 0, cls: "all",
    tip: "Your mail in every account, without spam, marketing, notifications, mailing lists, trash, drafts or what you sent",
    view: { kind: "filtered", folders: [], title: "All Mail - Filtered" },
  });
  sideItem({ key: "all", depth: 0, name: "All Mail", iconName: "layer-group", count: total, cls: "all", tip: `Every message in every account, once: ${number.format(total)}`, view: { kind: "all", folders: [], title: "All Mail" } });

  if (state.organize === "labels") {
    for (const label of labelList()) {
      const key = `label:${label.name.toLowerCase()}`;
      sideItem({
        key, depth: 0, name: label.name, iconName: "tag", count: label.messages,
        tip: `${label.name}: the folders of this name in every account (${label.folders.length})`,
        view: { kind: "label", folders: label.folders, title: label.name },
      });
    }
    return;
  }

  // Synthetic Views across the mailboxes chosen for them
  if (sideItem({ key: "#views", depth: 0, name: "Views", iconName: "rectangle-list", children: true, tip: "Receipts, Invoices, Renewals, Bank Transactions, New Accounts, 2FA codes: messages wherever they're filed" })) {
    addViews("combined", 1);
  }

  const accountsOpen = sideItem({
    key: "#accounts", depth: 0, name: "Accounts", iconName: "users", count: total, children: true, cls: "accounts",
    tip: `${state.accounts.accounts.length} accounts, ${number.format(total)} messages (each counted once)`,
    scope: { keys: ["accounts"], searched: sectionSearched("accounts") },
  });
  const own = state.accounts.accounts.filter((a) => a.kind !== "graph-shared");
  const sharedOnes = state.accounts.accounts.filter((a) => a.kind === "graph-shared");
  if (accountsOpen) for (const a of own) renderAccount(a, 1, true);
  // shared mailboxes: a section of their own below Accounts, each active
  // (downloaded in the background) or inactive (looked at only when opened and Check Mail is
  // clicked there); right-click one to choose
  if (sharedOnes.length) {
    const sharedOpen = sideItem({
      key: "#shared", depth: 0, name: "Shared Mailboxes", iconName: "users", count: sharedOnes.reduce((n, a) => n + a.messages, 0), children: true, cls: "accounts",
      scope: { keys: ["shared"], searched: sectionSearched("shared") },
      tip: `${sharedOnes.length} shared mailboxes. Right-click one to make it active (downloaded in the background) or inactive (checked only when you open it and click Check Mail).`,
    });
    if (sharedOpen) for (const a of sharedOnes) renderAccount(a, 1, false);
  }
}

/** The sidebar's contact counts (read on the slow connection; drawn when they come). */
async function loadCardCounts() {
  try {
    state.cardCounts = await invoke("card_counts");
    renderSidebar();
  } catch { /* (no address book yet) */ }
}

/** Whether a shared mailbox is inactive (not downloaded in the background). */
function sharedInactive(accountId) {
  const a = state.accounts?.accounts.find((x) => x.id === accountId);
  return a?.kind === "graph-shared" && !(state.sharedActive || []).some((s) => s.toLowerCase() === (a.address || "").toLowerCase());
}

/** One account in the sidebar: its folder tree (and, for the user's own, its Views). */
function renderAccount(a, depth, withViews) {
  {
    const tree = folderTree(a);
    const inactive = sharedInactive(a.id);
    const open = sideItem({
      key: `#${a.id}`, depth, name: inactive ? `${a.shown} (inactive)` : a.shown, iconName: "envelope", count: a.messages, children: tree.children.size > 0,
      cls: inactive ? "account inactive" : "account",
      scope: { keys: [`account:${a.id}`], searched: accountSearched(a) },
      tip: accountTip(a) + (a.kind === "graph-shared" ? (inactive ? "\nInactive: not downloaded; open a folder and click Check Mail to bring it up to date. Right-click to make it active." : "\nActive: downloaded in the background. Right-click to make it inactive.") : ""),
    });
    if (!open) return;
    const add = (node, depth) => {
      const children = [...node.children.values()].sort((x, y) => x.name.localeCompare(y.name, undefined, { numeric: true, sensitivity: "base" }));
      for (const child of children) {
        const childOpen = state.open.has(child.key);
        const shownCount = childOpen || !child.children.size ? child.messages : child.total;
        const shownUnread = childOpen || !child.children.size ? child.unread : child.unreadTotal;
        sideItem({
          key: child.key, depth, name: child.name, iconName: folderIcon(child.name, childOpen && child.children.size),
          count: shownCount, unread: shownUnread, children: child.children.size > 0,
          scope: child.folders.length ? { keys: child.folders.map((f) => `folder:${f}`), searched: !child.folders.every((f) => state.searchExcluded.has(f)) } : null,
          tip: `${a.shown} › ${child.key.slice(child.key.indexOf("/") + 1)}\n${number.format(child.messages)} messages here, ${number.format(child.total)} with the folders inside`,
          view: child.folders.length ? { kind: "folder", folders: child.folders, title: `${a.shown}: ${child.path}` } : null,
        });
        if (childOpen) add(child, depth + 1);
      }
    };
    add(tree, depth + 1);
    // this mailbox's own views
    if (withViews && sideItem({ key: `#${a.id}/#views`, depth: depth + 1, name: "Views", iconName: "rectangle-list", children: true, tip: `Receipts, Invoices… in ${a.shown}` })) {
      addViews(a.id, depth + 2, a.shown);
    }
  }
}

// right-click on a shared mailbox: active or inactive
/** The folders a sidebar row stands for, with everything under it in the tree: an account's
 *  every folder, a folder's own and its subfolders' (the Online Archive's side too). */
function foldersUnder(key) {
  const m = /^#(\d+)(?:\/(.+))?$/.exec(key || "");
  if (!m || m[2]?.startsWith("#")) return [];
  const account = Number(m[1]);
  const path = m[2];
  return state.folders
    .filter((f) => f.account_id === account)
    .filter((f) => {
      if (!path) return true;
      const shown = shownPath(f.path);
      return shown === path || shown.startsWith(`${path}/`);
    })
    .map((f) => f.id);
}

/** Mark All as Read: the numbers and rows on screen at once; the messages marked in the
 *  background, a batch at a time (the window never waits), then the servers told. */
async function markAllRead(key, label) {
  const folders = foldersUnder(key);
  const unread = folders.reduce((n, f) => n + (state.unread.get(f) || 0), 0);
  if (!folders.length) return;
  if (unread > 500 && !(await askYesNo("Mark All as Read", `Mark ${number.format(unread)} unread messages in ${label} and the folders under it as read? Microsoft 365 and your mail servers are told in the background.`, "Mark them read"))) return;
  for (const f of folders) state.unread.delete(f);
  renderSidebar();
  const inThem = new Set(folders);
  const shown = [...(state.list?.rows.values() || [])].filter((r) => r.flags & UNREAD && (r.labels || []).some(([f]) => inThem.has(f)));
  patchRows(shown.map((r) => r.id), (r) => { r.flags &= ~UNREAD; });
  status(`Marking ${unread ? number.format(unread) : "the"} unread messages in ${label} as read, in the background…`);
  try {
    const n = await invoke("mark_folders_read", { folders });
    status(`${number.format(n)} ${n === 1 ? "message" : "messages"} in ${label} marked read; the servers are told in the background (Tools ▸ Activity).`);
  } catch (err) { showError("Marking all as read", err); }
  recountUnread(true);
}

const folderMenu = el("div", "popup-menu");
folderMenu.hidden = true;
document.body.append(folderMenu);
document.addEventListener("click", (e) => { if (!folderMenu.contains(e.target)) folderMenu.hidden = true; });

$("folders").addEventListener("contextmenu", async (e) => {
  const item = e.target.closest(".folder");
  const key = item?.dataset.key || "";
  // a calendar: its color (calendar.js)
  if (window.calendarContext?.(key, e)) return;
  if (window.todoContext?.(key, e)) return;
  const id = Number(key.match(/^#(\d+)$/)?.[1]);
  const a = state.accounts?.accounts.find((x) => x.id === id);
  const label = item?.querySelector(".f-name")?.textContent || "this folder";
  const items = [];
  if (foldersUnder(key).length) items.push(["Mark All as Read", () => markAllRead(key, label)]);
  if (a?.kind === "graph-shared") items.push([sharedInactive(a.id) ? "Make it active…" : "Make it inactive…", () => sharedChoice(a)]);
  // mailboxes made, renamed and deleted, here and on the server
  const fm = /^#(\d+)(?:\/(.+))?$/.exec(key);
  if (fm && !fm[2]?.startsWith("#")) {
    const account = Number(fm[1]);
    const path = fm[2] || "";
    items.push([path ? "New Mailbox Inside…" : "New Mailbox…", () => newMailboxIn(account, path)]);
    const own = path && state.folders.find((f) => f.account_id === account && f.path === path);
    if (own && !specialFolder(path)) {
      items.push(["Rename…", () => renameMailbox(own, key)]);
      items.push(["Delete…", () => deleteMailbox(own, key)]);
    }
  }
  if (!items.length) return;
  e.preventDefault();
  folderMenu.replaceChildren(...items.map(([text, act]) => {
    const b = el("button", "", text);
    b.addEventListener("click", () => { folderMenu.hidden = true; act(); });
    return b;
  }));
  folderMenu.hidden = false;
  folderMenu.style.left = `${Math.min(e.clientX, innerWidth - folderMenu.offsetWidth - 8)}px`;
  folderMenu.style.top = `${Math.min(e.clientY, innerHeight - folderMenu.offsetHeight - 8)}px`;
});

/** The server's own top-level folders (never renamed or deleted; the core checks too). */
const SPECIAL_FOLDERS = ["inbox", "sent items", "sent", "sent messages", "drafts", "deleted items", "deleted messages", "trash", "junk email", "junk", "spam", "outbox", "out",
  "archive", "conversation history", "sync issues", "rss feeds", "rss subscriptions", "calendar", "contacts", "tasks", "notes", "journal", "online archive"];
function specialFolder(path) {
  return path.startsWith("Online Archive") || (!path.includes("/") && SPECIAL_FOLDERS.includes(path.toLowerCase()));
}

/** A mailbox made inside another (or at the account's top), here and on the server. */
async function newMailboxIn(account, path) {
  const name = (await askText(path ? `New Mailbox in ${path}` : "New Mailbox", "Name (use / for levels inside it)", ""))?.trim();
  if (!name) return;
  try {
    await invoke("folder_create", { account, path: path ? `${path}/${name}` : name });
    state.open.add(`#${account}`);
    if (path) state.open.add(`#${account}/${path}`);
    await refresh(true);
    status(`Made ${path ? `${path}/` : ""}${name}${serverNote(account)}.`);
  } catch (err) { showError("Making the mailbox", err); }
}

function serverNote(account) {
  const a = state.accounts?.accounts.find((x) => x.id === account);
  return a && ["graph", "graph-shared", "imap"].includes(a.kind) ? "; the server is changed in the background" : "";
}

/** Renames a mailbox (the folders inside go with it), here and on the server. */
async function renameMailbox(f, key) {
  const old = f.path.split("/").pop();
  const name = (await askText(`Rename ${old}`, "New name", old, "Rename"))?.trim();
  if (!name || name === old) return;
  try {
    const to = await invoke("folder_rename", { folder: f.id, name });
    const newKey = `#${f.account_id}/${to}`;
    if (state.open.has(key)) state.open.add(newKey);
    await refresh(true);
    if (state.list?.key === key) openView(newKey);
    status(`Renamed ${f.path} to ${to}${serverNote(f.account_id)}.`);
  } catch (err) { showError("Renaming the mailbox", err); }
}

/** Deletes a mailbox as Outlook does: into Deleted Items with its mail and folders. */
async function deleteMailbox(f, key) {
  const inside = foldersUnder(key);
  const count = inside.reduce((n, id) => n + (state.folders.find((x) => x.id === id)?.messages || 0), 0);
  const text = `${f.path}${inside.length > 1 ? ` and the ${inside.length - 1} mailbox${inside.length > 2 ? "es" : ""} inside it` : ""} (${number.format(count)} message${count === 1 ? "" : "s"}) go${inside.length > 1 ? "" : "es"} into Deleted Items, here${serverNote(f.account_id) ? " and on the server" : ""}. Empty Trash removes ${count === 1 ? "it" : "them"} for good.`;
  if (!(await askYesNo(`Delete ${f.path.split("/").pop()}?`, text, "Delete it"))) return;
  try {
    const to = await invoke("folder_delete", { folder: f.id });
    await refresh(true);
    if (state.list?.key === key) openView(to ? `#${f.account_id}/${to}` : "filtered");
    status(to ? `${f.path} is now in ${to}${serverNote(f.account_id)}.` : `${f.path} removed (it was empty, in the Trash).`);
  } catch (err) { showError("Deleting the mailbox", err); }
}

/** A shared mailbox made active or inactive. */
async function sharedChoice(a) {
  const inactive = sharedInactive(a.id);
  const choice = await ask({
    title: a.shown,
    text: inactive
      ? "This shared mailbox is inactive: it isn't downloaded in the background. Open one of its folders and click Check Mail to bring that folder up to date."
      : "This shared mailbox is active: it's downloaded in the background with your own mail.",
    buttons: [
      { label: "Cancel", value: null },
      inactive ? { label: "Make it active", value: "active", primary: true } : { label: "Make it inactive", value: "inactive", primary: true },
    ],
  });
  if (!choice) return;
  const list = (state.sharedActive || []).filter((s) => s.toLowerCase() !== (a.address || "").toLowerCase());
  if (choice === "active") list.push(a.address);
  try {
    await invoke("set_setting", { key: "shared-active", value: JSON.stringify(list) });
    state.sharedActive = list;
    renderSidebar();
    status(choice === "active" ? `${a.shown} is active: it's downloaded from the next pass on.` : `${a.shown} is inactive: it's no longer downloaded in the background.`);
  } catch (err) { showError("Changing the shared mailbox", err); }
}

function toggle(key) {
  state.open.has(key) ? state.open.delete(key) : state.open.add(key);
  store.set("openFolders", [...state.open]);
  renderSidebar();
}

$("folders").addEventListener("click", (e) => {
  const item = e.target.closest(".folder");
  if (!item) return;
  const key = item.dataset.key;
  if (key === "views:choose") { chooseViewMailboxes(); return; }
  if (window.calendarClick?.(key, e)) return;
  if (window.todoClick?.(key, e)) return;
  // Contacts: the twist opens the list of address books, a click opens the Address Book
  if (key.startsWith("contacts:") && !e.target.closest(".twist")) {
    const account = key === "contacts:all" ? null : Number(key.split(":")[1]);
    openAddressBook(account);
    return;
  }
  if (e.target.closest(".twist") || !item.dataset.view) {
    // a heading (an account, Accounts, Views, Shared Mailboxes) clicked from the Calendar, To Do
    // or Contacts: the mail comes back too; its arrow only opens or closes it
    if (!e.target.closest(".twist")) showMail();
    toggle(key);
    return;
  }
  openView(key);
});

/** Back to the mail from the Calendar, To Do or Contacts: the list that was open. */
function showMail() {
  if (!(document.body.classList.contains("calendar-mode") || document.body.classList.contains("todo-mode") || state.contactsOn)) return;
  window.closeCalendar?.();
  window.closeTodo?.();
  window.closeContacts?.();
  renderList(true);
}

$("folders").addEventListener("dblclick", (e) => {
  const item = e.target.closest(".folder");
  if (item && !e.target.closest(".twist") && item.dataset.view) toggle(item.dataset.key);
});

// ------------------------------------------------------------------ message list (virtual)

let rowHeight = 20;
const rowPool = new Map(); // index → element

function clearRows() {
  for (const e of rowPool.values()) e.remove();
  rowPool.clear();
}

function listCount(list) {
  return list.kind === "search" ? list.rows.size : list.total;
}

function setList(list) {
  state.list = list;
  if (list.kind !== "search") $("search-help").hidden = true;
  state.selected = -1;
  clearSelection();
  closeMessage();
  clearRows();
  $("list").scrollTop = 0;
  $("list-name").textContent = list.title;
  $("list-subs").hidden = !/(^|[/›] ?)(Notifications|Bulk|Marketing|Lists)\b/.test(list.title ?? "");
  showListCount();
  renderList();
  renderSidebar();
}

function showListCount() {
  const list = state.list;
  $("list-count").textContent = list.kind === "search"
    ? `${number.format(list.total)} found${list.total > list.rows.size ? `, newest ${number.format(list.rows.size)} listed` : ""}`
    : `${number.format(list.total)} messages`;
}

/** Mailbox ▸ Hide Mail I Sent in All Mail: on by default. */
state.hideSent = store.get("hideSent", true) !== false;

/** How many messages All Mail shows (without what you sent, when that's hidden). */
async function allMailTotal() {
  if (!state.hideSent) return state.accounts.total;
  try { return await invoke("count", { folders: [], hideSent: true }); } catch { return state.accounts.total; }
}

function toggleHideSent() {
  state.hideSent = !state.hideSent;
  store.set("hideSent", state.hideSent);
  status(state.hideSent
    ? "All Mail now leaves out mail you sent (from your own addresses, wherever it's filed). Mailbox ▸ Hide Mail I Sent in All Mail turns it off."
    : "All Mail now shows everything, mail you sent included.");
  if (state.list?.kind === "all") openView(state.list.key);
}

/** Opens a sidebar item's list (All Mail, a folder, a label). */
/** The open list's rows read again (its count changed, or it was shown from the cache): the
 *  rows on screen stay until the new ones come, then are drawn again (2026-10-18: they stayed as
 *  they were while the rows under them moved, so a click opened the message now in that place);
 *  the selection and the message shown are found again by id. */
function replaceRows(list, total) {
  // new mail comes in at the top (lists are newest first): every row moves down by that many. The
  // rows, the selection and the scroll move with it at once, so the message being read stays
  // where it is on screen and new mail slots in silently above it; the fresh rows then confirm
  // it, found by id (sorted by another column, new mail isn't at the top: found again by id only)
  const reading = !list.cached && !listSort().custom && (state.selected >= 0 || $("list").scrollTop > 0);
  list.reselect = {
    ids: state.allSel ? null : [...state.sel].map((i) => list.rows.get(i)?.id).filter((id) => id != null),
    shown: list.rows.get(state.selected)?.id ?? state.messageId ?? null,
    // how far the rows moved is found from the first page: where the old top row is now (else
    // how many more there are); until then nothing on screen moves
    keep: reading,
    oldTop: list.rows.get(0)?.id ?? null,
    grew: list.total != null ? Math.max(0, total - list.total) : 0,
    shift: reading ? null : 0,
    // where the one shown should be now (its page is read next)
    expected: -1,
  };
  list.total = total;
  list.stale = list.rows;
  list.rows = new Map();
  for (const e of rowPool.values()) e.classList.add("stale");
  showListCount();
  if (reading) loadPage(0);
  else renderList(true);
}

/** The first page of a refreshed list is in: the rows on screen, the selection and the scroll
 *  move down by the new mail above them, so what's on screen stays put. */
function settleShift(list) {
  const r = list.reselect;
  let shift = r.grew;
  for (const [i, row] of list.rows) {
    if (row.id === r.oldTop) { shift = i; break; }
  }
  r.shift = shift;
  if (shift > 0) {
    list.stale = new Map([...list.stale].map(([i, row]) => [i + shift, row]));
    for (const e of rowPool.values()) e.remove();
    rowPool.clear();
    if (!state.allSel) state.sel = new Set([...state.sel].map((i) => i + shift));
    if (state.selected >= 0) state.selected += shift;
    if (state.anchor >= 0) state.anchor += shift;
    $("list-space").style.height = `${listCount(list) * rowHeight}px`;
    $("list").scrollTop += shift * rowHeight;
  }
  r.expected = state.selected;
  if (r.expected >= 0) loadPage(Math.floor(r.expected / PAGE));
}

/** A folder opened: its newest mail fetched first, unless it's an inactive shared mailbox's
 *  (those only on Check Mail). */
function lookAtFolders(folders) {
  const wanted = folders.filter((id) => !sharedInactive(state.folders.find((f) => f.id === id)?.account_id));
  if (wanted.length) invoke("look_now", { folders: wanted }).catch(() => {});
}

/** All Mail - Filtered's count: the last one at once, counted again in the background (it reads
 *  every message: seconds), the list caught up and the number changed when it's in. */
/** Unread messages per folder: the last count at once (kept for the next start), counted again on the second
 *  connection every 10 minutes and soon after the user marks mail read or unread; marks made here
 *  change the numbers on screen at once. */
state.unread = new Map(store.get("unreadCounts", []) || []);
let unreadAt = 0;
let unreadCounting = false;
async function recountUnread(soon = false) {
  if (unreadCounting || (!soon && Date.now() - unreadAt < 600_000)) return;
  unreadCounting = true;
  unreadAt = Date.now();
  try {
    const counts = await invoke("unread_counts");
    state.unread = new Map(counts);
    try { store.set("unreadCounts", counts); } catch { /* (too big: this session only) */ }
    renderSidebar();
  } catch (err) { console.warn("unread counts", err); }
  finally { unreadCounting = false; }
}
/** Messages marked read (`by` < 0) or unread here: the folders' numbers at once. */
function adjustUnread(rows, by) {
  for (const row of rows) {
    for (const [folder] of row.labels || []) state.unread.set(folder, Math.max(0, (state.unread.get(folder) || 0) + by));
  }
  renderSidebar();
  clearTimeout(adjustUnread.timer);
  adjustUnread.timer = setTimeout(() => recountUnread(true), 30_000);
}
if (!READER) {
  setTimeout(() => recountUnread(true), 8000);
  setInterval(() => recountUnread(), 60_000);
}

state.filteredTotal = store.get("filteredTotal", null);
let filteredCounting = false;
function filteredTotal() {
  if (!filteredCounting) {
    filteredCounting = true;
    invoke("count", { folders: [], filtered: true })
      .then((n) => {
        const changed = n !== state.filteredTotal;
        state.filteredTotal = n;
        store.set("filteredTotal", n);
        const list = state.list;
        if (list?.kind === "filtered" && !list.cached && list.total !== n) replaceRows(list, n);
        if (changed) renderSidebar();
      })
      .catch((err) => console.warn("counting filtered", err))
      .finally(() => { filteredCounting = false; });
  }
  return state.filteredTotal ?? 1000;
}

/** Recent Mail's hours: setting `recent-mail` = {hours, monday}; Saturday and Sunday take the usual hours. */
state.recentPrefs = { hours: 48, monday: 72 };
function recentHours(at = new Date()) {
  const p = state.recentPrefs;
  return at.getDay() === 1 ? p.monday : p.hours;
}
/** Where Recent Mail begins (seconds), to the 10 minutes, so its pages stay one list while it's open. */
function recentSince() {
  const now = Math.floor(Date.now() / 1000);
  return Math.floor((now - recentHours() * 3600) / 600) * 600;
}
async function loadRecentPrefs() {
  try {
    const v = JSON.parse((await invoke("setting", { key: "recent-mail" })) || "null");
    if (v) state.recentPrefs = { hours: Number(v.hours) || 48, monday: Number(v.monday) || 72 };
  } catch { /* the defaults */ }
}
state.recentTotal = store.get("recentTotal", null);
let recentCounting = false;
function recentTotal() {
  if (!recentCounting) {
    recentCounting = true;
    invoke("count", { folders: [], recent: recentSince() })
      .then((n) => {
        const changed = n !== state.recentTotal;
        state.recentTotal = n;
        store.set("recentTotal", n);
        const list = state.list;
        if (list?.kind === "recent" && !list.cached && list.total !== n) replaceRows(list, n);
        if (changed) renderSidebar();
      })
      .catch((err) => console.warn("counting recent mail", err))
      .finally(() => { recentCounting = false; });
  }
  return state.recentTotal ?? 100;
}

/** How many messages a list holds (All Mail - Filtered: the last count, recounted lazily). */
async function listTotal(list) {
  if (list.kind === "recent") return recentTotal();
  if (list.kind === "all") return allMailTotal();
  if (list.kind === "filtered") return filteredTotal();
  if (list.kind === "folder") return folderTotal(list.folders);
  return invoke("count", listArgs(list));
}

/** A list's size as already known, without asking the library: the folders' totals, the View's
 *  number in the sidebar, the last counts (an estimate until the exact count comes). */
function quickTotal(list) {
  if (list.kind === "all") return state.accounts?.total ?? 1000;
  if (list.kind === "filtered") return state.filteredTotal ?? 1000;
  if (list.kind === "recent") return state.recentTotal ?? 100;
  if (list.kind === "view") {
    const scope = list.accounts?.length === 1 ? list.accounts[0] : "combined";
    const found = (state.viewCounts.get(scope) || []).find((v) => v.key === list.view);
    return found?.messages ?? 200;
  }
  const ids = new Set(list.folders || []);
  const sum = state.folders.filter((f) => ids.has(f.id)).reduce((n, f) => n + (f.messages || 0), 0);
  return sum || 200;
}

/** A list shown from the cache, brought up to date: its count, then its rows. */
async function catchUpList(list) {
  const total = await listTotal(list);
  if (state.list !== list) return;
  list.cached = false;
  replaceRows(list, total);
  if (list.kind === "folder") lookAtFolders(list.folders);
}

// ------------------------------------------------------------------ the start-up cache

/** How long each start-up step took (ms since the page began), to see a slow one. */
const startTimes = [];
window.startTimes = startTimes;
function mark(step) {
  startTimes.push([step, Math.round(performance.now())]);
}

const CACHE_KEY = "startCache";
const CACHED_ROWS = 60;

/** Keeps what's on screen for the next start: the folders, the accounts, the open list's first rows. */
function saveCache() {
  const list = state.list;
  if (READER || !list || list.kind === "search" || !state.folders.length || !state.accounts) return;
  const rows = [];
  for (let i = 0; i < CACHED_ROWS && list.rows.has(i); i++) rows.push(list.rows.get(i));
  try {
    store.set(CACHE_KEY, { folders: state.folders, accounts: state.accounts, cards: state.cardCounts || [], shared: state.sharedActive || [], views: [...state.viewCounts], list: { key: list.key, total: list.total, rows } });
  } catch { /* (storage full or refused: the next start is simply slower) */ }
}

/** The last start's folders, accounts and list, shown at once (true when there was one). */
function showCached() {
  let cache;
  try { cache = store.get(CACHE_KEY, null); } catch { cache = null; }
  if (!cache?.folders?.length || !cache.accounts || !cache.list) return false;
  try {
    state.folders = cache.folders;
    state.accounts = cache.accounts;
    state.cardCounts = cache.cards || [];
    state.sharedActive = cache.shared || [];
    for (const [scope, list] of cache.views || []) state.viewCounts.set(scope, list); // (recounted quietly after the start)
    const last = cache.list.key;
    if (last.startsWith("#")) {
      const parts = last.split("/");
      for (let i = 1; i < parts.length; i++) state.open.add(parts.slice(0, i).join("/"));
      state.open.add("#accounts");
    }
    renderSidebar();
    const view = state.views.get(last);
    if (!view) return false;
    setList({ ...view, key: last, total: cache.list.total, rows: new Map(cache.list.rows.map((r, i) => [i, r])), pending: new Set(), cached: true });
    // the message that was open, open again
    const was = store.get("lastMessage", null);
    const at = was?.key === last ? cache.list.rows.findIndex((r) => r.id === was.id) : -1;
    if (at >= 0) select(at);
    status("Showing your mail as it was; catching up…");
    return true;
  } catch (err) {
    console.warn("start-up cache", err);
    return false;
  }
}

/** Each view's first rows as last read (key → {total, rows}): a view opened again shows them at
 *  once and catches up behind. The 40 most recent in memory, the 12 most recent kept for the next start. */
const viewCache = new Map(Object.entries(store.get("viewCache", {}) || {}));
const VIEW_CACHE_MEMORY = 60;
const VIEW_CACHE_KEPT = 40;
let viewCacheTimer = 0;

function rememberView(list) {
  if (!list?.key || list.kind === "search") return;
  const rows = [];
  for (let i = 0; i < CACHED_ROWS && list.rows.has(i); i++) rows.push(list.rows.get(i));
  viewCache.delete(list.key);
  viewCache.set(list.key, { total: list.total, rows });
  while (viewCache.size > VIEW_CACHE_MEMORY) viewCache.delete(viewCache.keys().next().value);
  clearTimeout(viewCacheTimer);
  viewCacheTimer = setTimeout(() => {
    const kept = [...viewCache.entries()].slice(-VIEW_CACHE_KEPT);
    try { store.set("viewCache", Object.fromEntries(kept)); } catch { /* (storage full: only this session's) */ }
  }, 2000);
}

/** Keeps lists ready before they're clicked: every 2
 *  minutes, while the user isn't busy, the first rows of the lists opened lately and of the folders
 *  and Views in sight are read on the background connection into the cache a click shows at once.
 *  (The counts are ready already: kept by the database, and the sidebar's.) */
let warming = false;
async function warmLists() {
  if (READER || warming || listSort().custom || Date.now() - lastInput < 3000) return;
  warming = true;
  try {
    const keys = [];
    const add = (k) => { if (k && !keys.includes(k) && k !== state.list?.key && state.views.has(k)) keys.push(k); };
    [...viewCache.keys()].reverse().forEach(add); // (opened lately first)
    for (const e of document.querySelectorAll("#folders [data-key]")) add(e.dataset.key);
    for (const key of keys.slice(0, 40)) {
      if (Date.now() - lastInput < 3000) break; // (the user is back: later)
      const view = state.views.get(key);
      try {
        const rows = await invoke("page", { ...listArgs(view), page: 0, size: CACHED_ROWS, background: true });
        viewCache.delete(key);
        viewCache.set(key, { total: Math.max(quickTotal(view), rows.length), rows });
      } catch { /* (tried again next round) */ }
    }
    while (viewCache.size > VIEW_CACHE_MEMORY) viewCache.delete(viewCache.keys().next().value);
    try { store.set("viewCache", Object.fromEntries([...viewCache.entries()].slice(-VIEW_CACHE_KEPT))); } catch { /* (storage full) */ }
  } finally {
    warming = false;
  }
}
if (!READER) {
  setTimeout(warmLists, 20_000);
  setInterval(warmLists, 120_000);
}

async function openView(key) {
  const view = state.views.get(key);
  if (!view) return;
  window.closeCalendar?.(); // (a mailbox chosen: the mail comes back in place of the calendar)
  window.closeTodo?.();
  window.closeContacts?.();
  store.set("lastView", key);
  const seen = viewCache.get(key);
  // (kept newest first: sorted by another column, the list is read in its order)
  if (seen?.rows?.length && !listSort().custom) {
    // shown at once as it was last time, then brought up to date in place
    setList({ ...view, key, total: seen.total, rows: new Map(seen.rows.map((r, i) => [i, r])), pending: new Set(), cached: true });
    await catchUpList(state.list);
    return;
  }
  // shown at once with the number already known (the sidebar's), its rows read on the listing
  // connection; the exact count follows
  setList({ ...view, key, total: quickTotal(view), rows: new Map(), pending: new Set() });
  const list = state.list;
  listTotal(view).then((total) => {
    // (the rows read are right: only the length changes)
    if (state.list === list && total !== list.total) {
      list.total = total;
      showListCount();
      renderList();
    }
  }).catch((err) => showError("Opening the list", err));
  // a mailbox opened: its newest mail fetched first; the list catches up when it comes
  if (view.kind === "folder") lookAtFolders(view.folders);
}

/** After the list changed under it: the selected messages and the one shown, found by id in the
 *  rows read so far (their places now). Done once every one is found or the first page is in. */
function reselect(list) {
  const { ids, shown, expected, shift } = list.reselect;
  if (shift == null) return; // (the first page isn't in yet: nothing moves before it)
  const indexOf = new Map([...list.rows].map(([i, row]) => [row.id, i]));
  if (ids) {
    // (until a selected message's row is read, it keeps its expected place)
    const found = ids.map((id) => indexOf.get(id)).filter((i) => i != null);
    if (found.length === ids.length || list.rows.has(0)) state.sel = new Set(found);
  }
  if (shown != null && indexOf.has(shown)) {
    const now = indexOf.get(shown);
    // somewhere else than expected (mail moved out above it): the list moves so it stays put
    if (state.selected >= 0 && now !== state.selected) $("list").scrollTop += (now - state.selected) * rowHeight;
    state.selected = now;
    state.anchor = state.selected;
  }
  const allFound = (ids || []).every((id) => indexOf.has(id)) && (shown == null || indexOf.has(shown));
  // given up only once its expected place (else the first page) has been read
  const looked = expected >= 0 ? list.rows.has(expected) && list.rows.has(0) : list.rows.has(0);
  if (allFound || looked) {
    // (the one shown left this list: it stays in the reader, no row is its place now)
    if (shown != null && !indexOf.has(shown)) state.selected = -1;
    list.reselect = null;
  }
  showSelection();
}

// ------------------------------------------------------------------ sorting by a column

/** What each column sorts by (none: it can't be sorted). */
const SORT_KEYS = { attach: "attachments", status: "status", priority: "priority", from: "from", to: "to", cc: "cc", date: "date", added: "added", size: "size", attachments: "attachments", subject: "subject", preview: "preview", list: "list", msgid: "msgid" };
/** Sorted first the natural way: numbers and dates biggest / newest first, words A to Z. */
const SORT_DOWN_FIRST = new Set(["date", "added", "size", "attachments", "status", "priority"]);
state.sort = store.get("listSort", null) || { column: "date", key: "date", asc: false };

/** The open list's order; `custom` when it isn't the usual newest first. */
function listSort() {
  const s = state.sort;
  return { ...s, custom: !(s.key === "date" && !s.asc) };
}

/** What a column sorts by in this list ("Who" is the recipient in Sent / Out / Drafts). */
function sortKeyOf(column) {
  if (column === "who") {
    const title = (state.list?.title || "").toLowerCase();
    return /(^|[/›:] ?)(sent|sent items|sent mail|sent messages|old sent items|outbox|out|drafts)\b/.test(title) ? "to" : "from";
  }
  return SORT_KEYS[column] || null;
}

/** A click on a column heading: sorted by it; the same heading again, the other way. */
function sortBy(column) {
  const key = sortKeyOf(column);
  if (!key) {
    status(`The list can't be sorted by ${columnTitle(column)}.`);
    return;
  }
  const same = state.sort.column === column;
  state.sort = { column, key, asc: same ? !state.sort.asc : !SORT_DOWN_FIRST.has(key) };
  store.set("listSort", state.sort);
  showSortMark();
  resort();
}

/** The heading sorted by shows ▲ (ascending) or ▼ (descending). */
function showSortMark() {
  for (const cell of document.querySelectorAll("#list-head .cell")) {
    cell.classList.toggle("sorted", cell.dataset.key === state.sort.column);
    cell.dataset.sort = cell.dataset.key === state.sort.column ? (state.sort.asc ? "▲" : "▼") : "";
  }
  const stacked = document.querySelector("#list-head .stacked-head");
  if (stacked) stacked.textContent = listSort().custom ? `Sorted by ${columnTitle(state.sort.column)} (${state.sort.asc ? "ascending" : "descending"})` : "By date · newest on top";
}

/** The open list read again in its new order, from the top. */
function resort() {
  const list = state.list;
  if (!list) return;
  if (list.kind === "search") {
    // the results are all here: sorted in place
    const rows = [...list.rows.values()].sort(compareRows);
    list.rows = new Map(rows.map((row, i) => [i, row]));
    clearSelection();
    state.selected = -1;
    clearRows();
    $("list").scrollTop = 0;
    renderList(true);
    return;
  }
  list.reselect = { ids: [], shown: state.messageId ?? null, shift: 0, expected: -1 };
  clearSelection();
  state.selected = -1;
  list.rows = new Map();
  list.stale = null;
  list.pending = new Set();
  list.cached = false;
  clearRows();
  $("list").scrollTop = 0;
  if (listSort().custom && list.total > 50_000) status(`Sorting ${number.format(list.total)} messages by ${columnTitle(state.sort.column)}…`);
  renderList(true);
}

/** A page of the open list, in its order (newest first, or the column chosen). */
function readPage(list, page) {
  const sort = listSort();
  return sort.custom
    ? invoke("sorted_page", { ...listArgs(list), sort: sort.key, ascending: sort.asc, total: list.total, page, size: PAGE })
    : invoke("page", { ...listArgs(list), page, size: PAGE });
}

/** Two rows in the chosen order (search results, sorted here). */
function compareRows(a, b) {
  const { key, asc } = state.sort;
  const text = (s) => String(s || "").trim().replace(/^["'<\s]+/, "").toLowerCase();
  const subject = (s) => text(s).replace(/^((re|fw|fwd|aw|wg|sv)\s?:\s*)+/i, "");
  const value = {
    date: (r) => r.date, added: (r) => r.added, size: (r) => r.size, attachments: (r) => r.attachments,
    status: (r) => r.flags & 15, priority: (r) => (r.flags >> 8) & 7,
    subject: (r) => subject(r.subject), from: (r) => text(r.from), to: (r) => text(r.to), cc: (r) => text(r.cc),
    preview: (r) => text(r.snippet), list: (r) => text(r.list_id), msgid: (r) => text(r.message_id),
  }[key] || ((r) => r.date);
  const x = value(a);
  const y = value(b);
  // empty values last, whichever way
  if ((x === "") !== (y === "")) return x === "" ? 1 : -1;
  const order = x < y ? -1 : x > y ? 1 : 0;
  return (asc ? order : -order) || b.date - a.date;
}

async function loadPage(page) {
  const list = state.list;
  if (list.kind === "search" || list.pending.has(page)) return;
  list.pending.add(page);
  const started = performance.now();
  try {
    const rows = await readPage(list, page);
    if (state.list !== list) return;
    rows.forEach((row, i) => list.rows.set(page * PAGE + i, row));
    status(undefined, `Rows ${number.format(page * PAGE + 1)}-${number.format(page * PAGE + rows.length)} read in ${(performance.now() - started).toFixed(0)} ms`);
    if (page === 0 && list.reselect?.shift == null && list.reselect) settleShift(list);
    if (list.reselect) reselect(list);
    renderList(true);
    if (page === 0 && !list.cached) {
      saveCache();
      rememberView(list);
    }
  } catch (err) {
    showError("Reading the list", err);
    // (the list never stays frozen waiting for a first page that failed)
    if (page === 0 && list.reselect?.shift == null && list.reselect) list.reselect.shift = 0;
  } finally {
    list.pending.delete(page);
  }
}

/** The folders a message is in, as labels (in a folder's own list, the other folders only). */
function labelChips(row) {
  const list = state.list;
  const own = list?.kind === "folder" ? new Set(list.folders) : new Set();
  const names = [];
  for (const [id, name] of row.labels || []) {
    if (!own.has(id) && !names.includes(name)) names.push(name);
  }
  const cats = row.categories || [];
  if (!names.length && !cats.length) return null;
  const box = el("span", "labels");
  // its labels (Change Label / Outlook categories) first, in their colors
  for (const name of cats) {
    const chip = el("span", "label cat", name);
    chip.style.setProperty("--cat", labelColor(name));
    box.append(chip);
  }
  if (!names.length) return box;
  for (const name of names.slice(0, LABELS_SHOWN)) box.append(el("span", "label", name));
  if (names.length > LABELS_SHOWN) box.append(el("span", "label more", `+${names.length - LABELS_SHOWN}`));
  box.title = `In: ${names.join(", ")}`;
  return box;
}

/** Who a row is about: the sender, except for mail you sent (Sent, Outbox, Out, Drafts, or from
 *  one of your own addresses anywhere): then whom it went to. */
function whoOf(row) {
  const title = (state.list?.title || "").toLowerCase();
  const sentList = /(^|[/›:] ?)(sent|sent items|sent mail|sent messages|old sent items|outbox|out|drafts)\b/.test(title);
  const from = (row.from || "").toLowerCase();
  const mine = (state.accounts?.accounts || []).some((a) => a.kind !== "graph-shared" && a.address && from.includes(a.address.toLowerCase()));
  return (sentList || mine) && row.to ? `To: ${row.to}` : row.from;
}

function cellText(key, row) {
  if (key === "subject") return (row.flags & FLAGGED ? "⚑ " : "") + (row.subject || "(no subject)");
  if (key.startsWith("h:")) {
    const cached = headerCache.get(row.id);
    if (!cached) { wantHeaders(row.id); return "…"; }
    return cached.get(key.slice(2)) || "";
  }
  return COLUMN_CATALOG[key]?.text?.(row) ?? "";
}

function dateSpan(cls, row) {
  const date = el("span", cls);
  if (row) {
    date.textContent = shortDate(row.date);
    date.title = row.date ? fullFormat.format(new Date(row.date * 1000)) : "No date";
  }
  return date;
}

/** A Lookout-style row: sender and date, the subject, then (new style) the first words. */
function fillStacked(e, row, lines) {
  const top = el("div", "s-top");
  top.append(el("span", "s-from", row ? whoOf(row) || "(no sender)" : "…"));
  if (row?.attachments) top.append(icon("paperclip"));
  top.append(dateSpan("s-date", row));
  const subject = el("div", "s-subject");
  if (row) {
    const chips = labelChips(row);
    if (chips) subject.append(chips);
    subject.append(document.createTextNode(row.subject || "(no subject)"));
  }
  e.append(top, subject);
  if (lines > 2) e.append(el("div", "s-preview", row ? row.snippet : ""));
}

/** A Gmail / Yahoo-style row: sender, labels + subject - preview, attachment, date. */
function fillLine(e, row) {
  e.append(el("span", "l-from", row ? whoOf(row) || "(no sender)" : "…"));
  const text = el("span", "l-text");
  if (row) {
    const chips = labelChips(row);
    if (chips) text.append(chips);
    text.append(el("span", "l-subject", row.subject || "(no subject)"));
    if (row.snippet) text.append(el("span", "l-preview", ` - ${row.snippet}`));
  }
  e.append(text);
  const end = el("span", "l-end");
  if (row?.attachments) end.append(icon("paperclip"));
  end.append(dateSpan("l-date", row));
  e.append(end);
}

function makeRow(index, row, stale = false) {
  const e = el("div", stale ? "row stale" : "row");
  e.dataset.index = index;
  e.style.transform = `translateY(${index * rowHeight}px)`;
  const s = skin();
  if (s.stacked) fillStacked(e, row, s.stacked);
  else if (s.line) fillLine(e, row);
  else {
    for (const [key] of listColumns()) {
      const cell = el("div", `cell c-${key.startsWith("h:") ? "header" : key}`);
      if (!row) {
        if (key === "subject") cell.textContent = "…";
      } else if (key === "attach") {
        if (row.attachments) cell.append(icon("paperclip"));
      } else if ((key === "status" || key === "priority") && skin().classic) {
        // classic's own marks (scripts/classic_icons.sh)
        const mark = key === "status"
          ? (row.flags & UNREAD ? "status-unread" : row.flags & REDIRECTED ? "status-redirected" : row.flags & FORWARDED ? "status-forwarded" : row.flags & REPLIED ? "status-replied" : "")
          : ({ 1: "priority-1", 2: "priority-2", 4: "priority-4", 5: "priority-5" }[priorityOf(row.flags)] || "");
        if (mark) { const img = document.createElement("img"); img.src = `classic/${mark}.svg`; img.className = "classic-mark"; img.alt = cellText(key, row); cell.append(img); }
      } else if (key === "subject") {
        const chips = labelChips(row);
        if (chips) cell.append(chips);
        cell.append(document.createTextNode(cellText(key, row)));
      } else {
        cell.textContent = cellText(key, row);
        if (key === "date" || key === "added") cell.title = row[key] ? fullFormat.format(new Date(row[key] * 1000)) : "No date";
        else if (key !== "size" && cell.textContent.length > 20) cell.title = cell.textContent;
      }
      e.append(cell);
    }
    if (row) e.title = row.snippet;
  }
  if (row) {
    e.dataset.id = row.id;
    e.classList.toggle("unread", (row.flags & UNREAD) !== 0);
    e.classList.toggle("flagged", (row.flags & FLAGGED) !== 0);
  } else e.classList.add("loading");
  if (isSelected(index)) e.classList.add("on");
  if (index === state.selected && selectionCount() > 1) e.classList.add("focus");
  return e;
}

function renderList(force = false) {
  const list = state.list;
  const box = $("list");
  if (!list) return;
  // (a refreshed list waits for its first page: how far the rows moved; what's shown stays)
  if (list.reselect && list.reselect.shift == null) return;
  const count = listCount(list);
  $("list-space").style.height = `${count * rowHeight}px`;
  const first = Math.max(0, Math.floor(box.scrollTop / rowHeight) - 10);
  const last = Math.min(count - 1, Math.ceil((box.scrollTop + box.clientHeight) / rowHeight) + 10);
  for (const [index, e] of rowPool) {
    const replaceable = e.classList.contains("loading") || e.classList.contains("stale");
    if (index < first || index > last || (force && replaceable && list.rows.has(index))) {
      e.remove();
      rowPool.delete(index);
    }
  }
  const wanted = new Set();
  for (let i = first; i <= last; i++) {
    if (rowPool.has(i)) {
      if (rowPool.get(i).classList.contains("stale") && !list.rows.has(i)) wanted.add(Math.floor(i / PAGE));
      continue;
    }
    const row = list.rows.get(i);
    // while a refreshed page loads, the row shown before stays (no flicker)
    const old = row ? null : list.stale?.get(i);
    if (!row) wanted.add(Math.floor(i / PAGE));
    const e = makeRow(i, row || old, Boolean(old));
    rowPool.set(i, e);
    box.append(e);
  }
  for (const page of wanted) loadPage(page);
  // read ahead in the direction of scrolling: once what's on screen is here, the next pages
  // that way, one at a time, nearest first (each from the one before it: the quick keyset
  // read), then one the other way
  if (!wanted.size && list.kind !== "search" && !list.pending.size) {
    const pages = Math.ceil(count / PAGE);
    const down = list.dir !== -1;
    const near = Math.floor((down ? last : first) / PAGE);
    const order = down ? [near + 1, near + 2, near + 3, Math.floor(first / PAGE) - 1] : [near - 1, near - 2, near - 3, Math.floor(last / PAGE) + 1];
    const next = order.find((p) => p >= 0 && p < pages && !list.rows.has(p * PAGE));
    if (next != null) loadPage(next);
  }
}

$("list").addEventListener("scroll", () => {
  const list = state.list;
  const top = $("list").scrollTop;
  if (list && list.lastTop != null && top !== list.lastTop) list.dir = top > list.lastTop ? 1 : -1;
  if (list) list.lastTop = top;
  renderList();
});
window.addEventListener("resize", () => renderList());

// ------------------------------------------------------------------ selection
// One message (a click), several (Ctrl+click adds or removes one, Shift+click or Shift+arrows
// a run of them) or all (Ctrl+A). `state.selected` is the one shown in the reader.

state.sel = new Set();
state.allSel = false;
state.anchor = -1;

function clearSelection() {
  state.sel = new Set();
  state.allSel = false;
  state.anchor = -1;
}

function isSelected(index) {
  return state.allSel || state.sel.has(index);
}

function selectionCount() {
  if (READER) return current ? 1 : 0;
  return state.allSel && state.list ? listCount(state.list) : state.sel.size;
}

function showSelection() {
  for (const e of rowPool.values()) {
    const i = Number(e.dataset.index);
    e.classList.toggle("on", isSelected(i));
    e.classList.toggle("focus", i === state.selected && selectionCount() > 1);
  }
  const n = selectionCount();
  if (n > 1) status(`${number.format(n)} messages selected: Delete, Transfer, Copy To, Forward (as attachments) and Filter Messages work on all of them.`);
}

/** `how`: "only" (a plain click), "toggle" (Ctrl+click), "range" (Shift: from the anchor),
 *  "range-add" (Ctrl+Shift: a run added to what's selected). */
function select(index, scroll = true, how = "only") {
  const list = state.list;
  if (!list) return;
  if (index < 0 || index >= listCount(list)) return;
  if (how === "toggle") {
    if (state.allSel) {
      // everything but this one
      state.allSel = false;
      state.sel = new Set(Array.from({ length: listCount(list) }, (_, i) => i));
    }
    if (state.sel.has(index) && state.sel.size > 1) state.sel.delete(index);
    else state.sel.add(index);
    state.anchor = index;
  } else if (how === "range" || how === "range-add") {
    const from = state.anchor < 0 ? index : state.anchor;
    if (how === "range" || state.allSel) { state.sel = new Set(); state.allSel = false; }
    for (let i = Math.min(from, index); i <= Math.max(from, index); i++) state.sel.add(i);
    state.anchor = from;
  } else {
    clearSelection();
    state.sel.add(index);
    state.anchor = index;
  }
  state.selected = index;
  showSelection();
  if (scroll) {
    const box = $("list");
    const top = index * rowHeight;
    if (top < box.scrollTop) box.scrollTop = top;
    else if (top + rowHeight > box.scrollTop + box.clientHeight) box.scrollTop = top + rowHeight - box.clientHeight;
  }
  const row = list.rows.get(index);
  if (row) showMessage(row.id);
}

$("list").addEventListener("mousedown", (e) => {
  const row = e.target.closest(".row");
  if (!row) return;
  const index = Number(row.dataset.index);
  const ctrl = e.ctrlKey || e.metaKey;
  // a right-click on a selected message keeps the selection (its menu acts on all of them)
  if (e.button === 2 && isSelected(index)) return;
  const how = e.shiftKey ? (ctrl ? "range-add" : "range") : (ctrl ? "toggle" : "only");
  if (e.shiftKey) e.preventDefault(); // (no text selection)
  // pressed on a message already selected (with others, or with Ctrl / Shift held), the selection
  // changes only if the button is let go without dragging, so the whole selection can be dragged
  const later = e.button === 0 && isSelected(index) && (how !== "only" || selectionCount() > 1);
  if (!later) select(index, false, how);
  if (e.button === 0 && !READER) watchMessageDrag(e, later ? () => select(index, false, how) : null);
});

/** Dragging messages out of the list into a folder (Explorer, the Finder, the desktop) writes them
 *  there as Outlook .msg files, or, with Ctrl or Shift held, as .eml files: the original bytes
 *  exactly. The files are written by a task of its own while the window carries
 *  on, with the count on the status bar; the system's drag starts once they're ready, and if the
 *  button was let go first, Eudora asks which folder to put them in. `click` runs when the button
 *  is let go without a drag. */
function watchMessageDrag(down, click) {
  const x0 = down.clientX, y0 = down.clientY;
  let held = true;
  const move = (ev) => {
    if (Math.abs(ev.clientX - x0) + Math.abs(ev.clientY - y0) < 6) return;
    stop();
    dragMessagesOut(down.ctrlKey || down.shiftKey || ev.ctrlKey || ev.shiftKey, () => held);
  };
  const up = () => { held = false; stop(); click?.(); };
  const stop = () => {
    window.removeEventListener("mousemove", move);
    window.removeEventListener("mouseup", up);
  };
  window.addEventListener("mousemove", move);
  window.addEventListener("mouseup", up);
  // (once dragging, letting go is no longer a click: it ends the wait instead)
  window.addEventListener("mouseup", () => { held = false; }, { once: true });
}

let messageDragBusy = false;

async function dragMessagesOut(eml, stillHeld) {
  if (messageDragBusy) { status("Still writing the messages from the last drag…"); return; }
  const ids = (await selectedItems()).map((i) => i.id);
  if (!ids.length) return;
  const kind = eml ? ".eml" : ".msg";
  const what = ids.length === 1 ? "the message" : `${number.format(ids.length)} messages`;
  messageDragBusy = true;
  status(`Writing ${what} as ${kind} files…`);
  const win = window.__TAURI__.webviewWindow.getCurrentWebviewWindow();
  const unlisten = await win.listen("messages-written", (e) => {
    const { done, total } = e.payload;
    if (total > 1) status(`Writing messages as ${kind} files: ${number.format(done)} of ${number.format(total)}…`);
  }).catch(() => null);
  try {
    const written = await invoke("messages_write", { ids, eml });
    if (written.failed.length) {
      tell("Some messages weren't written", `${number.format(written.failed.length)} of ${number.format(ids.length)} couldn't be written as ${kind} files:\n\n${written.failed.slice(0, 10).map(([id, why]) => `Message ${id}: ${why}`).join("\n")}`);
    }
    if (!written.paths.length) return;
    const n = written.paths.length === 1 ? `1 ${kind} file` : `${number.format(written.paths.length)} ${kind} files`;
    if (stillHeld()) {
      await invoke("files_drag", { paths: written.paths });
      status(`Drop the ${n} in a folder (${eml ? "the original messages exactly" : "Outlook's format; hold Ctrl or Shift while dragging for .eml"}).`);
    } else {
      // the button was let go before the files were ready: into a folder chosen instead
      const chosen = await invoke("pick_paths", { what: "folder", title: `Save the ${n} into…`, kind: null }).catch((err) => { showError("Choosing a folder", err); return null; });
      if (!chosen?.length) { status(`The ${n} weren't saved.`); return; }
      const placed = await invoke("files_place", { paths: written.paths, dir: chosen[0] });
      status(`Saved ${placed.length === 1 ? `1 ${kind} file` : `${number.format(placed.length)} ${kind} files`} in ${chosen[0]}.`);
    }
  } catch (err) {
    showError(`Writing ${what} as ${kind} files`, err);
  } finally {
    messageDragBusy = false;
    unlisten?.();
  }
}

/** Ctrl+A: every message in the list shown (a mailbox, a folder, All Mail, search results). */
function selectAllMessages() {
  const list = state.list;
  if (!list || !listCount(list)) return;
  state.allSel = true;
  state.sel = new Set();
  if (state.selected < 0) state.selected = 0;
  state.anchor = state.selected;
  showSelection();
  $("list").focus();
}

/** The selected messages, each with the folder it's taken from (the open folder, else the
 *  first one it's in). Rows not loaded yet (a long Shift run, Select All) are looked up. */
async function selectedItems() {
  // a message window: its message, in the folder it's in
  if (READER) return current ? [{ id: current.id, from: current.folder_ids?.[0] ?? null, row: { id: current.id, subject: current.subject } }] : [];
  const list = state.list;
  if (!list) return [];
  const own = list.kind === "folder" ? list.folders[0] : null;
  if (state.allSel && list.kind !== "search" && list.rows.size < list.total) {
    const all = await invoke("list_ids", listArgs(list));
    return all.map(([id, folder]) => ({ id, from: own ?? folder }));
  }
  const indexes = state.allSel ? Array.from({ length: listCount(list) }, (_, i) => i) : [...state.sel].sort((a, b) => a - b);
  // pages of a long Shift run that were never shown
  const pages = [...new Set(indexes.filter((i) => !list.rows.has(i)).map((i) => Math.floor(i / PAGE)))];
  for (const page of pages) {
    const rows = await readPage(list, page);
    rows.forEach((row, i) => list.rows.set(page * PAGE + i, row));
  }
  return indexes.map((i) => list.rows.get(i)).filter(Boolean).map((row) => ({ id: row.id, from: own ?? row.labels?.[0]?.[0] ?? null, row }));
}

$("list").addEventListener("keydown", (e) => {
  const page = Math.max(1, Math.floor($("list").clientHeight / rowHeight) - 1);
  const count = state.list ? listCount(state.list) : 0;
  const moves = { ArrowDown: 1, ArrowUp: -1, PageDown: page, PageUp: -page };
  const how = e.shiftKey ? "range" : "only";
  // already there (a key held at the top or bottom of the list): nothing to do, so the message
  // shown isn't drawn again
  const to = (i) => {
    if (i === state.selected && (how === "range" || selectionCount() <= 1)) return;
    select(i, true, how);
  };
  if (e.key in moves) to(Math.min(count - 1, Math.max(0, state.selected + moves[e.key])));
  else if (e.key === "Home") to(0);
  else if (e.key === "End") to(count - 1);
  else if (e.key.toLowerCase() === "a" && (e.ctrlKey || e.metaKey)) selectAllMessages();
  else return;
  e.preventDefault();
});

// ------------------------------------------------------------------ search

let searchTimer = null;

async function runSearch() {
  const query = $("query").value.trim();
  window.closeCalendar?.(); // (a mail search: the results in place of the calendar)
  window.closeTodo?.();
  window.closeContacts?.();
  if (!query) {
    openView(state.views.has(store.get("lastView", "filtered")) ? store.get("lastView", "filtered") : "filtered");
    return;
  }
  const started = performance.now();
  try {
    const found = await invoke("search", { query, limit: SEARCH_LIMIT, order: $("order").value, everywhere: state.searchEverywhere });
    if ($("query").value.trim() !== query) return;
    // sorted by a column heading: the results too
    const rows = new Map((listSort().custom ? [...found.rows].sort(compareRows) : found.rows).map((row, i) => [i, row]));
    setList({ kind: "search", key: "search", folders: [], title: `Search: ${query}`, total: found.total, rows, pending: new Set() });
    showSearchHelp(found);
    status(undefined, `Searched ${state.searchExcluded.size && !state.searchEverywhere ? "the mailboxes searched" : "every account"} in ${found.millis.toFixed(0)} ms (${(performance.now() - started).toFixed(0)} ms in all)`);
    const c = found.header_coverage;
    if (c && c.covered < c.total) {
      status(`Searching by header covers ${number.format(c.covered)} of ${number.format(c.total)} messages so far: the download prepares it in the background (new mail first, then older mail, newest down).`);
    }
  } catch (err) {
    showError("The search couldn't run", err);
  }
}

/** Above the results: what was repaired in the search, corrected searches to try (click one to
 *  run it), or "No results." */
function showSearchHelp(found) {
  const box = $("search-help");
  const parts = (found.notes || []).map((n) => el("p", "", n));
  if (found.suggestions?.length) {
    const tries = el("div", "try");
    tries.append(el("span", "", found.total ? "Or try:" : "Try:"));
    for (const s of found.suggestions) {
      const b = el("button");
      b.append(el("b", "", s.query || "(nothing)"));
      b.title = s.why;
      b.addEventListener("click", () => { $("query").value = s.query; runSearch(); });
      tries.append(b);
    }
    parts.push(tries, ...found.suggestions.map((s) => el("p", "why", s.why)));
  }
  if (!found.total) parts.unshift(el("p", "", "No results."));
  // what the mailboxes left out of search would have added
  if (found.hidden) {
    const p = el("p", "", `${number.format(found.hidden)} more ${found.hidden === 1 ? "result is" : "results are"} in mailboxes left out of search. `);
    const b = el("button", "", "Include them");
    b.addEventListener("click", () => { state.searchEverywhere = true; showEverywhereButton(); runSearch(); });
    p.append(b);
    parts.push(p);
  }
  box.replaceChildren(...parts);
  box.hidden = !parts.length;
}

$("query").addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(runSearch, 250);
});
$("query").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { clearTimeout(searchTimer); runSearch(); }
  if (e.key === "Escape") { $("query").value = ""; runSearch(); }
  if (e.key === "ArrowDown") { $("list").focus(); select(Math.max(0, state.selected)); e.preventDefault(); }
});
$("order").addEventListener("change", runSearch);

document.addEventListener("keydown", (e) => {
  // (Ctrl+F searches the mail, except where the look's program uses it otherwise: Outlook
  // forwards, Thunderbird finds in the message; their menus carry those)
  if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "f" && !["outlook", "thunderbird"].includes(state.uiMode?.menus)) {
    $("query").focus();
    $("query").select();
    e.preventDefault();
  } else if (e.key === "Escape" && document.body.classList.contains("showing-message") && !$("prefs").open) {
    closeMessage();
    $("list").focus();
  }
});

// ------------------------------------------------------------------ the message

let current = null; // the message shown (from the "message" command)

async function showMessage(id) {
  state.messageId = id;
  // (the next start opens on it again)
  if (!READER && state.list) store.set("lastMessage", { key: state.list.key, id });
  try {
    const m = await invoke("message", { id });
    if (state.messageId !== id) return;
    current = m;
    markReadSoon(id);
    if (skin().reading === "replace") document.body.classList.add("showing-message");
    renderMessage();
  } catch (err) {
    showError("Reading the message", err);
  }
}

/** Looks that show the message in place of the list go back to the list. */
function closeMessage() {
  document.body.classList.remove("showing-message");
}

/** No message shown (the one shown moved away): the reading pane says to choose one. */
function emptyReader() {
  state.messageId = null;
  current = null;
  closeMessage();
  $("message").hidden = true;
  $("message-empty").textContent = "Choose a message to read it.";
  $("message-empty").hidden = false;
}

$("m-back").addEventListener("click", () => {
  closeMessage();
  $("list").focus();
});

function people(list) {
  return list.map(personText).join(", ");
}

function renderMessage() {
  const m = current;
  // a fraud (phishing): said plainly; its links and pictures are off and can't be turned on
  $("m-fraud").hidden = !m.warning;
  $("m-fraud").querySelector("span").textContent = m.warning || "";
  $("message-empty").hidden = true;
  $("message").hidden = false;
  $("m-subject").textContent = m.subject || "(no subject)";
  $("m-from").textContent = people(m.from) || "(no sender)";
  $("m-to").textContent = people(m.to);
  $("m-to-line").hidden = !m.to.length;
  $("m-cc").textContent = people(m.cc);
  $("m-cc-line").hidden = !m.cc.length;
  $("m-date").textContent = m.date ? fullFormat.format(new Date(m.date * 1000)) : "No date";
  $("m-folders").textContent = m.folders.join(" · ");
  $("m-id").textContent = m.message_id_made ? `(none; made from the content: ${m.message_id})` : `<${m.message_id}>`;
  // every original copy kept: where it came from; a click shows its own bytes (Source)
  state.sourceCopy = null;
  const copies = $("m-copies");
  copies.replaceChildren(...m.copies.map((c, i) => {
    const link = el("a", "copy-link", `${i + 1}. ${c.from}${c.format === "exchange-export" ? " (Exchange's export)" : ""}, ${sizeText(c.size)}`);
    link.href = "#";
    link.title = "Show this copy's own bytes";
    link.addEventListener("click", (e) => {
      e.preventDefault();
      state.sourceCopy = c;
      setView("source");
      renderBody();
    });
    return link;
  }));
  $("m-copies-line").hidden = !m.copies.length;
  // a payment's receipt / a receipt's payment: a click opens it
  $("m-linked-line").hidden = !m.linked?.length;
  $("m-linked").replaceChildren(...(m.linked || []).map((l) => {
    const link = el("a", "m-copy", `${l.kind === "receipt" ? "Receipt" : "Payment"}: ${l.from.replace(/\s*<[^>]*>/, "")}, ${l.subject} ($${(l.cents / 100).toFixed(2)}, ${new Date(l.date * 1000).toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })})`);
    link.href = "#";
    link.title = "Open it";
    link.addEventListener("click", (e) => { e.preventDefault(); showMessage(l.id); });
    return link;
  }));
  const problems = $("m-problems");
  problems.hidden = !m.problems.length;
  problems.lastElementChild.textContent = m.problems.join("; ");
  renderAttachments(m);
  // an invitation: its meeting and the answers (calendar.js)
  window.showInviteBar?.(m);
  const shownPictures = m.html ? m.attachments.filter((a) => a.inline).length : 0;
  const real = m.attachments.length - shownPictures;
  showPictureBar(m);
  if (m.html && m.web_pictures?.length && !m.picturesShown && safeSender(m)) loadPictures(m);
  renderBody();
  status(`${sizeText(m.size)} stored${m.html ? ", HTML" : ", text"}${real ? `, ${real} attachment${real > 1 ? "s" : ""}` : ""}${shownPictures ? `, ${shownPictures} picture${shownPictures > 1 ? "s" : ""} in it` : ""}`);
}

/** The system's icon for a file's kind (as Explorer / Finder shows it), one
 *  request per extension; the paperclip until it comes, or when the system has none. */
const fileIcons = new Map();
function fileIcon(name) {
  const dot = name.lastIndexOf(".");
  const ext = dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
  const holder = el("span", "file-icon");
  holder.append(icon("paperclip"));
  if (!fileIcons.has(ext)) fileIcons.set(ext, invoke("file_icon", { name: ext ? `file.${ext}` : "file" }).catch(() => null));
  fileIcons.get(ext).then((url) => {
    if (!url) return;
    const img = document.createElement("img");
    img.src = url;
    img.alt = "";
    holder.replaceChildren(img);
  });
  return holder;
}

/** The attachment chips: on the Message tab, not the pictures the message shows itself (user
 *  2026-10-19: "hide them from attachments and only show the actual attachments that are not
 *  shown"); on Text, Headers and Source every one. */
function renderAttachments(m) {
  const list = state.view === "message" && m.html ? m.attachments.filter((a) => !a.inline) : m.attachments;
  $("m-attachments").replaceChildren(...list.map((a) => {
    const chip = el("span", a.missing ? "chip missing" : "chip");
    if (a.missing) {
      // a classic Eudora file that wasn't found (Tools ▸ Find Old Attachments… looks again)
      chip.title = `Not found: classic Eudora kept this file apart from the message, at ${a.missing}. Tools ▸ Find Old Attachments… looks in a folder or an old backup drive.`;
      chip.append(icon("paperclip"), el("span", "", `${a.name} (not found)`));
    } else {
      // a click opens it with the system's program; the arrow saves it
      chip.title = `${a.mime}, ${sizeText(a.size)}: click to open`;
      chip.classList.add("openable");
      chip.append(fileIcon(a.name), el("span", "", `${a.name} (${sizeText(a.size)})`));
      const save = el("button", "chip-save");
      save.title = `Save ${a.name}…`;
      save.append(icon("download"));
      save.addEventListener("click", (e) => { e.stopPropagation(); saveAttachments(m, a); });
      chip.append(save);
      chip.addEventListener("click", () => {
        // (the click that ends a drag doesn't open it)
        if (chip.dataset.dragged) { delete chip.dataset.dragged; return; }
        openAttachment(m, a);
      });
      // dragged out as a real file: to the desktop, Slack, Teams, any program,
      // or into a message being written (attached there): the system's own drag, started once
      // the mouse moves a few pixels with the button down (a click still opens it)
      chip.draggable = false;
      chip.addEventListener("mousedown", (e) => {
        if (e.button !== 0 || e.target.closest(".chip-save")) return;
        const x0 = e.clientX;
        const y0 = e.clientY;
        const move = (ev) => {
          if (Math.abs(ev.clientX - x0) + Math.abs(ev.clientY - y0) < 6) return;
          stop();
          chip.dataset.dragged = "1";
          invoke("attachment_drag", { id: m.id, part: a.part }).catch((err) => tell("Not dragged", String(err)));
        };
        const stop = () => {
          window.removeEventListener("mousemove", move);
          window.removeEventListener("mouseup", stop);
        };
        window.addEventListener("mousemove", move);
        window.addEventListener("mouseup", stop);
      });
    }
    return chip;
  }));
  if (list.filter((a) => !a.missing).length > 1) {
    const all = el("button", "chip-all", "Save All…");
    all.title = "Save every attachment into a folder";
    all.addEventListener("click", () => saveAttachments(m, null));
    $("m-attachments").append(all);
  }
}

/** Opens an attachment with the system's program for its kind (started apart from Eudora: the
 *  window never waits for it; a program that then fails to start is said below). */
async function openAttachment(m, a) {
  status(`Opening ${a.name}…`);
  try {
    await invoke("attachment_open", { id: m.id, part: a.part });
    status(`Opening ${a.name} with its program.`);
  } catch (err) {
    // (a program, or a fraud's attachment: said plainly, Save offered where it makes sense)
    await tell("Not opened", String(err));
    status("");
  }
}

window.__TAURI__.webviewWindow.getCurrentWebviewWindow().listen("attachment-open-failed", (e) => { showError("Opening the attachment", e.payload); }).catch(() => {});

/** Saves one attachment (`a`), or every one (`a` null), into a folder chosen with the system's dialog. */
async function saveAttachments(m, a) {
  let chosen;
  try { chosen = await invoke("pick_paths", { what: "folder", title: a ? `Save ${a.name} into…` : "Save the attachments into…", kind: null }); } catch (err) { showError("Choosing a folder", err); return; }
  if (!chosen?.length) return;
  try {
    if (a) {
      const path = await invoke("attachment_save", { id: m.id, part: a.part, dir: chosen[0] });
      status(`Saved ${path}`);
    } else {
      const [saved, missing] = await invoke("attachments_save_all", { id: m.id, dir: chosen[0] });
      status(`Saved ${saved.length} attachment${saved.length === 1 ? "" : "s"} in ${chosen[0]}${missing.length ? `; ${missing.length} not found (classic Eudora kept them apart)` : ""}.`);
    }
  } catch (err) {
    showError("Saving", err);
  }
}

/** Quoted lines (">") in the color of their depth; links left as text. */
function textToHtml(text) {
  return text.split(/\r?\n/).map((line) => {
    const depth = (line.match(/^(\s*>)+/)?.[0].match(/>/g) || []).length;
    // (a fraud's addresses stay plain text: nothing in it can be clicked)
    const html = (current?.warning ? escapeHtml(line) : linkify(escapeHtml(line))) || " ";
    return depth ? `<span class="quote q${Math.min(depth, 3)}">${html}</span>` : html;
  }).join("\n");
}

/** Web addresses in (already escaped) plain text become links; trailing punctuation stays text. */
function linkify(escaped) {
  return escaped.replace(/\bhttps?:\/\/[^\s<>"']+/gi, (url) => {
    const tail = url.match(/[.,;:!?)\]]+$/)?.[0] || "";
    const href = url.slice(0, url.length - tail.length);
    return `<a href="${href}">${href}</a>${tail}`;
  });
}

/** A link clicked in a message: web links
 *  open in the default browser (the status bar showed the real address on hover), mailto: opens
 *  a new message to that address, a link within the message (#…) does nothing, and anything else
 *  (javascript:, file:…) is refused. */
async function openLink(href) {
  const link = (href || "").trim();
  if (!link || link.startsWith("#")) return;
  if (current?.warning) { status("Links in a message that looks like a fraud are turned off."); return; }
  if (/^mailto:/i.test(link)) {
    // (read by the core: addresses, Cc, Bcc, subject, and text)
    try {
      await invoke("open_mailto", { url: link });
    } catch (err) {
      showError("Opening a message window", err);
    }
    return;
  }
  try {
    await invoke("open_link", { url: link });
    status(`Opened in your browser: ${link}`);
  } catch (err) {
    status(String(err));
  }
}

// (the message frame: sandboxed, darkenMessage, scrollByWheel are in msgframe.js)

async function renderBody() {
  const body = $("m-body");
  const m = current;
  if (!m) return;
  // drawn again (its pictures came in, say): where it was scrolled is kept
  const shown = `${m.id}:${state.view}`;
  const was = body.querySelector("iframe")?.contentDocument?.scrollingElement || body.querySelector("pre");
  const keep = body.dataset.shown === shown && was ? [was.scrollLeft, was.scrollTop] : null;
  body.dataset.shown = shown;
  body.replaceChildren();
  if (state.view === "message" && m.html) {
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-same-origin");
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.srcdoc = sandboxed(m.html);
    frame.addEventListener("load", () => {
      const doc = frame.contentDocument;
      if (!doc) return;
      doc.addEventListener("click", (e) => {
        const a = e.target.closest?.("a[href]");
        if (a) { e.preventDefault(); openLink(a.getAttribute("href")); }
      });
      doc.addEventListener("mouseover", (e) => {
        const a = e.target.closest?.("a[href]");
        status(a ? a.getAttribute("href") : undefined);
      });
      darkenMessage(doc);
      scrollByWheel(doc);
      if (keep) doc.scrollingElement?.scrollTo(keep[0], keep[1]);
    });
    body.append(frame);
    return;
  }
  const pre = el("pre", "text");
  if (state.view === "headers") pre.textContent = m.headers;
  else if (state.view === "source") {
    pre.textContent = "Reading…";
    try {
      const c = state.sourceCopy;
      const source = c
        ? `[Original copy from ${c.from}]\n\n` + await invoke("copy_source", { copy: c.id, format: c.format })
        : await invoke("source", { id: m.id });
      if (current === m) pre.textContent = source;
    } catch (err) { pre.textContent = String(err); }
  } else {
    pre.innerHTML = textToHtml(m.text || "(no text)");
    pre.addEventListener("click", (e) => {
      const a = e.target.closest?.("a[href]");
      if (a) { e.preventDefault(); openLink(a.getAttribute("href").replace(/&amp;/g, "&")); }
    });
    pre.addEventListener("mouseover", (e) => {
      const a = e.target.closest?.("a[href]");
      status(a ? a.getAttribute("href") : undefined);
    });
  }
  body.append(pre);
  if (keep) pre.scrollTo(keep[0], keep[1]);
}

function setView(view) {
  state.view = view;
  for (const b of document.querySelectorAll("#m-views button")) b.classList.toggle("on", b.dataset.view === view);
}

$("m-views").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  state.sourceCopy = null; // the Source tab shows the message's own bytes
  setView(b.dataset.view);
  renderBody();
  // (the pictures the message shows itself are listed as attachments on the other tabs)
  if (current) renderAttachments(current);
});

// ------------------------------------------------------------------ pictures from the web

/** Picture settings. Pictures load only when asked, or for a sender on the safe
 * list (`senders`: addresses, lowercase); `keep` = fetch every time or keep on this computer.
 * Kept in the library. Settings from before (per account, "show for every account") are
 * dropped: everything asks again. */
const pictures = { keep: "fetch", senders: [] };

async function loadPictureSettings() {
  try {
    const saved = JSON.parse((await invoke("setting", { key: "pictures" })) || "null");
    if (saved?.senders) {
      pictures.senders = saved.senders;
      pictures.keep = saved.keep || "fetch";
    } else if (saved?.global) {
      // the old per-account settings: keep only how pictures are kept; ask again for all
      pictures.keep = saved.global.keep || "fetch";
      await savePictureSettings();
    }
  } catch { /* defaults */ }
}

async function savePictureSettings() {
  await invoke("set_setting", { key: "pictures", value: JSON.stringify(pictures) });
}

/** The sender's address (the first From). */
function senderOf(m) {
  return (m.from?.[0]?.address || "").toLowerCase();
}

/** Whether a message's pictures load without asking: only from a sender on the safe list. */
function safeSender(m) {
  const who = senderOf(m);
  return !!who && pictures.senders.includes(who);
}

function showPictureBar(m) {
  const bar = $("m-pictures");
  const n = m.web_pictures?.length || 0;
  bar.hidden = !m.html || !n || m.picturesShown;
  if (bar.hidden) return;
  bar.querySelector(".m-pic-text").textContent =
    `${n} picture${n > 1 ? "s" : ""} from the web ${n > 1 ? "aren't" : "isn't"} shown: loading them can tell the sender you opened this message.`;
  const who = senderOf(m);
  $("m-pic-always").hidden = !who;
  $("m-pic-always").textContent = `Always show from ${who}`;
}

async function loadPictures(m) {
  const rule = { keep: pictures.keep };
  const bar = $("m-pictures");
  bar.querySelector(".m-pic-text").textContent = `Loading ${m.web_pictures.length} pictures…`;
  try {
    const got = await invoke("web_pictures", { urls: m.web_pictures, keep: rule.keep === "store" });
    let html = m.html;
    let shown = 0;
    for (const p of got) {
      if (!p.data) continue;
      html = html.split(p.url).join(p.data);
      shown++;
    }
    if (current !== m) return;
    m.html = html;
    m.picturesShown = true;
    bar.hidden = true;
    status(`${shown} of ${got.length} pictures loaded${rule.keep === "store" ? " (kept on this computer)" : ""}`);
    if (state.view === "message") renderBody();
  } catch (err) {
    showError("Loading pictures", err);
  }
}

$("m-pic-show").addEventListener("click", () => current && loadPictures(current));
$("m-pic-always").addEventListener("click", async () => {
  if (!current) return;
  const who = senderOf(current);
  if (!who) return;
  if (!pictures.senders.includes(who)) pictures.senders.push(who);
  await savePictureSettings();
  status(`Pictures from ${who} will show by themselves (Settings ▸ Mail to undo).`);
  loadPictures(current);
});

// ------------------------------------------------------------------ preferences

// Settings ▸ Downloading and sync: the download Eudora runs while it's open (background.rs)
const BG_STATES = {
  running: "Downloading in the background now.",
  elsewhere: "Another copy of the download (download.cmd) is keeping your mail up to date; Eudora takes over when it ends.",
  off: "Not downloading.",
  restarting: "The download stopped; it starts again by itself.",
  missing: "The download program (eudora-cli) isn't beside Eudora's own program, so Eudora can't download by itself.",
  starting: "Starting…",
};

async function showBackground() {
  try {
    const [st, opts] = await invoke("background_state");
    $("p-bg-on").checked = opts.on;
    $("p-bg-archive").checked = opts.archive;
    $("p-bg-shared").checked = opts.shared;
    for (const id of ["p-bg-archive", "p-bg-shared"]) $(id).disabled = !opts.on;
    const why = st.note && st.state !== "running" ? ` ${st.note}` : "";
    $("p-bg-state").textContent = `${BG_STATES[st.state] || st.state}${why}${st.log ? ` Its record: ${st.log}` : ""}`;
  } catch (err) {
    $("p-bg-state").textContent = `Couldn't tell: ${err}`;
  }
}

for (const id of ["p-bg-on", "p-bg-archive", "p-bg-shared"]) {
  $(id).addEventListener("change", async () => {
    const options = { on: $("p-bg-on").checked, archive: $("p-bg-archive").checked, shared: $("p-bg-shared").checked };
    try {
      await invoke("background_set", { options });
      $("p-bg-state").textContent = "Applying…";
      setTimeout(showBackground, 1500);
    } catch (err) {
      $("p-bg-state").textContent = `Not changed: ${err}`;
    }
  });
}

// ------------------------------------------------------------------ updates (as PlanetPDF's)

let updateChecking = false;

window.__TAURI__.webviewWindow.getCurrentWebviewWindow().listen("update-progress", (e) => {
  const [done, total] = e.payload;
  const mb = (n) => (n / 1048576).toFixed(1);
  status(undefined, done < total ? `Downloading the Eudora update: ${mb(done)} of ${mb(total)} MB` : "");
});

/** Looks for an update: `force` = Help ▸ Check for Updates (says what it found); otherwise a
 *  quiet check, only when due, that speaks only when there's something to install. */
async function checkUpdates(force, starting = false) {
  if (updateChecking) {
    if (force) status("Already looking for an update…");
    return;
  }
  updateChecking = true;
  if (force) status("Looking for an update…");
  try {
    const found = await invoke("update_check", { force, starting });
    if (found.state === "ready") offerUpdate(found);
    else if (found.state === "manual") {
      const open = await ask({
        title: `Eudora ${found.version} is available`,
        text: `You have ${found.current}. ${found.why}${found.notes ? `\n\nWhat's new: ${found.notes}` : ""}`,
        buttons: [{ label: "Close", value: false }, { label: "Open the Download Page", value: true, primary: true }],
      });
      if (open) invoke("open_link", { url: found.page });
    } else if (force && found.state === "latest") tell("No update", `Eudora ${found.current} is the latest version.`);
    if (force) status("");
  } catch (err) {
    if (force) tell("Couldn't look for an update", String(err));
  } finally {
    updateChecking = false;
    if ($("prefs").open) showUpdates();
  }
}

async function offerUpdate(found) {
  if (found.on_close) {
    status(`Eudora ${found.version} goes in when you close Eudora.`);
    return;
  }
  const when = await ask({
    title: `Eudora ${found.version} is ready`,
    text: `You have ${found.current}. The update is downloaded and checked; installing it takes a few seconds, and Eudora starts again by itself.${found.notes ? `\n\nWhat's new: ${found.notes}` : ""}`,
    buttons: [
      { label: "Skip This Version", value: "skip" },
      { label: "When I Close Eudora", value: "close" },
      { label: "Restart Now", value: "now", primary: true },
    ],
  });
  if (!when) return;
  try {
    await invoke("update_install", { when });
    if (when === "close") status(`Eudora ${found.version} goes in when you close Eudora.`);
    if (when === "skip") status(`Eudora ${found.version} won't be offered again (Help ▸ Check for Updates still finds it).`);
  } catch (err) {
    tell("The update wasn't installed", String(err));
  }
}

// Settings ▸ Updates
async function showUpdates() {
  try {
    const s = await invoke("update_get");
    $("p-upd-start").checked = s.choices.on_start;
    $("p-upd-days").value = String(s.choices.every_days);
    if (![...$("p-upd-days").options].some((o) => o.value === String(s.choices.every_days))) $("p-upd-days").value = "14";
    const last = s.choices.last_check ? new Date(s.choices.last_check * 1000).toLocaleString() : "never";
    const found = s.waiting ? ` Eudora ${s.waiting.version} is downloaded and waiting.` : s.choices.last_found ? ` It found ${s.choices.last_found}.` : "";
    const how = s.installed.kind === "manual" ? ` ${s.installed.why}` : "";
    $("p-upd-state").textContent = `This is Eudora ${s.current}. Last looked: ${last}.${found}${how}`;
  } catch (err) {
    $("p-upd-state").textContent = `Couldn't tell: ${err}`;
  }
}

for (const id of ["p-upd-start", "p-upd-days"]) {
  $(id).addEventListener("change", async () => {
    try {
      await invoke("update_set", { onStart: $("p-upd-start").checked, everyDays: Number($("p-upd-days").value) });
    } catch (err) {
      $("p-upd-state").textContent = `Not changed: ${err}`;
    }
  });
}
$("p-upd-now").addEventListener("click", () => checkUpdates(true));

/** Eudora's folder couldn't move from its old name ("Eudora 27") this time: says what's in the
 *  way (another program with an attachment open, usually) and offers to restart once it's closed. */
async function folderMoveBlocked() {
  const blocked = await invoke("folder_move_blocked").catch(() => null);
  if (!blocked) return;
  const programs = blocked.programs || [];
  const who = programs.length
    ? `These programs have files from it open (attachments you opened, usually):\n\n${programs.map((p) => "• " + p).join("\n")}\n\nClose those files (or the programs), then restart Eudora.`
    : `Something has a file in it open (${blocked.why}). Close any attachments you opened from Eudora, then restart Eudora.`;
  const go = await askYesNo(
    "Eudora couldn't finish moving its folder",
    `Eudora now keeps your mail in ${blocked.to} (it was ${blocked.from}). Your mail is fine and Eudora works as usual from the old folder until it moves.\n\n${who}`,
    "Restart Eudora",
    "Later",
  );
  if (go) await invoke("restart_eudora").catch((e) => status(String(e)));
}

if (!READER) {
  setTimeout(folderMoveBlocked, 1500);
  setTimeout(() => checkUpdates(false, true), 20000);
  setInterval(() => checkUpdates(false), 3600000);
}

/** Settings: one window for every setting, a page at a time, the pages listed down the left as
 *  Outlook's Options are. `page`: the page to show (else the one
 *  shown last). The old separate commands (Personalities, Signatures, Plugins…) open its pages. */
const SETTINGS_PAGES = [
  ["accounts", "Accounts", "Your mail accounts, shared mailboxes, and signing in"],
  ["general", "General", "Look, folders or labels, Recent Mail, address completion"],
  ["mail", "Mail", "Labels, pictures, signatures, rules, and the spam filter"],
  ["calendar", "Calendar", "Week, business hours, places, Zoom"],
  ["todo", "To Do", "Microsoft To Do and Twos"],
  ["sync", "Downloading and sync", "What downloads, and sync between your computers"],
  ["library", "Library", "Back up, restore, import, optimize"],
  ["speed", "Speed and maintenance", "Memory, and the nightly upkeep"],
  ["programs", "Programs and plugins", "What other programs may do"],
  ["updates", "Updates", "Checking for new versions"],
  ["web", "Web access", "Phones on this network"],
];
function showSettingsPage(page) {
  if (!SETTINGS_PAGES.some(([k]) => k === page)) page = "accounts";
  store.set("settingsPage", page);
  for (const s of document.querySelectorAll("#p-pages > section")) s.hidden = s.dataset.page !== page;
  for (const b of $("p-nav").children) b.classList.toggle("on", b.dataset.page === page);
  $("p-pages").scrollTop = 0;
}
$("p-nav").replaceChildren(...SETTINGS_PAGES.map(([key, name, about]) => {
  const b = el("button", "", name);
  b.type = "button";
  b.dataset.page = key;
  b.title = about;
  b.addEventListener("click", () => showSettingsPage(key));
  return b;
}));
// (an editor opened from inside Settings goes straight over it: Settings isn't opened again,
// which would put back what's typed there)
const SETTINGS_EDITORS = { signatures: () => openSignatures(), plugins: () => openPlugins(), filters: () => openFilters(), "spam-rules": () => openSpamRules() };
for (const b of document.querySelectorAll("#p-pages [data-open]")) b.addEventListener("click", () => (SETTINGS_EDITORS[b.dataset.open] ?? ACTIONS[b.dataset.open])?.());
/** The manual (Help ▸ Eudora Help, F1), at the topic for what's in front: a Settings page, the
 *  calendar, contacts, To Do, else the start. */
function openManual() {
  let topic = "welcome";
  if ($("prefs").open) topic = "ref-options";
  else if (document.body.classList.contains("calendar-mode")) topic = "calendar";
  else if (document.body.classList.contains("todo-mode")) topic = "todo";
  else if (state.contactsOn) topic = "contacts";
  invoke("open_help", { topic }).catch((err) => showError("Opening the manual", err));
}

/** A command that lives in Settings: its page, then (for an editor of its own) the editor over it. */
function settingsAt(page, then) {
  return () => { openPrefs(page); then?.(); };
}

function openPrefs(page) {
  showSettingsPage(typeof page === "string" ? page : store.get("settingsPage", "accounts"));
  showUpdates();
  showBackground();
  showSync();
  showServerChanges();
  renderPrefLabels();
  renderPrefApi();
  $("p-skin").value = state.skin;
  $("p-recent-hours").value = String(state.recentPrefs.hours);
  invoke("setting", { key: "keep-exchange-exports" }).then((v) => { $("p-keep-originals").checked = v !== "off"; }).catch(() => {});
  showKeep();
  invoke("setting", { key: "gmail-idle-minutes" }).then((v) => { $("p-gmail-idle").value = v == null || v === "" ? "120" : String(v); }).catch(() => {});
  $("p-recent-monday").value = String(state.recentPrefs.monday);
  $("p-ctrl-r-all").checked = !!state.uiMode.ctrl_r_all;
  $("p-ctrl-j-junk").checked = !!state.uiMode.ctrl_j_junk;
  for (const r of document.querySelectorAll('input[name="p-organize"]')) r.checked = r.value === state.organize;
  const rows = state.accounts.accounts.map((a) => {
    const row = el("div", "p-account");
    const who = el("div", "p-who");
    who.append(el("div", "p-address", a.address || a.name), el("div", "p-kind", `${a.kind_name} · ${number.format(a.messages)} messages${a.source ? ` · from ${a.source}` : ""}`));
    const name = el("input");
    name.type = "text";
    name.placeholder = a.address || a.name;
    name.value = a.label || "";
    name.dataset.id = a.id;
    name.title = "A name of your own for this account (empty: its address)";
    const pics = el("div", "p-pics");
    if ((a.kind === "imap" || a.kind === "pop3") && a.address) {
      const change = el("button", "p-change", "Change…");
      change.title = "Servers (incoming and SMTP), user names, passwords";
      change.addEventListener("click", () => openAccount(a.address));
      pics.append(change);
    }
    // how often it's checked for new mail, account by account; accounts made from files have
    // no server to check
    if (a.address && ["graph", "graph-shared", "imap", "pop3", "gmail"].includes(a.kind)) {
      const every = el("select", "p-check-every");
      every.title = "How often Eudora looks for new mail in this account (Check Mail looks at once)";
      every.dataset.address = a.address;
      pics.append(el("span", "p-note", "Check every "), every);
    }
    who.append(pics);
    row.append(who, name);
    return row;
  });
  // (the intervals arrive a moment later; a change is kept at once)
  invoke("check_every_get").then((c) => {
    for (const sel of $("p-accounts").querySelectorAll(".p-check-every")) {
      const now = c.accounts[sel.dataset.address.toLowerCase()] ?? c.default;
      sel.replaceChildren(...c.choices.map((m) => {
        const o = el("option", "", m === 1 ? "minute" : m === 60 ? "hour" : `${m} minutes`);
        o.value = String(m);
        return o;
      }));
      sel.value = String(now);
      sel.addEventListener("change", async () => {
        try {
          await invoke("check_every_set", { address: sel.dataset.address, minutes: Number(sel.value) });
          status(`${sel.dataset.address} is checked for new mail every ${sel.selectedOptions[0].textContent} from its next check.`);
        } catch (err) {
          showError("Check interval", err);
        }
      });
    }
  }).catch(() => {});
  invoke("setting", { key: "completion" }).then((json) => {
    const c = Object.assign({ sent: true, received: true }, JSON.parse(json || "{}"));
    $("p-comp-sent").checked = c.sent;
    $("p-comp-received").checked = c.received;
  }).catch(() => {});
  for (const r of document.querySelectorAll('input[name="p-pic-keep"]')) r.checked = r.value === pictures.keep;
  renderSafeSenders();
  $("p-accounts").replaceChildren(...rows);
  $("p-web-port").value = store.get("webPort", 443);
  checkFirewall();
  if (!$("prefs").open) $("prefs").showModal();
}

/** The safe senders, each with Remove (takes effect at once). */
function renderSafeSenders() {
  const list = $("p-pic-senders");
  if (!pictures.senders.length) {
    list.replaceChildren(el("p", "p-note", "None yet: every message asks."));
    return;
  }
  list.replaceChildren(...[...pictures.senders].sort().map((who) => {
    const row = el("div", "p-safe");
    const remove = el("button", "f-small", "Remove");
    remove.title = `Ask again before showing pictures from ${who}`;
    remove.addEventListener("click", async () => {
      pictures.senders = pictures.senders.filter((s) => s !== who);
      try { await savePictureSettings(); } catch (err) { showError("Saving the picture settings", err); }
      renderSafeSenders();
    });
    row.append(el("span", "", who), remove);
    return row;
  }));
}

async function savePrefs() {
  const organize = document.querySelector('input[name="p-organize"]:checked')?.value || "folders";
  pictures.keep = document.querySelector('input[name="p-pic-keep"]:checked')?.value || "fetch";
  try { await savePictureSettings(); } catch (err) { showError("Saving the picture settings", err); }
  try {
    await invoke("set_setting", { key: "completion", value: JSON.stringify({ sent: $("p-comp-sent").checked, received: $("p-comp-received").checked }) });
  } catch (err) { showError("Saving the completion settings", err); }
  for (const input of $("p-accounts").querySelectorAll("input")) {
    const a = state.accounts.accounts.find((x) => String(x.id) === input.dataset.id);
    if (a && (a.label || "") !== input.value.trim()) {
      try { await invoke("set_account_label", { id: a.id, label: input.value.trim() }); } catch (err) { showError("Naming the account", err); }
    }
  }
  $("prefs").close();
  state.organize = organize;
  store.set("organize", organize);
  setSkin($("p-skin").value);
  await loadAccounts();
  renderSidebar();
  if (state.list && state.list.kind !== "search" && !state.views.has(state.list.key)) openView("filtered");
}

/** Settings ▸ Downloading and sync. */
async function showSync() {
  let s;
  try { s = await invoke("sync_state"); } catch (err) { $("p-sync-state").textContent = `Couldn't check (${err}).`; return; }
  $("p-sync-start").hidden = !!s.key;
  $("p-sync-on").hidden = !s.key;
  $("p-sync-phrase").hidden = true;
  $("p-sync-state").textContent = s.key
    ? `This computer syncs (key ${s.key}): your settings, and how mail is filed where servers can't keep it (the Online Archive, IMAP, old archives), with your other computers.`
    : "Sync isn't set up on this computer. Set it up on your first computer, then join on the others with the recovery phrase it shows.";
  if (!s.key) return;
  options($("p-sync-home"), s.mailboxes.map(([address, shown]) => [address, shown]), s.home);
  $("p-sync-carriers").replaceChildren(...s.mailboxes.filter(([address]) => address !== s.home).map(([address, shown]) => {
    const label = el("label", "sr-mailbox");
    const box = el("input");
    box.type = "checkbox";
    box.value = address;
    box.checked = s.carriers.includes(address);
    box.addEventListener("change", saveSyncCarriers);
    label.append(box, el("span", "", shown));
    label.title = address;
    return label;
  }));
}

async function saveSyncCarriers() {
  const carriers = [...$("p-sync-carriers").querySelectorAll("input:checked")].map((b) => b.value);
  try {
    await invoke("sync_set_carriers", { home: $("p-sync-home").value, carriers });
  } catch (err) {
    showError("Saving the sync choices", err);
  }
}

function showPhrase(phrase, intro) {
  const p = $("p-sync-phrase");
  p.replaceChildren(el("span", "p-note", intro), el("code", "", phrase));
  p.hidden = false;
}

$("p-sync-setup").addEventListener("click", async () => {
  try {
    const phrase = await invoke("sync_setup");
    await showSync();
    showPhrase(phrase, "Your recovery phrase: type it on your other computers (Settings ▸ Downloading and sync ▸ Join), and keep a copy somewhere safe. ");
  } catch (err) {
    showError("Setting up sync", err);
  }
});
$("p-sync-join").addEventListener("click", async () => {
  try {
    const id = await invoke("sync_join", { phrase: $("p-sync-phrase-in").value });
    $("p-sync-phrase-in").value = "";
    await showSync();
    status(`This computer now syncs (key ${id}). Your settings and filing come in at the next check for mail.`);
    invoke("sync_now", { repair: false }).catch(() => {});
  } catch (err) {
    showError("Joining sync", err);
  }
});
$("p-sync-show").addEventListener("click", async () => {
  try { showPhrase(await invoke("sync_phrase"), "Recovery phrase: "); } catch (err) { showError("Showing the phrase", err); }
});
$("p-sync-now").addEventListener("click", async () => {
  try { await invoke("sync_now", { repair: false }); status("Syncing with your other computers at the next check (now)."); } catch (err) { showError("Syncing", err); }
});
$("p-sync-repair").addEventListener("click", async () => {
  if (!(await askYesNo("Repair filing", "Ask your other computers for a full copy of how mail is filed? Use this if filing here looks wrong.", "Ask for a full copy"))) return;
  try { await invoke("sync_now", { repair: true }); status("Asked your other computers for a full copy (SOS): the next one to sync sends it."); } catch (err) { showError("Repair", err); }
});
$("p-sync-home").addEventListener("change", async () => { await saveSyncCarriers(); showSync(); });

/** Whether moves made in Eudora reach the Microsoft 365 server, and how many still wait. */
/** The default Microsoft account (Sign in again renews that one, not the browser's). */
let msDefault = null;

async function showServerChanges() {
  const note = $("p-ms-note");
  try {
    const s = await invoke("server_changes");
    msDefault = s.account || null;
    const waiting = s.waiting ? ` ${number.format(s.waiting)} move${s.waiting === 1 ? "" : "s"} waiting for the server.` : "";
    const stuck = s.given_up ? ` ${number.format(s.given_up)} couldn't be made (${s.last_error || "an error"}); signing in again tries them once more.` : "";
    if (!s.account) note.textContent = "No Microsoft 365 account is signed in on this computer.";
    else if (s.may_change) note.textContent = `${s.account}: moves and new folders are made on the server too.${waiting}${stuck}`;
    else note.textContent = `${s.account}: Eudora may only read mail there, so moves stay on this computer.${waiting} Sign in again and accept Microsoft's question to let Eudora move mail and make folders on the server.`;
  } catch (err) {
    note.textContent = `Couldn't check (${err}).`;
  }
}

/** Signs in to Microsoft 365 again in the browser (the consent covers moving mail). */
async function microsoftSignIn() {
  const button = $("p-ms-signin");
  button.disabled = true;
  $("p-ms-note").textContent = "Finish signing in in your browser (it waits up to 5 minutes)…";
  try {
    const account = await invoke("microsoft_sign_in", { address: msDefault });
    status(`Signed in as ${account}. The download starts making the waiting moves on the server within a few minutes.`);
  } catch (err) {
    showError("Signing in to Microsoft 365", err);
  }
  button.disabled = false;
  await showServerChanges();
}

/** The first run (no accounts yet): the three ways in. Any kind of account works on its own. */
function showWelcome() {
  const dialog = $("welcome");
  $("welcome-note").textContent = "You can add more accounts later in Settings ▸ Accounts.";
  // how much mail this new computer keeps, chosen before anything downloads: saved when an
  // account is added
  const keep = $("welcome-keep");
  keep.oninput = () => { $("welcome-keep-label").textContent = KEEP_STEPS[Number(keep.value)][1]; };
  keep.oninput();
  const saveKeep = async () => {
    const days = KEEP_STEPS[Number(keep.value)][0];
    try { await invoke("download_period_set", { days, mb: null }); } catch (err) { showError("Saving how much mail to keep", err); }
  };
  $("welcome-ms").onclick = async () => {
    await saveKeep();
    $("welcome-ms").disabled = true;
    $("welcome-note").textContent = "Finish signing in in your browser (it waits up to 5 minutes)…";
    try {
      const account = await invoke("microsoft_sign_in");
      dialog.close();
      status(`Signed in as ${account}. Your mail starts coming in now.`);
      invoke("check_mail").catch(() => {});
    } catch (err) {
      $("welcome-note").textContent = `Not signed in: ${err}`;
    }
    $("welcome-ms").disabled = false;
  };
  $("welcome-imap").onclick = async () => { await saveKeep(); dialog.close(); openAccount(); };
  $("welcome-import").onclick = () => { dialog.close(); openImport(); };
  // a new computer: the backup goes into the new, empty library just made here
  $("welcome-restore").onclick = () => { dialog.close(); openRestore({ fresh: true }); };
  $("welcome-later").onclick = () => dialog.close();
  dialog.showModal();
}

/** Finds the Microsoft 365 mailboxes the user can open; ticked ones become accounts. */
async function findMailboxes() {
  const button = $("p-find");
  button.disabled = true;
  button.textContent = "Checking which mailboxes you can open… (about a minute)";
  try {
    const found = await invoke("find_mailboxes");
    const list = $("p-found-list");
    list.replaceChildren(...found.map((m) => {
      const row = el("label", "p-found-row");
      const box = el("input");
      box.type = "checkbox";
      box.value = m.address;
      box.checked = m.added;
      box.disabled = m.added;
      row.append(box, el("span", "p-address", m.address), el("span", "p-kind", `${m.name} · Inbox ${number.format(m.inbox)}${m.added ? " · already added" : ""}`));
      return row;
    }));
    $("p-found").hidden = false;
    button.textContent = `You can open ${found.length} mailboxes: tick the ones to bring in`;
  } catch (err) {
    button.textContent = "Find shared mailboxes…";
    button.disabled = false;
    showError("Finding mailboxes", err);
  }
}

async function addFound() {
  const addresses = [...$("p-found-list").querySelectorAll("input:checked:not(:disabled)")].map((b) => b.value);
  if (!addresses.length) return;
  try {
    await invoke("add_shared_mailboxes", { addresses });
    for (const b of $("p-found-list").querySelectorAll("input:checked")) b.disabled = true;
    status(`Added ${addresses.length} mailbox${addresses.length > 1 ? "es" : ""}: the download brings them in on its next run.`);
    await loadAccounts();
    renderSidebar();
  } catch (err) {
    showError("Adding mailboxes", err);
  }
}

// ------------------------------------------------------------------ IMAP accounts

/** Known servers, filled in from the address (the user can change them). */
const ICLOUD_HINT = "iCloud: use an app-specific password (appleid.apple.com ▸ Sign-In and Security; needs two-factor authentication). User name: the part before the @ (if that's refused, the whole address). iCloud has no POP3.";
const SERVER_PRESETS = {
  "gmail.com": { imap: "imap.gmail.com", pop3: "pop.gmail.com", smtp: "smtp.gmail.com", hint: "Gmail: use an app password (Google Account ▸ Security ▸ 2-Step Verification ▸ App passwords), not your Google password." },
  "googlemail.com": { imap: "imap.gmail.com", pop3: "pop.gmail.com", smtp: "smtp.gmail.com", hint: "Gmail: use an app password, not your Google password." },
  "yahoo.com": { imap: "imap.mail.yahoo.com", pop3: "pop.mail.yahoo.com", smtp: "smtp.mail.yahoo.com", hint: "Yahoo: use an app password (Account security ▸ Generate app password)." },
  // (@icloud.com, @me.com and @mac.com are one iCloud mailbox)
  "icloud.com": { imap: "imap.mail.me.com", pop3: "", smtp: "smtp.mail.me.com", hint: ICLOUD_HINT },
  "me.com": { imap: "imap.mail.me.com", pop3: "", smtp: "smtp.mail.me.com", hint: ICLOUD_HINT },
  "mac.com": { imap: "imap.mail.me.com", pop3: "", smtp: "smtp.mail.me.com", hint: ICLOUD_HINT },
  "aol.com": { imap: "imap.aol.com", pop3: "pop.aol.com", smtp: "smtp.aol.com", hint: "AOL: use an app password." },
};
// (Outlook.com, Hotmail and Microsoft 365 addresses don't come here: they sign in with Microsoft)
/** Where this system keeps passwords (set at start from the `platform` command). */
let passwordStore = "this computer's password store";
invoke("platform").then((os) => {
  passwordStore = { windows: "Windows Credential Manager", macos: "the macOS Keychain", linux: "the system's keyring (Secret Service)" }[os] || passwordStore;
}).catch(() => {});
const defaultHint = () => `Passwords are kept in ${passwordStore} on this computer, never in the mail library.`;
const DEFAULT_PORTS = { imap: { tls: 993, starttls: 143, none: 143 }, pop3: { tls: 995, starttls: 110, none: 110 }, smtp: { tls: 465, starttls: 587, none: 25 } };
let editingImap = null; // the address of the account being changed, or null for a new one
let serverTouched = false; // the user typed a server name: don't replace it

/** Who hosts the address typed ("microsoft", "google" or null): Microsoft addresses sign in
 *  with Microsoft instead of a password over IMAP (Microsoft no longer accepts those). */
let addressHost = null;
/** The kind picked by hand at the top (detection never overrides it back). */
let kindChosen = null;
async function checkHost() {
  const address = $("a-address").value.trim();
  if (kindChosen === "microsoft") { addressHost = "microsoft"; return; } // chosen above: it stays
  addressHost = null;
  if (address.includes("@") && !editingImap) {
    try { addressHost = await invoke("mail_host", { address }); } catch { addressHost = null; }
  }
  if (addressHost === "microsoft") {
    showKind("microsoft");
    $("a-ms-title").textContent = `${address} is a Microsoft account`;
  }
  // Gmail and Google Workspace: Google's own sign-in offered (when this copy has it)
  $("a-google").hidden = !(addressHost === "google" && googleAvailable && !editingImap);
  if (!$("a-google").hidden) $("a-google-title").textContent = `${address} is a Google account`;
}

/** This copy of Eudora has Google's sign-in built in (`google_available`). */
let googleAvailable = false;
invoke("google_available").then((yes) => {
  googleAvailable = yes;
  $("p-google-row").hidden = !yes;
}).catch(() => {});

/** Google's sign-in in the browser (`address`: the account meant, else Google asks). */
async function signInGoogle(address) {
  status(`Finish signing in${address ? ` to ${address}` : ""} in your browser (it waits up to 5 minutes)…`);
  const account = await invoke("google_sign_in", { address: address || null });
  invoke("check_mail").catch(() => {});
  await showNewAccount(account);
  return account;
}

$("a-google-signin").addEventListener("click", async () => {
  const button = $("a-google-signin");
  button.disabled = true;
  $("a-google-title").textContent = "Finish signing in in your browser (it waits up to 5 minutes)…";
  try {
    await signInGoogle($("a-address").value.trim());
    $("account-box").close();
  } catch (err) {
    $("a-google-title").textContent = `Not signed in: ${err}`;
  }
  button.disabled = false;
});

$("p-add-google").addEventListener("click", async () => {
  const button = $("p-add-google");
  button.disabled = true;
  try { await signInGoogle(null); } catch (err) { showError("Adding the Google account", err); }
  button.disabled = false;
});

/** Microsoft 365 / Outlook.com (Sign in with Microsoft) or another account (IMAP / POP3): chosen
 *  at the top of Add account, or set by the address typed. */
function showKind(kind) {
  const microsoft = kind === "microsoft";
  for (const r of document.querySelectorAll('input[name="a-kind"]')) r.checked = r.value === kind;
  $("account-box").classList.toggle("ms-host", microsoft);
  $("a-ms").hidden = !microsoft;
  if (microsoft) $("a-ms-title").textContent = "Sign in with Microsoft";
  msButton();
}
for (const r of document.querySelectorAll('input[name="a-kind"]')) {
  r.addEventListener("change", () => { kindChosen = r.value; addressHost = r.value === "microsoft" ? "microsoft" : null; showKind(r.value); });
}

const protocol = () => document.querySelector('input[name="a-protocol"]:checked')?.value || "imap";

function accountConfig() {
  return {
    address: $("a-address").value.trim(),
    name: $("a-name").value.trim(),
    protocol: protocol(),
    server: $("a-server").value.trim(),
    port: Number($("a-port").value) || 0,
    security: $("a-security").value,
    tls_level: $("a-tls-level").value,
    username: $("a-username").value.trim(),
    smtp_server: $("a-smtp-server").value.trim(),
    smtp_port: Number($("a-smtp-port").value) || 0,
    smtp_security: $("a-smtp-security").value,
    smtp_tls_level: $("a-smtp-tls-level").value,
    smtp_username: $("a-smtp-username").value.trim(),
    smtp_auth: $("a-smtp-auth").checked,
    server_writes: $("a-writes").checked,
    pop_delete_after_days: $("a-pop-days").value === "" ? null : Number($("a-pop-days").value),
    pop_skip_over_mb: $("a-pop-mb").value === "" ? null : Number($("a-pop-mb").value),
  };
}

function accountResult(text, bad) {
  $("a-result").textContent = text;
  $("a-result").classList.toggle("bad", !!bad);
}

/** The usual server names for this address and protocol. */
function guessServers(force) {
  const address = $("a-address").value.trim();
  const domain = address.split("@")[1]?.toLowerCase() || "";
  if (!domain) return;
  // a company's domain on Google Workspace takes Gmail's servers
  const preset = SERVER_PRESETS[domain] || (addressHost === "google" ? SERVER_PRESETS["gmail.com"] : null);
  const incoming = preset ? preset[protocol()] : `${protocol() === "pop3" ? "pop" : "imap"}.${domain}`;
  if (force || !$("a-server").value.trim() || !serverTouched) {
    if (incoming) $("a-server").value = incoming;
  }
  if (!$("a-smtp-server").value.trim()) {
    const server = $("a-server").value.trim();
    // a server named mail.example.com usually sends too
    $("a-smtp-server").value = preset ? preset.smtp : (/^mail\./i.test(server) ? server : `smtp.${domain}`);
  }
  if (!$("a-username").value.trim()) $("a-username").value = address;
  $("a-hint").textContent = preset?.hint || defaultHint();
}

function showProtocol() {
  const pop = protocol() === "pop3";
  $("a-server-label").textContent = pop ? "POP3 server" : "IMAP server";
  $("a-server").placeholder = pop ? "pop.example.com" : "imap.example.com";
  // (POP3 has no folders or flags to change on the server)
  $("a-writes-row").hidden = $("a-writes-gap").hidden = pop;
  for (const e of document.querySelectorAll(".a-pop")) e.hidden = !pop;
}

/** At each start: accounts this computer has no sign-in for yet (after a restore, as Microsoft
 *  and Google sign-ins never travel in a backup) are named, and signed in on a yes: Microsoft
 *  and Google in the browser one after another, then IMAP / POP3 passwords in the account's
 *  window. "Later" asks again at the next start. */
async function checkSignIns() {
  let missing;
  try { missing = await invoke("missing_sign_ins"); } catch { return; }
  if (!missing.length) return;
  const list = missing.map((m) => `• ${m.address} (${m.kind_name})`).join("\n");
  const yes = await askYesNo(
    "Sign in again",
    `This computer has no sign-in yet for:\n${list}\n\nUntil you sign in, Eudora shows their mail already here but downloads nothing new and can't send from them.`,
    "Sign In Now", "Later",
  );
  if (!yes) return;
  const done = [];
  for (const m of missing.filter((m) => m.kind !== "password")) {
    status(`Signing in ${m.address}: finish in your browser (it waits up to 5 minutes)…`);
    try {
      await invoke(m.kind === "google" ? "google_sign_in" : "microsoft_sign_in", { address: m.address });
      done.push(m.address);
    } catch (err) {
      await tell("Not signed in", `${m.address}: ${err}`);
    }
  }
  if (done.length) {
    status(`Signed in: ${done.join(", ")}.`);
    invoke("check_mail").catch(() => {});
  }
  // passwords: the account's own window, one at a time (the next one at the next start)
  const password = missing.find((m) => m.kind === "password");
  if (password) openAccount(password.address);
}

async function openAccount(address) {
  editingImap = address || null;
  addressHost = null;
  kindChosen = null;
  showKind("server");
  // (an account being changed is the kind it is)
  $("a-kind").hidden = !!address;
  serverTouched = false;
  const have = address ? (await invoke("imap_accounts")).find((c) => c.address.toLowerCase() === address.toLowerCase()) : null;
  const c = Object.assign({ address: "", name: "", protocol: "imap", server: "", port: 993, security: "tls", tls_level: "high", username: "",
    smtp_server: "", smtp_port: 587, smtp_security: "starttls", smtp_tls_level: "high", smtp_username: "", smtp_auth: true }, have || {});
  $("a-title").textContent = have ? `Change ${c.address}` : "Add account";
  $("a-address").value = c.address;
  $("a-address").disabled = !!have;
  $("a-name").value = c.name;
  for (const r of document.querySelectorAll('input[name="a-protocol"]')) {
    r.checked = r.value === (c.protocol || "imap");
    // (an account stays IMAP or POP3: they'd be different folders)
    r.disabled = !!have;
  }
  showProtocol();
  $("a-server").value = c.server;
  $("a-security").value = c.security || "tls";
  $("a-port").value = c.port || DEFAULT_PORTS[c.protocol || "imap"][c.security || "tls"];
  $("a-username").value = c.username;
  $("a-password").value = "";
  $("a-password").placeholder = have ? "unchanged" : "";
  $("a-smtp-server").value = c.smtp_server;
  $("a-smtp-security").value = c.smtp_security || "starttls";
  $("a-smtp-port").value = c.smtp_port || DEFAULT_PORTS.smtp[c.smtp_security || "starttls"];
  $("a-smtp-auth").checked = c.smtp_auth !== false;
  $("a-writes").checked = c.server_writes !== false;
  $("a-pop-days").value = c.pop_delete_after_days ?? "";
  $("a-pop-mb").value = c.pop_skip_over_mb ?? "";
  $("a-tls-level").value = c.tls_level || "high";
  $("a-smtp-tls-level").value = c.smtp_tls_level || "high";
  showPorts();
  $("a-smtp-username").value = c.smtp_username;
  $("a-smtp-password").value = "";
  $("a-hint").textContent = defaultHint();
  if (have && !c.smtp_server) guessServers(false);
  accountResult(have && !c.smtp_server ? "Add the outgoing (SMTP) server to send from this account. Check it, then Test and Save." : "");
  $("account-box").showModal();
  (have ? $("a-smtp-server") : $("a-address")).focus();
}

$("a-address").addEventListener("change", async () => {
  await checkHost();
  if (addressHost !== "microsoft") guessServers(false);
});
$("a-ms-cancel").addEventListener("click", () => $("account-box").close());
/** An email address that can be signed in with (name@domain.tld). */
const validEmail = (v) => /^[^\s@]+@[^\s@]+\.[^\s@.]{2,}$/.test((v || "").trim());
// Sign in with Microsoft stays gray until the address above is a real one
const msButton = () => { $("a-ms-signin").disabled = !validEmail($("a-address").value); };
$("a-address").addEventListener("input", msButton);
$("a-ms-signin").addEventListener("click", async () => {
  const button = $("a-ms-signin");
  button.disabled = true;
  $("a-ms-title").textContent = "Finish signing in in your browser (it waits up to 5 minutes)…";
  try {
    const address = $("a-address").value.trim();
    const account = await invoke("microsoft_sign_in", { address });
    $("account-box").close();
    invoke("check_mail").catch(() => {});
    // said plainly: Microsoft often signs in without showing a page (the browser knew the account)
    await showNewAccount(account);
  } catch (err) {
    $("a-ms-title").textContent = `Not signed in: ${err}`;
  }
  msButton();
});
$("a-server").addEventListener("input", () => { serverTouched = true; });
for (const r of document.querySelectorAll('input[name="a-protocol"]')) {
  r.addEventListener("change", () => {
    showProtocol();
    $("a-port").value = DEFAULT_PORTS[protocol()][$("a-security").value];
    showPorts();
    guessServers(true);
  });
}

/** The standard ports beside each box; Encryption only applies with TLS / STARTTLS. */
function showPorts() {
  const incoming = DEFAULT_PORTS[protocol()][$("a-security").value];
  const outgoing = DEFAULT_PORTS.smtp[$("a-smtp-security").value];
  $("a-port-default").textContent = Number($("a-port").value) === incoming ? "standard" : `standard: ${incoming}`;
  $("a-smtp-port-default").textContent = Number($("a-smtp-port").value) === outgoing
    ? ($("a-smtp-security").value === "none" ? "standard" : "submission port") : `standard: ${outgoing}`;
  $("a-tls-level").disabled = $("a-security").value === "none";
  $("a-smtp-tls-level").disabled = $("a-smtp-security").value === "none";
}

/** Choosing the security moves the port to its standard one (SSL/TLS 465, STARTTLS 587 for
 *  sending: the submission ports), unless the port was set by hand to something unusual. */
function followPort(select, port, ports) {
  select.addEventListener("change", () => {
    const usual = Object.values(DEFAULT_PORTS).flatMap((p) => Object.values(p));
    if (!$(port).value || usual.includes(Number($(port).value))) $(port).value = ports()[select.value];
    showPorts();
  });
}
followPort($("a-security"), "a-port", () => DEFAULT_PORTS[protocol()]);
followPort($("a-smtp-security"), "a-smtp-port", () => DEFAULT_PORTS.smtp);
$("a-port").addEventListener("input", showPorts);
$("a-smtp-port").addEventListener("input", showPorts);
$("a-smtp-auth").addEventListener("change", () => {
  $("a-smtp-username").disabled = $("a-smtp-password").disabled = !$("a-smtp-auth").checked;
});

$("a-test").addEventListener("click", async () => {
  await checkHost();
  if (addressHost === "microsoft") return;
  const config = accountConfig();
  const password = $("a-password").value;
  if (!password) return accountResult("Type the password to test the account.", true);
  $("a-test").disabled = true;
  accountResult(`Signing in to ${config.server}${config.smtp_server ? ` and ${config.smtp_server}` : ""}…`);
  try {
    const r = await invoke("imap_test", { config, password, smtpPassword: $("a-smtp-password").value || null });
    const top = r.folders.slice().sort((a, b) => b.messages - a.messages).slice(0, 4).map((f) => `${f.path} ${number.format(f.messages)}`);
    let text = `Incoming: signed in (${r.security}). ${r.folders.length} folder${r.folders.length === 1 ? "" : "s"}, ${number.format(r.messages)} messages (${top.join(", ")}${r.folders.length > 4 ? ", …" : ""}).`;
    if (!r.smtp_tested) text += " Outgoing: no SMTP server, so this account won't send.";
    else if (r.smtp_error) text += ` Outgoing: ${r.smtp_error}`;
    else text += ` Outgoing: signed in to ${config.smtp_server} (${r.smtp_security}).`;
    accountResult(text, !!r.smtp_error);
  } catch (err) {
    accountResult(String(err), true);
  } finally {
    $("a-test").disabled = false;
  }
});

$("a-save").addEventListener("click", async () => {
  await checkHost();
  if (addressHost === "microsoft") return;
  const config = accountConfig();
  const password = $("a-password").value;
  if (!config.address.includes("@")) return accountResult("Type the account's email address.", true);
  if (!config.server) return accountResult("Type the incoming mail server's name.", true);
  if (!editingImap && !password) return accountResult("Type the password (it stays on this computer).", true);
  try {
    await invoke("imap_save", { config, password: password || null, smtpPassword: $("a-smtp-password").value || null });
    $("account-box").close();
    await invoke("check_mail").catch(() => {});
    status(`${config.address} ${editingImap ? "changed" : "added"}${editingImap ? "." : ": its mail is on the way (Check Mail started)."}`);
    await loadAccounts();
    renderSidebar();
    if ($("prefs").open) openPrefs();
  } catch (err) {
    accountResult(String(err), true);
  }
});

$("a-cancel").addEventListener("click", () => $("account-box").close());
$("p-add-account").addEventListener("click", () => openAccount());
// another Microsoft account beside the ones signed in (Microsoft lets you pick which in the browser)
$("p-add-microsoft").addEventListener("click", async () => {
  const button = $("p-add-microsoft");
  // which account: Microsoft's page goes to it (not to the one the browser is signed in to)
  const address = await askText("Add a Microsoft account", "The account's email address (Microsoft 365, Outlook.com or Hotmail):", "", "Sign in with Microsoft");
  if (!address || !address.includes("@")) return;
  button.disabled = true;
  status(`Finish signing in to ${address} in your browser (it waits up to 5 minutes)…`);
  try {
    const account = await invoke("microsoft_sign_in", { address: address.trim() });
    await showNewAccount(account);
    invoke("check_mail").catch(() => {});
  } catch (err) {
    showError("Adding the Microsoft account", err);
  }
  button.disabled = false;
});

// ------------------------------------------------------------------ web access: the firewall

function showFirewall(s) {
  const port = Number($("p-web-port").value) || 443;
  const open = s.allowed;
  // the Mac and Linux: Eudora can't change their firewall (the Mac asks by itself): it explains
  $("p-firewall").hidden = !s.supported;
  if (!s.supported) {
    $("p-firewall-about").textContent = "Eudora on the web (eudora-web) answers phones and computers on this network.";
    $("p-firewall-remove").hidden = true;
    $("p-firewall-status").textContent = s.detail;
    return;
  }
  $("p-firewall-status").textContent = open
    ? `Allowed: port ${s.ports} (${s.detail}).${String(port) !== s.ports ? ` Click Allow again to move it to port ${port}.` : ""}`
    : "Not allowed yet: phones on this network can't reach Eudora on the web.";
  $("p-firewall").textContent = open ? "Allow again" : "Allow phones on this network";
  $("p-firewall-remove").hidden = !open;
}

async function checkFirewall() {
  try {
    showFirewall(await invoke("firewall_status"));
  } catch (err) {
    $("p-firewall-status").textContent = `Couldn't read the firewall: ${err}`;
  }
}

async function setFirewall(allow) {
  const port = Number($("p-web-port").value) || 0;
  store.set("webPort", port);
  $("p-firewall").disabled = $("p-firewall-remove").disabled = true;
  $("p-firewall-status").textContent = "Waiting for Windows' permission prompt…";
  try {
    showFirewall(await invoke("firewall_set", { port, allow }));
  } catch (err) {
    $("p-firewall-status").textContent = String(err);
  } finally {
    $("p-firewall").disabled = $("p-firewall-remove").disabled = false;
  }
}

$("p-firewall").addEventListener("click", () => setFirewall(true));
$("p-firewall-remove").addEventListener("click", () => setFirewall(false));
$("p-web-port").addEventListener("change", checkFirewall);

$("p-find").addEventListener("click", findMailboxes);
$("p-ms-signin").addEventListener("click", microsoftSignIn);
$("p-add").addEventListener("click", addFound);
$("prefs-open").addEventListener("click", openPrefs);
$("p-save").addEventListener("click", savePrefs);
$("p-cancel").addEventListener("click", () => $("prefs").close());

// ------------------------------------------------------------------ new mail

const REFRESH_EVERY = 10_000; // ms
let refreshing = false;

/**
 * The accounts. `quick`: at once, each account's count taken from its folders' (a message in
 * two folders counts twice) and the total left unknown; the exact counts read every location
 * (seconds on a large library), so startup asks for them after the mail is on screen.
 */
/** An account just added: in the sidebar at once, under Accounts (opened), and said in the status bar. */
async function showNewAccount(address) {
  try {
    state.folders = await invoke("folders");
    await loadAccounts(true);
  } catch (err) {
    showError("Reading the accounts", err);
  }
  state.open.add("#accounts");
  renderSidebar();
  status(`Added ${address}: its mail is coming in.`);
}

async function loadAccounts(quick = false) {
  const accounts = await invoke("accounts", { counts: !quick });
  if (quick) {
    for (const a of accounts.accounts) a.messages = state.folders.filter((f) => f.account_id === a.id).reduce((n, f) => n + f.messages, 0);
    accounts.total = accounts.accounts.reduce((n, a) => n + a.messages, 0);
    accounts.estimated = true;
  }
  state.accounts = accounts;
}

/** Shows how the mailbox download is doing (toolbar badge). A running download writes its
 * status every 2 s: one that hasn't for a minute isn't running any more. */
/** The pass's count of new messages, and when it last rose (the badge says Downloading then). */
let dlNew = null;
async function showDownload() {
  let s = null;
  try { s = await invoke("download_status"); } catch { return; }
  const box = $("download");
  if (!s || (s.state === "done" && s.age > 24 * 3600)) { box.hidden = true; showStatusWork(); return; }
  const active = s.state === "running" || s.state === "waiting" || s.state === "watching" || s.state === "paused";
  const percent = s.total ? ` (${Math.floor((s.done / s.total) * 100)}%)` : "";
  const progress = `${number.format(s.done)} of ${number.format(s.total)}${percent}`;
  let kind, text;
  if (active && s.age > 60 && s.alive) {
    // running, but a long step keeps it from saying so (it holds the library's lock)
    kind = "running";
    text = `Still working at ${progress}: a long step (${Math.round(s.age / 60)} min so far)`;
  } else if (active && s.age > 60) {
    kind = "problem";
    text = `The download stopped at ${progress}; Eudora starts it again by itself in a moment, and it carries on from there.`;
  } else if (s.state === "waiting") {
    const mins = Math.max(1, Math.round((s.offline_secs || 0) / 60));
    kind = "waiting";
    text = `No connection for ${mins} min: waiting, continues by itself`;
  } else if (s.state === "paused") {
    kind = "paused";
    text = s.line.startsWith("Paused ·") ? `Paused at ${progress}` : `Old mail paused at ${progress}: new mail still comes in`;
  } else if (s.state === "watching") {
    kind = "done";
    text = s.line; // "Up to date. Next check at 10:42 PM"
  } else if (s.state === "running") {
    // "Downloading" only while mail is coming in; a pass that only checks
    // what's there says so: "… is up to date · checking folder 86 of 224"
    const now = Date.now();
    if (!dlNew || s.new < dlNew.count) dlNew = { count: s.new, at: 0 };
    else if (s.new > dlNew.count) dlNew = { count: s.new, at: now };
    const arriving = now - dlNew.at < 90_000;
    kind = arriving ? "running" : "done";
    const where = s.line.match(/folder ([\d,]+) of ([\d,]+)/);
    // (a step before the download proper says what it is: "Converting again archive item …")
    text = !s.total ? s.line
      : arriving ? `Downloading ${s.account}: ${number.format(s.new)} new so far`
      : `${s.account} is up to date · checking ${where ? `folder ${where[1]} of ${where[2]}` : "for changes"}${percent}`;
  } else if (s.state === "done") {
    kind = "done";
    text = s.line;
  } else {
    kind = "problem";
    text = s.line;
  }
  const changed = box.dataset.kind !== kind;
  box.className = kind;
  box.dataset.kind = kind;
  box.hidden = false;
  box.querySelector(".dl-text").textContent = text;
  box.title = `${s.line}\n${number.format(s.new)} new, ${number.format(s.already)} already here as other copies` +
    (s.outages ? `\nConnection lost and recovered ${s.outages} time${s.outages > 1 ? "s" : ""} so far` : "") +
    (s.offline_reason ? `\nWhy: ${s.offline_reason}` : "");
  // an alert line when it changes (lost, back, finished, stopped)
  if (changed && box.dataset.seen) {
    const wasWaiting = box.dataset.was === "waiting";
    const say = {
      waiting: "Connection lost: the download is waiting and will continue by itself.",
      running: wasWaiting ? "Connection back: the download continues." : null, // (mail arriving: the badge says so)
      done: s.state === "running" ? (wasWaiting ? "Connection back: the download continues." : null) : s.line,
      problem: text,
    };
    if (say[kind]) status(say[kind]);
  }
  box.dataset.was = kind;
  box.dataset.seen = "1";
  showStatusWork();
}

/** Catches up with mail a download or import has added: counts, new folders and accounts,
 * the open list's rows and the search index. The list only moves when it really changed. */
/** The rules' notices (their "notify" action): shown in the status bar, and listed on a click. */
async function showNotices() {
  let found;
  try { found = await invoke("notices"); } catch { return; }
  if (!found.length) return;
  state.notices = [...found, ...(state.notices || [])].slice(0, 50);
  const last = found[found.length - 1];
  status(found.length === 1 ? `Rule “${last.rule}”: ${last.from} · ${last.subject}` : `${found.length} messages matched rules that notify you (latest: ${last.subject}).`);
}

/** After new mail: the Views' counts, their senders' and the accounts' exact totals counted
 *  again in the background, at most every 5 minutes (the open list's own total at each check), the old numbers shown meanwhile (never
 *  "Counting…"); each number changes when its count is in. */
let lastRecount = 0;
let recounting = false;
async function recountLazily(withAccounts = true) {
  if (recounting || Date.now() - lastRecount < 300_000) return;
  recounting = true;
  lastRecount = Date.now();
  try {
    // only the numbers on screen;
    // the others are counted when their section is opened
    const shown = (prefix) => !!document.querySelector(`#folders [data-key^="${CSS.escape(prefix)}"]`);
    for (const scope of [...state.viewCounts.keys()]) {
      if (!shown(`view:${scope}:`)) continue; // (its old numbers kept: shown at once when opened)
      try { state.viewCounts.set(scope, await invoke("views_list", { accounts: scope === "combined" ? null : [scope] })); } catch { /* the old ones stay */ }
    }
    for (const key of [...state.viewGroups.keys()]) {
      if (!shown(`${key}:`)) continue;
      const [, scope, view] = key.split(":");
      try { state.viewGroups.set(key, await invoke("view_groups", { view, accounts: scope === "combined" ? null : [Number(scope)] })); } catch { /* the old ones stay */ }
    }
    if (withAccounts && (state.open.has("#accounts") || state.list?.kind === "all")) try { await loadAccounts(); } catch { /* the old totals stay */ }
    renderSidebar();
  } finally {
    recounting = false;
  }
}

async function refresh(force = false) {
  if (!force) { showDownload(); showNotices(); }
  if ((refreshing && !force) || !state.folders.length) return;
  refreshing = true;
  try {
    const folders = await invoke("folders", { refresh: true });
    const before = new Map(state.folders.map((f) => [f.id, f.messages]));
    const added = folders.reduce((n, f) => n + Math.max(0, f.messages - (before.get(f.id) || 0)), 0);
    if (!force && folders.length === state.folders.length && !added && folders.every((f) => before.get(f.id) === f.messages)) {
      // a View fills as the background sorts mail into it, after the mail itself came in
      // its own count, each time
      const list = state.list;
      if (list?.kind === "view" && !list.cached) {
        const total = await listTotal(list);
        if (state.list === list && total !== list.total) replaceRows(list, total);
      }
      return;
    }
    state.folders = folders;
    // the folders' new counts at once; the Views' and accounts' totals lazily
    renderSidebar();
    recountLazily();
    const list = state.list;
    if (!force && list && list.kind !== "search") {
      const total = list.kind === "folder" ? await folderTotal(list.folders, folders) : await listTotal(list);
      if (total !== list.total) replaceRows(list, total);
    }
    if (added) status(undefined, `${number.format(added)} new in the last ${REFRESH_EVERY / 1000} s (updated ${timeFormat.format(new Date())})`);
    // the tray icon shows new mail until the window is looked at again
    if (added && !document.hasFocus()) invoke("tray_set", { newMail: true }).catch(() => {});
    // new mail: the lists kept ready are read again soon, so a click shows it
    if (added) setTimeout(warmLists, 5000);
  } catch (err) {
    console.warn("refresh", err); // tried again in a moment
  } finally {
    refreshing = false;
  }
}

// ------------------------------------------------------------------ splitters

// pane sizes: the list's share is kept apart for a reading pane below it and one beside it
const sizes = Object.assign({ folders: 260, list: 0.42, listBeside: 0.4 }, store.get("sizes", {}));

function applySizes() {
  const beside = skin().reading === "right";
  document.documentElement.style.setProperty("--folders-width", `${sizes.folders}px`);
  document.documentElement.style.setProperty("--list-share", beside ? sizes.listBeside : sizes.list);
}

function setupSplitters() {
  applySizes();
  for (const bar of document.querySelectorAll(".splitter")) {
    bar.addEventListener("mousedown", (down) => {
      down.preventDefault();
      const right = $("right").getBoundingClientRect();
      document.body.classList.add("dragging"); // (the message's frame mustn't take the mouse meanwhile)
      const move = (e) => {
        if (bar.dataset.split === "folders") sizes.folders = Math.min(600, Math.max(140, e.clientX));
        else if (skin().reading === "right") sizes.listBeside = Math.min(0.7, Math.max(0.18, (e.clientX - right.left) / right.width));
        else sizes.list = Math.min(0.85, Math.max(0.12, (e.clientY - right.top) / right.height));
        applySizes();
        renderList();
      };
      const up = () => {
        document.removeEventListener("mousemove", move);
        document.removeEventListener("mouseup", up);
        store.set("sizes", sizes);
        document.body.classList.remove("dragging");
      };
      document.addEventListener("mousemove", move);
      document.addEventListener("mouseup", up);
    });
  }
}

// ------------------------------------------------------------------ actions: write, transfer, delete

/** The selected message's row (and the folder it's in here: the open folder, else its first). */
function selectedRow() {
  // a message window: its one message (moves and deletes are done from the main window)
  if (READER) return state.messageId != null ? { row: { id: state.messageId }, from: null } : null;
  const list = state.list;
  const row = list?.rows.get(state.selected);
  if (!row) return null;
  const from = list.kind === "folder" ? list.folders[0] : row.labels?.[0]?.[0];
  return { row, from };
}

function needMessage() {
  const s = selectedRow();
  if (!s) status("Choose a message first.");
  return s;
}

async function compose(mode) {
  if (mode === "forward" && selectionCount() > 1) {
    const items = await selectedItems();
    if (items.length > 100 && !(await askYesNo("Forward many messages", `Forward ${number.format(items.length)} messages, each attached to one new message?`, "Forward them"))) return;
    openComposer("forwardMany", null, items.map((i) => i.id));
    return;
  }
  const s = mode === "new" ? null : needMessage();
  if (mode !== "new" && !s) return;
  if (selectionCount() > 1) status("Replying to the message shown (several are selected).");
  openComposer(mode, s ? s.row.id : null);
}

// ------------------------------------------------------------------ where a message is written

/** Each look writes messages the way its program does: Eudora in a window inside the main
 * window, Gmail in a box docked at the bottom right, the new Outlook in the reading pane,
 * Yahoo in a tab over the mail; Thunderbird and classic Outlook in a window of its own. Every
 * one can be popped out into a window of its own. */
const COMPOSE_PLACE = {
  eudora: "inner",
  thunderbird: "window",
  "lookout-classic": "window",
  "lookout-new": "pane",
  gmail: "dock",
  yahoo: "tab",
};

/** Where a message is written: the look's place; in Gmail-style a reply or forward is written
 *  in the message's place at full width, as Gmail does, and only a new message in the corner
 *  box. */
function composePlace(mode) {
  const place = COMPOSE_PLACE[state.skin] || "inner";
  return place === "dock" && mode !== "new" ? "pane" : place;
}

const composers = new Map(); // frame id → { box, frame, place }
let composeCount = 0;
let innerZ = 10;

window.mainStatus = (text) => status(text);

window.composeTitle = (id, title) => {
  const c = composers.get(id);
  if (c) c.box.querySelector(".cw-title").textContent = title;
};

window.closeCompose = (id) => {
  const c = composers.get(id);
  if (!c) return;
  c.box.remove();
  composers.delete(id);
  if (c.place === "pane") document.body.classList.remove("composing-pane");
  if (c.place === "tab") document.body.classList.remove("composing-tab");
};

function composeFrame(id, mode, messageId, ids) {
  const frame = document.createElement("iframe");
  frame.className = "cw-frame";
  frame.dataset.compose = id;
  frame.src = `compose.html?mode=${mode}&id=${messageId ?? ""}&ids=${(ids || []).join(",")}&frame=${id}`;
  return frame;
}

/** The title bar of a message inside the main window: its buttons depend on where it is. */
function composeHeader(id, place) {
  const head = el("div", "cw-head");
  head.append(el("span", "cw-title", "New Message"));
  const button = (text, title, act) => {
    const b = el("button", "cw-btn", text);
    b.title = title;
    b.addEventListener("click", (e) => { e.stopPropagation(); act(); });
    head.append(b);
  };
  const c = () => composers.get(id);
  if (place === "inner" || place === "dock") button("–", "Minimize", () => c().box.classList.toggle("min"));
  if (place === "inner") button("□", "Maximize", () => c().box.classList.toggle("max"));
  button("⇱", "Pop out: in a window of its own", () => c().frame.contentWindow.composeAction?.("pop-out"));
  button("×", "Close", () => c().frame.contentWindow.composeAction?.("close"));
  return head;
}

async function openComposer(mode, messageId, ids = null) {
  // from a message window: written where the main window writes (inside it), or in a window of
  // its own when this one stands alone
  if (READER) {
    if (READER_FRAME && window.parent.openComposer) { window.parent.openComposer(mode, messageId, ids); return; }
    try { await invoke("open_compose", { mode, id: messageId, ids }); } catch (err) { showError("Opening a message window", err); }
    return;
  }
  const place = composePlace(mode);
  const single = place === "pane" || place === "tab";
  const busy = single && [...composers.values()].some((c) => c.place === place);
  if (place === "window" || busy) {
    try { await invoke("open_compose", { mode, id: messageId, ids }); } catch (err) { showError("Opening a message window", err); }
    return;
  }
  const id = `c${++composeCount}`;
  const box = el("div", `cw cw-${place}`);
  const frame = composeFrame(id, mode, messageId, ids);
  box.append(composeHeader(id, place), frame);
  composers.set(id, { box, frame, place });
  if (place === "inner") {
    // cascade, like classic Eudora's windows
    const n = [...composers.values()].filter((c) => c.place === "inner").length - 1;
    box.style.left = `${40 + (n % 8) * 28}px`;
    box.style.top = `${30 + (n % 8) * 28}px`;
    box.style.zIndex = ++innerZ;
    box.addEventListener("mousedown", () => { box.style.zIndex = ++innerZ; });
    dragByHead(box);
    $("panes").append(box);
  } else if (place === "dock") {
    $("dock").append(box);
  } else if (place === "pane") {
    document.body.classList.add("composing-pane");
    // (a look that shows the message in place of the list: the pane shows for the writing too)
    if (skin().reading === "replace") document.body.classList.add("showing-message");
    $("message-pane").append(box);
  } else {
    document.body.classList.add("composing-tab");
    $("right").append(box);
  }
  frame.addEventListener("load", () => frame.contentWindow?.focus());
}

function dragByHead(box) {
  const head = box.querySelector(".cw-head");
  head.addEventListener("mousedown", (down) => {
    if (down.target.closest("button") || box.classList.contains("max")) return;
    down.preventDefault();
    const start = { x: down.clientX, y: down.clientY, left: box.offsetLeft, top: box.offsetTop };
    // the message's frame mustn't swallow the mouse while dragging
    const cover = el("div", "cw-cover");
    document.body.append(cover);
    const move = (e) => {
      box.style.left = `${Math.max(0, start.left + e.clientX - start.x)}px`;
      box.style.top = `${Math.max(0, start.top + e.clientY - start.y)}px`;
    };
    const up = () => { cover.remove(); document.removeEventListener("mousemove", move); document.removeEventListener("mouseup", up); };
    document.addEventListener("mousemove", move);
    document.addEventListener("mouseup", up);
  });
}

// ------------------------------------------------------------------ a message on its own

/** Fewer headers (subject, From and Date) or all of them: one choice for every message, kept
 *  until changed. Message windows follow it too (the same storage). */
function applyCompactHeaders() {
  const compact = store.get("compactHeaders", false) === true;
  document.body.classList.toggle("compact-headers", compact);
  const b = $("m-hdr");
  b.title = compact ? "Show all the headers (kept for every message)" : "Show fewer headers (kept for every message)";
  b.replaceChildren(icon(compact ? "chevron-down" : "chevron-up"));
}
$("m-hdr").addEventListener("click", () => {
  store.set("compactHeaders", !(store.get("compactHeaders", false) === true));
  applyCompactHeaders();
});
window.addEventListener("storage", (e) => { if (e.key?.endsWith("compactHeaders")) applyCompactHeaders(); });

/** Maximize: the message in a window inside Eudora's window, maximized (□ makes it a window
 *  that moves and sizes; ⇱ pops it out; – minimizes it), as classic Eudora opened messages. */
let readerCount = 0;
function openInnerReader(id, title) {
  const key = `r${++readerCount}`;
  const box = el("div", "cw cw-inner cw-reader max");
  const head = el("div", "cw-head");
  head.append(el("span", "cw-title", title || "Message"));
  const frame = document.createElement("iframe");
  frame.className = "cw-frame";
  frame.dataset.reader = key;
  frame.src = `index.html?message=${id}&frame=${key}`;
  const button = (text, tip, act) => {
    const b = el("button", "cw-btn", text);
    b.title = tip;
    b.addEventListener("click", (e) => { e.stopPropagation(); act(); });
    head.append(b);
  };
  button("–", "Minimize", () => box.classList.toggle("min"));
  button("□", "Maximize / restore", () => box.classList.toggle("max"));
  button("⇱", "Pop out: in a window of its own", () => { popOutReader(id, title); box.remove(); });
  button("×", "Close (Esc)", () => box.remove());
  box.append(head, frame);
  const n = document.querySelectorAll(".cw-inner").length;
  box.style.left = `${40 + (n % 8) * 28}px`;
  box.style.top = `${30 + (n % 8) * 28}px`;
  box.style.zIndex = ++innerZ;
  box.addEventListener("mousedown", () => { box.style.zIndex = ++innerZ; });
  dragByHead(box);
  $("panes").append(box);
  frame.addEventListener("load", () => frame.contentWindow?.focus());
}

/** Pop out: the message in a window of its own. */
async function popOutReader(id, title) {
  try { await invoke("open_reader", { id, title: title || "Message" }); } catch (err) { showError("Opening the message in its own window", err); }
}

$("m-max").addEventListener("click", () => { if (state.messageId != null) openInnerReader(state.messageId, current?.subject); });
$("m-pop").addEventListener("click", () => { if (state.messageId != null) popOutReader(state.messageId, current?.subject); });

/** A message window inside Eudora that has the keyboard (menu commands go to it). */
function activeReader() {
  const f = document.activeElement;
  return f?.tagName === "IFRAME" && f.dataset.reader ? f : null;
}

/** A message window takes every command about its message; only what's about a list stays in the main window. */
const LIST_ONLY = new Set(["select-all", "sort-selected", "filter-messages", "mark-all-read", "next-message", "previous-message"]);
window.readerAction = (act) => {
  if (act === "close") { closeThisReader(); return; }
  if (LIST_ONLY.has(act)) status("That works in the main window, on the messages selected there.");
  else ACTIONS[act]?.();
};
/** Closes this message window (inside Eudora, or in a window of its own). */
function closeThisReader() {
  if (READER_FRAME) window.parent.closeReader?.(READER_FRAME);
  else window.__TAURI__.webviewWindow.getCurrentWebviewWindow().close();
}
window.closeReader = (key) => document.querySelector(`iframe[data-reader="${key}"]`)?.closest(".cw")?.remove();

/** The message being written that has the keyboard (menu commands go to it). */
function activeComposer() {
  const f = document.activeElement;
  return f?.tagName === "IFRAME" && f.dataset.compose ? f : null;
}

/** Reloads the open list in place (after a move), keeping the place in it. */
async function reloadList() {
  // a message window's message was moved (Spam, Delete, Transfer): the main window's list catches
  // up, and the window shows it where it is now (closed when it left the mail, deleted to the Trash)
  if (READER) {
    try { window.parent?.reloadList?.(); } catch { /* (a window of its own: the main one catches up within seconds) */ }
    if (current) {
      try { current = { ...current, ...(await invoke("message", { id: current.id })) }; } catch { /* (gone) */ }
    }
    return;
  }
  const list = state.list;
  if (!list || list.kind === "search") { runSearch(); return; }
  const keep = state.selected;
  const top = $("list").scrollTop;
  // the selection is of messages, not of places in the list: remembered by id, found again
  // after the reload; those that moved away are let go
  const wasAll = state.allSel;
  const keptIds = new Set([...state.sel].map((i) => list.rows.get(i)?.id).filter((id) => id != null));
  const shownId = state.messageId ?? list.rows.get(keep)?.id ?? null;
  const lastIndex = Math.max(keep, ...state.sel, 0);
  await refresh(true);
  if (state.list !== list) return;
  list.total = await listTotal(list);
  list.stale = null;
  list.rows = new Map();
  clearRows();
  // the rows where the selection was (one page more: new mail may have pushed them down)
  if (!wasAll && (keptIds.size || shownId != null)) {
    const last = Math.min(Math.floor(lastIndex / PAGE) + 1, Math.floor(Math.max(0, list.total - 1) / PAGE));
    for (let page = 0; page <= last; page++) {
      const rows = await readPage(list, page);
      if (state.list !== list) return;
      rows.forEach((row, i) => list.rows.set(page * PAGE + i, row));
    }
  }
  const indexOf = new Map([...list.rows].map(([i, row]) => [row.id, i]));
  state.sel = wasAll ? new Set() : new Set([...keptIds].map((id) => indexOf.get(id)).filter((i) => i != null));
  state.allSel = wasAll && list.total > 0;
  showListCount();
  $("list").scrollTop = top;
  renderList(true);
  if (shownId != null && indexOf.has(shownId)) {
    state.selected = indexOf.get(shownId);
  } else {
    // the message shown moved away: nothing new is opened in its place
    if (shownId != null && !wasAll) emptyReader();
    state.selected = state.sel.size ? Math.min(...state.sel) : -1;
  }
  state.anchor = state.selected;
  showSelection();
}

const undoStack = [];

/** The selected messages grouped by the folder they're taken from (moves work per folder). */
async function selectionByFolder() {
  if (!state.list || selectionCount() === 0) { status("Choose a message first."); return null; }
  const items = await selectedItems();
  const groups = new Map();
  let unknown = 0;
  for (const item of items) {
    if (item.from == null) { unknown++; continue; }
    if (!groups.has(item.from)) groups.set(item.from, []);
    groups.get(item.from).push(item.id);
  }
  if (unknown) status(`${unknown} of them aren't in a folder known here: open their folder to move them.`);
  return { groups, count: items.length - unknown };
}

/** Asks first when it's a lot of messages. */
async function confirmMany(count, what) {
  return count <= 100 || (await askYesNo(`${what} ${number.format(count)} messages`, `${what} ${number.format(count)} messages?`, `${what} them`));
}

function afterMove() {
  // (a message window keeps showing its message, now where it was moved to)
  if (READER) return reloadList().then(() => { if (current) $("m-folders").textContent = (current.folders || []).join(" · "); });
  state.messageId = null;
  const keep = state.selected;
  clearSelection();
  return reloadList().then(() => select(Math.min(keep, Math.max(0, listCount(state.list) - 1))));
}

async function moveSelected(to, label) {
  const sel = await selectionByFolder();
  if (!sel || !sel.count || !(await confirmMany(sel.count, "Move"))) return;
  try {
    let moved = 0;
    const done = [];
    for (const [from, ids] of sel.groups) {
      if (from === to) continue;
      moved += await invoke("move_messages", { ids, from, to });
      done.push({ ids, from, to });
    }
    undoStack.push({ groups: done, what: label });
    status(`${moved ? `Moved ${number.format(sel.count)} message${sel.count > 1 ? "s" : ""}` : "Already there"}: ${label}. (Edit ▸ Undo puts ${sel.count > 1 ? "them" : "it"} back.)`);
    await afterMove();
  } catch (err) {
    showError("Moving messages", err);
  }
}

async function deleteSelected() {
  const sel = await selectionByFolder();
  if (!sel || !sel.count || !(await confirmMany(sel.count, "Delete"))) return;
  try {
    const done = [];
    for (const [from, ids] of sel.groups) {
      const trash = await invoke("delete_messages", { ids, from });
      if (trash !== from) done.push({ ids, from, to: trash });
    }
    if (!done.length) { status("Already in the Trash."); return; }
    undoStack.push({ groups: done, what: "deleted" });
    status(`${sel.count > 1 ? `${number.format(sel.count)} messages` : "Moved"} to the Trash. (Edit ▸ Undo puts ${sel.count > 1 ? "them" : "it"} back.)`);
    await afterMove();
  } catch (err) {
    showError("Deleting messages", err);
  }
}

async function undo() {
  const last = undoStack.pop();
  if (!last) { status("Nothing to undo."); return; }
  try {
    for (const g of last.groups || [{ ids: last.ids, from: last.from, to: last.to }]) {
      await invoke("move_messages", { ids: g.ids, from: g.to, to: g.from });
    }
    status(`Undone (${last.what}).`);
    await reloadList();
  } catch (err) {
    showError("Undo", err);
  }
}

/** The folder the user is in: the open mailbox, else the folder of the message shown. */
function currentFolderId() {
  const list = state.list;
  if (!list) return null;
  if (list.kind === "folder" && list.folders?.length) return list.folders[0];
  return list.rows?.get(state.selected)?.labels?.[0]?.[0] ?? null;
}

/**
 * The mailbox chooser (Transfer To, Copy To, Open Mailbox): resolves to a folder, or null. A tree
 * like the sidebar's. Only one branch is open at a time; typing filters it (matches with the
 * folders above them). Keys: ↑ ↓ move, → opens / goes in, ← closes / goes up, Enter chooses.
 */
function pickFolder(title, okLabel = "OK") {
  return new Promise((resolve) => {
    const dialog = $("pick");
    $("pick-title").textContent = title;
    $("pick-ok").textContent = okLabel;
    // the tree: each account, its folders as the sidebar shows them (a folder and its Online
    // Archive twin are one choice: the mailbox's own, never moving mail out of the archive)
    const nodes = new Map();
    const roots = [];
    const byId = new Map(state.folders.map((f) => [f.id, f]));
    const own = state.accounts.accounts.filter((a) => a.kind !== "graph-shared");
    const shared = state.accounts.accounts.filter((a) => a.kind === "graph-shared");
    for (const a of [...own, ...shared]) {
      const tree = folderTree(a);
      const root = { key: `#${a.id}`, name: a.shown, iconName: "envelope", account: a, parent: null, children: [], target: null, count: a.messages, text: a.shown.toLowerCase() };
      nodes.set(root.key, root);
      roots.push(root);
      const add = (treeNode, parent) => {
        const children = [...treeNode.children.values()].sort((x, y) => x.name.localeCompare(y.name, undefined, { numeric: true, sensitivity: "base" }));
        for (const c of children) {
          const folders = c.folders.map((id) => byId.get(id)).filter(Boolean);
          const f = folders.find((x) => !x.path.startsWith("Online Archive/")) || folders[0] || null;
          const node = {
            key: c.key, name: c.name, account: a, parent, children: [], count: c.messages, unread: c.unread,
            target: f ? { ...f, shown: c.path } : null, text: `${a.shown} › ${c.path}`.toLowerCase(),
          };
          nodes.set(node.key, node);
          parent.children.push(node);
          add(c, node);
        }
      };
      add(tree, root);
    }
    const ancestors = (node) => {
      const out = [];
      for (let n = node.parent; n; n = n.parent) out.push(n);
      return out;
    };
    // where it starts: the current folder, its branch open (else the current account)
    const here = state.folders.find((f) => f.id === currentFolderId());
    let cur = (here && nodes.get(folderKey(here))) || (here && nodes.get(`#${here.account_id}`)) || roots[0];
    let open = new Set();
    const openOnly = (node) => {
      open = new Set([...ancestors(node), node].filter((n) => n.children.length).map((n) => n.key));
    };
    const close = (node) => {
      for (const k of [...open]) if (k === node.key || k.startsWith(`${node.key}/`)) open.delete(k);
    };
    if (cur) openOnly(cur.parent || cur);
    if (cur && cur.parent) open.delete(cur.key);
    // what shows: the open branch, or with words typed, every match and the folders above it
    let visible = [];
    const show = () => {
      const words = $("pick-filter").value.toLowerCase().split(/\s+/).filter(Boolean);
      visible = [];
      let shownSet = null;
      if (words.length) {
        shownSet = new Set();
        for (const n of nodes.values()) {
          if (words.every((w) => n.text.includes(w))) for (const x of [n, ...ancestors(n)]) shownSet.add(x.key);
        }
      }
      const walk = (list, depth) => {
        for (const n of list) {
          if (shownSet && !shownSet.has(n.key)) continue;
          visible.push({ n, depth });
          const expanded = shownSet ? n.children.some((c) => shownSet.has(c.key)) : open.has(n.key);
          if (expanded) walk(n.children, depth + 1);
        }
      };
      walk(roots, 0);
      if (shownSet && (!cur || !shownSet.has(cur.key) || !words.every((w) => cur.text.includes(w)))) {
        cur = visible.find((v) => v.n.target && words.every((w) => v.n.text.includes(w)))?.n || visible[0]?.n || null;
      }
      $("pick-list").replaceChildren(...visible.map(({ n, depth }) => {
        const row = el("div", `folder${n.parent ? "" : " account"}${n === cur ? " on" : ""}${n.target || !n.parent ? "" : " pick-parent"}`);
        row.style.setProperty("--depth", depth);
        const expanded = shownSet ? n.children.some((c) => shownSet.has(c.key)) : open.has(n.key);
        const twist = el("span", "twist");
        if (n.children.length) twist.append(icon(expanded ? "chevron-down" : "chevron-right"));
        twist.addEventListener("click", (e) => {
          e.stopPropagation();
          cur = n;
          if (open.has(n.key)) close(n); else openOnly(n);
          show();
        });
        const count = el("span", "f-count");
        count.append(el("span", "f-total", n.count ? number.format(n.count) : ""));
        if (n.unread) count.append(el("b", "f-unread", `(${number.format(n.unread)})`));
        row.append(twist, icon(n.iconName || folderIcon(n.name, expanded && n.children.length)), el("span", "f-name", n.name), count);
        row.title = n.parent ? `${n.account.shown} › ${n.target?.shown || n.key.slice(n.key.indexOf("/") + 1)}` : accountTip(n.account);
        // a click picks it and opens it (one branch at a time); a double-click chooses it
        row.addEventListener("click", () => {
          cur = n;
          if (n.children.length && !open.has(n.key)) openOnly(n);
          show();
        });
        row.addEventListener("dblclick", () => { if (n.target) done(n.target); });
        return row;
      }));
      $("pick-ok").disabled = !cur?.target;
      $("pick-list").querySelector(".folder.on")?.scrollIntoView({ block: "nearest" });
    };
    const done = (folder) => {
      dialog.onclose = null;
      dialog.close();
      resolve(folder);
    };
    $("pick-filter").value = "";
    $("pick-filter").oninput = show;
    $("pick-filter").onkeydown = (e) => {
      const at = visible.findIndex((v) => v.n === cur);
      const typing = $("pick-filter").value !== "";
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const next = visible[Math.max(0, Math.min(visible.length - 1, at + (e.key === "ArrowDown" ? 1 : -1)))];
        if (next) { cur = next.n; show(); }
      } else if (e.key === "ArrowRight" && !typing && cur?.children.length) {
        e.preventDefault();
        if (!open.has(cur.key)) openOnly(cur);
        else cur = cur.children[0];
        show();
      } else if (e.key === "ArrowLeft" && !typing && cur) {
        e.preventDefault();
        if (open.has(cur.key)) close(cur);
        else if (cur.parent) { cur = cur.parent; }
        show();
      } else if (e.key === "Enter" && cur) {
        e.preventDefault();
        if (cur.target) done(cur.target);
        else if (cur.children.length) { if (open.has(cur.key)) close(cur); else openOnly(cur); show(); }
      }
    };
    $("pick-ok").onclick = () => { if (cur?.target) done(cur.target); };
    $("pick-cancel").onclick = () => done(null);
    dialog.onclose = () => resolve(null);
    show();
    dialog.showModal();
    $("pick-filter").focus();
  });
}

async function transfer() {
  if (!state.list || selectionCount() === 0) { status("Choose a message first."); return; }
  const f = await pickFolder("Transfer To", "Transfer");
  if (f) moveSelected(f.id, `to ${f.shown}`);
}

/** Asks for a new mailbox: which account (the open mailbox's first) and its name. */
function askNewMailbox(title) {
  return new Promise((resolve) => {
    const dialog = $("newbox");
    $("newbox-title").textContent = title;
    const here = state.list?.folders?.length ? state.folders.find((f) => f.id === state.list.folders[0])?.account_id : null;
    options($("newbox-account"), state.accounts.accounts.map((a) => [String(a.id), a.shown]), String(here ?? state.accounts.accounts[0]?.id ?? ""));
    $("newbox-name").value = "";
    const finish = (value) => {
      $("newbox-ok").onclick = $("newbox-cancel").onclick = $("newbox-name").onkeydown = null;
      dialog.close();
      resolve(value);
    };
    const ok = () => {
      const path = $("newbox-name").value.trim();
      if (!path) { $("newbox-name").focus(); return; }
      finish({ account: Number($("newbox-account").value), path });
    };
    $("newbox-ok").onclick = ok;
    $("newbox-cancel").onclick = () => finish(null);
    $("newbox-name").onkeydown = (e) => { if (e.key === "Enter") ok(); };
    dialog.onclose = () => resolve(null);
    dialog.showModal();
    $("newbox-name").focus();
  });
}

/** Mailbox ▸ New… (classic's) and Transfer ▸ New… (makes it, then moves the selection there). */
async function newMailbox(thenMove) {
  if (thenMove && (!state.list || selectionCount() === 0)) { status("Choose a message first."); return; }
  const made = await askNewMailbox(thenMove ? "Transfer to a New Mailbox" : "New Mailbox");
  if (!made) return;
  try {
    const id = await invoke("folder_create", made);
    await refresh(true);
    if (thenMove) moveSelected(id, `to ${made.path}`);
    else status(`Made the mailbox ${made.path}.`);
  } catch (err) {
    showError("Making the mailbox", err);
  }
}

async function copyTo() {
  if (!state.list || selectionCount() === 0) { status("Choose a message first."); return; }
  const f = await pickFolder("Copy To", "Copy");
  if (!f) return;
  try {
    const ids = (await selectedItems()).map((i) => i.id);
    if (!(await confirmMany(ids.length, "Copy"))) return;
    const n = await invoke("copy_messages", { ids, to: f.id });
    status(n ? `Copied ${ids.length > 1 ? `${number.format(n)} messages ` : ""}to ${f.shown}.` : `Already in ${f.shown}.`);
    refresh(true);
  } catch (err) {
    showError("Copying the message", err);
  }
}

async function openMailbox() {
  const f = await pickFolder("Open Mailbox", "Open");
  if (!f) return;
  const key = folderKey(f);
  // open the account and the folders above it, so it shows in the sidebar
  const parts = key.split("/");
  for (let i = 1; i < parts.length; i++) state.open.add(parts.slice(0, i).join("/"));
  state.open.add("#accounts");
  if (state.organize !== "folders") { state.organize = "folders"; store.set("organize", "folders"); applySkin(); }
  renderSidebar();
  openView(key);
}

async function showDrafts() {
  try {
    const drafts = await invoke("drafts");
    const list = $("drafts-list");
    list.replaceChildren(...(drafts.length ? drafts.map((d) => {
      const row = el("div", "draft-row");
      const open = el("button", "pick-row", `${d.draft.subject || "(no subject)"} · to ${d.draft.to || "nobody yet"} · ${new Date(d.saved * 1000).toLocaleString()}`);
      open.addEventListener("click", () => { $("drafts-box").close(); invoke("open_compose", { mode: "draft", id: d.id }); });
      const remove = el("button", "", "Delete");
      remove.addEventListener("click", async () => { await invoke("delete_draft", { id: d.id }); showDrafts(); });
      row.append(open, remove);
      return row;
    }) : [el("p", "p-note", "No saved drafts.")]));
    if (!$("drafts-box").open) $("drafts-box").showModal();
  } catch (err) {
    showError("Reading the drafts", err);
  }
}
$("drafts-close").addEventListener("click", () => $("drafts-box").close());

function viewSource() {
  if (!current) return;
  state.sourceCopy = null;
  setView("source");
  renderBody();
}

// ------------------------------------------------------------------ filters (rules)

const FIELDS = ["From:", "To:", "Cc:", "Subject:", "Reply-To:", "List-Id:", "«Any Header»", "«Any Recipient»", "«Body»", "«Personality»"];
const VERBS = [
  ["contains", "contains"], ["not_contains", "doesn't contain"], ["is", "is"], ["is_not", "isn't"],
  ["starts_with", "starts with"], ["ends_with", "ends with"], ["appears", "appears"], ["not_appears", "doesn't appear"],
  ["regex", "matches regex"], ["regex_ignore_case", "matches regex (any case)"], ["greater", "greater than"], ["less", "less than"],
];
const DOES = [
  ["transfer", "Transfer to"], ["copy", "Copy to"], ["delete", "Delete"], ["sort", "Sort with the spam filter"], ["stop", "Skip the rest"],
  ["label", "Label"], ["priority", "Priority"], ["status", "Status"], ["junk", "Junk (spam)"],
  ["subject", "Change subject"], ["forward", "Forward to"], ["redirect", "Redirect to"], ["reply", "Reply with"], ["notify", "Notify me"], ["plugin", "Run a plugin"],
];

const filters = { rules: [], at: -1 };

function options(select, pairs, value) {
  select.replaceChildren(...pairs.map(([v, label]) => { const o = el("option", "", label); o.value = v; return o; }));
  select.value = value;
}

function ruleSummary(r) {
  const t = r.tests.map((x) => `${x.unless ? "unless " : ""}${x.field} ${VERBS.find((v) => v[0] === x.verb)?.[1] || x.verb} “${x.value}”`).join(r.all ? " and " : " or ");
  const a = r.actions.map((x) => x.do === "transfer" || x.do === "copy" ? `${x.do} → ${x.folder}` : x.do).join(", ");
  return `${t || "(no tests)"} → ${a || "(nothing)"}`;
}

function renderRuleList() {
  const list = $("f-list");
  list.replaceChildren(...filters.rules.map((r, i) => {
    const row = el("label", `f-item${i === filters.at ? " on" : ""}`);
    const on = el("input");
    on.type = "checkbox";
    on.checked = r.enabled;
    on.title = "On / off";
    on.addEventListener("change", () => { r.enabled = on.checked; });
    const text = el("span", "", r.name || "(no name)");
    text.title = ruleSummary(r);
    row.append(on, text);
    row.addEventListener("click", (e) => { if (e.target !== on) { saveEditor(); filters.at = i; renderRuleList(); renderEditor(); } });
    return row;
  }));
  $("f-list").children[filters.at]?.scrollIntoView({ block: "nearest" });
}

/** Rows editing a list of tests (field, verb, value; classic rules also have "unless"),
 *  shared by the Filters and the Mail Filter Rules windows. `changed` runs after any edit. */
function testRows(tests, rerender, withUnless, changed = () => {}) {
  ensureFieldList();
  return tests.map((t, i) => {
    const row = el("div", "f-test");
    if (withUnless) {
      const unless = el("label", "f-unless");
      const u = el("input");
      u.type = "checkbox";
      u.checked = t.unless;
      u.addEventListener("change", () => { t.unless = u.checked; changed(); });
      unless.append(u, document.createTextNode(" unless"));
      row.append(unless);
    }
    const field = el("input");
    field.setAttribute("list", "f-fields");
    field.value = t.field;
    field.title = "A header (with its colon), or «Any Header», «Any Recipient», «Body», «Personality»";
    field.addEventListener("input", () => { t.field = field.value; changed(); });
    const verb = el("select");
    options(verb, VERBS, t.verb);
    verb.addEventListener("change", () => { t.verb = verb.value; changed(); });
    const value = el("input");
    value.value = t.value;
    value.addEventListener("input", () => { t.value = value.value; changed(); });
    const remove = el("button", "f-x", "×");
    remove.title = "Remove";
    remove.addEventListener("click", () => { tests.splice(i, 1); changed(); rerender(); });
    row.append(field, verb, value, remove);
    return row;
  });
}

function ensureFieldList() {
  if ($("f-fields")) return;
  const dl = el("datalist");
  dl.id = "f-fields";
  dl.append(...FIELDS.map((f) => { const o = el("option"); o.value = f; return o; }));
  document.body.append(dl);
}

function renderEditor() {
  const r = filters.rules[filters.at];
  $("f-edit").hidden = !r;
  if (!r) return;
  $("f-name").value = r.name;
  $("f-incoming").checked = r.incoming;
  $("f-manual").checked = r.manual;
  $("f-outgoing").checked = r.outgoing;
  $("f-all").value = r.all ? "1" : "0";
  $("f-tests").replaceChildren(...testRows(r.tests, renderEditor, true));
  $("f-actions").replaceChildren(...r.actions.map((a, i) => {
    const row = el("div", "f-action");
    const does = el("select");
    options(does, DOES, a.do);
    does.addEventListener("change", () => {
      const next = { do: does.value };
      if (["transfer", "copy"].includes(next.do)) { next.account = a.account ?? null; next.folder = a.folder ?? ""; }
      if (next.do === "label") next.label = "";
      if (next.do === "priority") next.level = 3;
      if (next.do === "status") next.status = "";
      if (next.do === "subject") next.text = "^";
      if (["forward", "redirect"].includes(next.do)) next.to = "";
      if (next.do === "reply") next.with = "";
      if (next.do === "plugin") { next.plugin = ""; next.command = ""; }
      r.actions[i] = next;
      renderEditor();
    });
    row.append(does);
    if (a.do === "transfer" || a.do === "copy") {
      const where = el("button", "f-folder", a.folder ? `${a.account ? a.account + " › " : ""}${a.folder}` : "Choose a mailbox…");
      where.addEventListener("click", async () => {
        const f = await pickFolder(a.do === "copy" ? "Copy To" : "Transfer To");
        if (!f) return;
        const acct = state.accounts.accounts.find((x) => x.id === f.account_id);
        a.account = acct?.address || null;
        a.folder = f.path;
        renderEditor();
      });
      row.append(where);
    }
    // what the other actions need: a label, a priority, a status, words or addresses
    const choose = (pairs, value, set) => {
      const s = el("select");
      options(s, pairs, value);
      s.addEventListener("change", () => set(s.value));
      row.append(s);
    };
    const text = (value, placeholder, set, title = "") => {
      const t = el("input", "f-value");
      t.value = value ?? "";
      t.placeholder = placeholder;
      t.title = title;
      t.addEventListener("input", () => set(t.value));
      row.append(t);
    };
    if (a.do === "label") {
      const names = (state.labelDefs || []).map((l) => [l.name, l.name]);
      if (a.label && !names.some(([n]) => n === a.label)) names.push([a.label, a.label]);
      choose([["", "(choose a label)"], ...names], a.label || "", (v) => { a.label = v; });
    } else if (a.do === "priority") {
      choose([["1", "Highest"], ["2", "High"], ["3", "Normal"], ["4", "Low"], ["5", "Lowest"], ["-1", "Raise"], ["-2", "Lower"]], String(a.level ?? 3), (v) => { a.level = Number(v); });
    } else if (a.do === "status") {
      choose([["", "(choose)"], ["unread", "Unread"], ["read", "Read"], ["replied", "Replied"], ["forwarded", "Forwarded"], ["redirected", "Redirected"]], (a.status || "").toLowerCase(), (v) => { a.status = v; });
    } else if (a.do === "subject") {
      text(a.text, "New subject (^ = the old one)", (v) => { a.text = v; }, "^ stands for the subject it had: \"[Client] ^\"");
    } else if (a.do === "forward" || a.do === "redirect") {
      text(a.to, "To (addresses)", (v) => { a.to = v; }, "Sent at once through the Out mailbox; never for automatic, bulk or list mail");
    } else if (a.do === "reply") {
      text(a.with, "Reply text, or a classic stationery file", (v) => { a.with = v; }, "At most one reply a day to the same sender; never to automatic, bulk or list mail");
    } else if (a.do === "plugin") {
      const actions = state.plugins.flatMap((p) => p.manifest.rule_actions.map((c) => [`${p.manifest.id}|${c.id}`, `${p.manifest.name}: ${c.title}`]));
      const now = a.plugin ? `${a.plugin}|${a.command}` : "";
      if (now && !actions.some(([v]) => v === now)) actions.push([now, `${a.plugin}: ${a.command} (not installed)`]);
      choose([["", actions.length ? "(choose)" : "(no plugin offers rule actions)"], ...actions], now, (v) => { [a.plugin, a.command] = v.split("|"); });
    }
    const remove = el("button", "f-x", "×");
    remove.addEventListener("click", () => { r.actions.splice(i, 1); renderEditor(); });
    row.append(remove);
    return row;
  }));
}

function saveEditor() {
  const r = filters.rules[filters.at];
  if (!r) return;
  r.name = $("f-name").value;
  r.incoming = $("f-incoming").checked;
  r.manual = $("f-manual").checked;
  r.all = $("f-all").value === "1";
}

async function openFilters(newRule) {
  try {
    filters.rules = await invoke("rules_get");
  } catch (err) {
    showError("Reading the rules", err);
    return;
  }
  filters.at = filters.rules.length ? 0 : -1;
  if (newRule) {
    filters.rules.push(newRule);
    filters.at = filters.rules.length - 1;
  }
  options($("f-imp-account"), state.accounts.accounts.filter((a) => a.address).map((a) => [a.address, `${a.shown} (${a.kind_name})`]),
    state.accounts.accounts.find((a) => a.kind === "eudora")?.address || "");
  if (!$("f-fields")) {
    const dl = el("datalist");
    dl.id = "f-fields";
    dl.append(...FIELDS.map((f) => { const o = el("option"); o.value = f; return o; }));
    document.body.append(dl);
  }
  $("f-report").textContent = "";
  try {
    $("f-arriving-on").checked = (await invoke("setting", { key: "incoming-rules" })) === "on";
  } catch {
    $("f-arriving-on").checked = false;
  }
  renderRuleList();
  renderEditor();
  if (!$("filters").open) $("filters").showModal();
}

$("f-arriving-on").addEventListener("change", async () => {
  try {
    await invoke("set_setting", { key: "incoming-rules", value: $("f-arriving-on").checked ? "on" : "off" });
    $("f-report").textContent = $("f-arriving-on").checked
      ? "Rules marked “Mail arrives” now run on new mail as the download brings it in."
      : "Rules no longer run on arriving mail.";
  } catch (err) {
    $("f-report").textContent = `Not changed: ${err}`;
  }
});

function newRule(name = "New rule") {
  return { name, enabled: true, incoming: true, outgoing: false, manual: true, all: true, tests: [{ field: "From:", verb: "contains", value: "", unless: false }], actions: [{ do: "transfer", account: null, folder: "" }, { do: "stop" }] };
}

$("f-new").addEventListener("click", () => { saveEditor(); filters.rules.push(newRule()); filters.at = filters.rules.length - 1; renderRuleList(); renderEditor(); $("f-name").select(); });
$("f-del").addEventListener("click", async () => {
  if (filters.at < 0) return;
  if (!(await askYesNo("Delete rule", `Delete the rule “${filters.rules[filters.at].name}”?`, "Delete it"))) return;
  filters.rules.splice(filters.at, 1);
  filters.at = Math.min(filters.at, filters.rules.length - 1);
  renderRuleList();
  renderEditor();
});
for (const [id, step] of [["f-up", -1], ["f-down", 1]]) {
  $(id).addEventListener("click", () => {
    const to = filters.at + step;
    if (filters.at < 0 || to < 0 || to >= filters.rules.length) return;
    saveEditor();
    [filters.rules[filters.at], filters.rules[to]] = [filters.rules[to], filters.rules[filters.at]];
    filters.at = to;
    renderRuleList();
  });
}
$("f-add-test").addEventListener("click", () => { filters.rules[filters.at]?.tests.push({ field: "Subject:", verb: "contains", value: "", unless: false }); renderEditor(); });
$("f-add-action").addEventListener("click", () => { filters.rules[filters.at]?.actions.push({ do: "transfer", account: null, folder: "" }); renderEditor(); });
$("f-name").addEventListener("input", () => { saveEditor(); renderRuleList(); });
$("f-imp-go").addEventListener("click", async () => {
  try {
    const classic = await invoke("rules_import_classic", { path: $("f-imp-path").value, account: $("f-imp-account").value });
    saveEditor();
    filters.rules.push(...classic);
    filters.at = filters.rules.length - classic.length;
    renderRuleList();
    renderEditor();
    $("f-report").textContent = `Read ${classic.length} classic rules (added at the end; Save to keep them).`;
  } catch (err) {
    $("f-report").textContent = `Couldn't read them: ${err}`;
  }
});

function reportText(r, dry) {
  const lines = [`${number.format(r.checked)} messages checked, ${number.format(r.matched)} matched: ${number.format(r.moved)} ${dry ? "would be moved" : "moved"}, ${number.format(r.copied)} copied, ${number.format(r.deleted)} deleted.`];
  const top = r.by_rule.filter(([, n]) => n > 0).sort((a, b) => b[1] - a[1]).slice(0, 6).map(([name, n]) => `${name} (${number.format(n)})`);
  if (top.length) lines.push(`Most: ${top.join(", ")}.`);
  if (r.acted?.length) lines.push(`Also: ${r.acted.map(([w, n]) => `${w} (${n})`).join(", ")}.`);
  if (r.not_done.length) lines.push(`Couldn't: ${r.not_done.map(([w, n]) => `${w} (${n})`).join(", ")}.`);
  if (r.missing_folders.length) lines.push(`Mailboxes not found: ${r.missing_folders.slice(0, 5).join(", ")}.`);
  if (r.sorted?.length) lines.push(`Spam filter: ${r.sorted.map(([c, n]) => `${number.format(n)} ${c}`).join(", ")}.`);
  return lines.join(" ");
}

/** The rules on a selection anywhere (search results too: each message from its own folder). */
async function runRulesOnSelection(dry) {
  const items = (await selectedItems()).filter((i) => i.from != null);
  if (!items.length) { status("Choose a message first."); return null; }
  if (!dry && !(await confirmMany(items.length, "Filter"))) return null;
  status(dry ? "Trying the rules…" : `Running the rules on ${number.format(items.length)} messages…`);
  try {
    const r = await invoke("rules_run_items", { items: items.map((i) => [i.id, i.from]), dryRun: dry });
    status(reportText(r, dry));
    if (!dry && (r.moved || r.deleted)) await reloadList();
    return r;
  } catch (err) {
    showError("Running the rules", err);
    return null;
  }
}

async function runRules(dry, ids = null) {
  const list = state.list;
  if (list?.kind !== "folder") { status("Open a mailbox first: the rules run on a mailbox."); return null; }
  status(dry ? "Trying the rules…" : "Running the rules…");
  try {
    const r = await invoke("rules_run", { folder: list.folders[0], ids, dryRun: dry });
    status(reportText(r, dry));
    if (!dry && (r.moved || r.deleted)) await reloadList();
    return r;
  } catch (err) {
    showError("Running the rules", err);
    return null;
  }
}

async function saveFilters() {
  saveEditor();
  try {
    await invoke("rules_set", { rules: filters.rules });
    return true;
  } catch (err) {
    $("f-report").textContent = `Not saved: ${err}`;
    return false;
  }
}

$("f-try").addEventListener("click", async () => {
  if (!(await saveFilters())) return;
  const r = await runRules(true);
  if (r) $("f-report").textContent = reportText(r, true);
});
$("f-run").addEventListener("click", async () => {
  if (!(await saveFilters())) return;
  const r = await runRules(false);
  if (r) $("f-report").textContent = reportText(r, false);
});
$("f-save").addEventListener("click", async () => { if (await saveFilters()) { $("filters").close(); status(`${filters.rules.length} rules saved.`); } });
$("f-close").addEventListener("click", () => $("filters").close());

/** Make Filter: a rule from the selected message (its sender), then choose where it goes. */
async function makeFilter() {
  const s = needMessage();
  if (!s) return;
  const address = (s.row.from.match(/<([^>]+)>/)?.[1] || s.row.from).trim();
  const r = newRule(`From: ${address}`);
  r.tests[0].value = address;
  await openFilters(r);
}

/** Filter Messages: the rules (by hand) on the selected message, or on the whole mailbox. */
async function filterMessages() {
  if (state.list?.kind === "folder" && selectionCount() > 0) await runRules(false, (await selectedItems()).map((i) => i.id));
  else if (state.list && selectionCount() > 0) await runRulesOnSelection(false); // search results, All Mail, labels
  else await runRules(false);
}

/** Edit ▸ Select All (Ctrl+A) acts where the cursor is: the text being typed in, a message
 *  being written, the message being read, or else the messages in the list. */
let lastArea = "list";
document.addEventListener("mousedown", (e) => {
  lastArea = e.target.closest("#message-pane") ? "reader" : e.target.closest("#list, #list-pane") ? "list" : e.target.closest("#folders-pane") ? "sidebar" : lastArea;
}, true);

function selectAll() {
  const writing = activeComposer();
  if (writing) { writing.contentWindow.composeAction?.("select-all"); return; }
  const f = document.activeElement;
  if (f && (f.tagName === "INPUT" || f.tagName === "TEXTAREA")) { f.select(); return; }
  if (f?.isContentEditable) { document.execCommand("selectAll"); return; }
  if (lastArea === "reader" && !$("message").hidden) {
    const body = $("message").querySelector("iframe");
    if (body?.contentDocument) {
      body.contentWindow.focus();
      body.contentDocument.execCommand("selectAll");
    } else {
      const range = document.createRange();
      range.selectNodeContents($("message"));
      getSelection().removeAllRanges();
      getSelection().addRange(range);
    }
    return;
  }
  selectAllMessages();
}

/** Tools ▸ Mailboxes (classic Eudora): shows or hides the Mailboxes pane. Showing it opens
 *  the folder view with every account expanded and puts the keyboard in it. */
function toggleMailboxes() {
  const hide = !document.body.classList.contains("no-mailboxes");
  document.body.classList.toggle("no-mailboxes", hide);
  store.set("hideMailboxes", hide);
  if (hide) {
    status("Mailboxes hidden: Tools ▸ Mailboxes shows them again.");
    return;
  }
  if (state.organize !== "folders") {
    state.organize = "folders";
    store.set("organize", "folders");
    applySkin();
  }
  state.open.add("#accounts");
  for (const a of state.accounts.accounts) state.open.add(`#${a.id}`);
  store.set("openFolders", [...state.open]);
  renderSidebar();
  $("folders").focus();
}

// ------------------------------------------------------------------ pause

let pauseMode = "none";

/** The download's buttons on the status bar show only what can be done next: running = Pause (old mail waits, new mail still comes)
 *  and Stop (nothing downloads); paused = Resume and Stop; stopped = Resume. */
function showPause() {
  const pause = $("pause-dl");
  const stop = $("stop-dl");
  if (pauseMode === "none") {
    pause.textContent = "⏸ Pause";
    pause.title = "Pause the download of older mail; new mail still comes in (Inbox and Sent, every 5 minutes).";
  } else {
    pause.textContent = "▶ Resume";
    pause.title = pauseMode === "all" ? "Start downloading again, where it stopped." : "Download older mail again, where it paused.";
  }
  stop.textContent = "⏹ Stop";
  stop.title = "Stop everything Eudora does in the background until Resume: downloading (new mail too), changes to the servers, sorting into Views, maintenance, a spam-filter run. Mail you send still goes.";
  stop.hidden = pauseMode === "all";
}

async function setPause(mode) {
  try {
    await invoke("set_pause", { mode });
    pauseMode = mode;
    showPause();
    status({
      old: "Old mail paused: new mail still comes in (Inbox and Sent, every 5 minutes). Resume any time.",
      all: "Stopped: nothing runs in the background (downloads, server changes, Views, maintenance). Resume any time: it continues where it was.",
      none: "Downloading resumed: it continues where it was.",
    }[mode]);
    setTimeout(showDownload, 1500);
  } catch (err) {
    showError("Pausing the download", err);
  }
}

$("pause-dl").addEventListener("click", () => setPause(pauseMode === "none" ? "old" : "none"));
$("stop-dl").addEventListener("click", () => {
  invoke("sort_stop").catch(() => {}); // (a spam-filter run in the window stops too)
  setPause("all");
});
showPause();

/** Check Mail: the running download looks in every account now (new accounts included);
 *  the list updates as the mail comes in. */
async function checkMail() {
  try {
    if (pauseMode === "all") {
      status("Downloading is paused: Resume to check for mail.");
      return;
    }
    // an inactive shared mailbox's folder open: Check Mail brings that folder up to date (once;
    // the mailbox stays inactive)
    const list = state.list;
    if (list?.kind === "folder" && list.folders.some((id) => sharedInactive(state.folders.find((f) => f.id === id)?.account_id))) {
      await invoke("look_now", { folders: list.folders });
    }
    const running = await invoke("check_mail");
    status(running
      ? "Checking every account for new mail… New messages show up here as they arrive."
      : "Check Mail is waiting for the download to start (download.cmd); it checks as soon as it runs.");
    setTimeout(() => refresh(true), 5000);
    setTimeout(() => refresh(true), 20000);
  } catch (err) {
    showError("Checking mail", err);
  }
}

// ------------------------------------------------------------------ back up / restore

const gbText = (bytes) => `${(bytes / 1e9).toFixed(1)} GB`;

/** Follows the running backup or restore until it ends (`bar`, `line`: its progress). */
function followJob(prefix, done) {
  const timer = setInterval(async () => {
    let j;
    try { j = await invoke("job_state"); } catch { return; }
    if (!j) return;
    $(`${prefix}-progress`).hidden = false;
    $(`${prefix}-bar`).max = j.total || 1;
    $(`${prefix}-bar`).value = j.total ? j.done : 0;
    if (!j.total) $(`${prefix}-bar`).removeAttribute("value");
    $(`${prefix}-line`).textContent = j.running ? j.line : j.result;
    $(`${prefix}-line`).classList.toggle("bk-failed", !!j.failed);
    $(`${prefix}-stop`).hidden = !j.running;
    $(`${prefix}-go`).disabled = j.running;
    if (!j.running) {
      clearInterval(timer);
      status(j.result);
      done?.(j);
    }
  }, 500);
}

async function openBackup() {
  const box = $("backup-box");
  $("bk-progress").hidden = true;
  $("bk-go").disabled = false;
  $("bk-pass").value = $("bk-pass2").value = $("bk-agree").value = "";
  box.showModal();
  try {
    const info = await invoke("backup_info");
    const s = info.sizes;
    if (!$("bk-dest").value) $("bk-dest").value = info.suggested;
    $("bk-sizes").textContent = `Messages ${gbText(s.messages)} · database ${gbText(s.database)} · other ${gbText(s.other)} · search indexes ${gbText(s.indexes)} (optional). Make sure the place you save to has room for about ${gbText(s.messages + s.database + s.other)}, or ${gbText(s.messages + s.database + s.other + s.indexes)} with the indexes.`;
    $("bk-indexes-label").textContent = `Include the search indexes (${gbText(s.indexes)} more; a quicker restore, else they're rebuilt there)`;
  } catch (err) {
    $("bk-sizes").textContent = `Couldn't measure the library (${err}).`;
  }
  const running = await invoke("job_state").catch(() => null);
  if (running?.running && running.kind === "backup") followJob("bk");
}

function showAgree() {
  $("bk-agree-box").hidden = !($("bk-passwords").checked && !$("bk-pass").value);
}

$("bk-passwords").addEventListener("change", showAgree);
$("bk-pass").addEventListener("input", showAgree);
$("bk-split").addEventListener("change", () => {
  const custom = $("bk-split").value === "custom";
  $("bk-split-gb").hidden = $("bk-split-unit").hidden = !custom;
});
$("bk-go").addEventListener("click", async () => {
  const password = $("bk-pass").value;
  if (password !== $("bk-pass2").value) { status("The two passwords aren't the same."); $("bk-pass2").focus(); return; }
  if (password && password.length < 8) { status("The backup's password needs at least 8 characters."); return; }
  const split = $("bk-split").value;
  const partGb = split === "custom" ? Number($("bk-split-gb").value) : Number(split);
  try {
    await invoke("backup_start", {
      dest: $("bk-dest").value, password: password || null, indexes: $("bk-indexes").checked,
      passwords: $("bk-passwords").checked, agreed: $("bk-agree").value.trim(), partGb: partGb || null,
    });
    followJob("bk");
  } catch (err) {
    showError("Backing up", err);
  }
});
$("bk-stop").addEventListener("click", () => invoke("job_cancel"));
$("bk-cancel").addEventListener("click", () => $("backup-box").close());

// ------------------------------------------------------------------ views: which mailboxes

async function chooseViewMailboxes() {
  let list;
  try { list = await invoke("views_accounts"); } catch (err) { showError("Reading the mailboxes", err); return; }
  const box = $("views-accounts-list");
  box.replaceChildren(...list.filter(([, address]) => address).map(([, address, shown, on]) => {
    const label = el("label", "sr-mailbox");
    const tick = el("input");
    tick.type = "checkbox";
    tick.value = address;
    tick.checked = on;
    label.append(tick, el("span", "", shown));
    label.title = address;
    return label;
  }));
  $("views-accounts-box").showModal();
}

$("views-accounts-save").addEventListener("click", async () => {
  const addresses = [...$("views-accounts-list").querySelectorAll("input:checked")].map((b) => b.value);
  try {
    await invoke("views_accounts_set", { addresses });
    $("views-accounts-box").close();
    state.viewCounts.delete("combined");
    renderSidebar();
    if (state.list?.kind === "view" && !state.list.accounts) openView(state.list.key);
  } catch (err) {
    showError("Saving the mailboxes for Views", err);
  }
});
$("views-accounts-cancel").addEventListener("click", () => $("views-accounts-box").close());

// ------------------------------------------------------------------ statistics

async function openStats(fresh = false) {
  // (the copy the background counted comes at once; only a count asked for, or none kept yet, takes time)
  if (fresh) $("stats-body").prepend(el("p", "p-note", "Counting now… (up to half a minute on a large library)"));
  else $("stats-body").replaceChildren();
  if (!$("stats-box").open) $("stats-box").showModal();
  $("stats-recount").disabled = true;
  let s;
  try { s = await invoke("mail_stats", { fresh }); } catch (err) { $("stats-body").replaceChildren(el("p", "p-note", `Couldn't count (${err}).`)); $("stats-recount").disabled = false; return; }
  const n = (x) => number.format(x);
  const table = (head, rows) => {
    const tbl = el("table", "stats-table");
    const tr = el("tr");
    for (const h of head) tr.append(el("th", "", h));
    tbl.append(tr);
    for (const r of rows) {
      const row = el("tr");
      for (const c of r) row.append(el("td", "", c));
      tbl.append(row);
    }
    return tbl;
  };
  const bars = (pairs) => {
    const max = Math.max(1, ...pairs.map(([, v]) => v));
    const box = el("div", "stats-bars");
    for (const [label, value] of pairs) {
      const row = el("div", "stats-bar");
      const fill = el("span", "stats-fill");
      fill.style.width = `${(value / max) * 100}%`;
      const track = el("span", "stats-track");
      track.append(fill);
      row.append(el("span", "stats-label", label), track, el("span", "stats-n", n(value)));
      box.append(row);
    }
    return box;
  };
  const years = s.oldest && s.newest ? ` from ${new Date(s.oldest * 1000).getFullYear()} to ${new Date(s.newest * 1000).getFullYear()}` : "";
  const parts = [el("p", "", `${n(s.messages)} messages${years}, each counted once.`)];
  parts.push(el("h3", "", "Received and sent"), table(["", "Received", "Sent"], s.periods.map((p) => [p.label, n(p.received), n(p.sent)])));
  parts.push(el("h3", "", "Busiest days ever"), bars(s.busiest_days.map(([d, c]) => [new Date(d + "T12:00:00").toLocaleDateString([], { year: "numeric", month: "short", day: "numeric", weekday: "short" }), c])));
  parts.push(el("h3", "", "Last 12 months by weekday"), bars(["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"].map((d, i) => [d, s.weekdays[i] || 0])));
  if (s.sorted) {
    parts.push(el("h3", "", `Kinds of mail (the ${n(s.sorted)} message${s.sorted === 1 ? "" : "s"} the spam filter has sorted)`),
      bars(s.kinds.map(([k, c]) => [`${k} ${(c * 100 / s.sorted).toFixed(1)}%`, c])));
  }
  if (s.sorted < 1000) {
    parts.push(el("p", "p-note", "The share of personal mail, notifications, marketing and spam comes from the spam filter's decisions: it fills in as the filter sorts mail (on arrival, or Special ▸ Sort with the Spam Filter)."));
  }
  const st = s.storage, gb = (b) => `${(b / 1e9).toFixed(1)} GB`;
  parts.push(el("h3", "", "Storage"), table(["", "Size"], [
    ["Messages, uncompressed", gb(st.messages_uncompressed)],
    ["Other original copies kept, uncompressed", gb(st.copies_uncompressed)],
    ["Stored compressed (packs)", `${gb(st.packs)} (${((st.messages_uncompressed + st.copies_uncompressed) / Math.max(1, st.packs)).toFixed(1)}× smaller)`],
    ["Database", gb(st.database)],
    ["Search index", gb(st.search_index)],
    ["Header search", gb(st.header_index)],
  ]));
  if (s.counted) parts.unshift(el("p", "p-note", `Counted ${new Date(s.counted * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })} (Eudora counts again in the background every hour).`));
  $("stats-recount").disabled = false;
  $("stats-body").replaceChildren(...parts);
}

$("stats-close").addEventListener("click", () => $("stats-box").close());

/** What's new in this version, in About: its part of the manual's What's New
 *  (help/whats-new.html, one <h2 id="v0-1-3"> per version); a link in it opens the manual there. */
async function showWhatsNew(version) {
  const box = $("about-new");
  try {
    const html = await (await fetch("help/whats-new.html")).text();
    const doc = new DOMParser().parseFromString(html, "text/html");
    const head = doc.getElementById(`v${version.replaceAll(".", "-")}`);
    const list = head?.nextElementSibling;
    if (!list) { box.replaceChildren(el("p", "p-note", `Version ${version}.`)); return; }
    $("about-new-title").textContent = `What's new in ${version}`;
    const copy = document.importNode(list, true);
    for (const a of copy.querySelectorAll("a[href^='#']")) {
      a.addEventListener("click", (e) => { e.preventDefault(); invoke("open_help", { topic: a.getAttribute("href").slice(1) }).catch(() => {}); });
    }
    box.replaceChildren(copy);
  } catch { box.replaceChildren(); }
}
$("about-history").addEventListener("click", () => invoke("open_help", { topic: "whats-new" }).catch((err) => showError("Opening the manual", err)));

/** Help ▸ About, with how fast things are. */
async function openAbout() {
  const box = $("about-speed");
  box.replaceChildren();
  if (!$("about-box").open) $("about-box").showModal();
  // version and build date, from the program itself, so it always shows
  invoke("app_version").then((v) => {
    const built = v.built ? new Date(v.built * 1000).toLocaleString(undefined, { dateStyle: "long", timeStyle: "short" }) : "";
    $("about-version").textContent = `Version ${v.version}${built ? `, built ${built}` : ""}`;
    showWhatsNew(v.version);
  }).catch((err) => { $("about-version").textContent = `Version unknown (${err})`; });
  let s;
  try { s = await invoke("speed_stats"); } catch (err) { box.replaceChildren(el("p", "p-note", `Couldn't read them (${err}).`)); return; }
  const ms = (x) => (x < 1 ? "under 1 ms" : x < 1000 ? `${Math.round(x)} ms` : x < 60_000 ? `${(x / 1000).toFixed(1)} s` : `${(x / 60_000).toFixed(1)} min`);
  const table = (title, rows) => {
    if (!rows.length) return [];
    const tbl = el("table", "stats-table");
    const head = el("tr");
    for (const h of ["", "Times", "Usual", "Slow ones", "Slowest", "Last"]) head.append(el("th", "", h));
    tbl.append(head);
    for (const r of [...rows].sort((a, b) => b.count - a.count)) {
      const tr = el("tr");
      for (const c of [r.kind, number.format(r.count), ms(r.median_ms), ms(r.p95_ms), ms(r.max_ms), ms(r.last_ms)]) tr.append(el("td", "", c));
      tbl.append(tr);
    }
    return [el("h4", "", title), tbl];
  };
  const parts = [...table("In this window", s.window), ...table("In the background download", s.download)];
  box.replaceChildren(...(parts.length ? parts : [el("p", "p-note", "Nothing timed yet.")]));
}
$("about-close").addEventListener("click", () => $("about-box").close());
$("stats-recount").addEventListener("click", () => openStats(true));

// ------------------------------------------------------------------ the Out mailbox

async function openOutbox() {
  await renderOutbox();
  if (!$("out-box").open) $("out-box").showModal();
}

async function renderOutbox() {
  let list;
  try { list = await invoke("outbox_list"); } catch (err) { showError("Reading the Out mailbox", err); return; }
  const when = (t) => t ? new Date(t * 1000).toLocaleString([], { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) : "with Send Queued Messages";
  $("out-list").replaceChildren(...(list.length ? list.map((q) => {
    const row = el("div", "out-row");
    const what = el("div", "out-what");
    what.append(el("div", "p-address", q.subject || "(no subject)"), el("div", "p-kind", `To ${q.to} · from ${q.from}`));
    const state = q.state === "sending"
      ? "Being sent (or cut short: check Sent before sending it again)"
      : q.state === "failed" ? `Not sent yet (${q.last_error || "error"}); tried again soon` : when(q.send_at);
    const edit = el("button", "f-small", "Change");
    edit.title = "Back to a draft, to change it (then Send or Send Later again)";
    edit.addEventListener("click", async () => {
      try {
        const id = await invoke("outbox_unqueue", { id: q.id });
        invoke("open_compose", { mode: "draft", id });
        renderOutbox();
      } catch (err) { showError("Changing the message", err); }
    });
    const now = el("button", "f-small", "Send now");
    now.addEventListener("click", async () => {
      if (q.state === "sending" && !(await askYesNo("May have been sent already", "This one may have gone already (its sending was cut short). Send it again?", "Send it again", "Don't send"))) return;
      try { await invoke("outbox_reschedule", { id: q.id, sendAt: Math.floor(Date.now() / 1000) - 1 }); sendQueued(); } catch (err) { showError("Sending", err); }
    });
    row.append(what, el("span", "p-kind", state), now, edit);
    return row;
  }) : [el("p", "p-note", "Nothing waiting.")]));
}

async function sendQueued() {
  status("Sending the queued messages…");
  try {
    const notes = await invoke("outbox_send_now");
    status(notes.length ? notes.join(" · ") : "Nothing was waiting in the Out mailbox.");
  } catch (err) {
    showError("Sending queued messages", err);
  }
  if ($("out-box").open) renderOutbox();
}

$("out-send").addEventListener("click", sendQueued);
$("out-close").addEventListener("click", () => $("out-box").close());
// a link clicked in a message, handed over by the app (the Mac's WebKit runs no click listener
// inside the message's frame: links_go_to_eudora in lib.rs)
window.__TAURI__.webviewWindow.getCurrentWebviewWindow().listen("link-clicked", (e) => openLink(String(e.payload || "")));

window.__TAURI__.webviewWindow.getCurrentWebviewWindow().listen("outbox", (e) => {
  status(e.payload.join(" · "));
  if ($("out-box").open) renderOutbox();
});

function openOldAttachments() {
  $("oa-progress").hidden = true;
  $("oa-go").disabled = false;
  $("oa-box").showModal();
}

$("oa-go").addEventListener("click", async () => {
  const folder = $("oa-folder").value.trim();
  try {
    await invoke("find_old_attachments", { folders: folder ? [folder] : [] });
    followJob("oa", () => { if (state.messageId) showMessage(state.messageId); });
  } catch (err) {
    showError("Finding old attachments", err);
  }
});
$("oa-stop").addEventListener("click", () => invoke("job_cancel"));
$("oa-cancel").addEventListener("click", () => $("oa-box").close());

/** Restoring from the Welcome window: into the library just made, without asking to replace it. */
let restoreFresh = false;

async function openRestore({ fresh = false } = {}) {
  $("rs-progress").hidden = true;
  $("rs-go").disabled = false;
  restoreFresh = fresh && !state.accounts?.accounts?.length;
  if (restoreFresh && state.libraryPath) {
    $("rs-dest").value = state.libraryPath;
  } else if (!$("rs-dest").value) {
    try { $("rs-dest").value = await invoke("restore_suggested"); } catch { /* typed by the user */ }
  }
  $("restore-box").showModal();
  const running = await invoke("job_state").catch(() => null);
  if (running?.running && running.kind === "restore") followJob("rs");
}

async function lookAtBackup() {
  try {
    const d = await invoke("restore_describe", { path: $("rs-file").value });
    const s = d.sizes;
    $("rs-about").textContent = `Made ${new Date(d.created * 1000).toLocaleString()}: ${gbText(s.messages + s.database + s.indexes + s.other)} in ${d.parts} file${d.parts > 1 ? "s" : ""}`
      + `${d.encrypted ? ", with a password" : ", no password"}${d.passwords ? ", passwords included" : ""}${d.indexes ? ", search indexes included" : " (the search index is built after restoring)"}.`;
    return d;
  } catch (err) {
    $("rs-about").textContent = String(err);
    return null;
  }
}

$("rs-check").addEventListener("click", lookAtBackup);
$("rs-go").addEventListener("click", async () => {
  const d = await lookAtBackup();
  if (!d) return;
  if (d.encrypted && !$("rs-pass").value) { status("This backup has a password: type it first."); $("rs-pass").focus(); return; }
  // a phone's backup isn't the whole thing
  if (d.device === "phone") {
    let limit = "";
    try {
      const v = JSON.parse(d.limit || "null");
      if (v && v.mb) limit = `up to ${v.mb >= 1000 ? `${(v.mb / 1000).toFixed(1).replace(/\.0$/, "")} GB` : `${v.mb} MB`}`;
      else if (v && v.days) limit = `the last ${v.days} days`;
    } catch { /* fine */ }
    const anyway = await askYesNo("This backup is from a phone",
      `It isn't complete: your rules aren't in it (they run on your computers)${limit ? `, and it holds only the mail that phone kept (${limit})` : ""}. Use it only when no backup from a Mac, PC, or Linux computer is available.`,
      "Restore It Anyway", "Cancel");
    if (!anyway) return;
  }
  // a folder that isn't empty: a Eudora library there (a restore left half done too) is
  // replaced if the user says so;
  // other files are never touched
  const dest = $("rs-dest").value.trim();
  let replace = false;
  try {
    const there = await invoke("restore_destination", { dest });
    if (there.kind === "other") {
      await tell("Choose another folder", `${dest} has other files in it, not a Eudora library, so Eudora won't restore over it. Choose an empty folder, or type a new folder's name.`);
      return;
    }
    if (there.kind === "open" && restoreFresh) {
      replace = true; // (the empty library this window just made: nothing to lose)
    } else if (there.kind === "open") {
      replace = await askYesNo(
        "Replace the library Eudora has open?",
        `${dest} is the library Eudora has open (${gbText(there.bytes)}). The backup is restored beside it first, and your library stays as it is while that runs. When you restart Eudora afterward, the restored library takes its place and the old one is deleted. That can't be undone.`,
        "Restore and replace it",
      );
      if (!replace) return;
    } else if (there.kind === "library") {
      replace = await askYesNo(
        "Replace the library there?",
        `${dest} already holds a Eudora library (${gbText(there.bytes)}, ${there.items.toLocaleString()} files and folders). Restoring deletes everything in that folder first, then puts the backup there. This can't be undone.`,
        "Delete it and restore",
      );
      if (!replace) return;
    }
  } catch (err) {
    await tell("Can't restore there", String(err));
    return;
  }
  try {
    await invoke("restore_start", { path: $("rs-file").value, dest, password: $("rs-pass").value || null, replace });
    // the restored folder is Eudora's library from now on: offer to switch to it at once
    followJob("rs", async (j) => {
      if (j.failed) return;
      // said plainly that it worked; after the restart, checkSignIns asks
      const now = await askYesNo("Successfully restored", `${j.result}\n\nRestart Eudora now on the restored library? It then asks you to sign in again to the accounts this computer has no sign-in for yet.`, "Restart Now", "Later");
      if (now) invoke("library_switch", { path: dest }).catch((err) => tell("Couldn't switch", String(err)));
    });
  } catch (err) {
    showError("Restoring", err);
  }
});
$("rs-stop").addEventListener("click", () => invoke("job_cancel"));
$("rs-cancel").addEventListener("click", () => $("restore-box").close());

// ------------------------------------------------------------------ Classic and Modern modes

/** Classic mode's icons: one table, so an icon is changed in one place. The files are made by scripts/classic_icons.sh. */
const CLASSIC_ICONS = {
  delete: "delete", "mailbox-in": "in", outbox: "out", check: "check-mail", new: "new-message",
  reply: "reply", "reply-all": "reply-all", forward: "forward", redirect: "redirect", "send-again": "send-again",
  send: "send", "send-queued": "send-queued", previous: "previous", next: "next", attach: "attach",
  "check-spelling": "spelling", print: "print", about: "help", "mailbox-trash": "trash", "mailbox-junk": "junk",
  transfer: "transfer", "find-text": "find-text", find: "find-messages",
};

/** Classic Eudora 7.1's toolbar, in its order (mainfrm.cpp's buttons[]); "|" = a gap. The
 *  Address Book button comes with the contacts phase. */
const CLASSIC_TOOLBAR = [
  ["delete", "Delete (Ctrl+D)"], "|",
  ["mailbox-in", "In (Ctrl+1)"], ["outbox", "Out (Ctrl+0)"], "|",
  ["check", "Check Mail (Ctrl+M)"], "|",
  ["new", "New Message (Ctrl+N)"], ["reply", "Reply (Ctrl+R)"], ["reply-all", "Reply to All (Ctrl+Shift+R)"], ["forward", "Forward"], "|",
  ["previous", "Previous Message"], ["next", "Next Message"], "|",
  ["attach", "Attach File (Ctrl+H)"], "|",
  ["find", "Find Messages (Ctrl+F)"], "|",
  ["print", "Print (Ctrl+P)"], "|",
  ["about", "Help"],
];

function classicIcon(act) {
  const img = document.createElement("img");
  img.src = `classic/${CLASSIC_ICONS[act] || "help"}.svg`;
  img.alt = "";
  img.className = "classic-icon";
  return img;
}

function buildClassicToolbar() {
  const bar = $("classic-actions");
  if (!bar || bar.childElementCount) return;
  for (const b of CLASSIC_TOOLBAR) {
    if (b === "|") { bar.append(el("span", "classic-gap")); continue; }
    const [act, title] = b;
    const button = el("button", "tool classic-tool");
    button.dataset.act = act;
    button.title = title;
    button.append(classicIcon(act));
    button.addEventListener("click", () => ACTIONS[act]?.());
    bar.append(button);
  }
}

state.uiMode = { classic: false, ctrl_r_all: false, ctrl_j_junk: false, menus: "eudora" };

/** The mode follows the look (Eudora Classic = Classic): the menu bar, the toolbar and the
 *  wording change with it. */
async function applyMode() {
  const classic = !!skin().classic;
  buildClassicToolbar();
  // whose menus the menu bar has: Outlook's for the Lookout looks, Thunderbird's, Gmail's, Yahoo's
  const menus = state.skin.startsWith("lookout") ? "outlook" : ["thunderbird", "gmail", "yahoo"].includes(state.skin) ? state.skin : "eudora";
  state.uiMode = { ...state.uiMode, classic, menus };
  try { await invoke("ui_mode_set", { mode: state.uiMode }); } catch { /* the menu stays as it was */ }
  // the wording that differs: Junk (Classic) / Spam (Modern)
  const spam = rowMenu?.querySelector('[data-act="mark-spam"]'), notSpam = rowMenu?.querySelector('[data-act="mark-not-spam"]');
  if (spam) spam.textContent = classic ? "Junk" : "Spam";
  if (notSpam) notSpam.textContent = classic ? "Not Junk" : "Not Spam";
}

async function loadMode() {
  try { state.uiMode = await invoke("ui_mode_get"); } catch { /* defaults */ }
  // a mode kept from another computer (synced) chooses the matching look here
  if (state.uiMode.classic && !skin().classic) { state.skin = "eudora-classic"; store.set("skin", state.skin); applySkin(); }
  await applyMode();
}

/** Classic's Mailbox ▸ In / Junk / Trash: that mailbox of every account, together. */
function openRole(role) {
  const wanted = { in: "inbox", junk: "ban", trash: "trash-can" }[role];
  const folders = state.folders.filter((f) => {
    const name = folderName(f);
    return folderIcon(name) === wanted && !(role === "in" && /^(old|archive)/i.test(name));
  }).map((f) => f.id);
  if (!folders.length) { status(`No ${role === "in" ? "In" : role === "junk" ? "Junk" : "Trash"} mailbox yet.`); return; }
  const key = `#role:${role}`;
  state.views.set(key, { kind: "label", folders, title: role === "in" ? "In" : role === "junk" ? "Junk" : "Trash" });
  openView(key);
}

/** Previous / Next Message (classic's toolbar arrows). */
function stepMessage(by) {
  const list = state.list;
  if (!list) return;
  const next = Math.min(listCount(list) - 1, Math.max(0, (state.selected < 0 ? -1 : state.selected) + by));
  select(next, true, "only");
}

/** F7, classic's "Blah Blah Blah": every header of the message shown, and back. */
function toggleHeaders() {
  if (!current) { status("Open a message first."); return; }
  setView(state.view === "headers" ? "message" : "headers");
  renderBody();
}

// ------------------------------------------------------------------ Find Text, Print, Check Spelling

/** Where the message shown keeps its text: its own frame (HTML mail) or the page. */
function messagePlaces() {
  const frame = $("m-body").querySelector("iframe");
  const doc = frame?.contentDocument;
  return doc?.body ? [{ doc, root: doc.body }] : [{ doc: document, root: $("m-body") }];
}
const messageFinder = textFinder(messagePlaces);
const messageFind = findBar({ bar: "m-find", input: "m-find-text", count: "m-find-count", prev: "m-find-prev", next: "m-find-next", close: "m-find-close" }, messageFinder);

/** Edit ▸ Find Text: in the message being written when there is one, else the one shown. */
function findText(again = false) {
  const writing = activeComposer();
  if (writing) { writing.contentWindow.composeAction?.(again ? "find-again" : "find-text"); return; }
  if (!current) { status("Open a message first: Find Text looks inside it (Find Messages searches them all)."); return; }
  if (again) messageFind.again(1); else messageFind.open();
}

function checkSpellingAction() {
  const writing = activeComposer();
  if (writing) writing.contentWindow.composeAction?.("check-spelling");
  else status("Check Spelling works on a message being written.");
}

/** File ▸ Print: the message shown, with its header lines, through the system's print dialog. */
function printMessage() {
  const m = current;
  if (!m) { status("Open a message first: Print prints the message shown."); return; }
  const esc = (t) => String(t ?? "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const head = [
    `<div style="font:13px 'Segoe UI', sans-serif; margin-bottom: 12px">`,
    `<div style="font-size: 17px; font-weight: 600; margin-bottom: 6px">${esc(m.subject || "(no subject)")}</div>`,
    `<div><b>From:</b> ${esc(people(m.from))}</div>`,
    m.to.length ? `<div><b>To:</b> ${esc(people(m.to))}</div>` : "",
    m.cc.length ? `<div><b>Cc:</b> ${esc(people(m.cc))}</div>` : "",
    `<div><b>Date:</b> ${esc(m.date ? fullFormat.format(new Date(m.date * 1000)) : "")}</div>`,
    `<hr></div>`,
  ].join("");
  $("print-frame")?.remove();
  const frame = document.createElement("iframe");
  frame.id = "print-frame";
  frame.setAttribute("sandbox", "allow-same-origin allow-modals");
  frame.style.cssText = "position: fixed; width: 0; height: 0; border: 0; left: -9999px";
  frame.srcdoc = m.html ? sandboxed(m.html) : `<!doctype html><meta charset="utf-8"><pre style="white-space: pre-wrap; font: 13px 'Segoe UI', sans-serif">${esc(m.text || "")}</pre>`;
  frame.addEventListener("load", () => {
    const doc = frame.contentDocument;
    if (!doc?.body) return;
    doc.body.insertAdjacentHTML("afterbegin", head);
    frame.contentWindow.print();
  }, { once: true });
  document.body.append(frame);
}

document.addEventListener("keydown", (e) => {
  // (F3 itself is the menu's Find Again; Shift+F3 goes back)
  if (e.key === "F3" && e.shiftKey) { e.preventDefault(); if (activeComposer()) activeComposer().contentWindow.composeAction?.("find-previous"); else messageFind.again(-1); }
});

// ------------------------------------------------------------------ Browse…, Import, Open File

/** Browse… beside a path: the system's Open dialog fills it in. */
for (const b of document.querySelectorAll("[data-browse]")) {
  b.addEventListener("click", async (e) => {
    e.preventDefault();
    const field = $(b.dataset.browse);
    let chosen;
    try { chosen = await invoke("pick_paths", { what: b.dataset.what, title: b.title || b.textContent.replace("…", ""), kind: b.dataset.kind || null }); } catch (err) { showError("Choosing", err); return; }
    if (!chosen.length) return;
    if (b.dataset.keepName && field.value) {
      // a file path: the folder chosen, the same file name
      const name = field.value.split(/[\\/]/).pop();
      field.value = `${chosen[0].replace(/[\\/]$/, "")}\\${name}`;
    } else {
      field.value = chosen.join(" | ");
    }
    field.dispatchEvent(new Event("input"));
  });
}

function openImport(paths = "") {
  $("im-progress").hidden = true;
  $("im-go").disabled = false;
  if (paths) $("im-paths").value = paths;
  $("import-box").showModal();
}
$("im-cancel").addEventListener("click", () => $("import-box").close());
$("im-stop").addEventListener("click", () => invoke("job_cancel"));
$("im-go").addEventListener("click", async () => {
  const merge = $("im-merge").value;
  const paths = $("im-paths").value.split("|").map((p) => p.trim()).filter(Boolean);
  try {
    await invoke("import_files", {
      paths,
      into: $("im-into").value || null,
      account: $("im-account").value || null,
      merge: merge === "merge" ? true : merge === "separate" ? false : null,
    });
    followJob("im", async (j) => {
      if (!j.failed) { await refresh(true); await loadAccounts(true); renderSidebar(); offerClassic(); }
      else if (/already has an account|AccountExists|merge/i.test(j.result || "")) $("im-merge").focus();
    });
  } catch (err) { showError("Importing", err); }
});

/** File ▸ Open File: mailbox or message files, brought in under Imports (classic's Open). */
async function openFile() {
  let chosen;
  try { chosen = await invoke("pick_paths", { what: "files", title: "Open mailbox or message files", kind: "mail" }); } catch (err) { showError("Choosing files", err); return; }
  if (chosen.length) openImport(chosen.join(" | "));
}

// ------------------------------------------------------------------ Optimize, Forget Sign-ins, Work Offline, Activity

function openOptimize() {
  $("op-progress").hidden = true;
  $("op-go").disabled = false;
  $("optimize-box").showModal();
}
$("op-cancel").addEventListener("click", () => $("optimize-box").close());
$("op-stop").addEventListener("click", () => invoke("job_cancel"));
$("op-go").addEventListener("click", async () => {
  try {
    await invoke("optimize_library", { vacuum: $("op-vacuum").checked });
    followJob("op");
  } catch (err) { showError("Optimizing", err); }
});

async function forgetSignIns() {
  let list;
  try { list = await invoke("sign_ins", { forget: false }); } catch (err) { showError("Reading the sign-ins", err); return; }
  if (!list.length) { status("No sign-ins or passwords are kept on this computer."); return; }
  if (!(await askYesNo("Forget sign-ins", `Forget these on this computer?\n\n• ${list.join("\n• ")}\n\nDownloading and sending wait until you sign in or type the passwords again. Your mail, settings and sync key stay.`, "Forget them"))) return;
  try {
    const done = await invoke("sign_ins", { forget: true });
    status(`Forgotten: ${done.length} sign-ins and passwords.`);
  } catch (err) { showError("Forgetting the sign-ins", err); }
}

async function toggleOffline() {
  try {
    const now = await invoke("offline", { on: null });
    const offline = await invoke("offline", { on: !now });
    showOffline(offline);
    status(offline
      ? "Working offline: nothing is downloaded or sent, and changes wait for the servers. Messages you send wait in Out."
      : "Online again: sending what waits in Out and checking for mail.");
  } catch (err) { showError("Work Offline", err); }
}

function showOffline(offline) {
  document.body.classList.toggle("offline", offline);
  $("offline-badge")?.toggleAttribute("hidden", !offline);
}

/** The status bar's note of background work: the jobs that did something in the
 *  last 20 s, else changes still waiting for the servers; a click opens Activity. */
const BG_RECENT = 20;
async function showBackgroundWork() {
  const b = $("bg-text");
  let a = null;
  try { a = await invoke("background_jobs"); } catch { /* (none) */ }
  // the window's own job (the spam filter deciding for many messages): its count and Stop
  let job = null;
  try { job = await invoke("sort_progress"); } catch { /* (none) */ }
  $("job-stop").hidden = !job;
  const now = Math.floor(Date.now() / 1000);
  // (a long step, ANALYZE on a big database, can go a while without a word)
  const fresh = a && (now - a.updated < 60 || (a.jobs.some((j) => j.working) && now - a.updated < 1800));
  const busy = fresh ? a.jobs.filter((j) => now - j.active <= BG_RECENT && j.last > 0 && !j.working) : [];
  const waiting = fresh ? a.jobs.filter((j) => j.left > 0) : [];
  // a long job working now (maintenance): its step as it goes
  const working = fresh ? a.jobs.filter((j) => j.working) : [];
  const sep = $("download").hidden ? "" : " · ";
  const quiet = !job && !working.length && !busy.length;
  if (job) {
    b.replaceChildren(document.createTextNode(sep), el("span", "bg-spin"), document.createTextNode(` Spam filter: deciding ${number.format(job[0])} of ${number.format(job[1])}`));
  } else if (working.length) {
    b.replaceChildren(document.createTextNode(sep), el("span", "bg-spin"), document.createTextNode(` ${working.map((j) => `${j.name.replace(/\s*\(.*\)$/, "")}: ${jobStep(j)}`).join(" · ")}`));
  } else if (busy.length) {
    b.replaceChildren(document.createTextNode(sep), el("span", "bg-spin"), document.createTextNode(` ${busy.map((j) => `${j.name} (${number.format(j.last)})`).join(" · ")}`));
  } else if (waiting.length) {
    b.replaceChildren(document.createTextNode(sep + waiting.map((j) => `${number.format(j.left)} waiting: ${j.name.replace(/\s*\(.*\)$/, "")}`).join(" · ")));
  } else {
    b.replaceChildren();
  }
  b.classList.toggle("quiet", quiet);
  showStatusWork();
}

/** The status bar's right side shows when there's anything to say (it always opens Activity). */
function showStatusWork() {
  $("bg-work").hidden = $("download").hidden && !$("bg-text").textContent;
}
$("job-stop").addEventListener("click", async () => {
  try { await invoke("sort_stop"); status("Stopping the spam filter: nothing has moved."); } catch (err) { showError("Stopping", err); }
});
$("bg-work").addEventListener("click", () => openActivity());
if (!READER) {
  setTimeout(showBackgroundWork, 3000);
  setInterval(showBackgroundWork, 3000);
}

/** A long job's step in words: "Step 2 of 5: giving back the space… (400 of 9,000)". */
function jobStep(j) {
  return `${j.now || "starting"}${j.of > 5 ? ` (${number.format(j.done)} of ${number.format(j.of)})` : ""}`;
}

// The user is here (said at most once a minute): scheduled maintenance waits until Eudora hasn't
// been used for a while
let lastInput = Date.now();
let lastSaid = 0;
for (const kind of ["keydown", "mousedown", "wheel"]) window.addEventListener(kind, () => { lastInput = Date.now(); }, { passive: true, capture: true });
if (!READER) {
  invoke("user_active").catch(() => {});
  setInterval(() => {
    if (lastInput > lastSaid) {
      lastSaid = Date.now();
      invoke("user_active").catch(() => {});
    }
  }, 60_000);
}

/** Activity's table of background jobs. */
async function renderJobs(a) {
  const table = $("ac-jobs");
  if (!a?.jobs?.length) { table.replaceChildren(el("tr", "", "Nothing has run in the background yet (the download reports here every few seconds).")); return; }
  const now = Math.floor(Date.now() / 1000);
  const ago = (t) => (!t ? "not yet" : now - t < 60 ? "just now" : now - t < 3600 ? `${Math.round((now - t) / 60)} min ago` : timeFormat.format(new Date(t * 1000)));
  // (when it runs next instead of "not yet")
  const inTime = (t) => {
    const s = t - now;
    if (s <= 15) return "in a few seconds";
    if (s < 90) return `in ${s} seconds`;
    if (s < 5400) return `in ${Math.round(s / 60)} minutes`;
    if (s < 129600) return `in ${Math.round(s / 3600)} hours`;
    return `in ${Math.round(s / 86400)} days`;
  };
  const head = el("tr");
  for (const h of ["", "Job", "Last did", "When", "This session", "Waiting", ""]) head.append(el("th", "", h));
  // the download first (the main job), then the busiest
  const order = (j) => (j.key === "download" ? 0 : 1);
  const rows = [...a.jobs].sort((x, y) => order(x) - order(y) || (y.active || 0) - (x.active || 0)).map((j) => {
    const busy = !j.paused && (j.working || (now - j.active <= BG_RECENT && j.last > 0));
    const tr = el("tr", busy ? "busy" : j.paused ? "paused" : "");
    const mark = el("td", "");
    if (busy) mark.append(el("span", "bg-spin"));
    const took = j.took == null ? "" : ` (took ${j.took < 60 ? `${j.took} s` : `${Math.round(j.took / 60)} min`})`;
    const did = j.paused ? `Paused${j.said ? ` · last: ${j.said}` : ""}`
      : j.working ? `Now: ${jobStep(j)}${j.started ? ` (started ${ago(j.started)})` : ""}` : `${j.said || "—"}${took}`;
    const when = j.paused ? "paused"
      : j.working ? "now"
      : [j.active ? ago(j.active) : "", j.next ? `next ${inTime(j.next)}` : ""].filter(Boolean).join(" · ") || "—";
    // the next move for this job: the download as the status bar's, the others Pause / Resume
    const act = el("td", "ac-act");
    if (j.key === "download") {
      if (pauseMode === "none") act.append(jobButton("⏸ Pause", () => setPause("old")), jobButton("⏹ Stop", () => setPause("all"), true));
      else if (pauseMode === "old") act.append(jobButton("▶ Resume", () => setPause("none")), jobButton("⏹ Stop", () => setPause("all"), true));
      else act.append(jobButton("▶ Resume", () => setPause("none")));
    } else if (j.key !== "stats") {
      act.append(j.paused ? jobButton("▶ Resume", () => pauseJob(j.key, false)) : jobButton("⏸ Pause", () => pauseJob(j.key, true)));
    }
    // what it did to each message (the jobs that act on messages)
    if (DETAILED.has(j.key)) act.append(jobButton("Details", () => showDetails(j.key, j.name)));
    tr.append(mark, el("td", "", j.name), el("td", "", did), el("td", "", when), el("td", "num", number.format(j.total)), el("td", "num", j.left == null ? "" : number.format(j.left)), act);
    return tr;
  });
  // the window's own spam-filter run, with its Stop
  let sortJob = null;
  try { sortJob = await invoke("sort_progress"); } catch { /* (none) */ }
  if (sortJob) {
    const tr = el("tr", "busy");
    const mark = el("td", "");
    mark.append(el("span", "bg-spin"));
    const act = el("td", "ac-act");
    act.append(jobButton("⏹ Stop", () => invoke("sort_stop").then(renderActivity), true));
    tr.append(mark, el("td", "", "Spam filter (Sort with the Spam Filter)"), el("td", "", `Now: deciding ${number.format(sortJob[0])} of ${number.format(sortJob[1])}; nothing moves until you choose Move`), el("td", "", "now"), el("td", "num", ""), el("td", "num", ""), act);
    rows.splice(1, 0, tr);
  }
  // the user's own runs (Sort with the Spam Filter, Filter Messages): their details too
  for (const [key, name] of [["sort", "Sort with the Spam Filter (your own runs)"], ["filter", "Filter Messages (your own runs)"]]) {
    const tr = el("tr", "");
    const act = el("td", "ac-act");
    act.append(jobButton("Details", () => showDetails(key, name)));
    tr.append(el("td", ""), el("td", "", name), el("td", "", "—"), el("td", "", ""), el("td", "num", ""), el("td", "num", ""), act);
    rows.push(tr);
  }
  table.replaceChildren(head, ...rows);
}
/** The jobs that log what they did to each message (Activity ▸ Details). */
const DETAILED = new Set(["rules", "server", "archive", "views", "alerts"]);

/** A button in Activity's table. */
function jobButton(label, onClick, stop = false) {
  const b = el("button", stop ? "dl-btn stop" : "dl-btn", label);
  b.type = "button";
  b.addEventListener("click", async () => { await onClick(); renderActivity(); });
  return b;
}

/** Pauses or resumes one background job (the download passes it over until resumed). */
async function pauseJob(key, paused) {
  try {
    await invoke("job_pause", { key, paused });
    status(paused ? "Paused: that job waits until you resume it (Activity)." : "Resumed.");
  } catch (err) { showError("Pausing the job", err); }
}

/** Activity, docked above the status bar: the status bar's click and Tools ▸ Activity open it, or
 *  close it when open; its height is kept. The mail stays usable meanwhile. */
let activityTimer = 0;
async function openActivity() {
  const dock = $("activity-box");
  if (!dock.hidden) { closeActivity(); return; }
  dock.style.height = `${store.get("activityHeight", 300)}px`;
  dock.hidden = false;
  await renderActivity();
  clearInterval(activityTimer);
  activityTimer = setInterval(() => { if (!dock.hidden && $("ac-details").hidden) renderActivity(); }, 2000);
}
function closeActivity() {
  $("activity-box").hidden = true;
  clearInterval(activityTimer);
}
$("ac-close").addEventListener("click", closeActivity);
// its top edge: dragged to make it taller or shorter
$("ac-grip").addEventListener("mousedown", (e) => {
  e.preventDefault();
  const dock = $("activity-box");
  const startY = e.clientY;
  const start = dock.offsetHeight;
  document.body.classList.add("dragging");
  const move = (ev) => { dock.style.height = `${Math.max(120, Math.min(window.innerHeight - 200, start + startY - ev.clientY))}px`; };
  const up = () => {
    document.body.classList.remove("dragging");
    window.removeEventListener("mousemove", move);
    window.removeEventListener("mouseup", up);
    store.set("activityHeight", dock.offsetHeight);
  };
  window.addEventListener("mousemove", move);
  window.addEventListener("mouseup", up);
});

// ---- a job's details
const details = { job: null, before: null };
async function showDetails(job, name) {
  details.job = job;
  details.before = null;
  $("ac-details-title").textContent = name;
  $("ac-details-list").replaceChildren();
  $("ac-main").hidden = true;
  $("ac-details").hidden = false;
  await moreDetails();
}
const runFormat = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
async function moreDetails() {
  let page;
  try { page = await invoke("job_details", { job: details.job, before: details.before, runs: details.before == null ? 1 : 3 }); } catch (err) { showError("Reading the details", err); return; }
  const table = $("ac-details-list");
  if (!table.children.length) {
    const head = el("tr");
    for (const h of ["When", "From", "Subject", "What was done"]) head.append(el("th", "", h));
    table.append(head);
    if (!page.entries.length) table.append(el("tr", "", "Nothing logged yet: details are kept from now on, for 30 days."));
  }
  let run = null;
  for (const e of page.entries) {
    if (e.run !== run) {
      run = e.run;
      const count = page.entries.filter((x) => x.run === run).length;
      const tr = el("tr", "ac-run");
      const td = el("td", "", `Run at ${runFormat.format(new Date(run * 1000))}: ${number.format(count)} item${count === 1 ? "" : "s"}`);
      td.colSpan = 4;
      tr.append(td);
      table.append(tr);
    }
    const tr = el("tr", e.message && e.subject ? "ac-open" : "");
    tr.append(el("td", "", timeFormat.format(new Date(e.at * 1000))), el("td", "", e.from || ""), el("td", "", e.subject || (e.message ? "(no longer here)" : "")), el("td", "", e.what));
    if (e.message && e.subject) {
      tr.title = "Open this message";
      tr.addEventListener("click", () => openInnerReader(e.message, e.subject));
    }
    table.append(tr);
  }
  details.before = page.more;
  $("ac-more").hidden = page.more == null;
}
$("ac-more").addEventListener("click", moreDetails);
$("ac-back").addEventListener("click", () => {
  $("ac-details").hidden = true;
  $("ac-main").hidden = false;
  renderActivity();
});
$("ac-maintain").addEventListener("click", async () => {
  try {
    await invoke("maintenance_now");
    status("Maintenance asked for: the background download starts it within seconds; each step shows here and on the status bar.");
  } catch (err) { showError("Running maintenance", err); }
});

async function renderActivity() {
  let a;
  try { a = await invoke("activity"); } catch (err) { $("ac-summary").textContent = `Couldn't read it: ${err}`; return; }
  const lines = [];
  const d = a.download;
  if (a.offline) lines.push("Working offline (File ▸ Work Offline): nothing is downloaded or sent.");
  lines.push(d ? `Download: ${d.state}${d.line ? ` · ${d.line}` : ""}` : "Download: not running.");
  lines.push(`Waiting for the servers: ${number.format(a.moves_waiting)} moves, ${number.format(a.changes_waiting)} status / label / delete changes${a.given_up ? ` (${number.format(a.given_up)} given up after errors${a.last_error ? `: ${a.last_error}` : ""})` : ""}.`);
  lines.push(`Out mailbox: ${number.format(a.out_waiting)} waiting to be sent.`);
  if (a.job) lines.push(`${a.job.running ? "Running" : "Last job"}: ${a.job.running ? a.job.line : a.job.result}`);
  $("ac-summary").replaceChildren(...lines.map((l) => el("div", "", l)));
  await renderJobs(a.background);
  const log = $("ac-log");
  const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
  log.textContent = a.log.join("\n");
  if (atBottom) log.scrollTop = log.scrollHeight;
}

// ------------------------------------------------------------------ the local API: asking, sending, the log

const PERMISSIONS = {
  "accounts.read": "see your accounts' addresses and names",
  "mail.headers.read": "see who your mail is from, its subjects and dates (not the text)",
  "mail.bodies.read": "read the full text of your mail",
  "mail.attachments.read": "open your attachments",
  "mail.search": "search your mail",
  "mail.change": "file mail, mark it read or unread, change labels and priority",
  "mail.delete": "delete mail",
  "mail.send": "send mail as you (each message shown to you first)",
  "rules.read": "see your rules",
  "rules.write": "change your rules",
  "mail.compose": "write mail: drafts, replies and forwards (with the message they answer), spelling, address suggestions",
  "spam.filter": "use and change the spam filter (it moves mail)",
  "settings.read": "see your preferences (labels, signatures, views, the window's choices)",
  "settings.write": "change your preferences",
  "accounts.manage": "add and change accounts and their servers, and sign in or out (it can make Eudora connect to servers it names)",
  "status.read": "see what Eudora is doing (downloads, changes waiting, sync, statistics)",
  "library.manage": "run your library: check mail, pause, work offline, back up (copying all your mail to a file), restore, import, optimize, sync, plugins",
  "contacts.read": "see your address book (names, addresses, phone numbers, notes)",
  "contacts.write": "add, change, merge and delete your contacts, on the servers too",
};
const SENSITIVE = new Set(["mail.bodies.read", "mail.attachments.read", "mail.send", "mail.delete", "rules.write", "spam.filter", "settings.write", "accounts.manage", "library.manage", "contacts.read", "contacts.write"]);
const api = { busy: false };

function grantText(g) {
  let s = PERMISSIONS[g.permission] || g.permission;
  if (g.accounts?.length) s += ` (only ${g.accounts.join(", ")})`;
  if (g.folders?.length) s += ` (only the folders ${g.folders.join(", ")})`;
  return s;
}

/** Shows what waits for the user: a program asking for access, a message a program wants sent. */
async function checkApi() {
  if (api.busy || document.querySelector("dialog[open]")) return;
  let waiting;
  try { waiting = await invoke("api_waiting"); } catch { return; }
  const [requests, sends] = waiting;
  if (requests.length) { api.busy = true; showGrant(requests[0]); } else if (sends.length) { api.busy = true; showSend(sends[0]); }
}

function showGrant(r) {
  $("ag-title").textContent = `“${r.name}” asks to use your mail`;
  $("ag-who").textContent = `${r.kind === "plugin" ? "A plugin" : "A program"} from “${r.publisher}” (as it says), asking now.`;
  $("ag-reason").textContent = `Why, in its words: “${r.reason}”`;
  $("ag-list").replaceChildren(...r.asked.map((g, i) => {
    const line = el("label", `ag-line${SENSITIVE.has(g.permission) ? " sensitive" : ""}`);
    const box = el("input");
    box.type = "checkbox";
    // the strongest ones start unticked: the user ticks them on purpose
    box.checked = !SENSITIVE.has(g.permission);
    box.dataset.i = i;
    box.addEventListener("change", warn);
    line.append(box, el("span", "", grantText(g)));
    return line;
  }));
  function warn() {
    const ticked = r.asked.filter((_, i) => $("ag-list").querySelector(`input[data-i="${i}"]`).checked).map((g) => g.permission);
    const reads = ticked.includes("mail.bodies.read") || ticked.includes("mail.attachments.read");
    const msg = reads && ticked.includes("mail.send")
      ? "It could read your mail and send it on. Allow both only for a program you trust completely; each message it sends is still shown to you first."
      : reads ? "It could read the full text of your mail. Eudora logs every message it reads." : "";
    $("ag-warning").textContent = msg;
    $("ag-warning").hidden = !msg;
  }
  warn();
  const done = async (grants) => {
    try { await invoke("api_answer", { request: r.id, grants }); } catch (err) { showError("Answering the program", err); }
    $("api-grant-box").close();
    api.busy = false;
    status(grants ? `“${r.name}” may now use what you allowed (Settings ▸ Programs and plugins).` : `“${r.name}” was refused.`);
  };
  $("ag-allow").onclick = () => {
    const grants = r.asked.filter((_, i) => $("ag-list").querySelector(`input[data-i="${i}"]`).checked);
    done(grants.length ? grants : null);
  };
  $("ag-refuse").onclick = () => done(null);
  $("api-grant-box").showModal();
}

function showSend(s) {
  const d = s.draft;
  $("as-title").textContent = `“${s.client_name}” wants to send a message`;
  $("as-head").replaceChildren(...[["From", d.from], ["To", d.to], ["Cc", d.cc], ["Bcc", d.bcc], ["Subject", d.subject]]
    .filter(([, v]) => v).map(([k, v]) => { const row = el("div"); row.append(el("b", "", `${k}: `), document.createTextNode(v)); return row; }));
  $("as-body").textContent = d.text || (d.html ? new DOMParser().parseFromString(d.html, "text/html").body.innerText : "(no text)");
  $("as-trust").textContent = `Send, and allow all from “${s.client_name}”`;
  const answer = async (ok, trust) => {
    try { await invoke("api_answer_send", { send: s.id, ok, trust }); } catch (err) { showError("Answering", err); }
    $("api-send-box").close();
    api.busy = false;
    status(ok ? `Sent for “${s.client_name}”${trust ? "; it may send without asking from now on" : ""}.` : `Not sent (“${s.client_name}” is told).`);
  };
  $("as-send").onclick = () => answer(true, false);
  $("as-trust").onclick = async () => {
    if (await askYesNo("Allow sending without asking", `From now on “${s.client_name}” may send mail as you without showing it to you first. (You can take that back in Settings ▸ Programs and plugins.)`, "Allow it", "Don't allow")) answer(true, true);
  };
  $("as-refuse").onclick = () => answer(false, false);
  $("api-send-box").showModal();
}
for (const id of ["api-grant-box", "api-send-box"]) $(id).addEventListener("close", () => { api.busy = false; });
if (!READER) setInterval(checkApi, 3000); // (the main window answers programs, not message windows)

async function renderPrefApi() {
  let o;
  try { o = await invoke("api_overview"); } catch (err) { $("p-api-state").textContent = `Couldn't read it: ${err}`; return; }
  $("p-api-on").checked = o.settings.with_window;
  $("p-api-closed").checked = o.settings.without_window;
  $("p-api-state").textContent = o.state;
  const shown = o.clients.filter((c) => c.granted || c.revoked);
  $("p-api-clients").replaceChildren(...(shown.length ? shown.map((c) => {
    const row = el("div", "p-api-client");
    const who = el("div", "p-who");
    who.append(
      el("div", "p-address", `${c.name}${c.revoked ? " (revoked)" : ""}`),
      el("div", "p-kind", `${c.kind} from ${c.publisher} · ${c.last_used ? `last used ${new Date(c.last_used * 1000).toLocaleString()}` : "not used yet"}`),
      el("div", "p-kind", c.grants.map(grantText).join(" · ") || "nothing"),
    );
    row.append(who);
    if (!c.revoked) {
      const log = el("select");
      options(log, [["full", "Log everything"], ["changes", "Log changes only"], ["refusals", "Log refusals only"]], c.log_level);
      log.title = "How much of what it does is logged (refusals always are)";
      const trust = el("label");
      const box = el("input");
      box.type = "checkbox";
      box.checked = c.send_without_asking;
      trust.append(box, document.createTextNode(" sends without asking"));
      const save = () => invoke("api_set_trust", { client: c.id, log: log.value, sendWithoutAsking: box.checked }).catch((err) => showError("Changing it", err));
      log.addEventListener("change", save);
      box.addEventListener("change", save);
      const revoke = el("button", "f-small", "Revoke");
      revoke.addEventListener("click", async () => {
        if (!(await askYesNo("Revoke", `Take every permission away from “${c.name}”? It stops working with Eudora at once.`, "Revoke"))) return;
        try { await invoke("api_revoke", { client: c.id }); renderPrefApi(); } catch (err) { showError("Revoking", err); }
      });
      const logBtn = el("button", "f-small", "Log…");
      logBtn.addEventListener("click", () => openApiLog(c.id));
      row.append(log, trust, logBtn, revoke);
    }
    return row;
  }) : [el("p", "p-note", "No program or plugin has been allowed yet.")]));
}
const saveApiSettings = async () => {
  try {
    await invoke("api_settings_set", { settings: { with_window: $("p-api-on").checked, without_window: $("p-api-closed").checked } });
    renderPrefApi();
  } catch (err) { showError("Changing the API", err); }
};
$("p-api-on").addEventListener("change", saveApiSettings);
$("p-api-closed").addEventListener("change", saveApiSettings);
$("p-api-log").addEventListener("click", () => openApiLog(null));

async function openApiLog(client) {
  let o;
  try { o = await invoke("api_overview"); } catch { o = { clients: [] }; }
  options($("al-client"), [["", "Every program and plugin"], ...o.clients.map((c) => [String(c.id), c.name])], client == null ? "" : String(client));
  await renderApiLog();
  if (!$("api-log-box").open) $("api-log-box").showModal();
}
async function renderApiLog() {
  const client = $("al-client").value ? Number($("al-client").value) : null;
  let lines;
  try { lines = await invoke("api_log", { client, limit: 1000 }); } catch (err) { $("al-list").textContent = `Couldn't read the log: ${err}`; return; }
  $("al-list").replaceChildren(...(lines.length ? lines.map((l) => {
    const row = el("div", `al-row${l.allowed ? "" : " refused"}${l.write ? " write" : ""}`);
    row.append(
      el("span", "al-at", new Date(l.at * 1000).toLocaleString()),
      el("span", "al-who", l.client_name || "Eudora"),
      el("span", "al-what", `${l.method}${l.allowed ? "" : " (refused)"}`),
      el("span", "al-detail", l.detail),
    );
    return row;
  }) : [el("p", "p-note", "Nothing logged yet.")]));
}
$("al-client").addEventListener("change", renderApiLog);
$("al-close").addEventListener("click", () => $("api-log-box").close());

// ------------------------------------------------------------------ plugins (Tools ▸ Plugins)

state.plugins = [];

async function loadPlugins() {
  try { state.plugins = await invoke("plugins_list"); } catch { state.plugins = []; }
}

async function openPlugins() {
  await loadPlugins();
  renderPlugins();
  if (!$("plugins-box").open) $("plugins-box").showModal();
}

function renderPlugins() {
  $("pl-list").replaceChildren(...(state.plugins.length ? state.plugins.map((p) => {
    const m = p.manifest;
    const row = el("div", "pl-row");
    const who = el("div", "p-who");
    who.append(
      el("div", "p-address", `${m.name} ${m.version}`),
      el("div", "p-kind", `from ${m.publisher}${m.description ? ` · ${m.description}` : ""}`),
      el("div", "p-kind", p.granted ? "Allowed (Settings ▸ Programs and plugins shows what)" : "Waiting for you to say what it may do"),
    );
    row.append(who);
    for (const c of m.commands) {
      const run = el("button", "f-small", c.title);
      run.disabled = !p.enabled;
      run.addEventListener("click", () => runPlugin(m.id, c.id, c.title));
      row.append(run);
    }
    const on = el("label");
    const box = el("input");
    box.type = "checkbox";
    box.checked = p.enabled;
    box.addEventListener("change", async () => { await invoke("plugin_enable", { id: m.id, on: box.checked }).catch((err) => showError("Switching the plugin", err)); await openPlugins(); });
    on.append(box, document.createTextNode(" on"));
    const remove = el("button", "f-small", "Remove");
    remove.addEventListener("click", async () => {
      if (!(await askYesNo("Remove plugin", `Remove the plugin “${m.name}”? Its permissions go with it (its log stays).`, "Remove it"))) return;
      try { await invoke("plugin_remove", { id: m.id }); await openPlugins(); } catch (err) { showError("Removing the plugin", err); }
    });
    row.append(on, remove);
    return row;
  }) : [el("p", "p-note", "No plugins yet.")]));
}

$("pl-close").addEventListener("click", () => $("plugins-box").close());
$("pl-install").addEventListener("click", async () => {
  let chosen;
  try { chosen = await invoke("pick_paths", { what: "file", title: "Install a plugin (.wasm)", kind: null }); } catch (err) { showError("Choosing the plugin", err); return; }
  if (!chosen.length) return;
  if (!(await askYesNo("Unsigned plugin", "Eudora can't check who made this plugin (plugins aren't signed yet). It will run sealed off, with only what you allow it next, and everything it does is logged.", "Install it", "Don't install"))) return;
  try {
    const m = await invoke("plugin_install", { path: chosen[0] });
    await openPlugins();
    status(`Installed “${m.name}”: say what it may do in the window that opens.`);
    $("plugins-box").close();
    api.busy = false;
    checkApi();
  } catch (err) { showError("Installing the plugin", err); }
});

/** Runs a plugin's command on the selection; its answer goes to the status bar or a view. */
async function runPlugin(id, command, title) {
  const items = await selectedItems().catch(() => []);
  const list = state.list;
  const context = { ids: items.map((i) => i.id), folder: list?.kind === "folder" ? list.folders[0] : null };
  status(`Running ${title}…`);
  try {
    const out = await invoke("plugin_run", { id, command, context });
    if (out.error) status(`${title}: ${out.error}`);
    else if (out.message) status(`${title}: ${out.message}`);
    else status(`${title}: done.`);
    if (out.html) {
      $("pv-title").textContent = title;
      // its page shows without scripts, pictures from the web or links that open anything
      $("pv-frame").srcdoc = `<!doctype html><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:">${out.html}`;
      $("plugin-view-box").showModal();
    }
  } catch (err) { showError(title, err); }
}
$("pv-close").addEventListener("click", () => $("plugin-view-box").close());

/** The plugins' message commands, in the right-click menu. */
function fillPluginMenu() {
  const items = [];
  for (const p of state.plugins.filter((x) => x.enabled)) {
    for (const c of p.manifest.commands.filter((c) => (c.where || []).includes("message"))) {
      const b = el("button", "", `${c.title} (${p.manifest.name})`);
      b.addEventListener("click", () => { rowMenu.hidden = true; runPlugin(p.manifest.id, c.id, c.title); });
      items.push(b);
    }
  }
  $("row-menu-plugins").replaceChildren(...(items.length ? [el("hr"), ...items] : []));
}

// ------------------------------------------------------------------ Tools ▸ Signatures

const sigs = { data: null, at: 0 };

async function openSignatures() {
  try { sigs.data = await invoke("signatures"); } catch (err) { showError("Reading the signatures", err); return; }
  if (!sigs.data.list.length) sigs.data.list.push({ name: "Signature", html: "" });
  sigs.at = 0;
  renderSignatures();
  $("sig-box").showModal();
}

function keepEditing() {
  const s = sigs.data?.list[sigs.at];
  if (!s) return;
  s.name = $("sig-name").value;
  s.html = $("sig-editor").innerHTML;
}

async function renderSignatures() {
  const d = sigs.data;
  $("sig-list").replaceChildren(...d.list.map((s, i) => {
    const b = el("button", i === sigs.at ? "on" : "", s.name || "(no name)");
    b.addEventListener("click", () => { keepEditing(); sigs.at = i; renderSignatures(); });
    return b;
  }));
  const s = d.list[sigs.at];
  $("sig-name").value = s?.name || "";
  $("sig-editor").innerHTML = s?.html || "";
  // each address the user writes from, and the one for any other
  let identities = [];
  try { identities = (await invoke("identities")).filter((i) => i.can_send); } catch { /* none */ }
  const pick = (value, set) => {
    const select = el("select");
    select.append(el("option", "", "No signature"));
    select.firstChild.value = "";
    for (const x of d.list) { const o = el("option", "", x.name); o.value = x.name; select.append(o); }
    select.value = value || "";
    select.addEventListener("change", () => set(select.value));
    return select;
  };
  const rows = [];
  for (const i of identities) {
    const key = i.address.toLowerCase();
    rows.push(el("span", "", i.address), pick(d.defaults[key] ?? d.fallback, (v) => { d.defaults[key] = v; }));
  }
  rows.push(el("span", "", "Any other address"), pick(d.fallback, (v) => { d.fallback = v; }));
  $("sig-defaults").replaceChildren(...rows);
}

$("sig-add").addEventListener("click", () => {
  keepEditing();
  sigs.data.list.push({ name: `Signature ${sigs.data.list.length + 1}`, html: "" });
  sigs.at = sigs.data.list.length - 1;
  renderSignatures();
  $("sig-name").focus();
  $("sig-name").select();
});
$("sig-remove").addEventListener("click", () => {
  sigs.data.list.splice(sigs.at, 1);
  sigs.at = Math.max(0, sigs.at - 1);
  renderSignatures();
});
$("sig-name").addEventListener("input", () => {
  const s = sigs.data.list[sigs.at];
  if (!s) return;
  // the defaults follow a renamed signature
  for (const [a, n] of Object.entries(sigs.data.defaults)) if (n === s.name) sigs.data.defaults[a] = $("sig-name").value;
  if (sigs.data.fallback === s.name) sigs.data.fallback = $("sig-name").value;
  s.name = $("sig-name").value;
  $("sig-list").children[sigs.at].textContent = s.name || "(no name)";
});
for (const b of document.querySelectorAll("[data-sig-cmd]")) {
  b.addEventListener("mousedown", (e) => e.preventDefault());
  b.addEventListener("click", () => document.execCommand(b.dataset.sigCmd));
}
$("sig-link").addEventListener("mousedown", (e) => e.preventDefault());
$("sig-link").addEventListener("click", async () => {
  // (the question takes the focus: the text chosen for the link is put back first)
  const sel = window.getSelection();
  const range = sel.rangeCount ? sel.getRangeAt(0).cloneRange() : null;
  const url = await askText("Add a link", "Link to (web address):", "https://", "Add link");
  if (!url) return;
  if (range) { sel.removeAllRanges(); sel.addRange(range); }
  document.execCommand("createLink", false, url);
});
$("sig-cancel").addEventListener("click", () => $("sig-box").close());
$("sig-save").addEventListener("click", async () => {
  keepEditing();
  try {
    await invoke("signatures_set", { signatures: sigs.data });
    $("sig-box").close();
    status("Signatures kept.");
  } catch (err) { showError("Keeping the signatures", err); }
});

// ------------------------------------------------------------------ status, priority, labels

const UNREAD = 1, REPLIED = 2, FORWARDED = 4, REDIRECTED = 8;
const FLAGGED = 1 << 11; // flagged as a To Do (Outlook's flag on Microsoft 365 mail)
const priorityOf = (flags) => (flags >> 8) & 7;
const PRIORITY_NAMES = ["", "Highest", "High", "Normal", "Low", "Lowest"];

async function loadLabels() {
  try { state.labelDefs = await invoke("label_list"); } catch { state.labelDefs = []; }
}

function labelColor(name) {
  return state.labelDefs?.find((l) => l.name.toLowerCase() === name.toLowerCase())?.color || "#808080";
}

/** The rows shown with these ids changed by `change` (a row object), drawn again. */
function patchRows(ids, change) {
  const wanted = new Set(ids);
  for (const row of state.list?.rows.values() || []) if (wanted.has(row.id)) change(row);
  for (const [index, e] of rowPool) {
    const row = state.list?.rows.get(index);
    if (row && wanted.has(row.id)) {
      const fresh = makeRow(index, row);
      e.replaceWith(fresh);
      rowPool.set(index, fresh);
    }
  }
}

async function selectedIds() {
  const items = await selectedItems();
  if (!items.length) status("Choose a message first.");
  return items.map((i) => i.id);
}

/** Message ▸ Flag as To Do (Ctrl+Shift+G, Insert): the selection becomes To Do tasks, each
 *  linked to its message (Outlook's flag on Microsoft 365 mail); flagged ones are unflagged. */
/** Add to Trips (right-click): the selected emails read for flights, trains, stays, rentals…,
 *  for travel mail Eudora didn't recognize by itself. */
async function addToTrips() {
  const items = await selectedItems();
  if (!items.length) { status("Choose a message first."); return; }
  let found = 0;
  try {
    for (const i of items) found += await invoke("trips_from_message", { id: i.id });
  } catch (err) { showError("Adding to Trips", err); return; }
  if (!found) {
    await tell("Add to Trips", "Eudora couldn't find a flight, train, bus, ferry, hotel stay, car rental or booked ride in this email.");
    return;
  }
  status(`${found} ${found === 1 ? "reservation" : "reservations"} added to Trips (Calendar ▸ Trips); flights and trains go on your calendar.`);
  window.calTripsCount?.();
}

async function flagAsTodo() {
  const items = await selectedItems();
  if (!items.length) { status("Choose a message first."); return; }
  const ids = items.map((i) => i.id);
  const rows = ids.map((id) => [...(state.list?.rows.values() || [])].find((r) => r.id === id)).filter(Boolean);
  const on = !rows.length || !rows.every((r) => r.flags & FLAGGED);
  try {
    await invoke("todo_flag", { ids, on });
    patchRows(ids, (r) => { r.flags = on ? r.flags | FLAGGED : r.flags & ~FLAGGED; });
    status(on
      ? `${ids.length === 1 ? "Flagged" : `${number.format(ids.length)} messages flagged`} as To Do: in To Do (left), and flagged in Outlook for Microsoft 365 mail.`
      : `Flag taken off ${ids.length === 1 ? "the message" : `${number.format(ids.length)} messages`}; its task is done.`);
    window.todoRefresh?.();
  } catch (err) { showError("Flagging as To Do", err); }
}

async function changeStatus(name) {
  const ids = await selectedIds();
  if (!ids.length) return;
  try {
    // (the folders' unread numbers follow at once, from the rows on screen that change)
    const shown = [...(state.list?.rows.values() || [])].filter((r) => ids.includes(r.id));
    const before = shown.filter((r) => r.flags & UNREAD);
    const n = await invoke("set_status", { ids, status: name });
    const bits = { unread: UNREAD, replied: REPLIED, forwarded: FORWARDED, redirected: REDIRECTED };
    if (name === "unread") adjustUnread(shown.filter((r) => !before.includes(r)), +1);
    else if (name === "read") adjustUnread(before, -1);
    patchRows(ids, (r) => { r.flags = name === "unread" ? r.flags | UNREAD : (r.flags & ~UNREAD) | (bits[name] || 0); });
    status(`${number.format(n)} ${n === 1 ? "message" : "messages"} marked ${name}.`);
  } catch (err) { showError("Changing the status", err); }
}

async function changePriority(level, shift = null) {
  const ids = await selectedIds();
  if (!ids.length) return;
  try {
    await invoke("set_priority", { ids, level, shift });
    patchRows(ids, (r) => {
      const now = priorityOf(r.flags) || 3;
      const next = shift ? Math.min(5, Math.max(1, now + shift)) : level;
      r.flags = (r.flags & ~(7 << 8)) | (next << 8);
    });
    status(shift ? (shift < 0 ? "Priority raised." : "Priority lowered.") : level ? `Priority: ${PRIORITY_NAMES[level]}.` : "Priority as the message says.");
  } catch (err) { showError("Changing the priority", err); }
}

/** Shown messages are marked read after a moment on screen (as Outlook's reading pane does). */
function markReadSoon(id) {
  const row = [...(state.list?.rows.values() || [])].find((r) => r.id === id);
  if (!row || !(row.flags & UNREAD)) return;
  setTimeout(async () => {
    if (state.messageId !== id || !(row.flags & UNREAD)) return;
    try {
      await invoke("set_status", { ids: [id], status: "read" });
      adjustUnread([row], -1);
      patchRows([id], (r) => { r.flags &= ~UNREAD; });
    } catch { /* tried again next time it's shown */ }
  }, 1200);
}

async function openLabelPicker() {
  const ids = await selectedIds();
  if (!ids.length) return;
  await loadLabels();
  const rows = ids.map((id) => [...state.list.rows.values()].find((r) => r.id === id)).filter(Boolean);
  const has = (name) => rows.filter((r) => (r.categories || []).includes(name)).length;
  $("label-note").textContent = ids.length > 1 ? `${number.format(ids.length)} messages: a tick gives the label to all of them, an empty box takes it off.` : "A tick gives the message the label; several labels are fine.";
  $("label-list").replaceChildren(...state.labelDefs.map((l, i) => {
    const line = el("label", "label-pick");
    const box = el("input");
    box.type = "checkbox";
    const n = has(l.name);
    box.checked = n > 0;
    box.indeterminate = n > 0 && n < rows.length;
    box.addEventListener("change", async () => {
      try {
        await invoke("set_label", { ids, label: l.name, on: box.checked });
        patchRows(ids, (r) => {
          const set = new Set(r.categories || []);
          if (box.checked) set.add(l.name); else set.delete(l.name);
          r.categories = [...set];
        });
      } catch (err) { showError("Changing the label", err); }
    });
    const dot = el("span", "label-dot");
    dot.style.background = l.color;
    line.append(box, dot, el("span", "", `${i < 9 ? `${i + 1}. ` : ""}${l.name}`));
    return line;
  }));
  $("label-none").onclick = async () => {
    try {
      await invoke("set_label", { ids, label: null, on: false });
      patchRows(ids, (r) => { r.categories = []; });
      $("label-box").close();
    } catch (err) { showError("Taking the labels off", err); }
  };
  $("label-box").showModal();
}
$("label-done").addEventListener("click", () => $("label-box").close());
$("label-edit").addEventListener("click", () => {
  $("label-box").close();
  openPrefs("mail");
});
$("label-box").addEventListener("keydown", (e) => {
  // 1-9 ticks a label, as classic Eudora's label shortcuts
  const n = Number(e.key);
  if (n >= 1 && n <= 9) {
    const box = $("label-list").querySelectorAll("input")[n - 1];
    if (box) { box.checked = !box.checked; box.dispatchEvent(new Event("change")); }
  }
});

function renderPrefLabels() {
  const defs = (state.labelDefs || []).map((l) => ({ ...l, was: l.name }));
  const draw = () => {
    $("p-labels").replaceChildren(...defs.map((l, i) => {
      const row = el("div", "p-label");
      const color = el("input");
      color.type = "color";
      color.value = l.color;
      color.addEventListener("input", () => { l.color = color.value; });
      const name = el("input");
      name.type = "text";
      name.value = l.name;
      name.addEventListener("input", () => { l.name = name.value; });
      const remove = el("button", "f-small", "Remove");
      remove.title = "Take it out of the list (messages that have it keep it until you take it off them)";
      remove.addEventListener("click", () => { defs.splice(i, 1); draw(); });
      row.append(color, name, remove);
      return row;
    }));
  };
  draw();
  $("p-label-add").onclick = () => { defs.push({ name: `Label ${defs.length + 1}`, color: "#3498db", was: null }); draw(); };
  $("p-labels-save").onclick = async () => {
    const list = defs.filter((l) => l.name.trim()).map((l) => ({ name: l.name.trim(), color: l.color }));
    const renamed = defs.filter((l) => l.was && l.name.trim() && l.was !== l.name.trim()).map((l) => [l.was, l.name.trim()]);
    try {
      await invoke("label_list_set", { list, renamed });
      await loadLabels();
      renderPrefLabels();
      renderList(true);
      status(renamed.length ? `Labels kept; ${renamed.length} renamed on the messages that have them.` : "Labels kept.");
    } catch (err) { showError("Keeping the labels", err); }
  };
}

async function openEmptyTrash() {
  let list;
  try { list = await invoke("trash_summary"); } catch (err) { showError("Reading the Trash", err); return; }
  const rows = list.filter((t) => t.messages > 0 || t.archive_kept > 0);
  if (!rows.length) { status("Every Trash is empty."); return; }
  $("trash-list").replaceChildren(...rows.map((t) => {
    const line = el("label", "trash-line");
    const box = el("input");
    box.type = "checkbox";
    box.checked = t.messages > 0;
    box.disabled = t.messages === 0;
    box.dataset.account = t.account;
    const what = el("span");
    const where = t.on_server ? "deleted where it came from too (Microsoft 365, Outlook.com or Google)" : "in Eudora (the server, if any, keeps its copy)";
    what.textContent = `${t.shown} › ${t.path}: ${number.format(t.messages)} messages, ${where}`;
    if (t.archive_kept) what.append(el("div", "p-note", `${number.format(t.archive_kept)} in the Online Archive's Deleted Items are kept: Eudora never deletes from the archive.`));
    line.append(box, what);
    return line;
  }));
  $("trash-box").showModal();
}
$("trash-cancel").addEventListener("click", () => $("trash-box").close());
$("trash-go").addEventListener("click", async () => {
  const accounts = [...$("trash-list").querySelectorAll("input:checked")].map((b) => Number(b.dataset.account));
  if (!accounts.length) { $("trash-box").close(); return; }
  try {
    $("trash-go").disabled = true;
    const n = await invoke("empty_trash", { accounts });
    $("trash-box").close();
    status(`Emptied the Trash: ${number.format(n)} messages deleted.`);
    refresh(true);
  } catch (err) { showError("Emptying the Trash", err); }
  finally { $("trash-go").disabled = false; }
});


// ------------------------------------------------------------------ the Address Book

const ab = { rows: [], at: null, timer: 0 };
const CARD_KINDS = { work: "Work", home: "Home", mobile: "Mobile", fax: "Fax", other: "" };

/** Back to the mail (or another page) from Contacts. */
function closeContacts() {
  if (!state.contactsOn) return;
  state.contactsOn = false;
  document.body.classList.remove("contacts-mode");
  $("contacts").hidden = true;
}
window.closeContacts = closeContacts;

async function openAddressBook(account) {
  const accounts = state.accounts?.accounts || [];
  const select = $("ab-account");
  // (from the sidebar: that account's; from the menu or the toolbar, the last choice)
  const keep = typeof account === "number" ? String(account) : account === null ? "" : select.value;
  select.replaceChildren(el("option", "", "Every account"), ...accounts.map((a) => {
    const o = el("option", "", a.shown || a.address || `Account ${a.id}`);
    o.value = String(a.id);
    return o;
  }));
  select.firstChild.value = "";
  select.value = keep;
  // a page in the right-hand panel, like the calendar
  window.closeCalendar?.();
  window.closeTodo?.();
  state.contactsOn = true;
  document.body.classList.add("contacts-mode");
  $("contacts").hidden = false;
  renderSidebar();
  await loadAddressBook();
  $("ab-find").focus();
}

/** The address book shown at once from the last copy (this session's, else the last start's),
 *  filtered here as you type; the fresh copy read in the background replaces it (user
 *  2026-10-20: "it should use the last cached copy then look for updates"). */
const abCache = new Map(); // account id or "all" → rows
const abFresh = new Map(); // account id or "all" → when last read
function abFilter(rows, query) {
  const q = query.toLowerCase();
  if (!q) return rows;
  return rows.filter((r) => (r.shown || "").toLowerCase().includes(q) || JSON.stringify(r.card).toLowerCase().includes(q));
}

async function loadAddressBook(force = false) {
  if (force === true) abFresh.clear(); // (after an edit, a new contact, a deletion: read again now)
  const account = $("ab-account").value ? Number($("ab-account").value) : null;
  const key = account ?? "all";
  let rows = abCache.get(key) ?? (key === "all" ? store.get("abCache", null) : null);
  if (rows) {
    abCache.set(key, rows);
    renderAddressBook(abFilter(rows, $("ab-find").value.trim()));
  }
  // the fresh copy (not more than once a minute, never per keystroke)
  if (rows && Date.now() - (abFresh.get(key) || 0) < 60_000) return;
  abFresh.set(key, Date.now());
  const fresh = invoke("cards_list", { query: "", account })
    .then((list) => {
      abCache.set(key, list);
      if (key === "all") { try { store.set("abCache", list); } catch { /* (too big for the browser's storage: this session only) */ } }
      const now = $("ab-account").value ? Number($("ab-account").value) : null;
      if ((now ?? "all") === key && state.contactsOn) renderAddressBook(abFilter(list, $("ab-find").value.trim()));
    })
    .catch((err) => { abFresh.delete(key); if (!rows) showError("Reading the address book", err); });
  if (!rows) await fresh;
}

/** A contact's first and last names (from its parts, else its shown name's first and last words). */
function abNames(r) {
  const words = (r.shown || "").trim().split(/\s+/).filter(Boolean);
  const first = (r.card.given || "").trim() || (words.length > 1 ? words[0] : "");
  const last = (r.card.family || "").trim() || (words.length > 1 ? words[words.length - 1] : words[0] || "");
  return { first, last };
}

/** The order chosen: empty values last, then by the shown name. */
function abSorted(rows) {
  const by = $("ab-sort").value;
  // (leading punctuation left out: "(Cleaner)" sorts under C)
  const bare = (s) => s.replace(/^[^\p{L}\p{N}]+/u, "");
  const key = (r) => {
    const { first, last } = abNames(r);
    const name = (r.shown || "").toLowerCase();
    if (by === "last") return [bare(last.toLowerCase()), bare(first.toLowerCase()), bare(name)];
    if (by === "company") return [bare((r.card.company || "").trim().toLowerCase()), bare(name), ""];
    return [bare(first.toLowerCase()) || bare(name), bare(last.toLowerCase()), bare(name)];
  };
  const keyed = rows.map((r) => [key(r), r]);
  keyed.sort(([a], [b]) => {
    for (let i = 0; i < 3; i++) {
      if (a[i] === b[i]) continue;
      if (!a[i]) return 1;
      if (!b[i]) return -1;
      return a[i].localeCompare(b[i]);
    }
    return 0;
  });
  return keyed.map(([, r]) => r);
}

function renderAddressBook(rows) {
  ab.rows = abSorted(rows);
  $("ab-count").textContent = `${number.format(ab.rows.length)} contacts`;
  $("ab-empty").hidden = ab.rows.length > 0 || !!$("ab-find").value.trim();
  const byLast = $("ab-sort").value === "last";
  $("ab-list").replaceChildren(...ab.rows.slice(0, 3000).map((r) => {
    const row = el("div", `ab-row${r.id === ab.at ? " on" : ""}`);
    row.dataset.id = r.id;
    const c = r.card;
    // the name (last name first when sorted so),
    // the job and company, the first address and phone
    const { first, last } = abNames(r);
    const name = byLast && first && last ? `${last}, ${first}` : r.shown || "(no name)";
    row.append(el("div", "ab-name", name));
    const job = [c.title, c.company].filter(Boolean).join(", ");
    if (job) row.append(el("div", "ab-sub", job));
    const reach = [c.emails[0]?.value, c.phones[0]?.value].filter(Boolean).join(" · ");
    if (reach) row.append(el("div", "ab-sub", reach));
    row.title = [r.shown, job, ...c.emails.map((e) => e.value), ...c.phones.map((p) => `${CARD_KINDS[p.kind] ? `${CARD_KINDS[p.kind]}: ` : ""}${p.value}`), r.account_address && `In ${r.account_address}${r.book ? ` › ${r.book}` : ""}`].filter(Boolean).join("\n");
    row.addEventListener("click", () => showCard(r));
    return row;
  }));
  const current = ab.rows.find((r) => r.id === ab.at) || ab.rows[0];
  if (current) showCard(current); else $("ab-card").replaceChildren(el("p", "p-note", ab.rows.length ? "Choose a contact." : ""));
}

function showCard(r) {
  ab.at = r.id;
  for (const e of $("ab-list").children) e.classList.toggle("on", Number(e.dataset.id) === r.id);
  const c = r.card;
  const box = $("ab-card");
  box.replaceChildren(el("h3", "", r.shown || "(no name)"));
  const what = [c.title, c.department, c.company].filter(Boolean).join(", ");
  if (what) box.append(el("div", "ab-what", what));
  const field = (kind, content) => {
    const line = el("div", "ab-field");
    line.append(el("span", "ab-kind", kind), content);
    box.append(line);
  };
  for (const e of c.emails) {
    const a = el("a", "", e.value);
    a.title = `Write to ${e.value}`;
    a.addEventListener("click", () => invoke("open_compose", { mode: "new", to: r.shown ? `${r.shown} <${e.value}>` : e.value }).catch((err) => showError("Opening a message window", err)));
    field(CARD_KINDS[e.kind] || "Email", a);
  }
  for (const p of c.phones) field(CARD_KINDS[p.kind] || "Phone", el("span", "", p.value));
  for (const a of c.addresses) field(CARD_KINDS[a.kind] || "Address", el("span", "ab-notes", [a.street, [a.city, a.region, a.postal_code].filter(Boolean).join(" "), a.country].filter(Boolean).join("\n")));
  for (const w of c.websites) field("Web", el("span", "", w));
  if (c.nickname) field("Nickname", el("span", "", c.nickname));
  if (c.birthday) field("Birthday", el("span", "", c.birthday));
  if (c.categories.length) field("Categories", el("span", "", c.categories.join(", ")));
  if (c.notes) field("Notes", el("span", "ab-notes", c.notes));
  // where it came from, and how it stands with its server; Edit, Delete
  const where = el("div", "ab-where", `From ${r.account_address} › ${r.book}`);
  box.append(where);
  invoke("card_origin", { id: r.id }).then((o) => {
    if (ab.at !== r.id) return;
    const when = o.updated ? new Date(o.updated * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" }) : "";
    where.textContent = `From ${o.service} · ${o.address} · address book “${o.book}”` +
      (o.waiting ? ` · ${o.waiting}${o.last_error ? ` (last try: ${o.last_error})` : ""}` : o.on_server ? ` · up to date with the server${when ? ` (${when})` : ""}` : "") +
      (o.editable ? "" : " · change it in Google Contacts (changing Google contacts in Eudora comes later)");
    // (Google contacts read-only for now: no Edit or Delete that couldn't reach Google)
    edit.hidden = !o.editable;
    del.hidden = !o.editable;
  }).catch(() => {});
  const tools = el("div", "ab-tools");
  const edit = el("button", "", "Edit");
  edit.addEventListener("click", () => editCard(r));
  // Share: a new message with the contact attached as a vCard, to address and send
  const share = el("button", "", "Share…");
  share.title = "A new message with this contact attached as a vCard (Outlook, Apple, Google and phones read it)";
  share.addEventListener("click", async () => {
    try {
      const draft = await invoke("card_share", { id: r.id });
      await openComposer("draft", draft);
    } catch (err) { showError("Sharing the contact", err); }
  });
  const del = el("button", "danger", "Delete…");
  del.addEventListener("click", () => deleteCard(r));
  tools.append(edit, share, del);
  box.append(tools);
}

const CARD_KIND_CHOICES = {
  emails: [["work", "Work"], ["home", "Home"], ["other", "Other"]],
  phones: [["mobile", "Mobile"], ["work", "Work"], ["home", "Home"], ["fax", "Fax"], ["other", "Other"]],
  addresses: [["work", "Work"], ["home", "Home"], ["other", "Other"]],
};

/** The editor: a contact's every field (`r` null: a new contact). Saved at once here; it goes to
 *  its server through the download's queue. */
function editCard(r) {
  const c = r ? structuredClone(r.card) : { name: "", given: "", middle: "", family: "", prefix: "", suffix: "", nickname: "", company: "", department: "", title: "", emails: [{ kind: "work", value: "" }], phones: [], addresses: [], websites: [], birthday: "", notes: "", categories: [] };
  const box = $("ab-card");
  const form = el("form", "ab-edit");
  form.append(el("h3", "", r ? `Edit ${r.shown || "contact"}` : "New contact"));
  // a new one: which address book
  let accountSel = null;
  if (!r) {
    const books = state.cardCounts || [];
    const accounts = [...new Map(books.map((b) => [b[0], b[1]])).entries()];
    if (!accounts.length) { tell("No address book", "Contacts are added to a Microsoft 365 or Outlook.com address book: none has come in yet."); return; }
    accountSel = el("select");
    for (const [id, address] of accounts) {
      const o = el("option", "", address);
      o.value = String(id);
      accountSel.append(o);
    }
    if ($("ab-account").value) accountSel.value = $("ab-account").value;
    form.append(labeled("Address book", accountSel));
  }
  const text = (label, key, type = "text") => {
    const input = el("input");
    input.type = type;
    input.value = c[key] || "";
    input.addEventListener("input", () => { c[key] = input.value; });
    form.append(labeled(label, input));
  };
  text("First name", "given");
  text("Middle name", "middle");
  text("Last name", "family");
  text("Shown as", "name");
  text("Nickname", "nickname");
  text("Company", "company");
  text("Department", "department");
  text("Job title", "title");
  // lists: emails, phones, postal addresses
  const listOf = (label, key, fields) => {
    const section = el("div", "ab-list-edit");
    const draw = () => {
      section.replaceChildren(el("div", "ab-list-title", label));
      c[key].forEach((item, i) => {
        const row = el("div", "ab-item");
        const kind = el("select");
        for (const [v, t] of CARD_KIND_CHOICES[key]) {
          const o = el("option", "", t);
          o.value = v;
          kind.append(o);
        }
        kind.value = item.kind || "other";
        kind.addEventListener("change", () => { item.kind = kind.value; });
        row.append(kind);
        for (const [f, placeholder] of fields) {
          const input = el(f === "street" ? "textarea" : "input");
          input.placeholder = placeholder;
          input.value = item[f] || "";
          input.addEventListener("input", () => { item[f] = input.value; });
          row.append(input);
        }
        const remove = el("button", "ab-remove", "✕");
        remove.type = "button";
        remove.title = `Remove this ${label.toLowerCase().replace(/s$/, "")}`;
        remove.addEventListener("click", () => { c[key].splice(i, 1); draw(); });
        row.append(remove);
        section.append(row);
      });
      const add = el("button", "", { emails: "Add email", phones: "Add phone", addresses: "Add address" }[key]);
      add.type = "button";
      add.addEventListener("click", () => { c[key].push(Object.fromEntries([["kind", CARD_KIND_CHOICES[key][0][0]], ...fields.map(([f]) => [f, ""])])); draw(); });
      section.append(add);
    };
    draw();
    form.append(section);
  };
  listOf("Emails", "emails", [["value", "name@example.com"]]);
  listOf("Phones", "phones", [["value", "+1 555 0100"]]);
  listOf("Addresses", "addresses", [["street", "Street"], ["city", "City"], ["region", "State"], ["postal_code", "ZIP"], ["country", "Country"]]);
  const web = el("input");
  web.value = c.websites.join(", ");
  web.addEventListener("input", () => { c.websites = web.value.split(/[,\s]+/).filter(Boolean); });
  form.append(labeled("Website", web));
  text("Birthday", "birthday", "date");
  const cats = el("input");
  cats.value = c.categories.join(", ");
  cats.addEventListener("input", () => { c.categories = cats.value.split(",").map((s) => s.trim()).filter(Boolean); });
  form.append(labeled("Categories", cats));
  const notes = el("textarea", "ab-notes-edit");
  notes.value = c.notes || "";
  notes.addEventListener("input", () => { c.notes = notes.value; });
  form.append(labeled("Notes", notes));
  const buttons = el("div", "p-buttons");
  const cancel = el("button", "", "Cancel");
  cancel.type = "button";
  cancel.addEventListener("click", () => (r ? showCard(r) : loadAddressBook()));
  const save = el("button", "primary", "Save");
  save.type = "submit";
  buttons.append(cancel, save);
  form.append(buttons);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    // (empty rows left out)
    c.emails = c.emails.filter((x) => x.value.trim());
    c.phones = c.phones.filter((x) => x.value.trim());
    c.addresses = c.addresses.filter((x) => [x.street, x.city, x.region, x.postal_code, x.country].some((v) => (v || "").trim()));
    if (![c.name, c.given, c.family, c.company].some((v) => v.trim()) && !c.emails.length) { tell("Nothing to save", "Give it a name, a company or an email address."); return; }
    try {
      if (r) {
        await invoke("card_update", { id: r.id, card: c });
        status(`Saved ${c.name || c.given || c.company || "the contact"}: it goes to the server with the next check.`);
        ab.at = r.id;
      } else {
        ab.at = await invoke("card_create", { account: Number(accountSel.value), book: null, card: c });
        status("Added: it's made on the server with the next check.");
      }
      await loadAddressBook(true);
      loadCardCounts();
    } catch (err) { showError("Saving the contact", err); }
  });
  box.replaceChildren(form);
  form.querySelector("input")?.focus();
}

function labeled(text, control) {
  const line = el("label", "ab-field ab-edit-field");
  line.append(el("span", "ab-kind", text), control);
  return line;
}

/** Delete, after asking (the server keeps it in Deleted Items; Eudora keeps it in its history). */
async function deleteCard(r) {
  const ok = await askYesNo(
    `Delete ${r.shown || "this contact"}?`,
    `It's deleted here now and from ${r.account_address}'s address book on the server with the next check (Exchange keeps it in Deleted Items for a while). Eudora keeps a copy of it as it was.`,
    "Delete contact",
  );
  if (!ok) return;
  try {
    await invoke("card_delete", { id: r.id });
    if (ab.at === r.id) ab.at = null;
    status(`Deleted ${r.shown || "the contact"}.`);
    await loadAddressBook(true);
    loadCardCounts();
  } catch (err) { showError("Deleting the contact", err); }
}

async function exportAddressBook(ext) {
  let chosen;
  try { chosen = await invoke("pick_paths", { what: "folder", title: "Export the address book into this folder", kind: null }); } catch (err) { showError("Choosing a folder", err); return; }
  if (!chosen?.length) return;
  const sep = chosen[0].includes("\\") ? "\\" : "/";
  const path = `${chosen[0].replace(/[\\/]+$/, "")}${sep}Eudora contacts.${ext}`;
  const account = $("ab-account").value ? Number($("ab-account").value) : null;
  try {
    const n = await invoke("cards_export", { path, account });
    status(`Exported ${number.format(n)} contacts to ${path}.`);
  } catch (err) { showError("Exporting the address book", err); }
}

$("ab-find").addEventListener("input", () => { clearTimeout(ab.timer); ab.timer = setTimeout(loadAddressBook, 200); });
$("ab-account").addEventListener("change", loadAddressBook);
// the order kept for next time
try { $("ab-sort").value = store.get("abSort", "first") || "first"; } catch { /* (the default) */ }
$("ab-sort").addEventListener("change", () => { store.set("abSort", $("ab-sort").value); loadAddressBook(); });
$("ab-export-vcf").addEventListener("click", () => exportAddressBook("vcf"));
$("ab-export-csv").addEventListener("click", () => exportAddressBook("csv"));
$("ab-new").addEventListener("click", () => editCard(null));

/** Why in these Views?: every View's points for the message, each reason. */
async function viewsWhy() {
  const id = state.messageId ?? (await selectedItems())[0]?.id;
  if (id == null) { status("Choose a message first."); return; }
  let scores;
  try { scores = await invoke("view_why", { id }); } catch (err) { showError("Reading the Views' points", err); return; }
  $("vwhy-list").replaceChildren(...(scores.length ? scores.map((s) => {
    const box = el("div", `vwhy-view${s.hit ? " in" : ""}`);
    box.append(el("div", "vwhy-name", `${s.name}: ${s.points.toFixed(1).replace(/\.0$/, "")} points, ${s.hit ? "in this View" : "not enough to be in it"}`));
    const reasons = el("ul", "vwhy-reasons");
    reasons.append(...s.reasons.map((r) => el("li", "", r)));
    box.append(reasons);
    return box;
  }) : [el("p", "p-note", "No View sees anything in this message.")]));
  $("vwhy-box").showModal();
}
$("vwhy-close").addEventListener("click", () => $("vwhy-box").close());

// Subscriptions: every legitimate list, notification and marketing sender, and unsubscribing
const subs = { list: [] };

async function openSubscriptions() {
  $("subs-list").replaceChildren(el("p", "p-note", "Reading the last 12 months of mail with an unsubscribe link…"));
  $("subs-count").textContent = "";
  if (!$("subs-box").open) $("subs-box").showModal();
  try {
    subs.list = await invoke("subscriptions");
  } catch (err) {
    $("subs-list").replaceChildren(el("p", "p-note", `Couldn't read them: ${err}`));
    return;
  }
  renderSubscriptions();
}

function renderSubscriptions() {
  const find = $("subs-find").value.trim().toLowerCase();
  const kind = $("subs-kind").value;
  const left = $("subs-left").checked;
  const rows = subs.list.filter((s) => (!kind || s.kind === kind)
    && (left || !s.unsubscribed)
    && (!find || `${s.name} ${s.address} ${s.list_id ?? ""}`.toLowerCase().includes(find)));
  const total = rows.reduce((n, s) => n + s.year, 0);
  $("subs-count").textContent = `${number.format(rows.length)} senders, ${number.format(total)} messages in 12 months`;
  const table = el("table", "subs-table");
  const head = el("tr");
  for (const h of ["From", "Kind", "Mailbox", "30 days", "12 months", "Last", ""]) head.append(el("th", "", h));
  table.append(head);
  for (const s of rows.slice(0, 1000)) {
    const tr = el("tr", s.unsubscribed ? "subs-left" : "");
    const who = el("td", "subs-who");
    who.append(el("div", "subs-name", s.name || s.address), el("div", "p-note", s.list_id ? `${s.address} · list ${s.list_id}` : s.address));
    const last = new Date(s.last * 1000).toLocaleDateString();
    tr.append(who, el("td", "", s.kind), el("td", "", s.mailbox), el("td", "num", number.format(s.month)), el("td", "num", number.format(s.year)), el("td", "", last));
    const act = el("td", "subs-act");
    if (s.unsubscribed) {
      act.append(el("div", "p-note", `Left on ${new Date(s.unsubscribed * 1000).toLocaleDateString()}${s.since_unsubscribed ? `; ${number.format(s.since_unsubscribed)} since` : ""}`));
    }
    const show = el("button", "", "Show");
    show.title = "Show its mail";
    show.addEventListener("click", () => { $("subs-box").close(); $("query").value = s.list_id ? `list-id:"${s.list_id}"` : `from:${s.address}`; runSearch(); });
    act.append(show);
    if (s.way) {
      const how = { "one-click": "Eudora unsubscribes for you (one click)", page: "Opens its unsubscribe page in your browser", email: `Writes an unsubscribe message to ${s.way.address}` }[s.way.how];
      const go = el("button", s.unsubscribed ? "" : "primary", s.unsubscribed ? "Again" : "Unsubscribe");
      go.title = how;
      go.addEventListener("click", () => unsubscribeFrom(s, go));
      act.append(go);
    } else {
      act.append(el("span", "p-note", "no unsubscribe link"));
    }
    tr.append(act);
    table.append(tr);
  }
  $("subs-list").replaceChildren(rows.length ? table : el("p", "p-note", "None."));
  if (rows.length > 1000) $("subs-list").append(el("p", "p-note", `The first 1,000 of ${number.format(rows.length)} are listed: find a sender to narrow it.`));
}

async function unsubscribeFrom(s, button) {
  const how = { "one-click": "Eudora will tell the sender's server to stop (one click, as the sender offers).", page: "Eudora opens the sender's unsubscribe page in your browser: finish there.", email: `Eudora opens a message to ${s.way.address} asking to leave: press Send.` }[s.way.how];
  const ok = await askYesNo(`Unsubscribe from ${s.name || s.address}?`, `${how}\n\n${number.format(s.year)} messages in the last 12 months, ${number.format(s.month)} in the last 30 days.`, "Unsubscribe");
  if (!ok) return;
  button.disabled = true;
  try {
    const r = await invoke("unsubscribe", { key: s.key, newest: s.newest });
    if (r.done === "unsubscribed") {
      status(r.status < 400 ? `Unsubscribed from ${s.name || s.address}.` : `${s.name || s.address} answered ${r.status}: it may not have worked; try its page (Show, then the link in a message).`);
    } else if (r.done === "open-page") {
      await invoke("open_link", { url: r.url });
      status(`Opened ${s.name || s.address}'s unsubscribe page in your browser.`);
    } else if (r.done === "write-to") {
      await invoke("open_compose", { mode: "new", to: r.address, subject: r.subject });
    }
    s.unsubscribed = Math.floor(Date.now() / 1000);
    s.since_unsubscribed = 0;
    renderSubscriptions();
  } catch (err) {
    showError("Unsubscribing", err);
  } finally {
    button.disabled = false;
  }
}
$("subs-find").addEventListener("input", renderSubscriptions);
$("subs-kind").addEventListener("change", renderSubscriptions);
$("subs-left").addEventListener("change", renderSubscriptions);
$("subs-close").addEventListener("click", () => $("subs-box").close());
$("list-subs").addEventListener("click", openSubscriptions);

const ACTIONS = {
  "send-later": () => activeComposer()?.contentWindow.composeAction?.("send-later"),
  "status-unread": () => changeStatus("unread"),
  "status-read": () => changeStatus("read"),
  "status-replied": () => changeStatus("replied"),
  "status-forwarded": () => changeStatus("forwarded"),
  "status-redirected": () => changeStatus("redirected"),
  "priority-1": () => changePriority(1),
  "priority-2": () => changePriority(2),
  "priority-3": () => changePriority(3),
  "priority-4": () => changePriority(4),
  "priority-5": () => changePriority(5),
  "priority-0": () => changePriority(0),
  "priority-raise": () => changePriority(0, -1),
  "priority-lower": () => changePriority(0, 1),
  "change-label": openLabelPicker,
  "flag-todo": flagAsTodo,
  "add-trip": addToTrips,
  "empty-trash": openEmptyTrash,
  "find-text": () => findText(false),
  "find-again": () => findText(true),
  "check-spelling": checkSpellingAction,
  print: printMessage,
  signatures: settingsAt("mail", openSignatures),
  plugins: settingsAt("programs", openPlugins),
  "mailbox-in": () => openRole("in"),
  "mailbox-junk": () => openRole("junk"),
  "mailbox-trash": () => openRole("trash"),
  previous: () => stepMessage(-1),
  next: () => stepMessage(1),
  "show-headers": toggleHeaders,
  "open-file": openFile,
  import: () => openImport(),
  optimize: openOptimize,
  "forget-sign-ins": forgetSignIns,
  "work-offline": toggleOffline,
  activity: openActivity,
  new: () => compose("new"),
  reply: () => compose("reply"),
  "reply-all": () => compose("replyAll"),
  forward: () => compose("forward"),
  redirect: () => compose("redirect"),
  "send-again": () => compose("again"),
  transfer,
  "copy-to": copyTo,
  "new-mailbox": () => newMailbox(false),
  "transfer-new": () => newMailbox(true),
  delete: deleteSelected,
  "to-trash": deleteSelected,
  filters: settingsAt("mail", () => openFilters()),
  "make-filter": makeFilter,
  "filter-messages": filterMessages,
  "sort-selected": sortSelected,
  "sort-why": sortWhy,
  "views-why": viewsWhy,
  subscriptions: openSubscriptions,
  "address-book": openAddressBook,
  "spam-rules": settingsAt("mail", () => openSpamRules()),
  "mark-spam": () => markSpam(true),
  "mark-not-spam": () => markSpam(false),
  undo,
  "select-all": selectAll,
  source: viewSource,
  drafts: showDrafts,
  "all-mail": () => openView("all"),
  "open-mailbox": openMailbox,
  find: () => { $("query").focus(); $("query").select(); },
  check: checkMail,
  backup: openBackup,
  stats: () => openStats(),
  outbox: openOutbox,
  "send-queued": sendQueued,
  "old-attachments": openOldAttachments,
  restore: () => openRestore(),
  "hide-sent": toggleHideSent,
  columns: () => { const r = $("list-head").getBoundingClientRect(); showColumnMenu(r.left + 20, r.bottom, null); },
  "pause-old": () => setPause("old"),
  "pause-all": () => setPause("all"),
  resume: () => setPause("none"),
  options: openPrefs,
  personalities: settingsAt("accounts"),
  accounts: settingsAt("accounts"),
  manual: () => openManual(),
  "go-mail": () => showMail(),
  "go-calendar": () => openCalendar(),
  "go-people": () => openAddressBook(),
  "go-todo": () => openTodo(),
  "new-appointment": async () => { await openCalendar(); openNewAppointment(); },
  "settings-labels": settingsAt("mail"),
  "settings-mail": settingsAt("mail"),
  "add-account": () => openAccount(),
  mailboxes: toggleMailboxes,
  about: () => openAbout(),
  "check-updates": () => checkUpdates(true),
};

for (const b of document.querySelectorAll(".actions [data-act]")) b.addEventListener("click", () => ACTIONS[b.dataset.act]?.());

// ------------------------------------------------------------------ the spam filter

const CATEGORY_NAMES = {
  blocked: "Blacklisted", rule: "Your rules", spam: "Spam", priority: "High priority", people: "People",
  list: "Mailing lists", bulk: "Marketing", notification: "Notifications", other: "Other",
};
const sorting = { proposals: [], keep: new Set(), open: null, rules: new Map() };

/** The selection as (message, folder) pairs; nothing selected = the whole list shown. */
async function chosenItems() {
  if (!state.list) { status("Open a mailbox or search first."); return []; }
  if (selectionCount() === 0) {
    if (state.list.kind === "search") return [...state.list.rows.values()].map((r) => ({ id: r.id, from: r.labels?.[0]?.[0] })).filter((i) => i.from != null);
    const all = await invoke("list_ids", listArgs(state.list));
    const own = state.list.kind === "folder" ? state.list.folders[0] : null;
    return all.map(([id, folder]) => ({ id, from: own ?? folder }));
  }
  return (await selectedItems()).filter((i) => i.from != null);
}

/** Sort with the Spam Filter: decides for the selection (or the list), shows where each
 *  would go and why; nothing moves until Move. */
async function sortSelected() {
  const items = await chosenItems();
  if (!items.length) { status("Choose messages first."); return; }
  // a big run asks first, with how many, where and how long; about 330 messages a second on
  // half the processor
  if (items.length > 10_000) {
    const folders = new Set(items.map((i) => i.from)).size;
    const minutes = Math.max(1, Math.round(items.length / 330 / 60));
    const took = minutes >= 90 ? `about ${Math.round(minutes / 60)} hours` : `about ${minutes} minutes`;
    const ok = await askYesNo(
      "Run the spam filter on all of these?",
      `Run the spam filter on ${number.format(items.length)} messages in ${number.format(folders)} mailbox${folders === 1 ? "" : "es"}? Deciding takes ${took}; nothing moves until you've looked at the result and chosen Move. You can stop it from the status bar.`,
      `Sort ${number.format(items.length)} messages`,
      "Don't",
    );
    if (!ok) return;
  }
  status(`Deciding where ${number.format(items.length)} messages go… (Stop is on the status bar)`);
  setTimeout(showBackgroundWork, 1000);
  try {
    const [preview, rules] = await Promise.all([invoke("sort_preview", { items: items.map((i) => [i.id, i.from]) }), invoke("spam_rules")]);
    sorting.rules = new Map(rules.map((r) => [r.key, r.name]));
    showSortBox(preview);
    status("");
  } catch (err) {
    if (String(err).startsWith("Stopped")) status(String(err));
    else showError("Sorting", err);
  }
  showBackgroundWork();
}

function ruleLabel(key) {
  return sorting.rules.get(key) || key;
}

function showSortBox(preview, single = false) {
  sorting.proposals = preview.proposals;
  sorting.keep = new Set(preview.proposals.filter((p) => p.destination).map((p) => p.id));
  sorting.open = single ? preview.proposals[0]?.id : null;
  const counts = Object.entries(preview.counts).sort((a, b) => b[1] - a[1]).map(([c, n]) => `${number.format(n)} ${CATEGORY_NAMES[c] || c}`);
  $("sort-title").textContent = single ? "Why the Spam Filter Decides This" : "Sort with the Spam Filter";
  $("sort-summary").textContent = `${number.format(preview.looked_at)} messages: ${counts.join(", ")}.`;
  options($("sort-show"), [["moving", "Messages that would move"], ["all", "All"], ...Object.keys(preview.counts).map((c) => [c, CATEGORY_NAMES[c] || c])], single ? "all" : "moving");
  for (const id of ["sort-check-all", "sort-check-none", "sort-go"]) $(id).hidden = single;
  renderSortList();
  if (!$("sort-box").open) $("sort-box").showModal();
}

function renderSortList() {
  const show = $("sort-show").value;
  const rows = sorting.proposals.filter((p) => show === "all" || (show === "moving" ? p.destination : p.decision.category === show));
  const shown = rows.slice(0, 2000);
  $("sort-list").replaceChildren(...shown.map((p) => {
    const row = el("div", `sort-row${sorting.open === p.id ? " open" : ""}`);
    const tick = el("input");
    tick.type = "checkbox";
    tick.checked = sorting.keep.has(p.id);
    tick.disabled = !p.destination;
    tick.addEventListener("change", () => { if (tick.checked) sorting.keep.add(p.id); else sorting.keep.delete(p.id); updateSortButton(); });
    const top = p.decision.symbols[0];
    const line = el("div", "sort-line");
    line.append(
      el("span", "sort-from", p.from),
      el("span", "sort-subject", p.subject || "(no subject)"),
      el("span", `sort-where cat-${p.decision.category}`, `${p.destination ? `→ ${p.destination}` : "stays"}${p.decision.priority ? " · high priority" : ""}`),
      el("span", "sort-why", top ? `${ruleLabel(top.rule)}: ${top.why}` : "no rule applies"),
    );
    line.addEventListener("click", () => { sorting.open = sorting.open === p.id ? null : p.id; renderSortList(); });
    row.append(tick, line);
    if (sorting.open === p.id) {
      const detail = el("div", "sort-detail");
      detail.append(el("div", "", `Spam score ${p.decision.spam_score.toFixed(1)} (5 or more = spam). Every rule it hit, first the one that decided:`));
      for (const s of p.decision.symbols) {
        const r = el("div", "sort-sym");
        const name = el("button", "sort-rule", ruleLabel(s.rule));
        name.title = "Open this rule: switch it off, or give it an exception";
        name.addEventListener("click", (e) => { e.stopPropagation(); openSpamRules(s.rule); });
        r.append(name, el("span", "sort-score", s.score ? (s.score > 0 ? `+${s.score}` : `${s.score}`) : ""), el("span", "", s.why));
        detail.append(r);
      }
      row.append(detail);
    }
    return row;
  }));
  if (rows.length > shown.length) $("sort-list").append(el("div", "p-note", `…and ${number.format(rows.length - shown.length)} more (all of them are moved when ticked).`));
  updateSortButton();
}

function updateSortButton() {
  $("sort-go").textContent = `Move ${number.format(sorting.keep.size)} ${sorting.keep.size === 1 ? "message" : "messages"}`;
  $("sort-go").disabled = sorting.keep.size === 0;
}

$("sort-show").addEventListener("change", renderSortList);
$("sort-check-all").addEventListener("click", () => { for (const p of sorting.proposals) if (p.destination) sorting.keep.add(p.id); renderSortList(); });
$("sort-check-none").addEventListener("click", () => { sorting.keep.clear(); renderSortList(); });
$("sort-cancel").addEventListener("click", () => $("sort-box").close());
$("sort-rules").addEventListener("click", () => openSpamRules());
$("sort-go").addEventListener("click", async () => {
  const chosen = sorting.proposals.filter((p) => sorting.keep.has(p.id)).map((p) => ({
    id: p.id, folder: p.folder, category: p.decision.category, priority: p.decision.priority,
    action: p.decision.action, sender_key: p.decision.sender_key, reasons: p.decision.symbols.map((s) => s.why),
  }));
  // the window closes at once and the moves run in the background
  $("sort-box").close();
  status(`Sorting ${number.format(chosen.length)} ${chosen.length === 1 ? "message" : "messages"} in the background…`);
  try {
    const moved = await invoke("sort_apply", { chosen });
    status(`Sorted: moved ${number.format(moved)} ${moved === 1 ? "message" : "messages"}. Edit ▸ Undo isn't available for sorting yet: use Transfer to put one back.`);
    await reloadList();
  } catch (err) {
    showError("Moving the sorted messages", err);
  }
});

/** Why? for the message shown (or the first selected): every rule it hits. */
async function sortWhy() {
  const id = state.messageId ?? (await selectedItems())[0]?.id;
  if (id == null) { status("Choose a message first."); return; }
  try {
    const [p, rules] = await Promise.all([invoke("sort_why", { id }), invoke("spam_rules")]);
    sorting.rules = new Map(rules.map((r) => [r.key, r.name]));
    showSortBox({ proposals: [p], counts: { [p.decision.category]: 1 }, looked_at: 1 }, true);
  } catch (err) {
    showError("Asking the spam filter", err);
  }
}

/** Spam / Not Spam: the sender goes on the blacklist (and mail like it) / the whitelist, and
 *  the messages move to Junk Email / back to the Inbox. */
async function markSpam(spam) {
  const items = (await selectedItems()).filter((i) => i.from != null);
  if (!items.length) { status("Choose a message first."); return; }
  if (!(await confirmMany(items.length, spam ? "Mark as spam" : "Mark as not spam"))) return;
  try {
    const [notes, moved] = await invoke("mark_spam", { items: items.map((i) => [i.id, i.from]), spam });
    status(`${spam ? "Spam" : "Not spam"}: ${notes.slice(0, 3).join("; ")}${notes.length > 3 ? ` and ${notes.length - 3} more` : ""}. ${number.format(moved)} moved.`);
    await reloadList();
  } catch (err) {
    showError(spam ? "Marking as spam" : "Marking as not spam", err);
  }
}

// the Mail Filter Rules window
const spamRules = { list: [], mailboxes: [], at: null, adding: null, like: null, timer: 0 };

async function openSpamRules(key = null) {
  try {
    spamRules.list = await invoke("spam_rules");
    spamRules.mailboxes = await invoke("spam_mailboxes");
  } catch (err) {
    showError("Reading the mail filter rules", err);
    return;
  }
  spamRules.at = key && spamRules.list.some((r) => r.key === key) ? key : spamRules.at ?? spamRules.list[0]?.key;
  spamRules.adding = null;
  renderSpamMailboxes();
  renderSpamLanguages();
  renderSpamState();
  renderSpamList();
  renderSpamEditor();
  if (!$("spam-rules").open) $("spam-rules").showModal();
}

/** The switch, the learning and the Spamhaus key (Mail Filter Rules' top). */
async function renderSpamState() {
  let s;
  try { s = await invoke("spam_filter_state"); } catch { return; }
  $("sr-arrival").checked = s.on_arrival;
  $("sr-learned").textContent = s.learned.length
    ? `Learned: ${s.learned.map(([who, spam, good]) => `${who} ${number.format(spam)} spam / ${number.format(good)} good${spam < 50 || good < 50 ? " (not enough yet)" : ""}`).join(" · ")}`
    : "Learning: nothing yet (Learn now).";
  $("sr-spamhaus-state").textContent = s.spamhaus ? "A key is kept on this computer: new mail is checked." : "No key: Spamhaus isn't asked.";
}

$("sr-arrival").addEventListener("change", async () => {
  const on = $("sr-arrival").checked;
  if (on && !(await askYesNo("Sort arriving mail", "Sort mail arriving from now on with the spam filter, in the mailboxes ticked below? (Old mail isn't touched.)", "Sort it"))) { $("sr-arrival").checked = false; return; }
  try { await invoke("spam_filter_arrival_set", { on }); status(on ? "Arriving mail is now sorted by the spam filter." : "Arriving mail is no longer sorted by the spam filter."); } catch (err) { showError("The spam filter switch", err); }
  renderSpamState();
});
$("sr-learn").addEventListener("click", async () => {
  try { await invoke("spam_filter_learn"); } catch (err) { showError("Learning", err); return; }
  const timer = setInterval(async () => {
    const j = await invoke("job_state").catch(() => null);
    if (!j) return;
    $("sr-learn-line").textContent = j.running ? j.line : j.result;
    if (!j.running) { clearInterval(timer); renderSpamState(); }
  }, 700);
});
$("sr-spamhaus-save").addEventListener("click", async () => {
  try { await invoke("spamhaus_key_set", { key: $("sr-spamhaus").value }); $("sr-spamhaus").value = ""; renderSpamState(); } catch (err) { showError("The Spamhaus key", err); }
});

/** The languages the user reads: mail written in any other goes to Spam ▸ Other Languages. */
async function renderSpamLanguages() {
  let chosen, all;
  try { [chosen, all] = await invoke("spam_languages"); } catch { return; }
  spamRules.languages = { chosen, all };
  $("sr-languages").textContent = chosen.map((c) => all.find(([code]) => code === c)?.[1] || c).join(", ");
}

$("sr-languages-change").addEventListener("click", async () => {
  const { chosen = [], all = [] } = spamRules.languages || {};
  const grid = el("div", "sr-language-grid");
  for (const [code, name] of all) {
    const label = el("label");
    const box = el("input");
    box.type = "checkbox";
    box.value = code;
    box.checked = chosen.includes(code);
    label.append(box, el("span", "", ` ${name}`));
    grid.append(label);
  }
  const answer = await ask({
    title: "Languages you read",
    text: "Mail written in any other language goes to Spam ▸ Other Languages. Mail from people you write to is never sorted this way.\n\nThe words in the message decide, not where it comes from or how it's encoded: English written by anyone stays in your Inbox.",
    content: grid,
    buttons: [{ label: "Cancel", value: null }, { label: "Save", value: "save", primary: true }],
  });
  if (answer !== "save") return;
  const codes = [...grid.querySelectorAll("input:checked")].map((b) => b.value);
  if (!codes.length) { await tell("Languages you read", "Choose at least one language."); return; }
  try {
    await invoke("spam_languages_set", { codes });
    status("Your languages are saved: the spam filter uses them from now on.");
  } catch (err) {
    showError("Saving your languages", err);
  }
  renderSpamLanguages();
});

/** The mailboxes the spam filter works in. */
function renderSpamMailboxes() {
  $("sr-mailboxes").replaceChildren(...spamRules.mailboxes.map((m) => {
    const label = el("label", "sr-mailbox");
    const box = el("input");
    box.type = "checkbox";
    box.checked = m.on;
    box.addEventListener("change", async () => {
      m.on = box.checked;
      try {
        await invoke("spam_mailboxes_set", { addresses: spamRules.mailboxes.filter((x) => x.on).map((x) => x.address) });
        status(`The spam filter ${m.on ? "now sorts" : "no longer sorts"} arriving mail in ${m.shown}.`);
      } catch (err) {
        showError("Choosing the mailboxes", err);
      }
    });
    label.append(box, el("span", "", m.shown));
    label.title = `${m.address} · ${m.kind_name}`;
    return label;
  }));
}

const KIND_NAMES = { block: "blacklist", allow: "whitelist", like: "your rule", "built-in": "built in", language: "built in", phrases: "priority" };

function renderSpamList() {
  const find = $("sr-filter").value.trim().toLowerCase();
  $("sr-list").replaceChildren(...spamRules.list.filter((r) => !find || `${r.name} ${r.description}`.toLowerCase().includes(find)).map((r) => {
    const row = el("label", `f-item${r.key === spamRules.at ? " on" : ""}${r.settings.off ? " off" : ""}`);
    const on = el("input");
    on.type = "checkbox";
    on.checked = !r.settings.off;
    on.title = "Use this rule";
    on.addEventListener("change", () => { r.settings.off = !on.checked; saveSpamRule(r, true); renderSpamList(); renderSpamEditor(); });
    const n = r.settings.exceptions.length + r.settings.inclusions.length;
    const only = (r.settings.only_in || []).length;
    row.append(on, el("span", "", r.name), el("span", "sr-kind", `${KIND_NAMES[r.kind] || r.kind}${n ? ` · ${n} ±` : ""}${only ? ` · ${only} mailbox${only > 1 ? "es" : ""}` : ""}`));
    row.title = r.description;
    row.addEventListener("click", (e) => { if (e.target !== on) { spamRules.at = r.key; spamRules.adding = null; renderSpamList(); renderSpamEditor(); } });
    return row;
  }));
  $("sr-list").querySelector(".on")?.scrollIntoView({ block: "nearest" });
}

function renderSpamEditor() {
  const adding = spamRules.adding;
  for (const id of ["sr-on", "sr-add-exception", "sr-add-inclusion", "sr-remove", "sr-exceptions", "sr-inclusions", "sr-only-in"]) $(id).closest("label, button, div").hidden = !!adding;
  for (const h of $("sr-edit").querySelectorAll("h3:not(#sr-name)")) h.hidden = !!adding;
  $("sr-new").hidden = !adding;
  if (adding) { $("sr-edit").hidden = false; renderSpamAdding(); return; }
  const r = spamRules.list.find((x) => x.key === spamRules.at);
  $("sr-edit").hidden = !r;
  if (!r) return;
  $("sr-name").textContent = r.name;
  $("sr-desc").textContent = r.description;
  $("sr-on").checked = !r.settings.off;
  const again = () => renderSpamEditor();
  const changed = () => saveSpamRule(r);
  r.settings.only_in = r.settings.only_in || [];
  $("sr-only-in").replaceChildren(...spamRules.mailboxes.map((m) => {
    const label = el("label", "sr-mailbox");
    const box = el("input");
    box.type = "checkbox";
    box.checked = r.settings.only_in.includes(m.address);
    box.addEventListener("change", () => {
      r.settings.only_in = spamRules.mailboxes.filter((x) => x.address === m.address ? box.checked : r.settings.only_in.includes(x.address)).map((x) => x.address);
      saveSpamRule(r, true);
      renderSpamList();
    });
    label.append(box, el("span", "", m.shown));
    label.title = m.address;
    return label;
  }));
  $("sr-exceptions").replaceChildren(...testRows(r.settings.exceptions, again, false, changed));
  $("sr-inclusions").replaceChildren(...testRows(r.settings.inclusions, again, false, changed));
  $("sr-remove").hidden = !["block", "allow", "like"].includes(r.kind);
}

/** Saves a rule's switch, exceptions and inclusions (typing waits half a second). */
function saveSpamRule(r, now = false) {
  clearTimeout(spamRules.timer);
  const go = async () => {
    try {
      await invoke("spam_rule_set", { key: r.key, settings: r.settings });
    } catch (err) {
      showError("Saving the rule", err);
    }
  };
  if (now) go(); else spamRules.timer = setTimeout(go, 500);
}

$("sr-on").addEventListener("change", () => {
  const r = spamRules.list.find((x) => x.key === spamRules.at);
  if (!r) return;
  r.settings.off = !$("sr-on").checked;
  saveSpamRule(r, true);
  renderSpamList();
});
$("sr-filter").addEventListener("input", renderSpamList);
const newTest = () => ({ field: "From:", verb: "contains", value: "", unless: false });
$("sr-add-exception").addEventListener("click", () => { spamRules.list.find((x) => x.key === spamRules.at)?.settings.exceptions.push(newTest()); renderSpamEditor(); });
$("sr-add-inclusion").addEventListener("click", () => { spamRules.list.find((x) => x.key === spamRules.at)?.settings.inclusions.push(newTest()); renderSpamEditor(); });
$("sr-remove").addEventListener("click", async () => {
  const r = spamRules.list.find((x) => x.key === spamRules.at);
  if (!r || !(await askYesNo("Remove rule", `Remove “${r.name}”?`, "Remove it"))) return;
  try {
    await invoke("spam_rule_remove", { key: r.key });
    spamRules.at = null;
    await openSpamRules();
  } catch (err) {
    showError("Removing the rule", err);
  }
});
$("sr-close").addEventListener("click", () => $("spam-rules").close());

const SPAM_ACTIONS = [
  ["move:Ignored by rule", "Move to “Ignored by rule”"], ["spam", "Move to Junk Email"], ["trash", "Move to Trash"],
  ["move:Bulk", "Move to Bulk"], ["move:Lists", "Move to Lists"], ["pick", "Move to a mailbox I choose…"],
];

function actionFrom(value, folder) {
  if (value === "spam") return { do: "spam" };
  if (value === "trash") return { do: "trash" };
  if (value === "pick") return { do: "move", folder };
  return { do: "move", folder: value.slice(5) };
}

for (const [button, kind] of [["sr-add-block", "block"], ["sr-add-allow", "allow"]]) {
  $(button).addEventListener("click", () => {
    spamRules.adding = { kind };
    $("sr-new-pattern").value = "";
    renderSpamEditor();
    $("sr-new-pattern").focus();
  });
}
$("sr-add-like").addEventListener("click", async () => {
  const id = state.messageId ?? (await selectedItems())[0]?.id;
  if (id == null) { tell("No message selected", "Select a message in the list first: the rule is made from it."); return; }
  $("sr-like-preview").textContent = "Looking for messages like it in the last year…";
  spamRules.adding = { kind: "like" };
  renderSpamEditor();
  try {
    const [keys, what, count, sample] = await invoke("like_preview", { id });
    spamRules.adding.keys = keys;
    $("sr-like-preview").replaceChildren(
      el("div", "", `Looks at ${what}. It catches ${number.format(count)} messages from the last year; the newest:`),
      ...sample.map(([, date, from, subject]) => el("div", "sr-sample", `${new Date(date * 1000).toLocaleDateString()} · ${from} · ${subject}`)),
    );
    $("sr-new-name").value = `Like “${sample[0]?.[3]?.slice(0, 40) || what}”`;
  } catch (err) {
    $("sr-like-preview").textContent = `Couldn't make a rule from it: ${err}`;
  }
});

function renderSpamAdding() {
  const kind = spamRules.adding.kind;
  $("sr-name").textContent = { block: "New blacklist entry", allow: "New whitelist entry", like: "New rule: messages like the selected one" }[kind];
  $("sr-desc").textContent = { block: "Mail from this address, or from anyone @domain, is handled as you choose.", allow: "Mail from this address, or anyone @domain, is never spam and stays in the Inbox.", like: "Catches the same mailing even when its From address changes (by mailing list, signer, domain or subject)." }[kind];
  $("sr-new-pattern").closest("label").hidden = kind === "like";
  $("sr-new-action-row").hidden = kind === "allow";
  $("sr-new-name-row").hidden = kind !== "like";
  $("sr-like-preview").hidden = kind !== "like";
  options($("sr-new-action"), SPAM_ACTIONS, kind === "like" ? "spam" : "move:Ignored by rule");
  // every mailbox, or one only (each mailbox keeps its own lists)
  options($("sr-new-mailbox"), [["", "Every mailbox"], ...(spamRules.mailboxes || []).map((m) => [m.address, m.shown])], "");
}

$("sr-new-save").addEventListener("click", async () => {
  const adding = spamRules.adding;
  if (!adding) return;
  let action = null;
  if (adding.kind !== "allow") {
    let folder = null;
    if ($("sr-new-action").value === "pick") {
      const f = await pickFolder("Move To");
      if (!f) return;
      folder = f.path;
    }
    action = actionFrom($("sr-new-action").value, folder);
  }
  try {
    if (adding.kind === "like") {
      if (!adding.keys) return;
      const id = await invoke("like_add", { name: $("sr-new-name").value || "Like this", keys: adding.keys, action, mailbox: $("sr-new-mailbox").value });
      spamRules.at = `like:${id}`;
    } else {
      const pattern = $("sr-new-pattern").value.trim();
      if (!pattern) { $("sr-new-pattern").focus(); return; }
      await invoke("spam_entry_add", { pattern, kind: adding.kind, action, note: "added by you", mailbox: $("sr-new-mailbox").value });
      spamRules.at = `${adding.kind}:${pattern.toLowerCase()}`;
    }
    spamRules.adding = null;
    await openSpamRules(spamRules.at);
  } catch (err) {
    tell("Not added", String(err));
  }
});

// the right-click menu on a message
const rowMenu = $("row-menu");
$("list").addEventListener("contextmenu", (e) => {
  const row = e.target.closest(".row");
  if (!row) return;
  e.preventDefault();
  if (!isSelected(Number(row.dataset.index))) select(Number(row.dataset.index), false);
  fillPluginMenu();
  rowMenu.hidden = false;
  const { innerWidth, innerHeight } = window;
  rowMenu.style.left = `${Math.min(e.clientX, innerWidth - rowMenu.offsetWidth - 8)}px`;
  rowMenu.style.top = `${Math.min(e.clientY, innerHeight - rowMenu.offsetHeight - 8)}px`;
});
rowMenu.addEventListener("click", (e) => {
  const b = e.target.closest("[data-act]");
  rowMenu.hidden = true;
  if (b) ACTIONS[b.dataset.act]?.();
});
document.addEventListener("click", (e) => { if (!rowMenu.contains(e.target)) rowMenu.hidden = true; });

$("list").addEventListener("keydown", (e) => {
  if (e.key === "Delete") { e.preventDefault(); deleteSelected(); }
  if (e.key === "Insert") { e.preventDefault(); flagAsTodo(); } // (Outlook's key for a flag)
});

// menu bar commands meant for this window (a message inside it gets its own: Send, Save
// Draft, Attach File, Close)
let lastMenu = { id: "", at: 0 };
window.__TAURI__.webviewWindow.getCurrentWebviewWindow().listen("menu", (e) => {
  // the same command twice within half a second is one click
  const now = Date.now();
  if (e.payload === lastMenu.id && now - lastMenu.at < 500) return;
  lastMenu = { id: e.payload, at: now };
  const writing = activeComposer();
  if (writing && ["send", "send-later", "save", "attach", "close"].includes(e.payload)) {
    writing.contentWindow.composeAction?.(e.payload);
    return;
  }
  // a message window: one of its own takes its menu commands; one inside Eudora gets them from
  // the main page (it hears the main window's too, and mustn't act on them twice)
  if (READER) { if (!READER_FRAME) window.readerAction(e.payload); return; }
  const reading = activeReader();
  if (reading) { reading.contentWindow.readerAction?.(e.payload); return; }
  if (e.payload === "close") return; // (Close never closes the main window)
  ACTIONS[e.payload]?.();
});

// ------------------------------------------------------------------ start

async function start() {
  fillIcons();
  setupSplitters();
  // every dialog closes from a × in its top-right corner (= Cancel / Close), and Esc
  for (const d of document.querySelectorAll("dialog")) {
    const x = el("button", "dlg-x", "✕");
    x.type = "button";
    x.title = "Close";
    x.setAttribute("aria-label", "Close");
    x.addEventListener("click", () => d.close());
    d.prepend(x);
  }
  document.body.classList.toggle("no-mailboxes", store.get("hideMailboxes", false) === true);
  applySkin();
  applyCompactHeaders();
  // what was on screen last time, at once; the library catches up behind it
  const fromCache = !READER && showCached();
  mark("cached shown");
  try {
    if (!fromCache) status("Opening the library…");
    const info = await invoke("info");
    mark("info");
    state.libraryPath = info.path || "";
    if (info.replaced && !READER) {
      // a restored library took the old one's place: what was cached from the old one goes
      for (const key of [CACHE_KEY, "lastMessage", "lastView", "viewCache", "abCache"]) {
        try { localStorage.removeItem("eudora." + key); } catch { /* fine */ }
      }
      store.set("replacedNote", info.replaced);
      location.reload();
      return;
    }
    if (info.replace_failed) tell("The restored library wasn't put in place", `${info.replace_failed}

The library you had opens as it was.`);
    const replacedNote = !READER && store.get("replacedNote", null);
    if (replacedNote) {
      try { localStorage.removeItem("eudora.replacedNote"); } catch { /* fine */ }
      tell("Successfully restored", replacedNote);
    }
    if (info.problem) {
      $("message-empty").textContent = info.problem;
      status(info.problem);
      return;
    }
    if (READER) {
      // one message on its own: nothing of the main window's lists, counts or timers
      document.body.classList.add("reader-window");
      await loadPictureSettings();
      await loadLabels();
      try { state.uiMode = await invoke("ui_mode_get"); } catch { /* defaults */ } // (read only: the menu bar is the main window's)
      await showMessage(READER);
      document.title = current?.subject || "Message";
      if (READER_FRAME) {
        document.addEventListener("keydown", (e) => {
          if (e.key === "Escape" && $("m-find").hidden && !document.querySelector("dialog[open]")) window.parent.closeReader?.(READER_FRAME);
        });
      }
      return;
    }
    if (!fromCache) status("Reading the folders…");
    state.folders = await invoke("folders");
    mark("folders");
    // the window and the mail first; exact counts once that's on screen
    await loadAccounts(true);
    try { state.sharedActive = JSON.parse((await invoke("setting", { key: "shared-active" })) || "[]"); } catch { state.sharedActive = []; }
    mark("accounts");
    await loadPictureSettings();
    await loadLabels();
    await loadRecentPrefs();
    recentTotal();
    // its window moves with the clock: recounted every 5 minutes, and redrawn when it's open
    setInterval(() => { if (!document.hidden) recentTotal(); }, 300_000);
    mark("pictures, labels");
    loadPlugins();
    loadMode();
    invoke("offline", { on: null }).then(showOffline).catch(() => {});
    $("offline-badge").addEventListener("click", toggleOffline);
    renderSidebar();
    if (await pullChoices()) applyChoices();
    mark("sidebar, choices");
    // once: All Mail - Filtered became the default
    if (!store.get("filteredDefault", false)) {
      store.set("filteredDefault", true);
      if (store.get("lastView", "all") === "all") store.set("lastView", "filtered");
    }
    const last = store.get("lastView", "filtered");
    // open the account and folders above the last folder shown, so it's in view
    if (last.startsWith("#")) {
      const parts = last.split("/");
      for (let i = 1; i < parts.length; i++) state.open.add(parts.slice(0, i).join("/"));
      state.open.add("#accounts");
      renderSidebar();
    }
    const key = state.views.has(last) ? last : "filtered";
    // shown from the cache: brought up to date in place (rows redrawn as they come, the selection
    // kept); else opened
    if (fromCache && state.list?.key === key) await catchUpList(state.list);
    else await openView(key);
    mark("list");
    // the sidebar's contacts (the address books come in with the download, hourly)
    loadCardCounts();
    setInterval(loadCardCounts, 10 * 60 * 1000);
    // which mailboxes search leaves out (the magnifying glasses in the tree)
    loadSearchScope();
    // the first run: no accounts yet (any kind will do)
    if (!state.accounts.accounts.length) showWelcome();
    else checkSignIns();
    // the exact totals in the background, quietly
    loadAccounts().then(renderSidebar).catch((err) => console.warn("counting", err)).finally(() => recountLazily(false));
    setInterval(refresh, REFRESH_EVERY);
    setInterval(async () => { if (await pullChoices()) applyChoices(); }, 60_000);
    showDownload();
    invoke("get_pause").then((m) => { pauseMode = m; showPause(); }).catch(() => {});
    // the people for address completion, from the mail (in the background; again every 6 hours)
    const people = () => invoke("rebuild_contacts").then((n) => console.info(`completion: ${n} people`)).catch((e) => console.warn("completion", e));
    setTimeout(people, 5000);
    setInterval(people, 6 * 3600 * 1000);
  } catch (err) {
    showError("Opening the library", err);
  }
}

start();

// classic's shortcut options (Settings ▸ General): the menu bar is rebuilt with them
for (const id of ["p-ctrl-r-all", "p-ctrl-j-junk"]) {
  $(id).addEventListener("change", () => {
    state.uiMode = { ...state.uiMode, ctrl_r_all: $("p-ctrl-r-all").checked, ctrl_j_junk: $("p-ctrl-j-junk").checked };
    applyMode();
  });
}

// the tray menu's Minimize to Tray: kept (this computer), and given back at start
window.__TAURI__.event.listen("tray-minimize", (e) => store.set("minimizeToTray", !!e.payload));
if (!READER) invoke("tray_set", { minimize: !!store.get("minimizeToTray", false) }).catch(() => {});

/** Offers Classic mode once when classic Eudora turns up: mail imported from a classic Eudora folder, or classic Eudora on this computer. */
async function offerClassic() {
  if (skin().classic || store.get("classicOffered", false)) return;
  let found;
  try { found = await invoke("classic_eudora_found"); } catch { return; }
  if (!found) return;
  store.set("classicOffered", true);
  const classic = await ask({
    title: "Classic Eudora found",
    text: `Eudora found ${found}.\n\nClassic mode looks and works like the Eudora you know: its menus, shortcuts and icons. Modern mode is Eudora updated for today.`,
    detail: "You can switch any time in the look list or Settings ▸ General.",
    buttons: [
      { label: "No: use Modern Eudora mode", value: false },
      { label: "Yes: use Classic Eudora mode", value: true, primary: true },
    ],
  });
  if (classic) setSkin("eudora-classic");
}
if (!READER) setTimeout(offerClassic, 4000);

/** The toolbar's button labels fold away (the icons stay, with their tooltips) when the buttons
 *  don't fit, so nothing at its end, such as the look menu, is ever cut off. */
function fitToolbar() {
  const bar = $("toolbar");
  if (!bar) return;
  document.body.classList.remove("toolbar-tight");
  if (bar.scrollWidth > bar.clientWidth + 1) document.body.classList.add("toolbar-tight");
}
if (!READER) {
  new ResizeObserver(() => requestAnimationFrame(fitToolbar)).observe($("toolbar"));
  window.addEventListener("load", fitToolbar);
}

// Closing the main window asks first. Message windows close at once; File ▸ Exit (Ctrl+Q) quits
// without asking.
if (!READER) {
  const mainWindow = window.__TAURI__.webviewWindow.getCurrentWebviewWindow();
  let closing = false;
  mainWindow.onCloseRequested(async (e) => {
    e.preventDefault();
    if (closing) return;
    closing = true;
    const answer = await ask({
      title: "Close Eudora?",
      text: "This closes Eudora itself. To close a message, use the × on the message (or Ctrl+W).",
      buttons: [{ label: "Close Eudora", value: "close" }, { label: "Keep it open", value: null, primary: true }],
    });
    closing = false;
    if (answer === "close") await mainWindow.destroy();
  });
}

// files dropped on the main window outside a message being written: ignored (without this the
// page would open the file in place of Eudora; message windows attach them: compose.js)
document.addEventListener("dragover", (e) => { if ([...(e.dataTransfer?.types || [])].includes("Files")) { e.preventDefault(); e.dataTransfer.dropEffect = "none"; } });
document.addEventListener("drop", (e) => { if (e.dataTransfer?.files?.length) e.preventDefault(); });

// a "More…" link to the manual (data-help = the page's id)
document.addEventListener("click", (e) => {
  const link = e.target.closest?.("[data-help]");
  if (!link) return;
  e.preventDefault();
  invoke("open_help", { topic: link.dataset.help }).catch((err) => showError("Opening the manual", err));
});

// Settings ▸ Library ▸ Keep original messages exactly (setting keep-exchange-exports; this
// computer only): off = maintenance removes Microsoft's copies of Online Archive items
$("p-keep-originals").addEventListener("change", async (e) => {
  const box = e.target;
  if (!box.checked) {
    const sure = await askYesNo(
      "Stop keeping original copies?",
      "Eudora will remove Microsoft's own copy of each Online Archive item from this computer, at the next maintenance (it starts now) and for new archive mail. Your messages, their attachments, and the archive on Microsoft's server stay as they are, and other computers keep their copies. Backups made from now on won't have them.",
      "Remove them",
      "Keep them",
    );
    if (!sure) { box.checked = true; return; }
  }
  try {
    await invoke("set_setting", { key: "keep-exchange-exports", value: box.checked ? "on" : "off" });
    if (!box.checked) {
      await invoke("maintenance_now");
      status("Removing the original copies: maintenance starts within seconds; the status bar shows each step.");
    } else {
      status("Original copies are kept again for new Online Archive mail.");
    }
  } catch (err) { box.checked = !box.checked; showError("Saving Keep original messages", err); }
});

// Settings ▸ Library ▸ Mail kept on this computer: the download period (download-period), chosen
// with a slider or typed; nothing changes until Keep Only This Period… is confirmed. Older mail
// then leaves this computer only once its server confirms it still has it (trim.rs,
// keep_check.rs); a longer period brings older mail back from the servers.
const keepState = { current: null, chosen: false, token: 0, timer: 0 };

/** Says what a slow step is doing, with the seconds so far, until `done` is called (user
 *  2026-11-06: "Nothing should ever hang without explaining what it is doing"). */
function working(el, text) {
  const started = Date.now();
  const show = () => {
    const s = Math.round((Date.now() - started) / 1000);
    el.textContent = s >= 2 ? `${text} (${s} s)` : text;
  };
  show();
  const tick = setInterval(show, 1000);
  return () => clearInterval(tick);
}

function bigSize(bytes) {
  return bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(bytes / 1e6))} MB`;
}

function keepPeriodText(days) {
  if (days == null) return "everything";
  const step = KEEP_STEPS.find(([d]) => d === days);
  if (step) return `the last ${step[1]}`;
  if (days % 365 === 0) return `the last ${days / 365} years`;
  if (days % 7 === 0 && days < 365) return `the last ${days / 7} weeks`;
  return `the last ${number.format(days)} days`;
}

/** The period the slider and the boxes say (days; null = everything). */
function keepDays() {
  const n = Number($("p-keep-n").value);
  if (Number($("p-keep-range").value) === KEEP_STEPS.length - 1 && !(n > 0)) return null;
  if (!(n > 0)) return null;
  return Math.max(1, Math.round(n * Number($("p-keep-unit").value)));
}

/** Puts a period into the slider and the boxes. */
function setKeepControls(days) {
  const at = days == null ? KEEP_STEPS.length - 1 : KEEP_STEPS.reduce((best, [d], i) => (d != null && Math.abs(d - days) < Math.abs((KEEP_STEPS[best][0] ?? 1e9) - days) ? i : best), 0);
  $("p-keep-range").value = String(at);
  if (days == null) {
    $("p-keep-n").value = "";
  } else if (days % 365 === 0 || [365, 730, 1096, 1826, 3652, 7305].includes(days)) {
    $("p-keep-n").value = String(Math.round(days / 365.25)); $("p-keep-unit").value = "365.25";
  } else if ([30, 61, 91, 183].includes(days)) {
    $("p-keep-n").value = String(Math.round(days / 30.4375)); $("p-keep-unit").value = "30.4375";
  } else {
    $("p-keep-n").value = String(days); $("p-keep-unit").value = "1";
  }
  $("p-keep-label").textContent = days == null ? "Everything" : keepPeriodText(days).replace(/^the last /, "");
}

async function showKeep() {
  keepState.counts = null; // (counted afresh each time the page opens)
  const done = working($("p-keep-now"), "Calculating the space your mail takes on this computer…");
  try {
    const p = await invoke("download_period_get").finally(done);
    keepState.current = p.mb ? null : p.days ?? null;
    keepState.chosen = !!p.chosen;
    $("p-keep-now").innerHTML = p.mb
      ? `Now: this computer keeps up to <b>${bigSize(p.mb * 1e6)}</b> of mail (set on a phone).`
      : `Now: this computer keeps <b>${escapeHtml(keepPeriodText(keepState.current))}</b>. The library takes ${bigSize((p.used_mb || 0) * 1e6)}.`;
    setKeepControls(keepState.current);
    previewKeep();
  } catch (err) {
    $("p-keep-now").textContent = `The period kept here couldn't be read (${err}).`;
  }
}

/** What keeping the last `days` would mean, from the counts by day (trim.rs KeepCounts::preview). */
function keepFromCounts(days) {
  const c = keepState.counts;
  const edge = Date.now() / 1000 - days * 86400 - 86400;
  const p = { days, before: edge, older: 0, older_bytes: 0, removable: 0, removable_bytes: 0, flagged_or_waiting: 0, only_here: 0 };
  for (const [day, kinds] of c.days) {
    if ((day + 1) * 86400 > edge) continue;
    for (const [n, bytes] of kinds) { p.older += n; p.older_bytes += bytes; }
    p.removable += kinds[0][0]; p.removable_bytes += kinds[0][1];
    p.flagged_or_waiting += kinds[1][0];
    p.only_here += kinds[2][0];
  }
  return p;
}

function previewKeep() {
  clearTimeout(keepState.timer);
  const days = keepDays();
  const token = ++keepState.token;
  if (days == null) {
    $("p-keep-preview").textContent = "All your mail stays on this computer, and Eudora downloads all of it.";
    return;
  }
  if (!keepState.counts) {
    if (!keepState.counting) {
      const done = working($("p-keep-preview"), "Counting your mail by date and measuring the space it takes, to show what each period would remove…");
      keepState.counting = invoke("keep_counts").then((c) => { done(); keepState.counts = c; keepState.counting = null; previewKeep(); })
        .catch((err) => { done(); keepState.counting = null; $("p-keep-preview").textContent = `Your mail couldn't be counted (${err}).`; });
    }
    return;
  }
  keepState.timer = setTimeout(() => {
    try {
      const k = keepFromCounts(days);
      if (token !== keepState.token) return;
      keepState.preview = k;
      const since = new Date(k.before * 1000).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
      if (!k.older) {
        $("p-keep-preview").textContent = `Nothing here is older than ${since}: nothing would be removed.`;
        return;
      }
      const kept = [];
      if (k.only_here) kept.push(`<b>${number.format(k.only_here)}</b> exist only on this computer (POP3, imported files, classic Eudora) and always stay`);
      if (k.flagged_or_waiting) kept.push(`${number.format(k.flagged_or_waiting)} are flagged or have a change waiting for their server and stay`);
      $("p-keep-preview").innerHTML =
        `Mail older than <b>${escapeHtml(since)}</b>: ${number.format(k.older)} messages (${bigSize(k.older_bytes)}). ` +
        `<b>${number.format(k.removable)}</b> of them (${bigSize(k.removable_bytes)}) would leave this computer, each only after its server confirms it still has it.` +
        (kept.length ? ` ${kept.join("; ")}.` : "");
    } catch (err) {
      if (token === keepState.token) $("p-keep-preview").textContent = `What it would remove couldn't be worked out (${err}).`;
    }
  }, 30);
}

$("p-keep-range").addEventListener("input", () => {
  setKeepControls(KEEP_STEPS[Number($("p-keep-range").value)][0]);
  previewKeep();
});
for (const id of ["p-keep-n", "p-keep-unit"]) {
  $(id).addEventListener("input", () => {
    const days = keepDays();
    const at = days == null ? KEEP_STEPS.length - 1 : KEEP_STEPS.reduce((best, [d], i) => (d != null && Math.abs(d - days) < Math.abs((KEEP_STEPS[best][0] ?? 1e9) - days) ? i : best), 0);
    $("p-keep-range").value = String(at);
    $("p-keep-label").textContent = days == null ? "Everything" : keepPeriodText(days).replace(/^the last /, "");
    previewKeep();
  });
}

async function setKeep(days, said) {
  try {
    await invoke("download_period_set", { days, mb: null });
    status(said);
    showKeep();
  } catch (err) { showError("Saving the mail kept on this computer", err); }
}

$("p-keep-all").addEventListener("click", () => {
  setKeepControls(null);
  setKeep(null, "This computer keeps all your mail again: older mail comes back from the servers over the next checks.");
});

$("p-keep-apply").addEventListener("click", async () => {
  const days = keepDays();
  if (days == null) { $("p-keep-all").click(); return; }
  const now = keepState.current;
  // keeping more than now removes nothing: older mail just comes back from the servers
  if (now != null && days >= now) {
    setKeep(days, `This computer now keeps ${keepPeriodText(days)}: the older mail within it comes back from the servers over the next checks.`);
    return;
  }
  let k;
  const button = $("p-keep-apply");
  button.disabled = true;
  const done = working(button, "Checking what would be removed…");
  try { k = { days, ...(await invoke("keep_preview", { days })) }; } catch (err) { showError("Working out what would be removed", err); return; }
  finally { done(); button.disabled = false; button.textContent = "Keep Only This Period…"; }
  const since = new Date(k.before * 1000).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
  const sure = await askYesNo(
    `Keep only ${keepPeriodText(days)} on this computer?`,
    `Up to ${number.format(k.removable)} messages older than ${since} (${bigSize(k.removable_bytes)}) will be removed from THIS COMPUTER ONLY. ` +
      "They stay on your mail servers (Microsoft 365, Gmail, IMAP): Eudora asks each message's server first, and keeps any message its server can't confirm it still has. " +
      `${number.format(k.only_here)} messages that exist only on this computer (POP3 mail, imported files, classic Eudora mailboxes) are never removed. ` +
      "From now on, mail older than the period is removed as it ages, with the same check, and Eudora downloads only that period. Choose a longer period at any time to bring older mail back.",
    "Remove Older Mail From This Computer",
    "Cancel",
  );
  if (!sure) return;
  // a great deal at once: typed, so it's never a slip of the mouse
  if (k.removable > 10000) {
    const typed = await askText(
      "Type REMOVE to go ahead",
      `${number.format(k.removable)} messages will leave this computer (they stay on their servers). Type REMOVE:`,
      "",
      "Go Ahead",
    );
    if ((typed || "").trim().toUpperCase() !== "REMOVE") { status("Nothing was removed."); return; }
  }
  setKeep(days, `This computer now keeps ${keepPeriodText(days)}. Older mail leaves it within minutes, once each message's server confirms it still has it; the status bar shows each step.`);
});

// Settings ▸ Downloading and sync: how often Gmail is checked while nobody uses Eudora here
// (graph_sync::gmail_resting; this computer only)
$("p-gmail-idle").addEventListener("change", async (e) => {
  try {
    await invoke("set_setting", { key: "gmail-idle-minutes", value: e.target.value });
    status(e.target.value === "0" ? "Gmail is checked as usual, whether or not Eudora is being used here." : `While nobody uses Eudora here, Gmail is checked every ${e.target.selectedOptions[0].textContent}.`);
  } catch (err) { showError("Saving how often Gmail is checked", err); }
});

// Settings ▸ General: its hours on most days and on Mondays (the weekend's mail)
for (const id of ["p-recent-hours", "p-recent-monday"]) {
  $(id).addEventListener("change", async () => {
    const hours = Math.min(720, Math.max(1, Math.round(Number($("p-recent-hours").value) || 48)));
    const monday = Math.min(720, Math.max(1, Math.round(Number($("p-recent-monday").value) || 72)));
    $("p-recent-hours").value = String(hours);
    $("p-recent-monday").value = String(monday);
    state.recentPrefs = { hours, monday };
    try { await invoke("set_setting", { key: "recent-mail", value: JSON.stringify(state.recentPrefs) }); } catch (err) { showError("Saving Recent Mail's hours", err); }
    recentTotal();
    renderSidebar();
    if (state.list?.kind === "recent") openView("recent");
  });
}
