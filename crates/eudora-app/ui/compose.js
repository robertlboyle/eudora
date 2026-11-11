// The message window: a new message, a reply (to all), a forward or a saved draft. From is any
// of the user's addresses (their own, each shared mailbox); Send goes through that account.
"use strict";

// Inside the main window (an <iframe>: Eudora's inner windows, Gmail's docked boxes, the new
// Outlook's reading pane, Yahoo's tab) it uses the main window's connection and is closed by
// it; on its own it's a window of its own.
const embedded = window.parent !== window;
const TAURI = window.__TAURI__ || window.parent.__TAURI__;
const invoke = TAURI.core.invoke;
const $ = (id) => document.getElementById(id);
const params = new URLSearchParams(location.search);
const mode = params.get("mode") || "new";
const sourceId = params.get("id") ? Number(params.get("id")) : null;
// forwarding several messages at once: their ids
const sourceIds = (params.get("ids") || "").split(",").filter(Boolean).map(Number);
const frameId = params.get("frame");

const state = {
  draftId: mode === "draft" ? sourceId : null,
  inReplyTo: null,
  references: [],
  forwarded: [],   // { message, part, name, size, include }
  files: [],       // { name, mime, data (base64) }
  dirty: false,
  sending: false,
};

function el(tag, className, text) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

function icon(name) {
  const span = el("span", "icon");
  span.style.setProperty("--icon", `url("icons/${window.ICON_SET || "free"}/${name}.svg")`);
  return span;
}

function fillIcons() {
  for (const e of document.querySelectorAll("[data-icon]")) {
    e.classList.add("icon");
    e.style.setProperty("--icon", `url("icons/${window.ICON_SET || "free"}/${e.dataset.icon}.svg")`);
  }
}

function say(text, problem = false) {
  $("c-status").textContent = text;
  $("c-status").classList.toggle("problem", problem);
}

