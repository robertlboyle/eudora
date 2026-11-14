// The .msg viewer: one Outlook message file, shown with the main window's safety (msgframe.js: no
// scripts, nothing from the network; pictures from the web only when asked, or for senders on the
// always-show list; a fraud's links dead with no way back). Reply, Reply All, and Forward hand the
// message to the Eudora window; the rest happens here.

const invoke = window.__TAURI__.core.invoke;
const $ = (id) => document.getElementById(id);

// light or dark as Eudora's sun / moon button chose (else the system's)
try {
  const theme = JSON.parse(localStorage.getItem("eudora.theme") || "null");
  if (theme) invoke("msgview_theme", { theme }).catch(() => {});
} catch { /* the system's */ }

for (const el of document.querySelectorAll("[data-icon]")) {
  el.classList.add("icon");
  el.setAttribute("aria-hidden", "true");
  el.style.setProperty("--icon", `url("icons/${window.ICON_SET || "free"}/${el.dataset.icon}.svg")`);
}

let current = null;
let view = "message";

function status(text) {
  $("v-status").textContent = text || "";
}

function escapeHtml(text) {
  return String(text ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

function person(p) {
  return p.name && p.name !== p.address ? `${p.name} <${p.address}>` : p.address || p.name;
}

function people(list) {
  return (list || []).map(person).join(", ");
}

function sizeText(n) {
  if (n >= 1048576) return `${(n / 1048576).toFixed(1)} MB`;
  if (n >= 1024) return `${Math.round(n / 1024)} KB`;
  return `${n} bytes`;
}

const fullFormat = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short" });

/** Web addresses in (already escaped) plain text become links; a fraud's stay plain text. */
function textToHtml(text) {
  return text.split(/\r?\n/).map((line) => {
    const depth = (line.match(/^(\s*>)+/)?.[0].match(/>/g) || []).length;
    let html = escapeHtml(line);
    if (!current?.warning) {
      html = html.replace(/\bhttps?:\/\/[^\s<>"']+/gi, (url) => {
        const tail = url.match(/[.,;:!?)\]]+$/)?.[0] || "";
        const href = url.slice(0, url.length - tail.length);
        return `<a href="${href}">${href}</a>${tail}`;
      });
    }
    html = html || " ";
    return depth ? `<span class="quote">${html}</span>` : html;
  }).join("\n");
}

/** A link clicked: web links in the browser, mailto: a new message in Eudora; never a fraud's. */
async function openLink(href) {
  const link = (href || "").trim();
  if (!link || link.startsWith("#")) return;
  if (current?.warning) { status("Links in a message that looks like a fraud are turned off."); return; }
  try {
    await invoke("msgview_open_link", { url: link });
    status(/^mailto:/i.test(link) ? "A new message is opening in Eudora." : `Opened in your browser: ${link}`);
  } catch (err) {
    status(String(err));
  }
}

function hookLinks(root) {
  root.addEventListener("click", (e) => {
    const a = e.target.closest?.("a[href]");
    if (a) { e.preventDefault(); openLink(a.getAttribute("href").replace(/&amp;/g, "&")); }
  });
  root.addEventListener("mouseover", (e) => {
    const a = e.target.closest?.("a[href]");
    status(a ? a.getAttribute("href") : "");
  });
}

async function renderBody() {
  const body = $("v-body");
  const m = current;
  body.replaceChildren();
  if (!m) return;
  if (view === "message" && m.html) {
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-same-origin");
    frame.title = `Message: ${m.subject || "(no subject)"}`;
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.srcdoc = sandboxed(m.html);
    frame.addEventListener("load", () => {
      const doc = frame.contentDocument;
      if (!doc) return;
      hookLinks(doc);
      darkenMessage(doc);
      scrollByWheel(doc);
      shown();
    });
    body.append(frame);
    return;
  }
  const pre = document.createElement("pre");
  if (view === "headers") pre.textContent = m.headers;
  else if (view === "source") {
    pre.textContent = "Reading…";
    invoke("msgview_source").then((s) => { if (view === "source") pre.textContent = s; }).catch((err) => { pre.textContent = String(err); });
  } else {
    pre.className = "text";
    pre.innerHTML = textToHtml(m.text || "(no text)");
    hookLinks(pre);
  }
  body.append(pre);
  shown();
}

