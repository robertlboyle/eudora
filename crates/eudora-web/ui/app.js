// Eudora on the web. Plain JavaScript, no libraries. Talks to eudora-web's /api.
"use strict";

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};
const number = new Intl.NumberFormat();
const store = {
  get(k, d) { try { const v = localStorage.getItem(`eudora.${k}`); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(`eudora.${k}`, JSON.stringify(v)); } catch { /* private mode */ } },
};

function fillIcons(root = document) {
  for (const e of root.querySelectorAll("[data-icon]")) e.style.setProperty("--icon", `url("/icons/${e.dataset.icon}.svg")`);
}

let toastTimer;
function toast(text) {
  const t = $("toast");
  t.textContent = text;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 4000);
}

// ------------------------------------------------------------------ talking to the server

class SignedOut extends Error {}

async function api(path, body, method) {
  const options = { method: method || (body === undefined ? "GET" : "POST"), credentials: "same-origin", headers: {} };
  if (options.method !== "GET") {
    options.headers["x-eudora"] = "1";
    options.headers["content-type"] = "application/json";
    options.body = JSON.stringify(body ?? {});
  }
  const response = await fetch(path, options);
  if (response.status === 401 && !path.startsWith("/api/login") && !path.startsWith("/api/passkey/login")) {
    showGate();
    throw new SignedOut("sign in first");
  }
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || `${response.status} ${response.statusText}`);
  return data;
}

function report(err) {
  if (!(err instanceof SignedOut)) toast(String(err.message || err));
}

// ------------------------------------------------------------------ signing in

const b64url = {
  encode(buffer) {
    let s = "";
    for (const b of new Uint8Array(buffer)) s += String.fromCharCode(b);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  },
  decode(text) {
    const s = atob(text.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((text.length + 3) % 4));
    return Uint8Array.from(s, (c) => c.charCodeAt(0));
  },
};

let me = null;

async function showGate() {
  $("app").hidden = true;
  $("gate").hidden = false;
  me = await api("/api/me").catch(() => null);
  $("gate-error").textContent = "";
  $("login-form").hidden = !me?.password_set;
  $("setup-form").hidden = !(me && !me.password_set && me.local);
  $("setup-elsewhere").hidden = !(me && !me.password_set && !me.local);
  $("passkey-login").hidden = !(me?.passkeys && me.by_name && window.PublicKeyCredential);
  ($("login-form").hidden ? $("setup-password") : $("login-password")).focus();
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("gate-error").textContent = "";
  try {
    await api("/api/login", { password: $("login-password").value });
    $("login-password").value = "";
    startApp();
  } catch (err) {
    $("gate-error").textContent = err.message;
  }
});

$("setup-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  if ($("setup-password").value !== $("setup-again").value) { $("gate-error").textContent = "The two passwords aren't the same."; return; }
  try {
    await api("/api/setup", { password: $("setup-password").value });
    startApp();
  } catch (err) {
    $("gate-error").textContent = err.message;
  }
});

$("passkey-login").addEventListener("click", async () => {
  $("gate-error").textContent = "";
  try {
    const o = await api("/api/passkey/login-options");
    const credential = await navigator.credentials.get({
      publicKey: {
        challenge: b64url.decode(o.challenge),
        rpId: o.rpId,
        allowCredentials: o.allowCredentials.map((c) => ({ type: "public-key", id: b64url.decode(c.id) })),
        userVerification: o.userVerification,
        timeout: o.timeout,
      },
    });
    await api("/api/passkey/login", {
      id: b64url.encode(credential.rawId),
      client_data_json: b64url.encode(credential.response.clientDataJSON),
      authenticator_data: b64url.encode(credential.response.authenticatorData),
      signature: b64url.encode(credential.response.signature),
    });
    startApp();
  } catch (err) {
    $("gate-error").textContent = err.name === "NotAllowedError" ? "The passkey sign-in was canceled." : err.message;
  }
});

// ------------------------------------------------------------------ the mailboxes (drawer)

const state = {
  data: null, // accounts, folders, counts
  view: { kind: "all", folders: [], title: "All Mail" },
  rows: [],
  done: false,
  loading: false,
  search: "",
  open: null, // the message shown (id)
  selecting: false,
  picked: new Set(), // ids
  hideSent: store.get("hideSent", true),
  openAccounts: new Set(store.get("openAccounts", [])),
};

function openDrawer(open) {
  $("drawer").classList.toggle("open", open);
  $("scrim").hidden = !open;
}
for (const b of document.querySelectorAll(".open-drawer")) b.addEventListener("click", () => openDrawer(true));
for (const b of document.querySelectorAll(".close-drawer")) b.addEventListener("click", () => openDrawer(false));
$("scrim").addEventListener("click", () => openDrawer(false));