function sizeText(bytes) {
  if (bytes < 1024) return `${bytes} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

// the look chosen in the main window (same storage), and again whenever it's changed there
let skin = "eudora";
function applySkin(key) {
  try { skin = key || JSON.parse(localStorage.getItem("eudora.skin") || '"eudora"'); } catch { skin = "eudora"; }
  // (Eudora Classic is the Eudora look in Classic mode)
  document.body.className = skin === "eudora-classic" ? "skin-eudora classic compose" : `skin-${skin} compose`;
}
applySkin();
window.applySkin = applySkin; // (the main window calls it for the messages inside it)
window.addEventListener("storage", (e) => { if (e.key === "eudora.skin") applySkin(); });
if (!embedded) TAURI.event.listen("skin", (e) => applySkin(e.payload));

/** Between recipients: Outlook-style looks use semicolons, the others commas. */
function separator() {
  return skin.startsWith("lookout") ? "; " : ", ";
}

// ------------------------------------------------------------------ address completion

const suggest = { box: null, field: null, items: [], at: 0, timer: null, asked: "" };

function suggestBox() {
  if (!suggest.box) {
    suggest.box = el("div", "c-suggest");
    suggest.box.hidden = true;
    document.body.append(suggest.box);
    suggest.box.addEventListener("mousedown", (e) => {
      const row = e.target.closest(".c-sug");
      if (row) { e.preventDefault(); accept(Number(row.dataset.i)); }
    });
  }
  return suggest.box;
}

/** What's being typed in an address field: the part after the last comma or semicolon. */
function typing(field) {
  const v = field.value;
  const cut = Math.max(v.lastIndexOf(","), v.lastIndexOf(";"));
  return v.slice(cut + 1).trim();
}

function hideSuggestions() {
  if (suggest.box) suggest.box.hidden = true;
  suggest.items = [];
}

function showSuggestions(field, items) {
  const box = suggestBox();
  suggest.field = field;
  suggest.items = items;
  suggest.at = 0;
  if (!items.length) { box.hidden = true; return; }
  box.replaceChildren(...items.map((s, i) => {
    const row = el("div", `c-sug${i === 0 ? " on" : ""}`);
    row.dataset.i = i;
    row.append(el("span", "c-sug-name", s.name || s.address), el("span", "c-sug-address", s.name ? s.address : ""));
    return row;
  }));
  const r = field.getBoundingClientRect();
  box.style.left = `${r.left}px`;
  box.style.top = `${r.bottom + 2}px`;
  box.style.width = `${Math.max(r.width * 0.7, 320)}px`;
  box.hidden = false;
}

function accept(i) {
  const s = suggest.items[i];
  const field = suggest.field;
  if (!s || !field) return;
  const v = field.value;
  const cut = Math.max(v.lastIndexOf(","), v.lastIndexOf(";"));
  const before = cut >= 0 ? `${v.slice(0, cut + 1).trimEnd()} ` : "";
  const entry = s.name && s.name !== s.address ? `${s.name.includes(",") ? `"${s.name}"` : s.name} <${s.address}>` : s.address;
  field.value = `${before}${entry}${separator()}`;
  state.dirty = true;
  hideSuggestions();
  field.focus();
}

for (const id of ["c-to", "c-cc", "c-bcc"]) {
  const field = $(id);
  field.addEventListener("input", () => {
    clearTimeout(suggest.timer);
    const typed = typing(field);
    if (typed.length < 2 || typed.includes(">")) { hideSuggestions(); return; }
    suggest.timer = setTimeout(async () => {
      suggest.asked = typed;
      try {
        const items = await invoke("complete_address", { typed });
        if (suggest.asked === typed && typing(field) === typed) showSuggestions(field, items);
        // too few from your mail and Address Book: the organization's directory too, added
        // when it answers
        if (items.length < 8 && typed.trim().length >= 3) {
          const more = await invoke("complete_directory", { typed }).catch(() => []);
          const add = more.filter((m) => !items.some((i) => i.address.toLowerCase() === m.address.toLowerCase()));
          if (add.length && suggest.asked === typed && typing(field) === typed) showSuggestions(field, [...items, ...add].slice(0, 10));
        }
      } catch { hideSuggestions(); }
    }, 120);
  });
  field.addEventListener("keydown", (e) => {
    if (suggest.box?.hidden !== false || suggest.field !== field) return;
    const rows = [...suggest.box.children];
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      suggest.at = Math.max(0, Math.min(rows.length - 1, suggest.at + (e.key === "ArrowDown" ? 1 : -1)));
      rows.forEach((r, i) => r.classList.toggle("on", i === suggest.at));
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      accept(suggest.at);
    } else if (e.key === "Escape") {
      e.stopPropagation();
      hideSuggestions();
    }
  });
  field.addEventListener("blur", () => setTimeout(hideSuggestions, 150));
}

function setTitle(subject) {
  const what = { reply: "Reply", replyAll: "Reply to All", forward: "Forward", draft: "Draft" }[mode] || "New Message";
  const title = `${subject ? subject : what} – Eudora`;
  document.title = title;
  if (embedded) window.parent.composeTitle?.(frameId, subject || what);
  else TAURI.window.getCurrentWindow().setTitle(title).catch(() => {});
}

/** Closes this message: its inner window / box / pane in the main window, or its own window. */
function closeWindow() {
  if (embedded) window.parent.closeCompose?.(frameId);
  else TAURI.window.getCurrentWindow().close();
}

function renderFiles() {
  const box = $("c-files");
  const chips = [];
  state.forwarded.forEach((f) => {
    const chip = el("span", `chip${f.include ? "" : " off"}`);
    chip.title = f.include ? "Forwarded with the message (click × to leave it out)" : "Left out (click + to include it again)";
    const toggle = el("button", "", f.include ? "×" : "+");
    toggle.addEventListener("click", () => { f.include = !f.include; state.dirty = true; renderFiles(); });
    chip.append(icon("paperclip"), el("span", "", `${f.name} (${sizeText(f.size)})`), toggle);
    chips.push(chip);
  });
  state.files.forEach((f, i) => {
    const chip = el("span", "chip");
    const remove = el("button", "", "×");
    remove.title = "Remove";
    remove.addEventListener("click", () => { state.files.splice(i, 1); state.dirty = true; renderFiles(); });
    chip.append(icon("paperclip"), el("span", "", `${f.name} (${sizeText(Math.floor(f.data.length * 3 / 4))})`), remove);
    chips.push(chip);
  });
  box.replaceChildren(...chips);
}

function draft() {
  const from = $("c-from").selectedOptions[0];
  const priority = Number($("c-priority").value);
  return {
    from: from?.value || "",
    from_name: from?.dataset.name || "",
    to: $("c-to").value,
    cc: $("c-cc").value,
    bcc: $("c-bcc").value,
    subject: $("c-subject").value,
    html: $("c-body").innerHTML,
    text: state.plain ? plainText($("c-body")) : $("c-body").innerText,
    plain: !!state.plain,
    in_reply_to: state.inReplyTo,
    references: state.references,
    attachments: state.files,
    forwarded: state.forwarded.filter((f) => f.include).map(({ message, part, name, size }) => ({ message, part, name, size })),
    priority: priority === 3 ? null : priority,
    sender: null,
    origin: state.origin || null,
    redirect_of: state.redirectOf || null,
  };
}

/** Plain text or HTML (formatted). A plain-text message is sent as text only; the formatting
 *  buttons rest meanwhile. */
function setPlain(on) {
  state.plain = on;
  $("c-format").value = on ? "plain" : "html";
  document.body.classList.toggle("plain-text", on);
  for (const b of document.querySelectorAll("#c-bar [data-cmd], #c-link")) b.disabled = on;
}

/** The text as sent in plain text: quoted parts (blockquotes) as "> " lines, as Eudora quotes. */
function plainText(root) {
  // innerText needs the text laid out: a copy, off screen, as wide as the original
  const copy = root.cloneNode(true);
  copy.removeAttribute("id");
  copy.style.cssText = `position:absolute;left:-100000px;top:0;width:${root.clientWidth}px`;
  document.body.append(copy);
  try {
    for (const quote of [...copy.querySelectorAll("blockquote")].reverse()) {
      const lines = quote.innerText.replace(/\n+$/, "").split("\n").map((l) => (l.startsWith(">") ? `>${l}` : `> ${l}`));
      const block = document.createElement("div");
      block.style.whiteSpace = "pre-wrap";
      block.textContent = lines.join("\n");
      quote.replaceWith(block);
    }
    for (const img of copy.querySelectorAll("img")) img.replaceWith(img.alt ? `[${img.alt}]` : "");
    return copy.innerText;
  } finally {
    copy.remove();
  }
}

/** Puts text into the body as plain lines (no formatting left). */
function bodyAsText(text) {
  const body = $("c-body");
  body.replaceChildren();
  for (const line of text.split("\n")) {
    const div = document.createElement("div");
    div.textContent = line;
    if (!line) div.append(document.createElement("br"));
    body.append(div);
  }
}

$("c-format").addEventListener("change", async () => {
  if ($("c-format").value === "html") {
    setPlain(false);
    state.dirty = true;
    return;
  }
  const pictures = $("c-body").querySelectorAll("img").length;
  if (pictures && !(await askYesNo("Switch to plain text", `Plain text can't show pictures in the message: the ${pictures === 1 ? "picture" : `${pictures} pictures`} in it will be removed (attach ${pictures === 1 ? "it" : "them"} instead to send ${pictures === 1 ? "it" : "them"}). The formatting goes too.`, "Switch to plain text", "Keep HTML"))) {
    $("c-format").value = "html";
    return;
  }
  bodyAsText(plainText($("c-body")));
  setPlain(true);
  state.dirty = true;
});