/** Says once that the message is on screen (for the time it took: `EUDORA_VIEW_TIMING`). */
let told = false;
function shown() {
  if (told) return;
  told = true;
  requestAnimationFrame(() => invoke("msgview_shown").then((ms) => { if (ms != null) status(`Shown in ${ms} ms`); }).catch(() => {}));
}

function renderHead(m, file) {
  document.title = `${m.subject || file} – Eudora`;
  $("v-subject").textContent = m.subject || "(no subject)";
  const rows = [
    ["From", people(m.from)],
    ["To", people(m.to)],
    ["Cc", people(m.cc)],
    ["Bcc", people(m.bcc)],
    ["Date", m.date ? fullFormat.format(new Date(m.date * 1000)) : ""],
  ].filter(([, v]) => v);
  $("v-people").innerHTML = rows.map(([k, v]) => `<div><b>${k}:</b> ${escapeHtml(v)}</div>`).join("");
  $("v-warning").hidden = !m.warning;
  $("v-warning-text").textContent = m.warning ? `${m.warning} Its links are turned off and its pictures and attachments aren't opened.` : "";
  renderAttachments(m);
}

function renderAttachments(m) {
  const box = $("v-attachments");
  // (pictures the message shows itself aren't listed on the Message tab)
  const list = (m.attachments || []).filter((a) => !(view === "message" && m.html && a.inline));
  box.replaceChildren();
  box.hidden = !list.length;
  for (const a of list) {
    const chip = document.createElement("span");
    chip.className = "v-att";
    chip.title = `${a.name} (${a.mime}, ${sizeText(a.size)}): click to open`;
    const img = document.createElement("img");
    img.alt = "";
    img.hidden = true;
    invoke("msgview_file_icon", { name: a.name }).then((src) => { if (src) { img.src = src; img.hidden = false; } }).catch(() => {});
    const name = document.createElement("span");
    name.className = "v-name";
    name.textContent = a.name;
    const size = document.createElement("span");
    size.className = "v-size";
    size.textContent = sizeText(a.size);
    const save = document.createElement("button");
    save.title = "Save…";
    save.innerHTML = `<span class="icon" style="--icon: url('icons/${window.ICON_SET || "free"}/download.svg')"></span>`;
    save.addEventListener("click", (e) => { e.stopPropagation(); act("msgview_attachment_save", { part: a.part }, "Saved"); });
    chip.addEventListener("click", () => act("msgview_attachment_open", { part: a.part }, "Opened"));
    chip.append(img, name, size, save);
    box.append(chip);
  }
  if (list.length > 1) {
    const all = document.createElement("button");
    all.className = "v-all";
    all.textContent = "Save All…";
    all.addEventListener("click", () => act("msgview_attachments_save_all", {}, "Saved"));
    box.append(all);
  }
}

/** Runs a command that answers with where a file went (or nothing when canceled). */
async function act(command, args, done) {
  try {
    const where = await invoke(command, args);
    if (where) status(`${done}: ${Array.isArray(where) ? where.join(", ") : where}`);
  } catch (err) {
    status(String(err));
  }
}

function showPictureBar(m) {
  const n = m.web_pictures?.length || 0;
  $("v-pictures").hidden = !m.html || !n || m.picturesShown;
  $("v-pictures-text").textContent = `${n} picture${n > 1 ? "s" : ""} from the web ${n > 1 ? "aren't" : "isn't"} shown: loading them can tell the sender you opened this message.`;
}

async function loadPictures(m) {
  $("v-pictures-text").textContent = `Loading ${m.web_pictures.length} pictures…`;
  try {
    const got = await invoke("msgview_pictures");
    let html = m.html;
    let n = 0;
    for (const p of got) {
      if (!p.data) continue;
      html = html.split(p.url).join(p.data);
      n++;
    }
    m.html = html;
    m.picturesShown = true;
    $("v-pictures").hidden = true;
    status(`${n} of ${got.length} pictures loaded`);
    if (view === "message") renderBody();
  } catch (err) {
    status(String(err));
  }
}

