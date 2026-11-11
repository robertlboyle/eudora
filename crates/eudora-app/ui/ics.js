// A calendar file (.ics) opened with Eudora: its events, each with Add to my calendar (into the
// calendar chosen). Adding never sends the people in it anything: it's the user's own copy.
"use strict";

const TAURI = window.__TAURI__;
const invoke = TAURI.core.invoke;
const $ = (id) => document.getElementById(id);
const fileNumber = Number(new URLSearchParams(location.search).get("file"));

function el(tag, className, text) {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

// the look chosen in the main window (same storage)
function applySkin(key) {
  let skin = "eudora";
  try { skin = key || JSON.parse(localStorage.getItem("eudora.skin") || '"eudora"'); } catch { skin = "eudora"; }
  document.body.className = skin === "eudora-classic" ? "skin-eudora classic ics-file" : `skin-${skin} ics-file`;
}
applySkin();
window.addEventListener("storage", (e) => { if (e.key === "eudora.skin") applySkin(); });
TAURI.event.listen("skin", (e) => applySkin(e.payload));

const day = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric", year: "numeric" });
const dayTime = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit" });
const time = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });

/** When an event is, in this computer's time (all day: its dates as written). */
function when(e) {
  const s = new Date(e.start * 1000);
  const end = new Date(e.end * 1000);
  if (e.all_day) {
    const utc = (d) => new Date(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
    const last = utc(new Date((e.end - 86_400) * 1000));
    const first = utc(s);
    return first.getTime() === last.getTime() ? `${day.format(first)} (all day)` : `${day.format(first)} – ${day.format(last)} (all day)`;
  }
  const sameDay = s.toDateString() === end.toDateString();
  return sameDay ? `${dayTime.format(s)} – ${time.format(end)}` : `${dayTime.format(s)} – ${dayTime.format(end)}`;
}

function who(p) {
  return p[0] ? `${p[0]} <${p[1]}>` : p[1];
}

function row(box, label, text) {
  if (!text) return;
  const r = el("div", "ics-row");
  r.append(el("b", null, label), document.createTextNode(text));
  box.append(r);
}

async function show() {
  let file;
  try {
    file = await invoke("ics_file", { file: fileNumber });
  } catch (err) {
    $("i-about").textContent = String(err);
    return;
  }
  $("i-name").textContent = file.name;
  document.title = `${file.name} – Eudora`;
  const what = { REQUEST: "An invitation. ", CANCEL: "A cancellation. ", REPLY: "An answer to an invitation. " }[file.method] || "";
  $("i-about").textContent = file.events.length
    ? `${what}${file.events.length === 1 ? "One event" : `${file.events.length.toLocaleString()} events`} in it. Times are shown in this computer's time.`
    : "There are no events in this file that Eudora can read.";

  // the calendars an event can be added to (the default one first)
  let calendars = [];
  try { calendars = (await invoke("calendars_list")).filter((c) => c.can_edit); } catch { calendars = []; }
  calendars.sort((a, b) => b.is_default - a.is_default);
  const pick = $("i-calendar");
  for (const c of calendars) {
    const o = el("option", null, `${c.name} (${c.address})`);
    o.value = c.id;
    pick.append(o);
  }
  if (!calendars.length) {
    pick.disabled = true;
    pick.append(el("option", null, "No calendar you can add to: add a Microsoft 365 or Google account first"));
  }

  const list = $("i-events");
  file.events.forEach((e, i) => {
    const box = el("div", "ics-event");
    box.append(el("h2", null, e.summary));
    row(box, "When", when(e));
    row(box, "Where", e.location);
    if (e.organizer) row(box, "Organizer", who(e.organizer));
    if (e.attendees.length) row(box, "People", e.attendees.map(who).join("; "));
    if (e.repeats) row(box, "Repeats", "Yes: only the first is added");
    if (e.note) box.append(el("p", "p-note", e.note));
    if (e.description) box.append(el("div", "ics-notes", e.description));
    const actions = el("div", "ics-actions");
    const add = el("button", "primary", "Add to my calendar");
    add.disabled = !calendars.length;
    add.addEventListener("click", async () => {
      add.disabled = true;
      try {
        await invoke("ics_add", { file: fileNumber, event: i, calendar: Number(pick.value) });
        const chosen = pick.options[pick.selectedIndex]?.textContent || "your calendar";
        add.replaceWith(el("span", "ics-done", `Added to ${chosen}.`));
      } catch (err) {
        add.disabled = false;
        tell("Adding the event", String(err));
      }
    });
    actions.append(add);
    box.append(actions);
    list.append(box);
  });
}

show();