async function loadFolders(fresh = false) {
  state.data = await api(`/api/folders${fresh ? "?fresh=1" : ""}`);
  renderTree();
}

function treeItem(text, count, icon, on, onClick, cls = "") {
  const b = el("button", `tree-item ${cls}${on ? " on" : ""}`);
  const i = el("span");
  i.dataset.icon = icon;
  b.append(i, el("span", "name", text));
  if (count) b.append(el("span", "count", number.format(count)));
  b.addEventListener("click", onClick);
  return b;
}

function renderTree() {
  const box = $("folder-tree");
  const d = state.data;
  const v = state.view;
  const items = [treeItem("All Mail", state.hideSent ? d.all : d.all_with_sent, "layer-group", v.kind === "all" && !state.search, () => openView({ kind: "all", folders: [], title: "All Mail" }))];
  for (const a of d.accounts) {
    const open = state.openAccounts.has(a.id);
    const head = treeItem(a.shown, a.messages, open ? "folder-open" : "folder", false, () => {
      open ? state.openAccounts.delete(a.id) : state.openAccounts.add(a.id);
      store.set("openAccounts", [...state.openAccounts]);
      renderTree();
    }, "tree-account");
    head.title = `${a.address || a.shown} · ${a.kind_name}`;
    items.push(head);
    if (!open) continue;
    const kids = el("div", "tree-children");
    const folders = mergedFolders(d.folders.filter((f) => f.account_id === a.id)).sort((x, y) => folderRank(x.path) - folderRank(y.path) || x.path.localeCompare(y.path, undefined, { numeric: true, sensitivity: "base" }));
    for (const f of folders) {
      const depth = f.path.split("/").length - 1;
      const name = f.path.split("/").pop();
      const item = treeItem(name, f.messages, /inbox/i.test(name) ? "inbox" : "folder", v.kind === "folder" && v.folders[0] === f.ids[0] && !state.search, () => openView({ kind: "folder", folders: f.ids, title: `${name} · ${a.shown}` }));
      item.style.paddingLeft = `${10 + depth * 14}px`;
      item.title = f.path;
      kids.append(item);
    }
    items.push(kids);
  }
  box.replaceChildren(...items);
  fillIcons(box);
}

/**
 * The folders as shown: the Online Archive is part of the mailbox, so "Online Archive/Inbox"
 * joins "Inbox" and "Online Archive/Inbox_2021" shows as "Inbox_2021". Each entry: the path
 * shown, every folder id in it (`ids`, the mailbox's first) and `id` (where a move goes: the
 * server keeps archive mail in the archive whatever is chosen).
 */