$("v-pictures-show").addEventListener("click", () => current && loadPictures(current));

$("v-views").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b || !current) return;
  view = b.dataset.view;
  for (const x of document.querySelectorAll("#v-views button")) x.classList.toggle("on", x === b);
  renderBody();
  renderAttachments(current);
});

function answer(mode) {
  act("msgview_answer", { mode }, "Opening in Eudora");
}

function printMessage() {
  const m = current;
  if (!m) return;
  const head = [
    `<div style="font:13px 'Segoe UI', sans-serif; margin-bottom: 12px">`,
    `<div style="font-size: 17px; font-weight: 600; margin-bottom: 6px">${escapeHtml(m.subject || "(no subject)")}</div>`,
    `<div><b>From:</b> ${escapeHtml(people(m.from))}</div>`,
    m.to.length ? `<div><b>To:</b> ${escapeHtml(people(m.to))}</div>` : "",
    m.cc.length ? `<div><b>Cc:</b> ${escapeHtml(people(m.cc))}</div>` : "",
    `<div><b>Date:</b> ${escapeHtml(m.date ? fullFormat.format(new Date(m.date * 1000)) : "")}</div>`,
    `<hr></div>`,
  ].join("");
  $("print-frame")?.remove();
  const frame = document.createElement("iframe");
  frame.id = "print-frame";
  frame.setAttribute("aria-hidden", "true");
  frame.tabIndex = -1;
  frame.setAttribute("sandbox", "allow-same-origin allow-modals");
  frame.style.cssText = "position: fixed; width: 0; height: 0; border: 0; left: -9999px";
  frame.srcdoc = m.html ? sandboxed(m.html) : `<!doctype html><meta charset="utf-8"><pre style="white-space: pre-wrap; font: 13px 'Segoe UI', sans-serif">${escapeHtml(m.text || "")}</pre>`;
  frame.addEventListener("load", () => {
    const doc = frame.contentDocument;
    if (!doc?.body) return;
    doc.body.insertAdjacentHTML("afterbegin", head);
    frame.contentWindow.print();
  }, { once: true });
  document.body.append(frame);
}

$("v-reply").addEventListener("click", () => answer("reply"));
$("v-reply-all").addEventListener("click", () => answer("replyAll"));
$("v-forward").addEventListener("click", () => answer("forward"));
$("v-save").addEventListener("click", () => act("msgview_save_eml", {}, "Saved"));
$("v-print").addEventListener("click", printMessage);

document.addEventListener("keydown", (e) => {
  const ctrl = e.ctrlKey || e.metaKey;
  if (e.key === "Escape") { window.__TAURI__.window.getCurrentWindow().close(); return; }
  if (!ctrl) return;
  const k = e.key.toLowerCase();
  const run = { r: () => answer(e.shiftKey ? "replyAll" : "reply"), f: () => answer("forward"), s: () => $("v-save").click(), p: printMessage, w: () => window.__TAURI__.window.getCurrentWindow().close() }[k];
  if (run) { e.preventDefault(); run(); }
});

(async () => {
  let opened;
  try {
    opened = await invoke("msgview_message");
  } catch (err) {
    $("v-subject").textContent = "This file can't be shown";
    status(String(err));
    for (const b of document.querySelectorAll("#v-bar button")) b.disabled = true;
    return;
  }
  current = opened.view;
  renderHead(current, opened.file);
  showPictureBar(current);
  // senders on the always-show list (Eudora's Settings ▸ Mail): their pictures load by themselves
  if (opened.pictures && current.web_pictures?.length && !current.warning) loadPictures(current);
  renderBody();
})();

// the views as tabs for screen readers and the keyboard (as a11y.js does in the main window)
(() => {
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
  const list = document.getElementById("v-views");
  list.setAttribute("aria-label", "Show the message as");
  makeTabs(list);
})();