function load(d) {
  for (const o of $("c-from").options) o.selected = o.value.toLowerCase() === (d.from || "").toLowerCase();
  $("c-to").value = d.to || "";
  $("c-cc").value = d.cc || "";
  $("c-bcc").value = d.bcc || "";
  $("c-subject").value = d.subject || "";
  $("c-body").innerHTML = d.html || "";
  state.inReplyTo = d.in_reply_to || null;
  state.references = d.references || [];
  state.files = d.attachments || [];
  state.origin = d.origin || state.origin || null;
  state.redirectOf = d.redirect_of || null;
  if (state.redirectOf) say(`Redirecting: it goes as "${state.redirectOf} (by way of you)", and replies go to them.`);
  state.forwarded = (d.forwarded || []).map((f) => ({ ...f, include: true }));
  $("c-priority").value = String(d.priority || 3);
  setPlain(!!d.plain);
  renderFiles();
  setTitle(d.subject);
}

async function start() {
  fillIcons();
  try {
    // both at once
    const [, { seed, identities }] = await Promise.all([
      loadSignatures(),
      invoke("compose_seed", { mode: mode === "draft" ? "new" : mode, id: mode === "draft" ? null : sourceId, ids: sourceIds }),
    ]);
    const from = $("c-from");
    for (const i of identities) {
      const o = el("option", "", `${i.name && i.name !== i.address ? `${i.name} <${i.address}>` : i.address} · ${i.kind}${i.can_send ? "" : " (can't send yet)"}`);
      o.value = i.address;
      o.dataset.name = i.name && i.name !== i.address ? i.name : "";
      o.disabled = !i.can_send;
      from.append(o);
    }
    // answer from the address it was sent to, when that's one we can send from
    const usable = identities.filter((i) => i.can_send);
    const pick = usable.find((i) => seed.to_me && i.address.toLowerCase() === seed.to_me.toLowerCase()) || usable.find((i) => i.kind === "Microsoft 365") || usable[0];
    if (pick) from.value = pick.address;
    if (mode === "draft") {
      const saved = (await invoke("drafts")).find((d) => d.id === sourceId);
      if (saved) load(saved.draft);
    } else {
      // replying or forwarding marks the original Replied / Forwarded once this is sent
      const kind = { reply: "reply", replyAll: "reply", forward: "forward", redirect: "redirect" }[mode];
      state.origin = kind && sourceId ? { message: sourceId, kind } : null;
      load({ ...seed, from: pick?.address });
      // a redirect passes the message on as it is: no signature
      if (mode !== "redirect") useSignature(signatureFor(pick?.address));
    }
    // a mailto: link clicked in a message: its address (and subject, when it has one)
    if (params.get("to")) $("c-to").value = params.get("to");
    if (params.get("cc")) $("c-cc").value = params.get("cc");
    if (params.get("bcc")) {
      $("c-bcc").value = params.get("bcc");
      $("c-bcc-row").hidden = false;
    }
    if (params.get("subject")) {
      $("c-subject").value = params.get("subject");
      setTitle(params.get("subject"));
    }
    // text to start with (an event forwarded from the calendar: its details), above the signature
    if (params.get("body")) {
      const block = document.createElement("div");
      for (const line of params.get("body").split("\n")) {
        const div = document.createElement("div");
        div.textContent = line;
        if (!line) div.append(document.createElement("br"));
        block.append(div);
      }
      $("c-body").prepend(document.createElement("br"), block);
    }
    if (!usable.length) say("No account can send yet: sign in to Microsoft 365 (eudora-cli graph login --archive).", true);
    (["new", "forward", "redirect"].includes(mode) ? $("c-to") : $("c-body")).focus();
    if (!["new", "forward", "redirect", "again"].includes(mode)) placeCaretAtStart($("c-body"));
  } catch (err) {
    say(`Couldn't start the message: ${err}`, true);
  }
}