function mergedFolders(folders) {
  const merged = new Map();
  for (const f of folders) {
    const path = f.path === "Online Archive" ? "" : f.path.replace(/^Online Archive\//, "");
    if (!path) continue;
    const key = `${f.account_id}/${path}`;
    const archive = f.path.startsWith("Online Archive/");
    const have = merged.get(key);
    if (!have) merged.set(key, { account_id: f.account_id, path, id: f.id, ids: [f.id], messages: f.messages });
    else {
      have.messages += f.messages;
      if (archive) have.ids.push(f.id);
      else { have.ids.unshift(f.id); have.id = f.id; }
    }
  }
  return [...merged.values()];
}

/** Inbox, then Sent, then the rest. */
function folderRank(path) {
  const n = path.toLowerCase();
  if (n === "inbox") return 0;
  if (n === "sent" || n === "sent items" || n === "sent messages") return 1;
  return 2;
}

// ------------------------------------------------------------------ the list

const PAGE = 50;

function shortDate(t) {
  if (!t) return "";
  const d = new Date(t * 1000);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (d.getFullYear() === now.getFullYear()) return d.toLocaleDateString([], { month: "short", day: "numeric" });
  return d.toLocaleDateString([], { year: "numeric", month: "short", day: "numeric" });
}

function openView(view) {
  state.view = view;
  state.search = "";
  $("search-input").value = "";
  openDrawer(false);
  reloadList();
  renderTree();
}

function reloadList() {
  state.rows = [];
  state.done = false;
  endSelecting();
  $("list").replaceChildren();
  $("list").scrollTop = 0;
  $("list-title").textContent = state.search ? `Search` : state.view.title;
  $("list-info").textContent = "";
  $("search-help").hidden = true;
  if (state.search) runSearch();
  else loadMore();
}

async function loadMore() {
  if (state.loading || state.done || state.search) return;
  state.loading = true;
  const last = state.rows[state.rows.length - 1];
  const params = new URLSearchParams({ folders: state.view.folders.join(","), size: PAGE });
  if (last) { params.set("after_date", last.date); params.set("after_id", last.id); }
  if (state.view.kind === "all" && state.hideSent) params.set("hide_sent", "1");
  const more = el("li", "more", "Loading…");
  $("list").append(more);
  try {
    const rows = await api(`/api/list?${params}`);
    more.remove();
    addRows(rows);
    if (rows.length < PAGE) state.done = true;
    if (!state.rows.length) $("list").append(el("li", "more", "No messages here."));
  } catch (err) {
    more.textContent = "Couldn't load the list.";
    report(err);
  } finally {
    state.loading = false;
  }
}

function addRows(rows) {
  const items = rows.map((row) => {
    state.rows.push(row);
    const li = el("li", "row");
    li.dataset.id = row.id;
    li.append(el("span", "check"), el("span", "from", row.from || "(no sender)"), el("span", "date", shortDate(row.date)));
    const subject = el("span", "subject", row.subject || "(no subject)");
    if (row.attachments) {
      const clip = el("span", "clip");
      clip.dataset.icon = "paperclip";
      subject.prepend(clip, " ");
    }
    li.append(subject, el("span", "snippet", row.snippet || ""));
    if (row.id === state.open) li.classList.add("on");
    if (state.picked.has(row.id)) li.classList.add("picked");
    return li;
  });
  $("list").append(...items);
  fillIcons($("list"));
}

$("list").addEventListener("scroll", () => {
  const l = $("list");
  if (l.scrollTop + l.clientHeight > l.scrollHeight - 600) loadMore();
});

// tap = read; long press (or a right-click) = start choosing several
let pressTimer = null;
let pressed = false;
$("list").addEventListener("pointerdown", (e) => {
  const li = e.target.closest(".row");
  if (!li) return;
  pressed = false;
  pressTimer = setTimeout(() => { pressed = true; startSelecting(Number(li.dataset.id)); }, 500);
});
for (const ev of ["pointerup", "pointercancel", "pointerleave"]) $("list").addEventListener(ev, () => clearTimeout(pressTimer));
$("list").addEventListener("scroll", () => clearTimeout(pressTimer), { passive: true });
$("list").addEventListener("contextmenu", (e) => {
  const li = e.target.closest(".row");
  if (!li) return;
  e.preventDefault();
  if (!state.selecting) startSelecting(Number(li.dataset.id));
});
$("list").addEventListener("click", (e) => {
  const li = e.target.closest(".row");
  if (!li || pressed) { pressed = false; return; }
  const id = Number(li.dataset.id);
  if (state.selecting) togglePick(id);
  else openMessage(id);
});

function startSelecting(id) {
  state.selecting = true;
  state.picked = new Set([id]);
  $("list").classList.add("selecting");
  showPicked();
}

function endSelecting() {
  state.selecting = false;
  state.picked = new Set();
  $("list").classList.remove("selecting");
  showPicked();
}

function togglePick(id) {
  state.picked.has(id) ? state.picked.delete(id) : state.picked.add(id);
  if (!state.picked.size) { endSelecting(); return; }
  showPicked();
}

function showPicked() {
  for (const li of $("list").querySelectorAll(".row")) li.classList.toggle("picked", state.picked.has(Number(li.dataset.id)));
  $("select-bar").hidden = !state.selecting;
  $("select-count").textContent = `${state.picked.size} selected`;
}
$("select-cancel").addEventListener("click", endSelecting);

// ------------------------------------------------------------------ search

$("search-toggle").addEventListener("click", () => { $("search-input").focus(); $("search-input").select(); });
$("search-form").addEventListener("submit", (e) => {
  e.preventDefault();
  state.search = $("search-input").value.trim();
  $("search-input").blur();
  reloadList();
  renderTree();
});
$("search-input").addEventListener("search", () => {
  // the ✕ in the box: back to the mailbox
  if (!$("search-input").value && state.search) { state.search = ""; reloadList(); renderTree(); }
});

async function runSearch() {
  const query = state.search;
  const more = el("li", "more", "Searching…");
  $("list").append(more);
  try {
    const found = await api(`/api/search?${new URLSearchParams({ q: query, limit: 500 })}`);
    if (state.search !== query) return;
    more.remove();
    state.done = true;
    addRows(found.rows);
    let info = `${number.format(found.total)} found${found.total > found.rows.length ? `, the newest ${number.format(found.rows.length)} listed` : ""} · ${Math.round(found.millis)} ms`;
    const c = found.header_coverage;
    if (c && c.covered < c.total) info += ` · searching by header covers ${number.format(c.covered)} of ${number.format(c.total)} messages so far`;
    $("list-info").textContent = info;
    showSearchHelp(found);
    if (!found.rows.length) $("list").append(el("li", "more", "No results."));
  } catch (err) {
    more.textContent = `The search couldn't run: ${err.message || err}`;
    if (!(err instanceof SignedOut)) $("list-info").textContent = "";
  }
}

/** What was repaired in the search, and corrected searches to try (tap one to run it). */
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
      b.addEventListener("click", () => {
        $("search-input").value = s.query;
        state.search = s.query.trim();
        reloadList();
      });
      tries.append(b);
      parts.push(el("p", "small", s.why));
    }
    parts.splice((found.notes || []).length, 0, tries);
  }
  box.replaceChildren(...parts);
  box.hidden = !parts.length;
}

// ------------------------------------------------------------------ reading

let current = null; // the message shown

function person(p) {
  return p.name && p.name !== p.address ? `${p.name} <${p.address}>` : p.address;
}

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

async function openMessage(id) {
  if ($("reader").hidden) history.pushState({ screen: "reader" }, "");
  state.open = id;
  for (const li of $("list").querySelectorAll(".row")) li.classList.toggle("on", Number(li.dataset.id) === id);
  $("reader").hidden = false;
  $("m-subject").textContent = "…";
  $("m-meta").replaceChildren();
  $("m-content").replaceChildren();
  $("m-attachments").replaceChildren();
  $("m-pictures").hidden = true;
  try {
    const m = await api(`/api/message/${id}`);
    if (state.open !== id) return;
    current = m;
    showMessage(m);
  } catch (err) {
    $("m-subject").textContent = "This message couldn't be read.";
    report(err);
  }
}

function showMessage(m, pictures = null) {
  $("m-subject").textContent = m.subject || "(no subject)";
  document.querySelector("#reader .bar h2").textContent = "";
  const meta = $("m-meta");
  const line = (label, people) => {
    if (!people.length) return;
    const d = el("div");
    d.append(el("b", "", `${label}: `), people.map(person).join(", "));
    meta.append(d);
  };
  meta.replaceChildren();
  line("From", m.from);
  line("To", m.to);
  line("Cc", m.cc);
  meta.append(el("div", "", m.date ? new Date(m.date * 1000).toLocaleString() : "No date"));
  if (m.folders.length) meta.append(el("div", "", `In: ${m.folders.join(", ")}`));

  const files = $("m-attachments");
  files.replaceChildren(...m.attachments.filter((a) => !/^(uu|binhex):/.test(a.part)).map((a) => {
    if (a.missing) {
      const chip = el("span", "chip missing");
      chip.title = `Not found: classic Eudora kept this file at ${a.missing}`;
      chip.append(el("span", "name", `${a.name} (not found)`));
      return chip;
    }
    const link = el("a", "chip");
    link.href = `/api/message/${m.id}/attachment/${encodeURIComponent(a.part)}`;
    link.setAttribute("download", a.name);
    const i = el("span");
    i.dataset.icon = "paperclip";
    link.append(i, el("span", "name", a.name), el("span", "small", `${Math.max(1, Math.round(a.size / 1024))} KB`));
    return link;
  }));
  fillIcons(files);

  const box = $("m-content");
  if (m.html) {
    let html = m.html;
    if (pictures) for (const p of pictures) if (p.data) html = html.split(p.url).join(p.data);
    // shown in a sandbox: no scripts; links open outside; pictures only as data: (the page's policy)
    const frame = el("iframe");
    frame.setAttribute("sandbox", "allow-same-origin allow-popups allow-popups-to-escape-sandbox");
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><base target="_blank"><style>body{margin:8px;font:15px/1.45 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;color:#1c1f24;overflow-wrap:anywhere}img{max-width:100%;height:auto}table{max-width:100%}pre{white-space:pre-wrap}</style></head><body>${html}</body></html>`;
    frame.addEventListener("load", () => {
      const fit = () => { try { frame.style.height = `${frame.contentDocument.documentElement.scrollHeight + 4}px`; } catch { /* sandboxed */ } };
      fit();
      setTimeout(fit, 400);
    });
    box.replaceChildren(frame);
    $("m-pictures").hidden = !(m.web_pictures.length && !pictures);
    // a fraud (phishing): said plainly; its links are dead and its pictures never load
    $("m-fraud").hidden = !m.warning;
    $("m-fraud").querySelector("span").textContent = m.warning || "";
  } else {
    box.replaceChildren(el("pre", "", m.text || ""));
    $("m-fraud").hidden = !m.warning;
    $("m-fraud").querySelector("span").textContent = m.warning || "";
  }
  $("reader-body").scrollTop = 0;
}

$("m-show-pictures").addEventListener("click", async () => {
  if (!current) return;
  $("m-show-pictures").disabled = true;
  try {
    const pictures = await api("/api/pictures", { urls: current.web_pictures });
    showMessage(current, pictures);
  } catch (err) {
    report(err);
  } finally {
    $("m-show-pictures").disabled = false;
  }
});

$("reader-back").addEventListener("click", closeMessage);
function closeMessage() {
  $("reader").hidden = true;
  state.open = null;
  current = null;
}

for (const b of document.querySelectorAll("#reader .actions button")) b.addEventListener("click", () => current && openComposer(b.dataset.act, current.id));

/** The folder a message is taken from: the open folder, else the first it's in. */
function fromFolder(id) {
  if (state.view.kind === "folder" && !state.search) return state.view.folders[0];
  return state.rows.find((r) => r.id === id)?.labels?.[0]?.[0] ?? null;
}

function groupByFolder(ids) {
  const groups = new Map();
  for (const id of ids) {
    const from = fromFolder(id);
    if (from == null) continue;
    if (!groups.has(from)) groups.set(from, []);
    groups.get(from).push(id);
  }
  return groups;
}

function removeRows(ids) {
  const gone = new Set(ids);
  state.rows = state.rows.filter((r) => !gone.has(r.id));
  for (const li of $("list").querySelectorAll(".row")) if (gone.has(Number(li.dataset.id))) li.remove();
  if (gone.has(state.open)) closeMessage();
}

async function deleteIds(ids) {
  if (ids.length > 20 && !confirm(`Delete ${ids.length} messages?`)) return;
  try {
    for (const [from, group] of groupByFolder(ids)) await api("/api/delete", { ids: group, from });
    removeRows(ids);
    endSelecting();
    toast(ids.length > 1 ? `${ids.length} messages moved to the Trash.` : "Moved to the Trash.");
    loadFolders(true).catch(() => {});
  } catch (err) {
    report(err);
  }
}

async function moveIds(ids) {
  const to = await pickFolder("Move To");
  if (to == null) return;
  try {
    for (const [from, group] of groupByFolder(ids)) if (from !== to.id) await api("/api/move", { ids: group, from, to: to.id });
    removeRows(ids);
    endSelecting();
    toast(`Moved to ${to.path}.`);
    loadFolders(true).catch(() => {});
  } catch (err) {
    report(err);
  }
}

$("reader-delete").addEventListener("click", () => current && deleteIds([current.id]));
$("reader-move").addEventListener("click", () => current && moveIds([current.id]));
$("select-delete").addEventListener("click", () => deleteIds([...state.picked]));
$("select-move").addEventListener("click", () => moveIds([...state.picked]));
$("select-forward").addEventListener("click", () => {
  const ids = [...state.picked];
  endSelecting();
  ids.length === 1 ? openComposer("forward", ids[0]) : openComposer("forwardMany", null, ids);
});

/** The folder list to choose from (type to find one). */
function pickFolder(title) {
  return new Promise((resolve) => {
    const dialog = $("pick");
    $("pick-title").textContent = title;
    const name = new Map(state.data.accounts.map((a) => [a.id, a.shown]));
    const all = mergedFolders(state.data.folders).map((f) => ({ f, text: `${name.get(f.account_id)} › ${f.path}` }));
    const show = () => {
      const words = $("pick-filter").value.toLowerCase().split(/\s+/).filter(Boolean);
      $("pick-list").replaceChildren(...all.filter((x) => words.every((w) => x.text.toLowerCase().includes(w))).slice(0, 200).map((x) => {
        const li = el("li");
        const b = el("button", "", x.text);
        b.addEventListener("click", () => { dialog.close(); resolve(x.f); });
        li.append(b);
        return li;
      }));
    };
    $("pick-filter").value = "";
    $("pick-filter").oninput = show;
    $("pick-cancel").onclick = () => { dialog.close(); resolve(null); };
    dialog.onclose = () => resolve(null);
    show();
    dialog.showModal();
  });
}

// ------------------------------------------------------------------ writing

let compose = null; // { mode, seed, draftId, files, dirty }

async function openComposer(mode, id = null, ids = []) {
  try {
    const params = new URLSearchParams({ mode });
    if (id != null) params.set("id", id);
    if (ids.length) params.set("ids", ids.join(","));
    const start = await api(`/api/compose?${params}`);
    const seed = start.seed;
    compose = { mode, seed, draftId: null, files: [], dirty: false };
    const from = $("c-from");
    const usable = start.identities.filter((i) => i.can_send);
    from.replaceChildren(...usable.map((i) => {
      const o = el("option", "", `${i.address} · ${i.kind}`);
      o.value = i.address;
      o.dataset.name = i.name;
      return o;
    }));
    const preferred = seed.to_me && usable.find((i) => i.address.toLowerCase() === seed.to_me.toLowerCase());
    if (preferred) from.value = preferred.address;
    else if (store.get("lastFrom") && usable.some((i) => i.address === store.get("lastFrom"))) from.value = store.get("lastFrom");
    $("c-to").value = seed.to || "";
    $("c-cc").value = seed.cc || "";
    $("c-bcc").value = "";
    for (const e of document.querySelectorAll(".c-extra")) e.hidden = !seed.cc;
    $("c-subject").value = seed.subject || "";
    $("c-body").value = "";
    $("c-error").textContent = "";
    $("c-title").textContent = { reply: "Reply", replyAll: "Reply All", forward: "Forward", forwardMany: "Forward" }[mode] || "New Message";
    $("c-quoted").hidden = !seed.html;
    $("c-quoted").open = false;
    // the original, shown as text (it's sent as it was)
    const quoted = document.createElement("div");
    quoted.textContent = htmlText(seed.html || "");
    $("c-quoted-body").replaceChildren(quoted);
    $("c-quoted-body").style.whiteSpace = "pre-wrap";
    showFiles();
    history.pushState({ screen: "composer" }, "");
    $("composer").hidden = false;
    (seed.to ? $("c-body") : $("c-to")).focus();
  } catch (err) {
    report(err);
  }
}

function htmlText(html) {
  const doc = new DOMParser().parseFromString(html, "text/html");
  return doc.body?.innerText || doc.body?.textContent || "";
}

function showFiles() {
  const list = [
    ...compose.seed.forwarded.map((f) => ({ name: f.name, size: f.size, forwarded: true })),
    ...compose.files.map((f, i) => ({ name: f.name, size: f.size, index: i })),
  ];
  $("c-files").replaceChildren(...list.map((f) => {
    const chip = el("span", "chip");
    const i = el("span");
    i.dataset.icon = "paperclip";
    chip.append(i, el("span", "name", f.name), el("span", "small", `${Math.max(1, Math.round(f.size / 1024))} KB`));
    if (f.index != null) {
      const x = el("button", "icon-btn");
      x.dataset.icon = "xmark";
      x.title = "Remove";
      x.addEventListener("click", () => { compose.files.splice(f.index, 1); showFiles(); });
      chip.append(x);
    }
    return chip;
  }));
  fillIcons($("c-files"));
}

$("c-attach").addEventListener("change", async () => {
  for (const file of $("c-attach").files) {
    const data = await new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => resolve(String(r.result).split(",")[1] || "");
      r.onerror = reject;
      r.readAsDataURL(file);
    });
    compose.files.push({ name: file.name, mime: file.type || "application/octet-stream", data, size: file.size });
  }
  $("c-attach").value = "";
  compose.dirty = true;
  showFiles();
});