function placeCaretAtStart(node) {
  const range = document.createRange();
  range.setStart(node, 0);
  range.collapse(true);
  const sel = getSelection();
  sel.removeAllRanges();
  sel.addRange(range);
}

async function send() {
  if (state.sending) return;
  const d = draft();
  if (!d.to.trim() && !d.cc.trim() && !d.bcc.trim()) { say("Who is it to? (To, Cc or Bcc)", true); $("c-to").focus(); return; }
  if (!d.subject.trim() && !(await askYesNo("No subject", "Send it without a subject?", "Send without a subject", "Add a subject"))) { $("c-subject").focus(); return; }
  state.sending = true;
  $("c-send").disabled = true;
  say(`Sending from ${d.from}…`);
  try {
    // into the Out mailbox, going now in the background: the window closes at once (how it went
    // shows in the main window's status bar; one that can't go yet waits in Out and is retried)
    // (the autosaved draft is removed once the message is in Out)
    await invoke("send_message", { draft: d, draftId: state.draftId ?? null });
    state.dirty = false;
    if (embedded) window.parent.mainStatus?.(`Sending from ${d.from}…`);
    closeWindow();
  } catch (err) {
    say(`Not sent: ${err}`, true);
    $("c-send").disabled = false;
    state.sending = false;
  }
}