$("c-more").addEventListener("click", () => { for (const e of document.querySelectorAll(".c-extra")) e.hidden = false; $("c-cc").focus(); });
for (const id of ["c-to", "c-cc", "c-bcc", "c-subject", "c-body"]) $(id).addEventListener("input", () => { if (compose) compose.dirty = true; });

function draft() {
  const from = $("c-from").selectedOptions[0];
  const typed = $("c-body").value;
  const mine = typed.trim() ? typed.split(/\n{2,}/).map((p) => `<p>${escapeHtml(p).replace(/\n/g, "<br>")}</p>`).join("") : "<p><br></p>";
  const seed = compose.seed;
  return {
    from: from?.value || "",
    from_name: from?.dataset.name || "",
    to: $("c-to").value,
    cc: $("c-cc").value,
    bcc: $("c-bcc").value,
    subject: $("c-subject").value,
    html: seed.html ? `${mine}${seed.html}` : mine,
    text: "",
    in_reply_to: seed.in_reply_to ?? null,
    references: seed.references || [],
    attachments: compose.files.map((f) => ({ name: f.name, mime: f.mime, data: f.data })),
    forwarded: seed.forwarded || [],
    priority: null,
    sender: null,
  };
}

$("c-send").addEventListener("click", async () => {
  const d = draft();
  if (!d.to.trim() && !d.cc.trim() && !d.bcc.trim()) { $("c-error").textContent = "Who is it to?"; return; }
  if (!d.subject.trim() && !confirm("Send it without a subject?")) return;
  $("c-send").disabled = true;
  $("c-error").textContent = "";
  try {
    await api("/api/send", d);
    store.set("lastFrom", d.from);
    $("composer").hidden = true;
    compose = null;
    toast("Sent.");
  } catch (err) {
    $("c-error").textContent = err.message;
  } finally {
    $("c-send").disabled = false;
  }
});