/** Send Later: the message waits in the Out mailbox until then (Eudora or its download sends it). */
function localInput(date) {
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function showLater() {
  const box = $("c-later-box");
  box.hidden = !box.hidden;
  if (!box.hidden && !$("c-later-at").value) $("c-later-at").value = localInput(new Date(Date.now() + 3600_000));
}

async function queueLater() {
  if (state.sending) return;
  const d = draft();
  if (!d.to.trim() && !d.cc.trim() && !d.bcc.trim()) { say("Who is it to? (To, Cc or Bcc)", true); $("c-to").focus(); return; }
  const when = new Date($("c-later-at").value);
  if (isNaN(when)) { say("Choose when it goes.", true); return; }
  if (when.getTime() < Date.now() - 60_000) { say("That time has passed: choose a later one, or Send.", true); return; }
  if (!d.subject.trim() && !(await askYesNo("No subject", "Queue it without a subject?", "Queue without a subject", "Add a subject"))) { $("c-subject").focus(); return; }
  state.sending = true;
  try {
    await invoke("queue_message", { draft: d, sendAt: Math.floor(when.getTime() / 1000) });
    if (state.draftId != null) await invoke("delete_draft", { id: state.draftId }).catch(() => {});
    state.dirty = false;
    const at = when.toLocaleString([], { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
    say(`Queued for ${at} (Mailbox ▸ Out).`);
    if (embedded) window.parent.mainStatus?.(`Queued for ${at}: it waits in the Out mailbox.`);
    setTimeout(closeWindow, 600);
  } catch (err) {
    say(`Not queued: ${err}`, true);
    state.sending = false;
  }
}

async function save() {
  try {
    state.draftId = await invoke("save_draft", { id: state.draftId, draft: draft() });
    state.dirty = false;
    state.keptByUser = true;
    say(`Draft saved at ${new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}.`);
  } catch (err) {
    say(`The draft wasn't saved: ${err}`, true);
  }
}

function attach() {
  $("c-file").click();
}

/** Files attached: chosen with Attach File, dropped on the window, or pasted. */
async function attachFiles(files) {
  let n = 0;
  for (const file of files) {
    const data = await new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result).split(",")[1] || "");
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
    // (a picture pasted from the clipboard has no name of its own)
    const name = file.name && file.name !== "image.png" ? file.name : `Pasted picture ${new Date().toLocaleString("en-US").replace(/[/:,]/g, "-")}.${(file.type.split("/")[1] || "png").replace("jpeg", "jpg")}`;
    state.files.push({ name, mime: file.type || "application/octet-stream", data });
    n++;
  }
  if (!n) return;
  state.dirty = true;
  renderFiles();
  say(`${n === 1 ? "1 file" : `${n} files`} attached.`);
}

$("c-file").addEventListener("change", async () => {
  await attachFiles($("c-file").files);
  $("c-file").value = "";
});

// drag files from Explorer / Finder, or an attachment from a message being read, onto the window
const EUDORA_PART = "application/x-eudora-attachment";
/** An attachment dragged from a message in Eudora: { message, part, name, size }. */
function draggedPart(dt) {
  try {
    const raw = dt.getData(EUDORA_PART) || dt.getData("text/plain");
    const v = raw.startsWith("eudora-attachment:") ? JSON.parse(raw.slice(18)) : raw.startsWith("{") ? JSON.parse(raw) : null;
    return v && v.message && v.part ? v : null;
  } catch { return null; }
}
document.addEventListener("dragover", (e) => {
  const dt = e.dataTransfer;
  if (!dt || ![...dt.types].some((t) => t === "Files" || t === EUDORA_PART || t === "text/plain")) return;
  e.preventDefault();
  dt.dropEffect = "copy";
  document.body.classList.add("dropping");
});
document.addEventListener("dragleave", (e) => { if (!e.relatedTarget) document.body.classList.remove("dropping"); });
document.addEventListener("drop", async (e) => {
  document.body.classList.remove("dropping");
  const dt = e.dataTransfer;
  if (!dt) return;
  if (dt.files?.length) {
    e.preventDefault();
    await attachFiles(dt.files);
    return;
  }
  const part = draggedPart(dt);
  if (part) {
    e.preventDefault();
    if (!state.forwarded.some((f) => f.message === part.message && f.part === part.part)) {
      state.forwarded.push({ message: part.message, part: part.part, name: part.name, size: part.size || 0, include: true });
      state.dirty = true;
      renderFiles();
      say(`Attached: ${part.name}`);
    }
  }
});

// paste: a picture pasted into the text goes into it; copied files, and pictures pasted anywhere
// else (the To line…), attach
document.addEventListener("paste", async (e) => {
  const clip = e.clipboardData;
  const inBody = e.target instanceof Node && $("c-body").contains(e.target);
  const files = [...(clip?.files || [])];
  if (!files.length) {
    // formatted text pasted into a plain-text message comes in as its words only
    if (state.plain && inBody && clip?.types.includes("text/html")) {
      e.preventDefault();
      document.execCommand("insertText", false, clip.getData("text/plain"));
    }
    return;
  }
  e.preventDefault();
  const pictures = inBody ? files.filter((f) => f.type.startsWith("image/")) : [];
  const others = files.filter((f) => !pictures.includes(f));
  if (others.length) await attachFiles(others);
  if (!pictures.length) return;
  // where the picture goes (the question below takes the focus away)
  const sel = getSelection();
  const at = sel.rangeCount && $("c-body").contains(sel.getRangeAt(0).startContainer) ? sel.getRangeAt(0).cloneRange() : null;
  if (state.plain) {
    const one = pictures.length === 1;
    const answer = await ask({
      title: "Paste a picture into a plain-text message",
      text: `This message is plain text, which can't show ${one ? "a picture" : "pictures"} in it. Switch it to HTML (formatted) and paste ${one ? "the picture" : "them"} where you're typing, or attach ${one ? "it" : "them"} as ${one ? "a file" : "files"}?`,
      buttons: [{ label: one ? "Attach it as a file" : "Attach them as files", value: "attach" }, { label: "Switch to HTML and paste", value: "html", primary: true }],
    });
    if (answer === "attach") await attachFiles(pictures);
    if (answer !== "html") return;
    setPlain(false);
  }
  $("c-body").focus();
  if (at) {
    sel.removeAllRanges();
    sel.addRange(at);
  }
  for (const file of pictures) {
    const url = await new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result));
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
    // (a data: URL has no quotes in it; the core sends it as a picture of its own, cid:)
    document.execCommand("insertHTML", false, `<img src="${url}" style="max-width:100%">`);
  }
  state.dirty = true;
  say(pictures.length === 1 ? "Picture pasted into the message." : `${pictures.length} pictures pasted into the message.`);
});

/** Saves the message into Drafts every 20 s while it changes, silently. */
const AUTOSAVE_EVERY = 20_000;
setInterval(async () => {
  if (!state.dirty || state.sending || state.autosaving) return;
  state.autosaving = true;
  try {
    state.draftId = await invoke("save_draft", { id: state.draftId, draft: draft() });
    state.dirty = false;
    state.autosaved = true;
  } catch (err) {
    console.warn("autosave", err); // (tried again in 20 s)
  } finally {
    state.autosaving = false;
  }
}, AUTOSAVE_EVERY);

async function close() {
  // a new message only autosave kept: asked about as unsaved, and "Don't save" removes that copy
  const onlyAutosaved = state.autosaved && !state.keptByUser && mode !== "draft";
  if ((state.dirty || onlyAutosaved) && !state.sending) {
    const choice = await ask({
      title: "Save this message?",
      text: "It hasn't been sent. Save it as a draft before closing?",
      buttons: [
        { label: "Keep writing", value: "keep" },
        { label: "Don't save", value: "discard" },
        { label: "Save draft", value: "save", primary: true },
      ],
    });
    if (choice === "keep" || choice === null) return;
    if (choice === "save") await save();
    if (choice === "discard" && onlyAutosaved && state.draftId != null) await invoke("delete_draft", { id: state.draftId }).catch(() => {});
  }
  closeWindow();
}

/** Pop out: the message moves to a window of its own (saved as a draft on the way). */
async function popOut() {
  await save();
  if (state.draftId == null) return;
  state.dirty = false;
  await invoke("open_compose", { mode: "draft", id: state.draftId });
  closeWindow();
}

// what the main window asks of a message inside it (its menu commands)
/** Edit ▸ Select All (Ctrl+A): everything in the field being typed in. */
function selectAll() {
  const f = document.activeElement;
  if (f && (f.tagName === "INPUT" || f.tagName === "TEXTAREA")) f.select();
  else document.execCommand("selectAll");
}

// ------------------------------------------------------------------ signatures

let signatures = { list: [], defaults: {}, fallback: "" };

async function loadSignatures() {
  try { signatures = await invoke("signatures"); } catch { /* none */ }
  $("c-signature").replaceChildren(el("option", "", "No signature"), ...signatures.list.map((s) => {
    const o = el("option", "", s.name);
    o.value = s.name;
    return o;
  }));
  $("c-signature").firstChild.value = "";
}

/** The From address's own signature (else the one for every address). */
function signatureFor(address) {
  const name = signatures.defaults[(address || "").toLowerCase()] ?? signatures.fallback;
  return signatures.list.find((s) => s.name === name) || null;
}