$("c-save").addEventListener("click", saveDraft);
async function saveDraft() {
  try {
    compose.draftId = await api("/api/draft", { id: compose.draftId, draft: draft() });
    compose.dirty = false;
    toast("Draft saved (it's in the desktop viewer's Drafts too).");
  } catch (err) {
    report(err);
  }
}

$("c-close").addEventListener("click", async () => {
  if (compose?.dirty) {
    if (confirm("Save this message as a draft before closing?")) await saveDraft();
  }
  $("composer").hidden = true;
  compose = null;
});

// address completion: the part being typed after the last comma / semicolon
let suggestFor = null;
let suggestTimer = null;
for (const id of ["c-to", "c-cc", "c-bcc"]) {
  const input = $(id);
  input.addEventListener("input", () => {
    clearTimeout(suggestTimer);
    suggestTimer = setTimeout(() => suggest(input), 150);
  });
  input.addEventListener("blur", () => setTimeout(() => { $("c-suggest").hidden = true; }, 200));
}

async function suggest(input) {
  const typed = input.value.split(/[,;]/).pop().trim();
  const box = $("c-suggest");
  if (typed.length < 2) { box.hidden = true; return; }
  suggestFor = input;
  try {
    const found = await api(`/api/complete?${new URLSearchParams({ q: typed })}`);
    if (suggestFor !== input) return;
    box.replaceChildren(...found.map((p) => {
      const li = el("li", "", p.name ? `${p.name} <${p.address}>` : p.address);
      li.addEventListener("mousedown", (e) => {
        e.preventDefault();
        const parts = input.value.split(/([,;])/);
        parts[parts.length - 1] = ` ${p.name ? `"${p.name.replace(/"/g, "")}" <${p.address}>` : p.address}`;
        input.value = `${parts.join("").replace(/^\s+/, "")}, `;
        box.hidden = true;
        input.focus();
      });
      return li;
    }));
    box.style.top = `${input.offsetTop + input.offsetHeight + 4}px`;
    box.hidden = !found.length;
  } catch {
    box.hidden = true;
  }
}