/** Puts signature `sig` in the message (null: none), replacing the one there. A new message
 *  ends with it; a reply or forward has it under where you write, above the original. */
function useSignature(sig) {
  const body = $("c-body");
  body.querySelector(".eudora-signature")?.remove();
  $("c-signature").value = sig ? sig.name : "";
  if (!sig) return;
  const box = document.createElement("div");
  box.className = "eudora-signature";
  box.innerHTML = sig.html;
  const first = body.firstElementChild;
  if (mode === "new" || mode === "again" || !first) body.append(box);
  else first.after(box);
}

$("c-signature").addEventListener("change", () => {
  useSignature(signatures.list.find((s) => s.name === $("c-signature").value) || null);
  state.dirty = true;
});
$("c-from").addEventListener("change", () => {
  // a signature still as it was put in follows the From address; one you edited stays
  const now = $("c-body").querySelector(".eudora-signature");
  const was = signatures.list.find((s) => s.name === $("c-signature").value);
  if (!now || (was && now.innerHTML === was.html)) useSignature(signatureFor($("c-from").value));
});

// ------------------------------------------------------------------ Find Text, Check Spelling

const finder = textFinder(() => [{ doc: document, root: $("c-body") }]);
const find = findBar({ bar: "c-find", input: "c-find-text", count: "c-find-count", prev: "c-find-prev", next: "c-find-next", close: "c-find-close" }, finder, () => $("c-body").focus());

const spelling = { items: [], at: -1, ignored: new Set() };