$("compose-fab").addEventListener("click", () => openComposer("new"));
$("compose-btn-drawer").addEventListener("click", () => { openDrawer(false); openComposer("new"); });

// ------------------------------------------------------------------ mail coming in

$("check-mail").addEventListener("click", async () => {
  try {
    const running = await api("/api/check-mail", {});
    toast(running ? "Checking every account for new mail…" : "The download isn't running on the computer: it checks as soon as it starts.");
    setTimeout(() => refreshList(), 8000);
  } catch (err) {
    report(err);
  }
});

async function showStatus() {
  try {
    const s = await api("/api/status");
    const box = $("download-status");
    if (!s.status) { box.textContent = ""; return; }
    const stale = s.age > 90 && ["running", "waiting", "watching", "paused"].includes(s.status.state);
    box.textContent = stale ? "The download isn't running on the computer." : s.status.line;
  } catch { /* signed out, offline */ }
}

/** New mail at the top of the list (every minute, and after Check Mail). */
async function refreshList() {
  if (state.search || state.selecting || !state.rows.length) return;
  try {
    const params = new URLSearchParams({ folders: state.view.folders.join(","), size: 20 });
    if (state.view.kind === "all" && state.hideSent) params.set("hide_sent", "1");
    const top = await api(`/api/list?${params}`);
    const known = new Set(state.rows.map((r) => r.id));
    const fresh = top.filter((r) => !known.has(r.id) && r.date >= (state.rows[0]?.date ?? 0));
    if (!fresh.length) return;
    // new ones first, then what was there
    const old = state.rows;
    const oldItems = [...$("list").children];
    state.rows = [];
    $("list").replaceChildren();
    addRows(fresh);
    $("list").append(...oldItems);
    state.rows.push(...old);
    loadFolders(true).catch(() => {});
  } catch { /* offline: next time */ }
}

// ------------------------------------------------------------------ settings