/** The text nodes of the message being written, except the original quoted below a reply. */
function spellNodes() {
  const nodes = [];
  const walker = document.createTreeWalker($("c-body"), NodeFilter.SHOW_TEXT, {
    acceptNode: (n) => (n.parentElement?.closest("blockquote, .eudora-signature") ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
  });
  for (let n = walker.nextNode(); n; n = walker.nextNode()) if (n.nodeValue.trim()) nodes.push(n);
  return nodes;
}

async function checkSpelling() {
  const nodes = spellNodes();
  const subject = $("c-subject");
  let found;
  try { found = await invoke("spell_check", { texts: [subject.value, ...nodes.map((n) => n.nodeValue)] }); } catch (err) { say(`Spelling: ${err}`, true); return; }
  spelling.items = [];
  found.forEach((list, i) => {
    for (const m of list) spelling.items.push({ ...m, node: i === 0 ? null : nodes[i - 1] });
  });
  spelling.at = -1;
  if (!spelling.items.length) { say("No spelling mistakes found."); return; }
  if (!$("c-spell-box").open) $("c-spell-box").showModal();
  nextMistake();
}

function nextMistake() {
  spelling.at += 1;
  while (spelling.at < spelling.items.length && spelling.ignored.has(spelling.items[spelling.at].word.toLowerCase())) spelling.at += 1;
  const m = spelling.items[spelling.at];
  if (!m) { $("c-spell-box").close(); say("Spelling checked."); return; }
  const text = m.node ? m.node.nodeValue : $("c-subject").value;
  $("c-spell-word").textContent = m.action === "delete" ? `“${m.word}” is repeated` : `Not in the dictionary: “${m.word}”`;
  $("c-spell-context").textContent = `…${text.slice(Math.max(0, m.start - 40), m.start)}[${m.word}]${text.slice(m.start + m.len, m.start + m.len + 40)}…`;
  $("c-spell-to").value = m.action === "delete" ? "" : m.suggestions[0] || m.word;
  $("c-spell-list").replaceChildren(...m.suggestions.map((s) => el("option", "", s)));
  $("c-spell-list").selectedIndex = m.suggestions.length ? 0 : -1;
  if (m.node) {
    const r = document.createRange();
    r.setStart(m.node, m.start);
    r.setEnd(m.node, m.start + m.len);
    const sel = getSelection();
    sel.removeAllRanges();
    sel.addRange(r);
    m.node.parentElement?.scrollIntoView({ block: "center" });
  }
}

$("c-spell-list").addEventListener("change", () => { $("c-spell-to").value = $("c-spell-list").value; });
$("c-spell-list").addEventListener("dblclick", () => $("c-spell-change").click());
$("c-spell-change").addEventListener("click", () => {
  const m = spelling.items[spelling.at];
  if (!m) return;
  const to = $("c-spell-to").value;
  // a repeated word goes with the space before it
  const start = m.action === "delete" && !to ? Math.max(0, m.start - 1) : m.start;
  const len = m.len + (m.start - start);
  if (m.node) {
    m.node.nodeValue = m.node.nodeValue.slice(0, start) + to + m.node.nodeValue.slice(start + len);
  } else {
    const s = $("c-subject").value;
    $("c-subject").value = s.slice(0, start) + to + s.slice(start + len);
  }
  // later mistakes in the same text move with the change
  const shift = to.length - len;
  for (const later of spelling.items.slice(spelling.at + 1)) if (later.node === m.node && later.start > m.start) later.start += shift;
  state.dirty = true;
  nextMistake();
});
$("c-spell-ignore").addEventListener("click", nextMistake);
$("c-spell-ignore-all").addEventListener("click", () => {
  const m = spelling.items[spelling.at];
  if (m) spelling.ignored.add(m.word.toLowerCase());
  nextMistake();
});
$("c-spell-add").addEventListener("click", async () => {
  const m = spelling.items[spelling.at];
  if (!m) return;
  try { await invoke("spell_add", { word: m.word }); } catch (err) { say(`Adding the word: ${err}`, true); }
  spelling.ignored.add(m.word.toLowerCase());
  nextMistake();
});
$("c-spell-done").addEventListener("click", () => $("c-spell-box").close());
$("c-spell").addEventListener("click", checkSpelling);
// Dictate: the system's voice typing types where the cursor is, so the button keeps the cursor
// where it was (in a field or the text), else puts it in the text, then starts it
$("c-dictate").addEventListener("mousedown", (e) => e.preventDefault());
$("c-dictate").addEventListener("click", async () => {
  const at = document.activeElement;
  if (!at || !(at.isContentEditable || at.tagName === "INPUT" || at.tagName === "TEXTAREA")) $("c-body").focus();
  try { await invoke("dictate"); } catch (err) { say(String(err), true); }
});
document.addEventListener("keydown", (e) => {
  if (e.key === "F3" && e.shiftKey && !embedded) { e.preventDefault(); find.again(-1); } // (F3 and Ctrl+6 are the menu's)
});

window.composeAction = (id) => ({
  send,
  save,
  attach,
  close,
  "send-later": showLater,
  "pop-out": popOut,
  "select-all": selectAll,
  "find-text": () => find.open(),
  "find-again": () => find.again(1),
  "find-previous": () => find.again(-1),
  "check-spelling": checkSpelling,
}[id] || (() => {}))();

$("c-popout").hidden = !embedded;
$("c-popout").addEventListener("click", popOut);
$("c-send").addEventListener("click", send);
$("c-later").addEventListener("click", showLater);
$("c-later-go").addEventListener("click", queueLater);
$("c-later-hour").addEventListener("click", () => { $("c-later-at").value = localInput(new Date(Date.now() + 3600_000)); });
$("c-later-morning").addEventListener("click", () => {
  const d = new Date();
  d.setDate(d.getDate() + 1);
  d.setHours(8, 0, 0, 0);
  $("c-later-at").value = localInput(d);
});
$("c-save").addEventListener("click", save);
$("c-attach").addEventListener("click", attach);
$("c-link").addEventListener("click", async () => {
  // (the question takes the focus: the text chosen for the link is put back first)
  const sel = window.getSelection();
  const range = sel.rangeCount ? sel.getRangeAt(0).cloneRange() : null;
  const url = await askText("Add a link", "Link to (web address):", "https://", "Add link");
  if (!url) return;
  if (range) { sel.removeAllRanges(); sel.addRange(range); }
  document.execCommand("createLink", false, url);
});
for (const b of document.querySelectorAll("[data-cmd]")) {
  b.addEventListener("mousedown", (e) => e.preventDefault()); // keep the selection in the text
  b.addEventListener("click", () => document.execCommand(b.dataset.cmd));
}
for (const id of ["c-to", "c-cc", "c-bcc", "c-subject", "c-body"]) $(id).addEventListener("input", () => { state.dirty = true; });
$("c-subject").addEventListener("input", () => setTitle($("c-subject").value));

// menu commands meant for this window only (File ▸ Save Draft, Message ▸ Send Immediately /
// Attach File, Close); a message inside the main window gets them from it
if (!embedded) {
  TAURI.webviewWindow.getCurrentWebviewWindow().listen("menu", (e) => window.composeAction(e.payload));
}
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !e.defaultPrevented && !$("c-spell-box").open && suggest.box?.hidden !== false) close();
});

start();