$("open-settings").addEventListener("click", () => { openDrawer(false); openSettings(); });
$("settings-back").addEventListener("click", () => { $("settings").hidden = true; });

async function openSettings() {
  if ($("settings").hidden) history.pushState({ screen: "settings" }, "");
  $("settings").hidden = false;
  $("s-hide-sent").checked = state.hideSent;
  $("s-message").textContent = "";
  const canPasskey = me?.by_name && window.PublicKeyCredential;
  $("s-add-passkey").hidden = !canPasskey;
  $("s-passkey-note").hidden = !!canPasskey;
  try {
    const s = await api("/api/security");
    $("s-passkeys").replaceChildren(...(s.passkeys.length ? s.passkeys.map((p) => {
      const li = el("li");
      li.append(el("span", "", `${p.name} · added ${new Date(p.created * 1000).toLocaleDateString()}${p.last_used ? ` · last used ${new Date(p.last_used * 1000).toLocaleDateString()}` : ""}`));
      const b = el("button", "danger", "Remove");
      b.addEventListener("click", async () => {
        if (!confirm(`Remove the passkey "${p.name}"?`)) return;
        await api("/api/passkeys/remove", { id: p.id }).catch(report);
        openSettings();
      });
      li.append(b);
      return li;
    }) : [el("li", "small", "None yet.")]));
    $("s-sessions").replaceChildren(...s.sessions.map((x) => {
      const li = el("li");
      const mine = x.hash === s.this;
      li.append(el("span", "", `${x.device}${mine ? " (this one)" : ""} · ${x.address} · last seen ${new Date(x.last_seen * 1000).toLocaleString()}`));
      if (!mine) {
        const b = el("button", "danger", "Sign Out");
        b.addEventListener("click", async () => { await api("/api/sessions/end", { id: x.hash }).catch(report); openSettings(); });
        li.append(b);
      }
      return li;
    }));
  } catch (err) {
    report(err);
  }
}

$("s-hide-sent").addEventListener("change", () => {
  state.hideSent = $("s-hide-sent").checked;
  store.set("hideSent", state.hideSent);
  renderTree();
  if (state.view.kind === "all") reloadList();
});

$("s-add-passkey").addEventListener("click", async () => {
  try {
    const o = await api("/api/passkey/register-options");
    const name = prompt("A name for this passkey (e.g. Pat's iPhone):", "") ?? "";
    const credential = await navigator.credentials.create({
      publicKey: {
        challenge: b64url.decode(o.challenge),
        rp: o.rp,
        user: { id: b64url.decode(o.user.id), name: o.user.name, displayName: o.user.displayName },
        pubKeyCredParams: o.pubKeyCredParams,
        authenticatorSelection: o.authenticatorSelection,
        attestation: o.attestation,
        excludeCredentials: o.excludeCredentials.map((c) => ({ type: "public-key", id: b64url.decode(c.id) })),
        timeout: o.timeout,
      },
    });
    await api("/api/passkey/register", {
      client_data_json: b64url.encode(credential.response.clientDataJSON),
      attestation_object: b64url.encode(credential.response.attestationObject),
      name,
    });
    $("s-message").textContent = "Passkey added: next time, sign in with it.";
    openSettings();
  } catch (err) {
    $("s-message").textContent = err.name === "NotAllowedError" ? "Adding the passkey was canceled." : err.message;
  }
});

$("s-password-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await api("/api/password", { current: $("s-current").value, new: $("s-new").value });
    $("s-current").value = $("s-new").value = "";
    $("s-message").textContent = "Password changed.";
  } catch (err) {
    $("s-message").textContent = err.message;
  }
});

$("s-logout").addEventListener("click", async () => {
  await api("/api/logout", {}).catch(() => {});
  $("settings").hidden = true;
  showGate();
});

// ------------------------------------------------------------------ start

let timers = [];
async function startApp() {
  $("gate").hidden = true;
  $("app").hidden = false;
  me = await api("/api/me").catch(() => me);
  try {
    await loadFolders();
    openView(state.view);
  } catch (err) {
    report(err);
  }
  showStatus();
  for (const t of timers) clearInterval(t);
  timers = [setInterval(showStatus, 30000), setInterval(refreshList, 60000)];
}

// the phone's back button / gesture closes what's on top
window.addEventListener("popstate", () => {
  if (!$("composer").hidden) $("c-close").click();
  else if (!$("settings").hidden) $("settings").hidden = true;
  else if (!$("reader").hidden) closeMessage();
});

fillIcons();
(async () => {
  try {
    me = await api("/api/me");
    if (me.signed_in) startApp();
    else showGate();
  } catch {
    showGate();
  }
})();
