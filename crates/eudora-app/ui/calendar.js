// The calendar (phase begun 2026-10-19): in the main window, in place of the message list and
// reader when Calendar is chosen in the sidebar. Day, Week, Month and Agenda over every
// calendar, each in its own color with a tick to show or hide it. Read-only for now: editing
// and invitations come next. Uses app.js's helpers ($, el, icon, invoke, store, status…).
//
// Times: events are kept in UTC and shown in the computer's time. An all-day event is kept as
// midnight UTC of its first day to midnight UTC after its last, and shown on those dates
// wherever the computer is (never shifted to the evening before).

const calState = {
  on: false,
  mode: store.get("calMode", "week"),
  day: calMidnight(new Date()),
  weekStart: store.get("weekStart", 0), // 0 Sunday (user's default), 1 Monday
  business: store.get("calBusiness", true), // Day and Week: the business day, else all 24 hours
  calendars: null,
  rows: [],
  scrolled: false,
  query: "",
};

/** The business day (Settings ▸ Calendar). */
function calDayFrom() {
  return Math.min(23, Math.max(0, Number(store.get("calDayFrom", 7))));
}
function calDayTo() {
  return Math.max(calDayFrom() + 1, Math.min(24, Number(store.get("calDayTo", 19))));
}
function calSetBusiness(on) {
  calState.business = on;
  store.set("calBusiness", on);
  calRender();
}

function calMidnight(d) {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}
function calAddDays(d, n) {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
}
function calSameDay(a, b) {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}
function calWeekStart(d) {
  return calAddDays(d, -((d.getDay() - calState.weekStart + 7) % 7));
}

/** The days the view shows: [first, after). */
function calRange() {
  const d = calState.day;
  if (calState.mode === "day") return [d, calAddDays(d, 1)];
  if (calState.mode === "week") {
    const s = calWeekStart(d);
    return [s, calAddDays(s, 7)];
  }
  if (calState.mode === "month") {
    const s = calWeekStart(new Date(d.getFullYear(), d.getMonth(), 1));
    return [s, calAddDays(s, 42)];
  }
  return [d, calAddDays(d, 60)]; // agenda: two months from the day chosen
}

/** An event's place on the calendar, in this computer's dates: all-day ones by their UTC dates. */
function calSpan(row) {
  if (row.all_day) {
    const s = new Date(row.start * 1000);
    const e = new Date(row.end * 1000);
    const first = new Date(s.getUTCFullYear(), s.getUTCMonth(), s.getUTCDate());
    let after = new Date(e.getUTCFullYear(), e.getUTCMonth(), e.getUTCDate());
    if (after <= first) after = calAddDays(first, 1);
    return { start: first, end: after, allDay: true };
  }
  const start = new Date(row.start * 1000);
  const end = new Date(Math.max(row.end, row.start + 60) * 1000);
  // a timed event of a day or more goes with the all-day ones
  return { start, end, allDay: row.end - row.start >= 86_400 };
}

function calColor(row) {
  return calState.calendars?.find((c) => c.id === row.calendar)?.color || "#3b7dd8";
}
function calShown(row) {
  const c = calState.calendars?.find((x) => x.id === row.calendar);
  return !c || c.shown;
}

async function calLoadCalendars() {
  try { calState.calendars = await invoke("calendars_list"); } catch (err) { calState.calendars = []; console.warn("calendars", err); }
}

/** The sidebar's Calendar line (under Contacts), and each calendar with its color when open. */
function calendarSidebar() {
  if (calState.calendars === null) {
    calState.calendars = [];
    calLoadCalendars().then(renderSidebar);
  }
  const list = calState.calendars;
  const open = sideItem({
    key: "cal:all", depth: 0, name: "Calendar", iconName: "calendar-days", children: list.length > 0, cls: "accounts",
    tip: list.length ? `${list.length} calendars (click: the calendar)` : "Your calendars come in with the download (Microsoft 365 / Outlook.com)",
  });
  if (calState.on) $("folders").lastElementChild.classList.add("on");
  if (!open) return;
  for (const c of list) {
    sideItem({
      key: `cal:${c.id}`, depth: 1, name: list.filter((x) => x.name === c.name).length > 1 ? `${c.name} (${c.address})` : c.name, iconName: "calendar-days",
      tip: `${c.name} · ${c.address}${c.is_default ? " (default)" : ""}: ${number.format(c.events)} events. Click to ${c.shown ? "hide" : "show"} it.`,
    });
    const line = $("folders").lastElementChild;
    const swatch = el("span", "cal-swatch");
    swatch.style.background = c.shown ? c.color : "transparent";
    swatch.style.borderColor = c.color;
    line.querySelector(".icon")?.replaceWith(swatch);
    if (!c.shown) line.classList.add("cal-hidden");
  }
}

/** Right-click on a calendar in the sidebar: its color. A palette, any color through the system's picker, or the calendar's own
 *  color again. Kept in Eudora only (Microsoft and Google keep theirs). */
const CAL_PALETTE = ["#d13438", "#e8590c", "#f2c94c", "#7fba00", "#2e9e4f", "#00a3a3", "#0078d4", "#3b5bdb", "#8764b8", "#c239b3", "#e3008c", "#8a6d3b", "#69797e", "#4a4a4a"];
function calendarContext(key, e) {
  const m = /^cal:(\d+)$/.exec(key);
  if (!m) return false;
  e.preventDefault();
  const c = calState.calendars.find((x) => x.id === Number(m[1]));
  if (!c) return true;
  colorMenu(e, {
    name: c.name, color: c.color, own: c.own_color, ownLabel: "The calendar's own color", ownTip: `Back to ${c.server_color}, as Microsoft or Google shows it`,
    shown: c.shown, noun: "calendar",
    pick: async (color) => {
      try {
        await invoke("calendar_color", { id: c.id, color });
        await calLoadCalendars();
        renderSidebar();
        if (calState.on) calRender();
        status(color ? `${c.name} is shown in its new color.` : `${c.name} has its own color again.`);
      } catch (err) { showError("Changing the calendar's color", err); }
    },
    toggle: () => calendarClick(key, { target: document.body }),
  });
  return true;
}

/** The color menu of a calendar or a To Do list: the palette, any color through the system's
 *  picker, its own color again, and Show / Hide. `o`: name, color, own (a color was chosen),
 *  ownLabel, ownTip, shown, noun, pick(color | null), toggle(). */
function colorMenu(e, o) {
  calCloseMenu();
  const set = (color) => { calCloseMenu(); o.pick(color); };
  const c = { color: o.color };
  const box = el("div", "popup-menu cal-menu cal-colors");
  box.append(el("div", "cal-colors-title", `Color for ${o.name}`));
  const grid = el("div", "cal-colors-grid");
  for (const color of CAL_PALETTE) {
    const b = el("button", `cal-color${color === c.color ? " on" : ""}`);
    b.type = "button";
    b.style.background = color;
    b.title = color;
    b.setAttribute("aria-label", `Color ${color}`);
    b.addEventListener("click", (ev) => { ev.stopPropagation(); set(color); });
    grid.append(b);
  }
  box.append(grid);
  // any color, through the system's own color picker (its input lives apart from the menu, which
  // closes while the picker is open)
  const custom = el("button", "", "Another color…");
  custom.type = "button";
  custom.addEventListener("click", (ev) => {
    ev.stopPropagation();
    calCloseMenu();
    const pick = el("input");
    pick.type = "color";
    pick.value = c.color;
    pick.style.cssText = "position: fixed; left: 0; top: 0; width: 0; height: 0; opacity: 0; border: 0; padding: 0;";
    document.body.append(pick);
    pick.addEventListener("change", () => { set(pick.value); pick.remove(); });
    pick.addEventListener("blur", () => setTimeout(() => pick.remove(), 60_000));
    pick.click();
  });
  box.append(custom);
  if (o.own) {
    const own = el("button", "", o.ownLabel);
    own.type = "button";
    if (o.ownTip) own.title = o.ownTip;
    own.addEventListener("click", (ev) => { ev.stopPropagation(); set(null); });
    box.append(own);
  }
  const showHide = el("button", "", o.shown ? `Hide this ${o.noun}` : `Show this ${o.noun}`);
  showHide.type = "button";
  showHide.addEventListener("click", (ev) => { ev.stopPropagation(); calCloseMenu(); o.toggle(); });
  box.append(showHide);
  box.addEventListener("click", (ev) => ev.stopPropagation());
  document.body.append(box);
  box.style.left = `${Math.min(e.clientX, innerWidth - box.offsetWidth - 8)}px`;
  box.style.top = `${Math.min(e.clientY, innerHeight - box.offsetHeight - 8)}px`;
  calMenuBox = box;
}

/** A click in the sidebar on a calendar line (true: handled). */
function calendarClick(key, e) {
  if (!key.startsWith("cal:")) return false;
  if (e.target.closest(".twist")) { toggle(key); return true; }
  if (key === "cal:all") { openCalendar(); return true; }
  const c = calState.calendars.find((x) => x.id === Number(key.slice(4)));
  if (!c) return true;
  c.shown = !c.shown;
  invoke("calendar_shown", { id: c.id, shown: c.shown }).catch((err) => showError("Showing the calendar", err));
  renderSidebar();
  if (calState.on) calRender();
  return true;
}

async function openCalendar() {
  window.closeTodo?.();
  window.closeContacts?.();
  calState.on = true;
  document.body.classList.add("calendar-mode");
  $("calendar").hidden = false;
  await calLoadCalendars(); // (new calendars from the download)
  renderSidebar();
  calRender();
}

/** Back to the mail (a mailbox, folder or view chosen). */
function closeCalendar() {
  if (!calState.on) return;
  calState.on = false;
  document.body.classList.remove("calendar-mode");
  $("calendar").hidden = true;
}

/** An event for the keyboard and screen readers: Tab reaches
 *  it, Enter or Space opens it, Shift+F10 / the Menu key its right-click menu; it's read with its
 *  day, time, place, and whether it's an invitation waiting for an answer. */
function calAccessible(e, row, span) {
  e.tabIndex = 0;
  e.setAttribute("role", "button");
  const when = span.allDay
    ? `${calDayTitle.format(span.start)}, all day`
    : `${calDayTitle.format(span.start)}, ${calShortTime.format(span.start)} to ${calShortTime.format(span.end)}`;
  const waiting = row.response === "notResponded" ? "not answered yet" : row.response === "tentativelyAccepted" ? "tentative" : row.cancelled ? "canceled" : "";
  e.setAttribute("aria-label", [row.subject || "no subject", when, row.location, waiting].filter(Boolean).join(", "));
}

function calSetMode(mode) {
  calState.mode = mode;
  store.set("calMode", mode);
  calRender();
}

function calMove(direction) {
  if (calState.mode === "trips" || calState.mode === "invites") return;
  const d = calState.day;
  if (calState.mode === "day") calState.day = calAddDays(d, direction);
  else if (calState.mode === "week") calState.day = calAddDays(d, 7 * direction);
  else if (calState.mode === "month") calState.day = new Date(d.getFullYear(), d.getMonth() + direction, 1);
  else calState.day = calAddDays(d, 30 * direction);
  calRender();
}

const calMonthFormat = new Intl.DateTimeFormat("en-US", { month: "long", year: "numeric" });
const calDayTitle = new Intl.DateTimeFormat("en-US", { weekday: "long", month: "long", day: "numeric", year: "numeric" });
const calShortTime = new Intl.DateTimeFormat("en-US", { hour: "numeric", minute: "2-digit" });
const calHourFormat = new Intl.DateTimeFormat("en-US", { hour: "numeric" });

function calTitle() {
  const [first, after] = calRange();
  if (calState.mode === "day") return calDayTitle.format(calState.day);
  if (calState.mode === "trips") return "Trips ahead";
  if (calState.mode === "invites") return "Invitations waiting for your answer";
  if (calState.mode === "month") return calMonthFormat.format(calState.day);
  const last = calAddDays(after, -1);
  if (calState.mode === "agenda") return `${monthDayFormat.format(first)} – ${monthDayFormat.format(last)}, ${last.getFullYear()}`;
  return first.getMonth() === last.getMonth()
    ? `${calMonthFormat.format(first)}`
    : `${monthDayFormat.format(first)} – ${monthDayFormat.format(last)}, ${last.getFullYear()}`;
}

/** Draws the view: its bar at once, then the events of its days. */
async function calRender() {
  if (!calState.on) return;
  $("cal-title").textContent = calState.query ? `Events matching “${calState.query}”` : calTitle();
  for (const b of document.querySelectorAll("#cal-modes button")) {
    b.classList.toggle("on", b.dataset.mode === calState.mode && !calState.query);
    b.setAttribute("aria-pressed", String(b.classList.contains("on")));
  }
  const hb = $("cal-hours");
  // (it keeps its place when it doesn't apply, so the buttons beside it never move)
  hb.style.visibility = !["day", "week"].includes(calState.mode) || calState.query ? "hidden" : "";
  hb.textContent = "Business hours";
  hb.classList.toggle("on", calState.business);
  let rows;
  if (calState.query) {
    try { rows = await invoke("calendar_search", { words: calState.query }); } catch (err) { showError("Searching the calendar", err); return; }
    calState.rows = rows;
    calAgenda(rows, true);
    return;
  }
  if (calState.mode === "trips") {
    calTripsView();
    return;
  }
  if (calState.mode === "invites") {
    calInvitesView();
    return;
  }
  const [first, after] = calRange();
  // a day more each side: all-day events are kept by their UTC dates
  const from = Math.floor(first.getTime() / 1000) - 86_400;
  const to = Math.floor(after.getTime() / 1000) + 86_400;
  try { rows = await invoke("calendar_events", { from, to }); } catch (err) { showError("Reading the calendar", err); return; }
  calState.rows = rows.filter(calShown);
  if (calState.mode === "month") calMonth();
  else if (calState.mode === "agenda") calAgenda(calState.rows, false);
  else if (calState.mode === "trips") calTripsView();
  else calGrid(calState.mode === "day" ? 1 : 7);
}

/** Events on a day: all-day ones (and those of a day or more), then the timed ones. */
function calOnDay(day) {
  const next = calAddDays(day, 1);
  const out = { allDay: [], timed: [] };
  for (const row of calState.rows) {
    const s = calSpan(row);
    if (s.start >= next || s.end <= day) continue;
    (s.allDay ? out.allDay : out.timed).push({ row, span: s });
  }
  out.timed.sort((a, b) => a.span.start - b.span.start || b.span.end - a.span.end);
  return out;
}

/** A chip for an event (month cells, all-day rows, the agenda). */
function calChip(row, span, withTime) {
  const chip = el("div", "cal-chip");
  chip.dataset.id = row.id;
  const color = calColor(row);
  if (span.allDay) {
    chip.classList.add("cal-allday");
    chip.style.background = color;
  } else {
    const dot = el("span", "cal-dot");
    dot.style.background = color;
    chip.append(dot);
    if (withTime) chip.append(el("span", "cal-time", calShortTime.format(span.start).replace(":00", "").replace(" ", "").toLowerCase()));
  }
  chip.append(el("span", "cal-subject", row.subject || "(no subject)"));
  calMarks(chip, row);
  chip.title = `${row.subject}${row.location ? ` · ${row.location}` : ""}`;
  calAccessible(chip, row, span);
  return chip;
}

/** Cancelled, declined, tentative or free: shown as such. */
function calMarks(node, row) {
  if (row.cancelled || row.response === "declined") node.classList.add("cal-cancelled");
  if (["tentativelyAccepted", "notResponded"].includes(row.response) || row.show_as === "tentative") node.classList.add("cal-tentative");
  if (row.show_as === "free") node.classList.add("cal-free");
}

/** Day (1 column) and Week (7): an all-day row, then the hours. */
function calGrid(days) {
  const [first] = calRange();
  const today = calMidnight(new Date());
  const body = $("cal-body");
  const scrollBefore = body.querySelector(".cal-scroll")?.scrollTop;
  const grid = el("div", "cal-grid");
  grid.style.setProperty("--days", days);
  // headings
  const head = el("div", "cal-head");
  head.append(el("div", "cal-gutter"));
  for (let i = 0; i < days; i++) {
    const d = calAddDays(first, i);
    const h = el("div", "cal-dayhead" + (calSameDay(d, today) ? " today" : ""));
    h.append(el("span", "cal-wd", weekdayFormat.format(d)), el("span", "cal-dn", String(d.getDate())));
    h.dataset.day = d.getTime();
    head.append(h);
  }
  // all-day row
  const allRow = el("div", "cal-allrow");
  allRow.append(el("div", "cal-gutter cal-alllabel", "all day"));
  const perDay = [];
  for (let i = 0; i < days; i++) {
    const d = calAddDays(first, i);
    const on = calOnDay(d);
    perDay.push(on);
    const cell = el("div", "cal-allcell");
    for (const { row, span } of on.allDay) cell.append(calChip(row, span, false));
    allRow.append(cell);
  }
  // the hours: the business day filling the pane, or all 24, half a day in view
  const business = calState.business;
  const fromH = business ? calDayFrom() : 0;
  const toH = business ? calDayTo() : 24;
  const scroll = el("div", "cal-scroll");
  grid.append(head, allRow, scroll);
  body.replaceChildren(grid);
  const hourPx = Math.max(business ? 40 : 56, Math.floor((scroll.clientHeight - 4) / (business ? toH - fromH : 12)));
  const hours = el("div", "cal-hours");
  hours.style.height = `${(toH - fromH) * hourPx}px`;
  hours.style.setProperty("--hour", `${hourPx}px`);
  const gutter = el("div", "cal-gutter cal-times");
  for (let h = fromH; h < toH; h++) {
    const t = el("div", "cal-hourlabel", h === fromH ? "" : calHourFormat.format(new Date(2000, 0, 1, h)).replace(" ", "").toLowerCase());
    t.style.top = `${(h - fromH) * hourPx}px`;
    gutter.append(t);
  }
  if (business) gutter.firstChild.textContent = calHourFormat.format(new Date(2000, 0, 1, fromH)).replace(" ", "").toLowerCase();
  hours.append(gutter);
  const min0 = fromH * 60;
  const min1 = toH * 60;
  for (let i = 0; i < days; i++) {
    const d = calAddDays(first, i);
    const col = el("div", "cal-col" + (calSameDay(d, today) ? " today" : ""));
    // a double-click on an empty time: a new appointment there (to the half hour)
    col.addEventListener("dblclick", (e) => {
      if (e.target.closest(".cal-event")) return;
      const y = e.clientY - col.getBoundingClientRect().top;
      const minutes = Math.max(0, Math.round(((y / hourPx) * 60 + min0) / 30) * 30);
      openNewAppointment(new Date(d.getTime() + minutes * 60_000));
    });
    let earlier = 0;
    let later = 0;
    for (const { row, span, lane, of } of calLanes(perDay[i].timed, d)) {
      const startMin = (span.start - d) / 60_000;
      const endMin = (span.end - d) / 60_000;
      if (endMin <= min0) { earlier++; continue; }
      if (startMin >= min1) { later++; continue; }
      const top = Math.max(min0, startMin) - min0;
      const bottom = Math.min(min1, endMin) - min0;
      const height = Math.max(24, ((bottom - top) / 60) * hourPx - 2);
      const box = el("div", "cal-event" + (height < 40 ? " cal-short" : ""));
      box.dataset.id = row.id;
      box.style.top = `${(top / 60) * hourPx}px`;
      box.style.height = `${height}px`;
      box.style.left = `${(lane / of) * 100}%`;
      box.style.width = `calc(${100 / of}% - 3px)`;
      box.style.setProperty("--cal", calColor(row));
      const when = `${calShortTime.format(span.start)} – ${calShortTime.format(span.end)}`;
      box.append(el("div", "cal-ev-subject", row.subject || "(no subject)"));
      box.append(el("div", "cal-ev-when", when));
      if (row.location) box.append(el("div", "cal-ev-where", row.location));
      calMarks(box, row);
      box.title = `${row.subject}\n${when}${row.location ? `\n${row.location}` : ""}`;
      calAccessible(box, row, span);
      col.append(box);
    }
    // what's outside the business day: counted at its edge (a click shows the full day)
    const edge = (n, cls, text) => {
      if (!n) return;
      const b = el("button", `cal-edge ${cls}`, text);
      b.title = "Show the full day (H)";
      b.addEventListener("click", (e) => { e.stopPropagation(); calSetBusiness(false); });
      col.append(b);
    };
    edge(earlier, "top", `▲ ${earlier} earlier`);
    edge(later, "bottom", `▼ ${later} later`);
    if (calSameDay(d, today)) {
      const nowMin = (new Date() - d) / 60_000;
      if (nowMin >= min0 && nowMin <= min1) {
        const line = el("div", "cal-now");
        line.style.top = `${((nowMin - min0) / 60) * hourPx}px`;
        col.append(line);
      }
    }
    hours.append(col);
  }
  scroll.append(hours);
  // the full day: the business day's start in view the first time, the same place after
  scroll.scrollTop = business ? 0 : scrollBefore ?? calDayFrom() * hourPx;
}

/** Side by side when they overlap: each event's lane, and how many lanes its group needs. */
function calLanes(timed, day) {
  const out = [];
  let group = [];
  let groupEnd = 0;
  const flush = () => {
    const of = Math.max(1, ...group.map((g) => g.lane + 1));
    for (const g of group) out.push({ ...g, of });
    group = [];
  };
  for (const item of timed) {
    const start = Math.max(item.span.start.getTime(), day.getTime());
    const end = item.span.end.getTime();
    if (group.length && start >= groupEnd) flush();
    const lanesEnd = [];
    for (const g of group) lanesEnd[g.lane] = Math.max(lanesEnd[g.lane] || 0, g.end);
    let lane = 0;
    while (lanesEnd[lane] != null && lanesEnd[lane] > start) lane++;
    group.push({ ...item, lane, end });
    groupEnd = Math.max(groupEnd, end);
  }
  if (group.length) flush();
  return out;
}

/** Month: six weeks of days, each with what fits and "+N more". */
function calMonth() {
  const [first] = calRange();
  const today = calMidnight(new Date());
  const month = calState.day.getMonth();
  const box = el("div", "cal-month");
  for (let i = 0; i < 7; i++) box.append(el("div", "cal-mhead", weekdayFormat.format(calAddDays(first, i))));
  for (let i = 0; i < 42; i++) {
    const d = calAddDays(first, i);
    const cell = el("div", "cal-mcell" + (d.getMonth() !== month ? " other" : "") + (calSameDay(d, today) ? " today" : ""));
    const num = el("button", "cal-mnum", String(d.getDate()));
    num.title = "Show this day";
    num.setAttribute("aria-label", `${calDayTitle.format(d)}: show this day`);
    num.addEventListener("click", () => { calState.day = d; calSetMode("day"); });
    cell.append(num);
    const on = calOnDay(d);
    const all = [...on.allDay.map((x) => [x, false]), ...on.timed.map((x) => [x, true])];
    const fit = 4;
    for (const [{ row, span }, timed] of all.slice(0, all.length > fit ? fit - 1 : fit)) cell.append(calChip(row, span, timed));
    if (all.length > fit) {
      const more = el("button", "cal-more", `+${all.length - fit + 1} more`);
      more.addEventListener("click", () => { calState.day = d; calSetMode("day"); });
      cell.append(more);
    }
    box.append(cell);
  }
  $("cal-body").replaceChildren(box);
}

/** Puts a scrolling view (Agenda, Trips) in place, keeping where it was scrolled when it's the
 *  same view drawn again (the minute's refresh, an answer to a trip), so a refresh never jumps
 *  back to the top. */
function calShowKeepingScroll(box) {
  const body = $("cal-body");
  const view = `${calState.mode}|${calState.day.getTime()}|${calState.query || ""}`;
  const old = body.firstElementChild;
  const top = body.dataset.view === view && old?.className === box.className ? old.scrollTop : 0;
  body.dataset.view = view;
  body.replaceChildren(box);
  if (top) box.scrollTop = top;
}

/** Agenda: day by day, each event with its time, place and calendar (also search results). */
function calAgenda(rows, found) {
  const box = el("div", "cal-agenda");
  const byDay = new Map();
  const [first, after] = calRange();
  for (const row of rows) {
    const span = calSpan(row);
    let d = calMidnight(span.start);
    if (!found && d < first) d = first;
    if (!found && d >= after) continue;
    const key = d.getTime();
    if (!byDay.has(key)) byDay.set(key, []);
    byDay.get(key).push({ row, span });
  }
  const days = [...byDay.keys()].sort((a, b) => (found ? b - a : a - b));
  if (!days.length) box.append(el("p", "p-note", found ? "No events match." : "Nothing on the calendar in these weeks."));
  for (const key of days) {
    const d = new Date(key);
    box.append(el("h3", "cal-aday" + (calSameDay(d, new Date()) ? " today" : ""), calDayTitle.format(d)));
    for (const { row, span } of byDay.get(key).sort((a, b) => a.span.start - b.span.start)) {
      const line = el("div", "cal-aline");
      line.dataset.id = row.id;
      const bar = el("span", "cal-abar");
      bar.style.background = calColor(row);
      const when = span.allDay ? "All day" : `${calShortTime.format(span.start)} – ${calShortTime.format(span.end)}`;
      line.append(bar, el("span", "cal-awhen", when), el("span", "cal-asubject", row.subject || "(no subject)"), el("span", "cal-aplace", row.location));
      calMarks(line, row);
      box.append(line);
    }
  }
  calShowKeepingScroll(box);
}

const calResponse = { accepted: "accepted", tentativelyAccepted: "maybe", declined: "declined", notResponded: "hasn't answered", none: "", organizer: "organizer" };

/** An event whole: when, where, who, the meeting link. */
async function calShowEvent(id) {
  let v;
  try { v = await invoke("calendar_event", { id }); } catch (err) { showError("Opening the event", err); return; }
  const e = v.event;
  const box = el("div", "cal-detail");
  const span = calSpan({ start: e.start, end: e.end, all_day: e.all_day });
  const when = e.all_day
    ? (calSameDay(calAddDays(span.end, -1), span.start) ? calDayTitle.format(span.start) : `${calDayTitle.format(span.start)} – ${calDayTitle.format(calAddDays(span.end, -1))}`)
    : `${calDayTitle.format(span.start)}, ${calShortTime.format(span.start)} – ${calSameDay(span.start, span.end) ? calShortTime.format(span.end) : fullFormat.format(span.end)}`;
  const line = (label, value) => {
    if (!value) return;
    const row = el("div", "cal-dline");
    row.append(el("span", "cal-dlabel", label), el("span", "cal-dvalue", value));
    box.append(row);
  };
  line("When", when + (e.kind === "occurrence" || e.kind === "exception" ? " (repeats)" : ""));
  line("Where", e.location);
  line("Calendar", `${v.calendar_name} · ${v.address}`);
  if (e.organizer?.address) line("Organizer", e.organizer.name ? `${e.organizer.name} <${e.organizer.address}>` : e.organizer.address);
  if (e.attendees.length) {
    line("People", e.attendees.map((a) => `${a.name || a.address}${calResponse[a.response] ? ` (${calResponse[a.response]})` : ""}${a.kind === "optional" ? ", optional" : ""}`).join("; "));
  }
  if (!e.is_organizer && calResponse[e.response]) line("You", calResponse[e.response]);
  if (e.reminder != null) line("Reminder", e.reminder === 0 ? "at the start" : `${e.reminder} minutes before`);
  if (e.cancelled) line("Status", "Cancelled");
  if (e.preview) {
    const p = el("p", "cal-dpreview", e.preview);
    box.append(p);
  }
  const buttons = [];
  if (e.is_organizer && calState.calendars?.find((c) => c.id === v.calendar)?.can_edit !== false) buttons.push({ label: "Edit", value: "edit" });
  buttons.push({ label: "Send as .ics…", value: "ics" });
  if (e.join_url) buttons.push({ label: "Join the meeting", value: "join" });
  if (e.web_link) buttons.push({ label: "Open in Outlook on the web", value: "web" });
  buttons.push({ label: "Close", value: null, primary: true });
  const answer = await ask({ title: e.subject || "(no subject)", text: "", content: box, buttons });
  if (answer === "edit") openEditAppointment(id);
  if (answer === "ics") sendAsIcs(id);
  if (answer === "join") invoke("open_link", { url: e.join_url }).catch((err) => showError("Opening the meeting", err));
  if (answer === "web") invoke("open_link", { url: e.web_link }).catch((err) => showError("Opening Outlook on the web", err));
}

// the bar's buttons and clicks on events
$("cal-today").addEventListener("click", () => { calState.query = ""; $("cal-search").value = ""; calState.day = calMidnight(new Date()); calRender(); });
$("cal-prev").addEventListener("click", () => calMove(-1));
$("cal-next").addEventListener("click", () => calMove(1));
$("cal-modes").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-mode]");
  if (!b) return;
  calState.query = "";
  $("cal-search").value = "";
  calSetMode(b.dataset.mode);
});
let calSearchTimer = 0;
$("cal-search").addEventListener("input", () => {
  clearTimeout(calSearchTimer);
  calSearchTimer = setTimeout(() => { calState.query = $("cal-search").value.trim(); calRender(); }, 300);
});
$("cal-body").addEventListener("click", (e) => {
  const item = e.target.closest("[data-id]");
  if (item) calShowEvent(Number(item.dataset.id));
});
// an event with the keyboard: Enter or Space opens it, Shift+F10 / the Menu key its menu
$("cal-body").addEventListener("keydown", (e) => {
  const item = e.target.closest?.("[data-id][role=button]");
  if (!item) return;
  if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); calShowEvent(Number(item.dataset.id)); }
  else if ((e.key === "F10" && e.shiftKey) || e.key === "ContextMenu") { e.preventDefault(); e.stopPropagation(); window.openContextMenu?.(item); }
});
// a day's heading in Week opens that day
$("cal-body").addEventListener("dblclick", (e) => {
  const h = e.target.closest(".cal-dayhead");
  if (h) { calState.day = new Date(Number(h.dataset.day)); calSetMode("day"); }
});
// the keyboard: ← → move, T today, D W M A the views
document.addEventListener("keydown", (e) => {
  if (!calState.on || e.ctrlKey || e.altKey || e.metaKey || e.target.closest("input, textarea, select, [contenteditable], dialog")) return;
  const keys = { h: () => calSetBusiness(!calState.business), ArrowLeft: () => calMove(-1), ArrowRight: () => calMove(1), t: () => $("cal-today").click(), d: () => calSetMode("day"), r: () => calSetMode("trips"), i: () => calSetMode("invites"), w: () => calSetMode("week"), m: () => calSetMode("month"), a: () => calSetMode("agenda"), n: () => openNewAppointment() };
  const act = keys[e.key];
  if (act) { e.preventDefault(); act(); }
});
// the time line moves, and the calendar follows new events from the download
setInterval(() => { if (calState.on && !document.querySelector("dialog[open]")) calRender(); }, 60_000);
$("cal-hours").addEventListener("click", () => calSetBusiness(!calState.business));

// Settings ▸ Calendar: the first day of the week and the business day
(() => {
  const hour = (h) => (h === 24 ? "midnight" : calHourFormat.format(new Date(2000, 0, 1, h)));
  const from = $("p-day-from");
  const to = $("p-day-to");
  for (let h = 0; h < 24; h++) from.append(new Option(hour(h), h));
  for (let h = 1; h <= 24; h++) to.append(new Option(hour(h), h));
  from.value = String(calDayFrom());
  to.value = String(calDayTo());
  $("p-week-start").value = String(calState.weekStart);
  const saved = () => { if (calState.on) calRender(); };
  from.addEventListener("change", () => { store.set("calDayFrom", Number(from.value)); to.value = String(calDayTo()); saved(); });
  to.addEventListener("change", () => { store.set("calDayTo", Number(to.value)); saved(); });
  $("p-week-start").addEventListener("change", () => {
    calState.weekStart = Number($("p-week-start").value);
    store.set("weekStart", calState.weekStart);
    saved();
  });
})();

// ------------------------------------------------------------------ an event's right-click
// menu: Open, Join, Quick Print; Accept / Tentative / Decline (invitations); Reply, Reply All,
// Forward (a new message to its people); Private, Show As, Categorize (Eudora's labels are
// Outlook's categories); Delete (declines someone else's meeting, cancels your own).

let calMenuBox = null;

function calCloseMenu() {
  calMenuBox?.remove();
  calMenuBox = null;
}

/** A menu item: a label, a tick when it's on, what it does or a submenu. */
function calMenuItem(label, act, { on = false, sub = null, disabled = false } = {}) {
  const b = el("button", sub ? "cal-sub" : "");
  b.type = "button";
  b.append(el("span", "cal-tick", on ? "✓" : ""), el("span", "", label));
  if (sub) {
    b.append(el("span", "cal-arrow", "›"));
    const inner = el("div", "popup-menu cal-submenu");
    inner.append(...sub);
    b.append(inner);
  }
  b.disabled = disabled;
  if (act) b.addEventListener("click", (e) => { e.stopPropagation(); calCloseMenu(); act(); });
  return b;
}

async function calMenu(id, x, y) {
  calCloseMenu();
  let v;
  try { v = await invoke("calendar_event", { id }); } catch (err) { showError("Opening the event", err); return; }
  const e = v.event;
  const editable = calState.calendars?.find((c) => c.id === v.calendar)?.can_edit !== false;
  const invited = !e.is_organizer && !!e.organizer?.address && e.attendees.length > 0;
  const me = v.address.toLowerCase();
  const items = [];
  const hr = () => items.push(el("hr"));
  items.push(calMenuItem("Open", () => calShowEvent(id)));
  if (e.is_organizer && editable) items.push(calMenuItem("Edit…", () => openEditAppointment(id)));
  items.push(calMenuItem("Send as .ics file…", () => sendAsIcs(id)));
  if (e.join_url) items.push(calMenuItem("Join the meeting", () => invoke("open_link", { url: e.join_url }).catch((err) => showError("Opening the meeting", err))));
  items.push(calMenuItem("Quick Print", () => calPrint(v)));
  if (invited && editable && e.no_answers_asked) {
    hr();
    items.push(calMenuItem("No answer asked by the organizer", null, { disabled: true }));
    items.push(calMenuItem("Remove from Calendar", async () => {
      try { await invoke("event_respond", { id, answer: "decline" }); status("Removed from your calendar; the organizer isn't told."); calRender(); } catch (err) { showError("Removing the meeting", err); }
    }));
  } else if (invited && editable) {
    hr();
    const answer = (what, said) => async () => {
      try { await invoke("event_respond", { id, answer: what }); status(`${said}: the organizer is told when the change reaches the server.`); calRender(); } catch (err) { showError("Answering the invitation", err); }
    };
    items.push(calMenuItem("Accept", answer("accept", "Accepted"), { on: e.response === "accepted" }));
    items.push(calMenuItem("Tentative", answer("tentative", "Tentatively accepted"), { on: e.response === "tentativelyAccepted" }));
    items.push(calMenuItem("Decline", answer("decline", "Declined (it leaves your calendar)"), { on: e.response === "declined" }));
    if (!e.all_day) items.push(calMenuItem("Propose New Time…", () => proposeNewTime({ respond_to: id, series: false, start: e.start, end: e.end, all_day: false, subject: e.subject, organizer: e.organizer?.name || e.organizer?.address || "" }, () => calRender())));
  }
  hr();
  const people = [e.organizer, ...e.attendees].filter((p) => p?.address && p.address.toLowerCase() !== me);
  const unique = [...new Map(people.map((p) => [p.address.toLowerCase(), p])).values()];
  const addr = (p) => (p.name ? `${p.name} <${p.address}>` : p.address);
  const write = (to, prefix, body) => invoke("open_compose", { mode: "new", id: null, ids: null, to: to.map(addr).join(", "), subject: `${prefix}${e.subject}`, body: body || null }).catch((err) => showError("Opening a message window", err));
  const organizerElse = e.organizer?.address && e.organizer.address.toLowerCase() !== me;
  items.push(calMenuItem("Reply", () => write([e.organizer], "RE: "), { disabled: !organizerElse }));
  items.push(calMenuItem("Reply All", () => write(unique, "RE: "), { disabled: !unique.length }));
  items.push(calMenuItem("Forward", () => write([], "FW: ", calDetailsText(v))));
  if (editable) {
    hr();
    const change = (args, said) => async () => {
      try { await invoke("event_update", { id, ...args }); status(said); calRender(); } catch (err) { showError("Changing the event", err); }
    };
    items.push(calMenuItem("Private", change({ private: !e.private }, e.private ? "No longer private." : "Private: others who can see your calendar see only that you're busy."), { on: e.private }));
    const showAs = [["free", "Free"], ["workingElsewhere", "Working elsewhere"], ["tentative", "Tentative"], ["busy", "Busy"], ["oof", "Away"]];
    items.push(calMenuItem("Show As", null, { sub: showAs.map(([key, name]) => calMenuItem(name, change({ showAs: key }, `Shows as ${name.toLowerCase()}.`), { on: e.show_as === key })) }));
    const labels = state.labelDefs || [];
    const has = new Set(e.categories.map((c) => c.toLowerCase()));
    const toggleLabel = (name) => {
      const next = has.has(name.toLowerCase()) ? e.categories.filter((c) => c.toLowerCase() !== name.toLowerCase()) : [...e.categories, name];
      return change({ categories: next }, `Categories: ${next.join(", ") || "none"}.`);
    };
    const subs = labels.map((l) => {
      const item = calMenuItem(l.name, toggleLabel(l.name), { on: has.has(l.name.toLowerCase()) });
      const dot = el("span", "cal-dot");
      dot.style.background = l.color;
      item.insertBefore(dot, item.children[1]);
      return item;
    });
    if (e.categories.length) subs.push(el("hr"), calMenuItem("None", change({ categories: [] }, "Categories taken off.")));
    items.push(calMenuItem("Categorize", null, { sub: subs.length ? subs : [calMenuItem("No labels yet (Settings ▸ Mail)", null, { disabled: true })] }));
    hr();
    const what = invited ? "Decline and delete" : e.is_organizer && e.attendees.length && !e.cancelled ? "Cancel the meeting" : "Delete";
    // (a meeting of a series: this one or every one, as Outlook asks)
    const remove = (series) => async () => {
      const which = series ? `every meeting of “${e.subject}”` : `“${e.subject}”${e.series ? " (this meeting only)" : ""}`;
      const text = invited
        ? `Decline ${which}? The organizer is told, and it leaves your calendar.`
        : what === "Cancel the meeting"
          ? `Cancel ${which}? Everyone invited is told, and it leaves the calendars.`
          : `Delete ${which}?`;
      const title = series ? `${what === "Delete" ? "Delete" : what} (the whole series)` : what;
      if (!(await askYesNo(title, text, series ? `${what === "Delete" ? "Delete" : what.split(" ")[0]} the series` : what))) return;
      try { const done = await invoke("event_delete", { id, series }); status(`“${e.subject}”${series ? " (the whole series)" : ""} ${done}.`); calRender(); } catch (err) { showError("Deleting the event", err); }
    };
    const label = what === "Delete" ? "Delete" : `Delete (${what.toLowerCase()})`;
    if (e.series) {
      items.push(calMenuItem(label, null, { sub: [calMenuItem("This meeting", remove(false)), calMenuItem("The whole series", remove(true))] }));
    } else {
      items.push(calMenuItem(label, remove(false)));
    }
  }
  const box = el("div", "popup-menu cal-menu");
  box.append(...items);
  document.body.append(box);
  box.style.left = `${Math.min(x, innerWidth - box.offsetWidth - 8)}px`;
  box.style.top = `${Math.min(y, innerHeight - box.offsetHeight - 8)}px`;
  calMenuBox = box;
}

/** An event's details as plain text (Forward, Quick Print). */
function calDetailsText(v) {
  const e = v.event;
  const span = calSpan({ start: e.start, end: e.end, all_day: e.all_day });
  const last = calAddDays(span.end, -1);
  const lines = [
    e.subject,
    e.all_day
      ? `When: ${calDayTitle.format(span.start)}${calSameDay(last, span.start) ? "" : ` – ${calDayTitle.format(last)}`} (all day)`
      : `When: ${calDayTitle.format(span.start)}, ${calShortTime.format(span.start)} – ${calShortTime.format(span.end)}`,
  ];
  if (e.location) lines.push(`Where: ${e.location}`);
  if (e.organizer?.address) lines.push(`Organizer: ${e.organizer.name || e.organizer.address}`);
  if (e.attendees.length) lines.push(`People: ${e.attendees.map((a) => a.name || a.address).join(", ")}`);
  if (e.join_url) lines.push(`Join: ${e.join_url}`);
  if (e.preview) lines.push("", e.preview);
  return lines.join("\n");
}

/** Quick Print: the event's details through the system's print dialog. */
function calPrint(v) {
  const esc = (t) => String(t ?? "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  $("print-frame")?.remove();
  const frame = document.createElement("iframe");
  frame.id = "print-frame";
  frame.setAttribute("aria-hidden", "true");
  frame.tabIndex = -1;
  frame.setAttribute("sandbox", "allow-same-origin allow-modals");
  frame.style.cssText = "position: fixed; width: 0; height: 0; border: 0; left: -9999px";
  const [title, ...rest] = calDetailsText(v).split("\n");
  frame.srcdoc = `<!doctype html><meta charset="utf-8"><div style="font: 13px 'Segoe UI', sans-serif"><div style="font-size: 18px; font-weight: 600; margin-bottom: 8px">${esc(title)}</div><pre style="white-space: pre-wrap; font: inherit">${esc(rest.join("\n"))}</pre></div>`;
  frame.addEventListener("load", () => frame.contentWindow.print(), { once: true });
  document.body.append(frame);
}

$("cal-body").addEventListener("contextmenu", (e) => {
  const item = e.target.closest("[data-id]");
  if (!item) return;
  e.preventDefault();
  calMenu(Number(item.dataset.id), e.clientX, e.clientY);
});
document.addEventListener("click", (e) => { if (calMenuBox && !calMenuBox.contains(e.target)) calCloseMenu(); });
document.addEventListener("keydown", (e) => { if (e.key === "Escape" && calMenuBox) calCloseMenu(); });

// ------------------------------------------------------------------ Trips: a view of the
// calendar. The reservations ahead found in Travel mail, grouped into trips (one leads into the
// next within three days), each with what it is, whether it's on the calendar, and buttons that
// act on it: Add to calendar / Don't add (only when it isn't there), Show in calendar, Open the
// email. Nothing pops up by itself: the Trips button counts what's waiting.

const calTripDay = new Intl.DateTimeFormat("en-US", { weekday: "short", month: "short", day: "numeric" });
const calTripLong = new Intl.DateTimeFormat("en-US", { month: "long", day: "numeric" });
const calTripState = {
  added: ["On your calendar (added by Eudora)", "ok"],
  exists: ["On your calendar", "ok"],
  suggested: ["Not on your calendar", "ask"],
  declined: ["Not on your calendar (you chose not to add it)", "off"],
};

/** A trip's place: the city its first flight lands in, else the stay's place. */
/** A flight's two airports ("✈ UA 1547 EWR → MSY") and their cities (from its details). */
function calFlightEnds(r) {
  const m = r.title.match(/([A-Z]{3}) → ([A-Z]{3})/);
  if (!m) return null;
  const city = (code) => r.details.match(new RegExp(`([^()·→:\\n]+) \\(${code}\\)`))?.[1].trim() || code;
  return { from: m[1], to: m[2], fromCity: city(m[1]), toCity: city(m[2]) };
}

/** Home: the airport most of the flights start or end at. */
function calHomeAirport(list) {
  const count = new Map();
  for (const r of list) {
    const e = r.kind === "flight" && calFlightEnds(r);
    if (e) for (const a of [e.from, e.to]) count.set(a, (count.get(a) || 0) + 1);
  }
  return [...count].sort((a, b) => b[1] - a[1])[0]?.[0] || null;
}

/** A trip's name, from where it's away from home: a hotel's city, a flight's destination that
 *  isn't home, else (only the flight back) "Return from …". */
function calTripName(items, home) {
  const stay = items.find((r) => r.kind === "stay" && r.location.includes(","));
  if (stay) return `Trip to ${stay.location.split(",").slice(-2, -1)[0].trim()}`;
  const ends = items.map((r) => r.kind === "flight" && calFlightEnds(r)).filter(Boolean);
  const away = ends.find((e) => e.to !== home);
  if (away) return `Trip to ${away.toCity}`;
  if (ends.length) return `Return from ${ends[0].fromCity}`;
  const any = items.find((r) => r.kind === "stay");
  if (any) return `Stay at ${any.title.replace(/^🛏\s*/, "")}`;
  // (shows, concerts, and conferences on their own aren't trips: named for the event)
  const show = items.find((r) => r.kind === "event");
  return show ? show.title.replace(/^[^\p{L}\p{N}]+/u, "") : "Trip";
}

/** An address said once ("100 Harbor Way, Miami, FL 33101, USA, Miami, FL" → without the
 *  repeated "Miami, FL": stays found before the core stopped repeating it). */
function calNoRepeat(text) {
  const kept = [];
  for (const part of text.split(", ")) {
    if (!kept.some((k) => k.toLowerCase().includes(part.trim().toLowerCase()))) kept.push(part);
  }
  return kept.join(", ");
}

/** Reservations into trips: sorted by time, a trip goes on while the next starts within three
 *  days of the end of what's in it so far. */
function calTripGroups(list) {
  const trips = [];
  for (const r of [...list].sort((a, b) => a.start - b.start)) {
    const last = trips[trips.length - 1];
    if (last && r.start <= last.end + 3 * 86_400) {
      last.items.push(r);
      last.end = Math.max(last.end, r.end);
    } else {
      trips.push({ items: [r], start: r.start, end: r.end });
    }
  }
  return trips;
}

function calTripWhen(r) {
  const s = new Date(r.start * 1000);
  const e = new Date(r.end * 1000);
  if (r.kind === "stay") return `Check in ${calTripDay.format(s)} · check out ${calTripDay.format(e)}`;
  if (r.kind === "car") return `Pick up ${calTripDay.format(s)}, ${calShortTime.format(s)} · return ${calTripDay.format(e)}, ${calShortTime.format(e)}`;
  if (r.kind === "ride") return `Pickup ${calTripDay.format(s)}, ${calShortTime.format(s)}`;
  return `${calTripDay.format(s)}, ${calShortTime.format(s)} – ${calShortTime.format(e)} (your time)`;
}

async function calAnswerTrip(add, decline, said) {
  try {
    await invoke("reservations_answer", { add, decline });
    status(said);
    calTripsCount();
    calRender();
  } catch (err) { showError("Changing the trip", err); }
}

/** The Trips view. */
async function calTripsView() {
  let list;
  try { list = await invoke("reservations_list"); } catch (err) { showError("Reading your trips", err); return; }
  const box = el("div", "cal-tripsview");
  const home = calHomeAirport(list);
  if (!list.length) {
    box.append(el("p", "p-note", "No flights or hotel stays ahead were found in your travel mail of the last year. New ones are picked up as the mail arrives."));
  }
  for (const trip of calTripGroups(list)) {
    const s = new Date(trip.start * 1000);
    const e = new Date(trip.end * 1000);
    const card = el("section", "cal-trip-card");
    const head = el("div", "cal-trip-head");
    head.append(el("h3", "", calTripName(trip.items, home)), el("span", "cal-trip-dates", calSameDay(s, e) ? calTripLong.format(s) : `${calTripLong.format(s)} – ${calTripLong.format(e)}`));
    const waiting = trip.items.filter((r) => r.status === "suggested");
    if (waiting.length > 1) {
      const all = el("button", "cal-trip-btn primary", `Add all ${waiting.length} to calendar`);
      all.addEventListener("click", () => calAnswerTrip(waiting.map((r) => r.key), [], `${waiting.length} added to your calendar.`));
      head.append(all);
    }
    card.append(head);
    for (const r of trip.items) {
      const item = el("div", `cal-trip-item ${calTripState[r.status]?.[1] || ""}`);
      const mark = el("div", "cal-trip-icon");
      mark.append(icon({ flight: "plane", stay: "bed", car: "car", train: "train", bus: "bus", ferry: "ferry", ride: "taxi", event: "ticket" }[r.kind] || "calendar-days"));
      const body = el("div", "cal-trip-body");
      const name = r.title.replace(/^[^\p{L}\p{N}]+/u, ""); // (without its ✈ 🛏 🚗 🚆 mark: the icon says it)
      body.append(el("div", "cal-trip-title", name));
      body.append(el("div", "cal-trip-when", calTripWhen(r)));
      const details = r.details.split("\n").filter((l) => !/^(Departs|Arrives|Check in|Pick up|Return|Pickup)/.test(l) && !l.startsWith(name.split(" ").slice(0, 2).join(" ")));
      if (details.length) body.append(el("div", "cal-trip-details", details.map(calNoRepeat).join(" · ")));
      const [said, kind] = calTripState[r.status] || [r.status, ""];
      body.append(el("span", `cal-trip-status ${kind}`, said));
      const actions = el("div", "cal-trip-actions");
      const button = (label, act, cls = "") => { const b = el("button", `cal-trip-btn ${cls}`, label); b.addEventListener("click", act); actions.append(b); };
      if (r.status === "suggested" || r.status === "declined") {
        button("Add to calendar", () => calAnswerTrip([r.key], [], `Added to your calendar: ${r.title}`), "primary");
      }
      if (r.status === "suggested") button("Don't add", () => calAnswerTrip([], [r.key], "Not added; it won't be offered again."));
      if (r.status === "added" || r.status === "exists") {
        button("Remove from calendar", async () => {
          if (!(await askYesNo("Remove from calendar", `Take “${name}” off your calendar? It stays in Trips, where you can add it again.`, "Remove it"))) return;
          try { const done = await invoke("reservation_off", { key: r.key }); status(`“${name}” ${done === "already gone" ? "was already off your calendar" : `${done} on your calendar`}.`); calRender(); } catch (err) { showError("Removing it from the calendar", err); }
        });
      }
      button("Show in calendar", () => { calState.day = calMidnight(new Date(r.start * 1000)); calSetMode("day"); });
      if (r.message) button("Open the email", () => openInnerReader(r.message, r.subject));
      button("Remove from Trips", async () => {
        try { await invoke("reservation_hide", { key: r.key }); status(`“${name}” removed from Trips (its calendar event, if any, stays).`); calTripsCount(); calRender(); } catch (err) { showError("Removing it from Trips", err); }
      });
      // the email's boarding passes, tickets, receipts: each opens
      if (r.files?.length) {
        const files = el("div", "cal-trip-files");
        for (const f of r.files) {
          const b = el("button", "cal-trip-file");
          b.title = `${f.name}: open it`;
          b.append(icon(f.label.startsWith("Boarding") || f.label === "Wallet pass" ? "ticket" : "file-lines"), el("span", "", f.label));
          b.addEventListener("click", () => invoke("attachment_open", { id: r.message, part: f.part }).catch((err) => showError("Opening the file", err)));
          files.append(b);
        }
        body.append(files);
      }
      item.append(mark, body, actions);
      card.append(item);
    }
    box.append(card);
  }
  calShowKeepingScroll(box);
}

// ------------------------------------------------------------------ New appointment

// Places: Google Maps with the user's key, else OpenStreetMap; online meetings: only the kinds
// set up; Microsoft sends the people their invitations.

const appt = { place: null, places: [], placeAt: -1, placeTimer: 0, people: [], peopleAt: -1, peopleTimer: 0 };
const pad2 = (n) => String(n).padStart(2, "0");
const dayValue = (d) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
const timeValue = (d) => `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;

/** Opens New Appointment, starting at `start` (else the next half hour of the day shown), an hour long. */
async function openNewAppointment(start) {
  if (!calState.calendars?.length) await calLoadCalendars();
  const editable = (calState.calendars || []).filter((c) => c.can_edit !== false);
  if (!editable.length) { tell("New appointment", "There's no calendar to put it in yet: your Microsoft 365 calendar comes in with the mail once you're signed in."); return; }
  if (!start) {
    const now = new Date();
    const base = calSameDay(calState.day, now) ? now : new Date(calState.day.getFullYear(), calState.day.getMonth(), calState.day.getDate(), 9);
    start = new Date(Math.ceil(base.getTime() / 1_800_000) * 1_800_000);
  }
  const end = new Date(start.getTime() + 3_600_000);
  const cal = $("appt-calendar");
  cal.replaceChildren(...editable.map((c) => { const o = el("option", "", `${c.name} (${c.address})`); o.value = String(c.id); return o; }));
  cal.value = String((editable.find((c) => c.is_default) || editable[0]).id);
  $("appt-title").value = "";
  $("appt-start-day").value = dayValue(start);
  $("appt-start-time").value = timeValue(start);
  $("appt-end-day").value = dayValue(end);
  $("appt-end-time").value = timeValue(end);
  $("appt-allday").checked = false;
  $("appt-location").value = "";
  appt.place = null;
  showChosenPlace();
  $("appt-people").value = "";
  setNotes("");
  $("appt-showas").value = "busy";
  $("appt-reminder").value = "15";
  $("appt-private").checked = false;
  $("appt-status").textContent = "";
  appt.editing = null;
  $("appt-heading").textContent = "New Appointment";
  $("appt-calendar").disabled = false;
  $("appt-conference").disabled = false;
  apptAllDay();
  await apptConferences();
  apptSaveLabel();
  $("appt").showModal();
  $("appt-title").focus();
}

// ---- Notes: formatted text, edited in a protected frame (an event's text came from someone
// else: no scripts run in it, no pictures fetched)
function setNotes(html) {
  const frame = $("appt-notes");
  frame.srcdoc = sandboxed(html);
  frame.onload = () => {
    const doc = frame.contentDocument;
    if (!doc) return;
    doc.designMode = "on";
    appt.notesStart = doc.body.innerHTML;
  };
}
function notesHtml() {
  const doc = $("appt-notes").contentDocument;
  return doc?.body ? doc.body.innerHTML : "";
}

/** Opens an appointment you organized for changes: the same window, filled in; its whole text asked of Microsoft. */
async function openEditAppointment(id) {
  if (!calState.calendars?.length) await calLoadCalendars();
  let a;
  try { a = await invoke("appointment_get", { id }); } catch (err) { showError("Opening the appointment", err); return; }
  const e = a.event;
  if (!e.is_organizer) { tell("Edit", "It's someone else's meeting: only its organizer can change it. You can still answer it, show it as busy or free, and categorize it (right-click)."); return; }
  const cal = $("appt-calendar");
  const c = calState.calendars.find((x) => x.id === a.calendar);
  cal.replaceChildren(el("option", "", c ? `${c.name} (${c.address})` : "Calendar"));
  cal.firstChild.value = String(a.calendar);
  cal.disabled = true; // (moving an event between calendars isn't offered)
  $("appt-title").value = e.subject;
  const s = e.all_day ? new Date(new Date(e.start * 1000).toISOString().slice(0, 10) + "T00:00") : new Date(e.start * 1000);
  const end = e.all_day ? new Date(new Date((e.end - 86_400) * 1000).toISOString().slice(0, 10) + "T00:00") : new Date(e.end * 1000);
  $("appt-start-day").value = dayValue(s);
  $("appt-start-time").value = timeValue(s);
  $("appt-end-day").value = dayValue(end);
  $("appt-end-time").value = timeValue(end);
  $("appt-allday").checked = e.all_day;
  $("appt-location").value = e.location || "";
  appt.place = a.map_url ? { name: e.location, address: "", lat: null, lon: null, map_url: a.map_url, source: "" } : null;
  showChosenPlace();
  $("appt-people").value = e.attendees.map((p) => (p.name && p.name !== p.address ? `${p.name} <${p.address}>` : p.address)).join(", ");
  // its text: the whole of it, else the preview (then left alone unless changed)
  setNotes(a.body ?? (e.preview ? `<p>${e.preview.replace(/&/g, "&amp;").replace(/</g, "&lt;")}</p>` : ""));
  $("appt-showas").value = ["busy", "tentative", "free", "oof"].includes(e.show_as) ? e.show_as : "busy";
  $("appt-reminder").value = e.reminder == null ? "" : [5, 15, 30, 60, 1440].includes(e.reminder) ? String(e.reminder) : "15";
  $("appt-private").checked = e.private;
  $("appt-status").textContent = a.body == null ? "Only a preview of its text is here (Microsoft couldn't be asked for the rest): the text is left as it is unless you change it." : "";
  appt.editing = id;
  $("appt-heading").textContent = "Edit Appointment";
  apptAllDay();
  await apptConferences();
  // an online meeting already in it stays as it is
  $("appt-conference").disabled = a.online;
  if (a.online) {
    const o = el("option", "", "Already an online meeting (kept)");
    o.value = "";
    $("appt-conference").replaceChildren(o);
  }
  apptSaveLabel();
  $("appt").showModal();
  $("appt-title").focus();
}

/** An appointment as an .ics file attached to a new message: anyone's calendar program adds it from there. */
async function sendAsIcs(id) {
  try {
    const [a, text] = await Promise.all([invoke("calendar_event", { id }), invoke("appointment_ics", { id })]);
    const e = a.event;
    const data = btoa(unescape(encodeURIComponent(text)));
    const name = `${(e.subject || "Appointment").replace(/[\\/:*?"<>|]+/g, " ").trim().slice(0, 60) || "Appointment"}.ics`;
    const span = calSpan({ start: e.start, end: e.end, all_day: e.all_day });
    const when = e.all_day ? calDayTitle.format(span.start) : `${calDayTitle.format(span.start)}, ${calShortTime.format(span.start)} – ${calShortTime.format(span.end)}`;
    const draft = {
      from: "", from_name: "", to: "", cc: "", bcc: "",
      subject: `Invitation: ${e.subject || "Appointment"} (${when})`,
      html: `<p>${(e.subject || "").replace(/&/g, "&amp;").replace(/</g, "&lt;")}<br>${when}${e.location ? `<br>${e.location.replace(/&/g, "&amp;").replace(/</g, "&lt;")}` : ""}</p><p>The attached file adds it to your calendar.</p>`,
      text: "",
      in_reply_to: null, references: [],
      attachments: [{ name, mime: "text/calendar", data }],
      forwarded: [], priority: null, sender: null, origin: null, redirect_of: null,
    };
    const saved = await invoke("save_draft", { id: null, draft });
    await invoke("open_compose", { mode: "draft", id: saved });
  } catch (err) { showError("Sending the appointment as an .ics file", err); }
}

/** The online meetings this calendar can have: only the kinds set up. */
async function apptConferences() {
  const select = $("appt-conference");
  const keep = select.value;
  select.replaceChildren(el("option", "", "None"));
  select.firstChild.value = "";
  let o = {};
  try { o = await invoke("conference_options", { calendar: Number($("appt-calendar").value) }); } catch { /* none */ }
  const add = (value, label) => { const opt = el("option", "", label); opt.value = value; select.append(opt); };
  if (o.teams) add("teams", "Microsoft Teams");
  for (const g of o.google || []) add(`meet:${g}`, (o.google.length > 1 ? `Google Meet (${g})` : "Google Meet"));
  if (o.zoom) add("zoom", "Zoom (your meeting room)");
  select.value = [...select.options].some((x) => x.value === keep) ? keep : "";
  const missing = [!o.teams && "Teams (a Microsoft 365 work calendar)", !(o.google || []).length && "Google Meet (sign in to a Google account in Settings ▸ Accounts)", !o.zoom && "Zoom (your room's link in Settings ▸ Calendar)"].filter(Boolean);
  $("appt-conference-note").textContent = missing.length ? `Also possible once set up: ${missing.join("; ")}.` : "";
}
$("appt-calendar").addEventListener("change", apptConferences);

function apptAllDay() {
  const all = $("appt-allday").checked;
  $("appt-start-time").hidden = all;
  $("appt-end-time").hidden = all;
}
$("appt-allday").addEventListener("change", apptAllDay);

// the end follows the start (the same length)
let apptLength = 3_600_000;
function apptTimes() {
  const at = (day, time) => new Date(`${$(day).value}T${$(time).value || "00:00"}`);
  return [at("appt-start-day", "appt-start-time"), at("appt-end-day", "appt-end-time")];
}
for (const id of ["appt-start-day", "appt-start-time"]) {
  $(id).addEventListener("focus", () => { const [s, e] = apptTimes(); if (e > s) apptLength = e - s; });
  $(id).addEventListener("change", () => {
    const [s] = apptTimes();
    if (isNaN(s)) return;
    const e = new Date(s.getTime() + apptLength);
    $("appt-end-day").value = dayValue(e);
    $("appt-end-time").value = timeValue(e);
  });
}

/** Save, or Send when people are invited. */
function apptSaveLabel() {
  const people = $("appt-people").value.trim();
  $("appt-save").textContent = appt.editing ? (people ? "Save and send update" : "Save") : people ? "Send invitations" : "Save";
}

// ---- Location: suggestions as you type
function showChosenPlace() {
  const box = $("appt-place");
  box.replaceChildren();
  box.hidden = !appt.place;
  if (!appt.place) return;
  const p = appt.place;
  const link = el("a", "", "Show on the map");
  link.href = "#";
  link.addEventListener("click", (e) => { e.preventDefault(); openLink(p.map_url); });
  box.append(icon("location-dot"), el("span", "", p.address ? `${p.name} · ${p.address}` : p.name), link);
}

function renderPlaces() {
  const box = $("appt-places");
  box.replaceChildren(...appt.places.map((p, i) => {
    const row = el("div", `appt-sug${i === appt.placeAt ? " on" : ""}`);
    row.append(el("div", "appt-sug-main", p.name), el("div", "appt-sug-sub", p.address || ""));
    row.addEventListener("mousedown", (e) => { e.preventDefault(); choosePlace(p); });
    return row;
  }));
  if (appt.places.length) {
    const from = el("div", "appt-sug-from", appt.places[0].source === "google" ? "Suggestions from Google Maps" : "Suggestions from OpenStreetMap (a Google Maps key in Settings ▸ Calendar gives Google's)");
    box.append(from);
  }
  box.hidden = !appt.places.length;
}

function choosePlace(p) {
  appt.place = p;
  $("appt-location").value = p.address ? `${p.name}, ${p.address}` : p.name;
  appt.places = [];
  renderPlaces();
  showChosenPlace();
}

$("appt-location").addEventListener("input", () => {
  appt.place = null;
  showChosenPlace();
  clearTimeout(appt.placeTimer);
  const text = $("appt-location").value.trim();
  if (text.length < 3) { appt.places = []; renderPlaces(); return; }
  appt.placeTimer = setTimeout(async () => {
    try {
      const found = await invoke("places_suggest", { text });
      if ($("appt-location").value.trim() !== text) return;
      appt.places = found;
      appt.placeAt = -1;
      renderPlaces();
    } catch (err) { $("appt-status").textContent = String(err); }
  }, 300);
});
$("appt-location").addEventListener("keydown", (e) => {
  if ($("appt-places").hidden) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    appt.placeAt = (appt.placeAt + (e.key === "ArrowDown" ? 1 : -1) + appt.places.length) % appt.places.length;
    renderPlaces();
  } else if ((e.key === "Enter" || e.key === "Tab") && appt.placeAt >= 0) {
    e.preventDefault();
    choosePlace(appt.places[appt.placeAt]);
  } else if (e.key === "Escape") {
    e.preventDefault();
    appt.places = [];
    renderPlaces();
  }
});
$("appt-location").addEventListener("blur", () => setTimeout(() => { appt.places = []; renderPlaces(); }, 150));

// ---- People: completion from the people you write to
function peopleWord() {
  const v = $("appt-people").value;
  const at = Math.max(v.lastIndexOf(","), v.lastIndexOf(";"));
  return [v.slice(0, at + 1), v.slice(at + 1).trim()];
}
function renderPeople() {
  const box = $("appt-people-suggest");
  box.replaceChildren(...appt.people.map((p, i) => {
    const row = el("div", `appt-sug${i === appt.peopleAt ? " on" : ""}`);
    row.append(el("div", "appt-sug-main", p.name || p.address), el("div", "appt-sug-sub", p.name ? p.address : ""));
    row.addEventListener("mousedown", (e) => { e.preventDefault(); choosePerson(p); });
    return row;
  }));
  box.hidden = !appt.people.length;
}
function choosePerson(p) {
  const [before] = peopleWord();
  const entry = p.name && p.name !== p.address ? `${p.name} <${p.address}>` : p.address;
  $("appt-people").value = `${before}${before ? " " : ""}${entry}, `;
  appt.people = [];
  renderPeople();
  apptSaveLabel();
  $("appt-people").focus();
}
$("appt-people").addEventListener("input", () => {
  apptSaveLabel();
  clearTimeout(appt.peopleTimer);
  const [, typed] = peopleWord();
  if (typed.length < 2) { appt.people = []; renderPeople(); return; }
  appt.peopleTimer = setTimeout(async () => {
    try {
      const items = await invoke("complete_address", { typed });
      appt.people = (items || []).slice(0, 8);
      appt.peopleAt = -1;
      renderPeople();
      // the organization's directory when the rest found too few
      if (appt.people.length < 8 && typed.length >= 3) {
        const more = await invoke("complete_directory", { typed }).catch(() => []);
        const add = more.filter((m) => !appt.people.some((p) => p.address.toLowerCase() === m.address.toLowerCase()));
        if (add.length && peopleWord()[1] === typed) {
          appt.people = [...appt.people, ...add].slice(0, 10);
          renderPeople();
        }
      }
    } catch { /* (no suggestions) */ }
  }, 150);
});
$("appt-people").addEventListener("keydown", (e) => {
  if ($("appt-people-suggest").hidden) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    appt.peopleAt = (appt.peopleAt + (e.key === "ArrowDown" ? 1 : -1) + appt.people.length) % appt.people.length;
    renderPeople();
  } else if ((e.key === "Enter" || e.key === "Tab") && appt.peopleAt >= 0) {
    e.preventDefault();
    choosePerson(appt.people[appt.peopleAt]);
  } else if (e.key === "Escape") {
    e.preventDefault();
    appt.people = [];
    renderPeople();
  }
});
$("appt-people").addEventListener("blur", () => setTimeout(() => { appt.people = []; renderPeople(); }, 150));

// ---- Save
$("appt-cancel").addEventListener("click", () => $("appt").close());
$("appt-save").addEventListener("click", async () => {
  const all = $("appt-allday").checked;
  let [s, e] = apptTimes();
  if (all) {
    // all-day events are kept as UTC midnights of their dates
    const sd = $("appt-start-day").value, ed = $("appt-end-day").value || sd;
    s = new Date(`${sd}T00:00:00Z`);
    e = new Date(new Date(`${ed}T00:00:00Z`).getTime() + 86_400_000);
  }
  if (isNaN(s) || isNaN(e)) { $("appt-status").textContent = "Choose when it starts and ends."; return; }
  const conf = $("appt-conference").value;
  const event = {
    calendar: Number($("appt-calendar").value),
    subject: $("appt-title").value.trim(),
    start: Math.floor(s.getTime() / 1000),
    end: Math.floor(e.getTime() / 1000),
    all_day: all,
    location: $("appt-location").value.trim(),
    place: appt.place,
    conference: conf.startsWith("meet:") ? "meet" : conf || null,
    google: conf.startsWith("meet:") ? conf.slice(5) : null,
    attendees: $("appt-people").value.replace(/[,;\s]+$/, ""),
    notes: notesHtml().replace(/^(\s|<br>|<p><br><\/p>)+$/i, ""),
    reminder: $("appt-reminder").value ? Number($("appt-reminder").value) : null,
    show_as: $("appt-showas").value,
    private: $("appt-private").checked,
  };
  const save = $("appt-save");
  save.disabled = true;
  $("appt-status").textContent = conf.startsWith("meet:") ? "Asking Google for a Meet…" : "Saving…";
  try {
    if (appt.editing) {
      await invoke("appointment_edit", { id: appt.editing, event, bodyChanged: notesHtml() !== appt.notesStart });
      $("appt").close();
      status(event.attendees ? `“${event.subject}” changed; Microsoft is sending the people the update.` : `“${event.subject}” changed.`);
    } else {
      await invoke("appointment_new", { event });
      $("appt").close();
      status(event.attendees ? `“${event.subject}” is on your calendar; Microsoft is sending the invitations.` : `“${event.subject}” is on your calendar.`);
    }
    calState.day = calMidnight(s);
    calRender();
  } catch (err) {
    $("appt-status").textContent = `Not saved: ${err}`;
  } finally {
    save.disabled = false;
  }
});
$("cal-new").addEventListener("click", () => openNewAppointment());

// ---- Settings ▸ Calendar: the Google Maps key and the Zoom room
async function calPrefsFill() {
  try {
    const has = await invoke("maps_key_present");
    $("p-maps-key").value = "";
    $("p-maps-state").textContent = has ? "A Google Maps key is set: suggestions come from Google Maps." : "No key: suggestions come from OpenStreetMap.";
  } catch { /* (left as it was) */ }
  try {
    const room = JSON.parse((await invoke("setting", { key: "zoom-room" })) || "{}");
    $("p-zoom-link").value = room.link || "";
    $("p-zoom-passcode").value = room.passcode || "";
    $("p-zoom-dial").value = room.dial || "";
  } catch { /* (none yet) */ }
}
$("prefs-open")?.addEventListener("click", () => setTimeout(calPrefsFill, 0));

// Zoom app and sign-in (meetings of their own)
async function zoomPrefsFill() {
  try {
    const z = await invoke("zoom_get");
    $("p-zoom-id").value = z.id || "";
    $("p-zoom-secret").value = "";
    $("p-zoom-redirect").textContent = z.redirect;
    $("p-zoom-state").textContent = z.signed_in ? "Signed in to Zoom: each appointment's Zoom meeting is a meeting of its own." : z.has_app ? "The Zoom app is set: Sign in to Zoom to make meetings." : "No Zoom app yet: Online meeting ▸ Zoom uses your room above.";
    $("p-zoom-signout").disabled = !z.signed_in;
  } catch { /* (left as it was) */ }
}
$("prefs-open")?.addEventListener("click", () => setTimeout(zoomPrefsFill, 0));
$("p-zoom-app-save").addEventListener("click", async () => {
  const id = $("p-zoom-id").value.trim();
  const secret = $("p-zoom-secret").value.trim();
  if (!id || !secret) { $("p-zoom-state").textContent = "Type both the Client ID and the Client Secret (Zoom shows them on the app's App Credentials page)."; return; }
  try { await invoke("zoom_set", { id, secret }); zoomPrefsFill(); } catch (err) { showError("Saving the Zoom app", err); }
});
$("p-zoom-signin").addEventListener("click", async () => {
  $("p-zoom-state").textContent = "Zoom's page is open in your browser: sign in there and allow Eudora…";
  try { await invoke("zoom_sign_in"); zoomPrefsFill(); status("Signed in to Zoom."); } catch (err) { $("p-zoom-state").textContent = `Not signed in: ${err}`; }
});
$("p-zoom-signout").addEventListener("click", async () => {
  try { await invoke("zoom_sign_out"); zoomPrefsFill(); } catch (err) { showError("Signing out of Zoom", err); }
});
$("p-maps-save").addEventListener("click", async () => {
  const key = $("p-maps-key").value.trim();
  if (!key) { $("p-maps-state").textContent = "Type the key first."; return; }
  try { await invoke("maps_key_set", { key }); calPrefsFill(); } catch (err) { showError("Saving the Google Maps key", err); }
});
$("p-maps-remove").addEventListener("click", async () => {
  try { await invoke("maps_key_set", { key: null }); calPrefsFill(); } catch (err) { showError("Removing the Google Maps key", err); }
});
$("p-zoom-save").addEventListener("click", async () => {
  const link = $("p-zoom-link").value.trim();
  if (link && !link.startsWith("https://")) { tell("Zoom room", "The link starts with https:// (zoom.us ▸ Profile ▸ Personal Meeting ID ▸ Invite link)."); return; }
  try {
    await invoke("set_setting", { key: "zoom-room", value: JSON.stringify({ link, passcode: $("p-zoom-passcode").value.trim(), dial: $("p-zoom-dial").value.trim() }) });
    status(link ? "Zoom room saved: Online meeting ▸ Zoom in a new appointment." : "Zoom room removed.");
  } catch (err) { showError("Saving the Zoom room", err); }
});

// ------------------------------------------------------------------ Invites

// Microsoft 365 has already put each invitation on the calendar, tentative; these answer it
// (the organizer is told).

const calInviteDay = new Intl.DateTimeFormat("en-US", { weekday: "long", month: "long", day: "numeric" });
const calInviteDate = new Intl.DateTimeFormat("en-US", { weekday: "short", month: "short", day: "numeric" });

/** When a meeting is: "Thu, Oct 29, 11:00 AM – 12:00 PM" (all day, several days…). */
function calInviteWhen(inv) {
  const s = new Date(inv.start * 1000);
  const e = new Date(inv.end * 1000);
  if (inv.all_day) {
    // (kept as UTC midnights: shown on their own dates)
    const first = new Date(s.getUTCFullYear(), s.getUTCMonth(), s.getUTCDate());
    const last = new Date(e.getUTCFullYear(), e.getUTCMonth(), e.getUTCDate() - 1);
    return calSameDay(first, last) ? `All day, ${calInviteDate.format(first)}` : `All day, ${calInviteDate.format(first)} – ${calInviteDate.format(last)}`;
  }
  return calSameDay(s, e)
    ? `${calInviteDate.format(s)}, ${calShortTime.format(s)} – ${calShortTime.format(e)}`
    : `${calInviteDate.format(s)}, ${calShortTime.format(s)} – ${calInviteDate.format(e)}, ${calShortTime.format(e)}`;
}

/** The answer so far, in words, and its badge kind. */
const CAL_ANSWERS = {
  none: ["Not answered yet", "ask"],
  notResponded: ["Not answered yet", "ask"],
  accepted: ["You accepted", "ok"],
  tentativelyAccepted: ["You said tentative", "ok"],
  declined: ["You declined", "off"],
};

/** Answers an invitation: here at once, then the organizer is told. */
async function calAnswerInvite(inv, answer, after) {
  if (answer === "decline" && !(await askYesNo("Decline the meeting", `Decline “${inv.subject}”${inv.series ? " (every meeting in the series)" : ""}? It leaves your calendar, and ${inv.organizer || "the organizer"} is told.`, "Decline it", "Keep it"))) return;
  try {
    await invoke("event_respond", { id: inv.respond_to, answer, series: inv.series });
    status(`${{ accept: "Accepted", tentative: "Tentatively accepted", decline: "Declined" }[answer]}: ${inv.subject}. ${inv.organizer ? inv.organizer.replace(/\s*<.*$/, "") : "The organizer"} will be told.`);
    calInvitesCount();
    after?.();
  } catch (err) { showError("Answering the invitation", err); }
}

/** Propose New Time (Outlook's): Tentative or Decline, the time proposed instead, and a message;
 *  Microsoft passes them to the organizer, who can accept the new time. */
async function proposeNewTime(inv, after) {
  const s = new Date(inv.start * 1000);
  const e = new Date(inv.end * 1000);
  const box = el("div", "propose");
  box.innerHTML = `
    <label class="appt-row"><span>Answer</span><select class="pr-answer"><option value="tentative">Tentative</option><option value="decline">Decline</option></select></label>
    <div class="appt-row"><span>New start</span><input class="pr-sday" type="date"><input class="pr-stime" type="time" step="900"></div>
    <div class="appt-row"><span>New end</span><input class="pr-eday" type="date"><input class="pr-etime" type="time" step="900"></div>
    <label class="appt-row appt-notes"><span>Message</span><textarea class="pr-comment" rows="3" placeholder="(optional) why, or what suits you"></textarea></label>`;
  const q = (c) => box.querySelector(c);
  q(".pr-sday").value = dayValue(s);
  q(".pr-stime").value = timeValue(s);
  q(".pr-eday").value = dayValue(e);
  q(".pr-etime").value = timeValue(e);
  // the end keeps the meeting's length when the start moves
  const length = e - s;
  const moved = () => {
    const ns = new Date(`${q(".pr-sday").value}T${q(".pr-stime").value || "00:00"}`);
    if (isNaN(ns)) return;
    const ne = new Date(ns.getTime() + length);
    q(".pr-eday").value = dayValue(ne);
    q(".pr-etime").value = timeValue(ne);
  };
  q(".pr-sday").addEventListener("change", moved);
  q(".pr-stime").addEventListener("change", moved);
  const answer = await ask({ title: `Propose a new time: ${inv.subject || "(no subject)"}`, text: `Now: ${calInviteWhen(inv)}`, content: box, buttons: [{ label: "Cancel", value: null }, { label: "Send", value: "send", primary: true }] });
  if (answer !== "send") return;
  const ns = new Date(`${q(".pr-sday").value}T${q(".pr-stime").value}`);
  const ne = new Date(`${q(".pr-eday").value}T${q(".pr-etime").value}`);
  if (isNaN(ns) || isNaN(ne) || ne <= ns) { tell("Propose a new time", "The new time has to end after it starts."); return; }
  const what = q(".pr-answer").value;
  try {
    await invoke("event_respond", { id: inv.respond_to, answer: what, series: inv.series, comment: q(".pr-comment").value, proposeStart: Math.floor(ns / 1000), proposeEnd: Math.floor(ne / 1000) });
    status(`${what === "decline" ? "Declined" : "Tentatively accepted"} “${inv.subject}”, proposing ${calInviteWhen({ start: ns / 1000, end: ne / 1000, all_day: false })}: ${inv.organizer ? inv.organizer.replace(/\s*<.*$/, "") : "the organizer"} will be told.`);
    calInvitesCount();
    after?.();
  } catch (err) { showError("Proposing a new time", err); }
}

/** An invitation's lines and buttons (the Invites view and the email's bar share them). */
function calInviteParts(inv, after, fromEmail) {
  const body = el("div", "cal-trip-body");
  body.append(el("div", "cal-trip-title", inv.subject || "(no subject)"));
  body.append(el("div", "cal-trip-when", calInviteWhen(inv)));
  const details = [inv.organizer && `From ${inv.organizer}`, inv.location, inv.attendees > 1 && `${inv.attendees} people`, inv.series && "a series: your answer covers every meeting in it"].filter(Boolean);
  if (details.length) body.append(el("div", "cal-trip-details", details.join(" · ")));
  const [said, kind] = inv.cancelled ? ["The organizer canceled it", "off"] : CAL_ANSWERS[inv.response] || [inv.response, ""];
  body.append(el("span", `cal-trip-status ${kind}`, said));
  // what else is on the calendar then
  const clash = el("div", "cal-invite-conflicts");
  if (inv.conflicts.length) {
    clash.classList.add("has");
    clash.append(el("b", "", inv.conflicts.length === 1 ? "Conflicts with: " : `Conflicts with ${inv.conflicts.length}: `));
    clash.append(document.createTextNode(inv.conflicts.map((c) => `${c.subject || "(busy)"} (${calShortTime.format(new Date(c.start * 1000))} – ${calShortTime.format(new Date(c.end * 1000))}${c.show_as === "tentative" ? ", tentative" : ""})`).join("; ")));
  } else if (!inv.all_day) {
    clash.textContent = "No conflicts: nothing else on your calendar then.";
  }
  if (clash.childNodes.length) body.append(clash);
  const actions = el("div", "cal-trip-actions");
  const button = (label, act, cls = "") => { const b = el("button", `cal-trip-btn ${cls}`, label); b.addEventListener("click", act); actions.append(b); };
  // (someone else's calendar shared with the user, or no answers asked: no answer buttons)
  if (inv.no_answer) body.append(el("div", "cal-trip-details", `No answer needed: ${inv.no_answer}.`));
  if (!inv.cancelled && !inv.no_answer) {
    if (inv.response !== "accepted") button("Accept", () => calAnswerInvite(inv, "accept", after), "primary");
    if (inv.response !== "tentativelyAccepted") button("Tentative", () => calAnswerInvite(inv, "tentative", after));
    button("Decline", () => calAnswerInvite(inv, "decline", after));
    if (!inv.all_day) button("Propose New Time…", () => proposeNewTime(inv, after));
  }
  // spam
  button("Spam", async () => {
    const who = inv.organizer || "the sender";
    if (!(await askYesNo("Invitation is spam", `Mark “${inv.subject}” as spam? Nothing is sent to ${who}: no answer of any kind. The meeting leaves your calendar${inv.series ? " (every meeting in the series)" : ""}, the email goes to Spam, and mail from ${who.replace(/^.*<|>$/g, "")} goes to Spam from now on.`, "Mark as spam", "Cancel"))) return;
    try {
      const said = await invoke("invite_spam", { event: inv.event, series: inv.series });
      status(`Spam: ${said.join("; ")}.`);
      calInvitesCount();
      after?.();
    } catch (err) { showError("Marking the invitation as spam", err); }
  }, "danger-soft");
  if (inv.join_url) button("Join the meeting", () => openLink(inv.join_url));
  button("Show in calendar", async () => {
    if (fromEmail) await openCalendar();
    calState.day = calMidnight(new Date(inv.start * 1000));
    calSetMode("day");
  });
  if (inv.message && !fromEmail) button("Open the email", () => openInnerReader(inv.message, inv.subject));
  return [body, actions];
}

/** The Invites view: invitations ahead waiting for an answer, a card per day. */
async function calInvitesView() {
  let list;
  try { list = await invoke("invites_list"); } catch (err) { showError("Reading your invitations", err); return; }
  const box = el("div", "cal-tripsview");
  if (!list.length) box.append(el("p", "p-note", "No invitations are waiting for an answer. (Invitations from senders Eudora treats as spam aren't listed.)"));
  const days = new Map();
  for (const inv of list) {
    const s = new Date(inv.start * 1000);
    const day = inv.all_day ? new Date(s.getUTCFullYear(), s.getUTCMonth(), s.getUTCDate()) : calMidnight(s);
    if (!days.has(day.getTime())) days.set(day.getTime(), []);
    days.get(day.getTime()).push(inv);
  }
  for (const [day, invs] of days) {
    const card = el("section", "cal-trip-card");
    const head = el("div", "cal-trip-head");
    const clashes = invs.filter((i) => i.conflicts.length).length;
    head.append(el("h3", "", calInviteDay.format(new Date(day))), el("span", "cal-trip-dates", `${invs.length} ${invs.length === 1 ? "invitation" : "invitations"}${clashes ? ` · ${clashes} with a conflict` : ""}`));
    card.append(head);
    for (const inv of invs) {
      const item = el("div", `cal-trip-item${inv.conflicts.length ? " clash" : ""}`);
      const mark = el("div", "cal-trip-icon");
      mark.append(icon("calendar-days"));
      item.append(mark, ...calInviteParts(inv, () => calRender(), false));
      card.append(item);
    }
    box.append(card);
  }
  calShowKeepingScroll(box);
}

/** The Invites button's count: invitations waiting for an answer. */
async function calInvitesCount() {
  let list;
  try { list = await invoke("invites_list"); } catch { return; }
  const badge = $("cal-invites-count");
  badge.textContent = list.length ? String(list.length) : "";
  badge.hidden = !list.length;
}
if (!READER) {
  setTimeout(calInvitesCount, 25_000);
  setInterval(calInvitesCount, 300_000);
}

/** An invitation email's bar: its meeting and the answers, above the message. */
async function showInviteBar(m) {
  const bar = $("m-invite");
  bar.hidden = true;
  bar.replaceChildren();
  // (a meeting in the message itself, as Outlook and Exchange send one, or an attached .ics)
  const hasFile = m.calendar || (m.attachments || []).some((a) => /text\/calendar/i.test(a.mime) || /\.ics$/i.test(a.name || ""));
  if (!hasFile) return;
  let found;
  try { found = await invoke("invitation", { id: m.id }); } catch { return; }
  if (!found || current !== m || found.method === "REPLY") return;
  if (found.method === "CANCEL") {
    bar.append(el("span", "cal-trip-status off", "Canceled"), document.createTextNode(` The organizer canceled “${found.summary || m.subject}”.`));
  } else if (!found.invite) {
    // not on a calendar Eudora answers through (an IMAP account's, or not read from the server
    // yet): answered by email, as calendars answer each other, or only added to the user's
    // calendar (nobody is told)
    const name = found.summary || m.subject;
    const added = found.added || store.get(`inviteAdded:${m.id}`, null);
    const answered = { accept: "Accepted", tentative: "Tentative", decline: "Declined" }[found.answered];
    const body = el("div", "cal-trip-body");
    body.append(el("span", `cal-trip-status ${answered ? (found.answered === "decline" ? "off" : "ok") : "ask"}`, answered ? `${answered} by email` : "Invitation"));
    body.append(document.createTextNode(
      answered ? ` You answered “${name}”; the organizer's calendar records it from your email.${added ? " It's on your calendar." : ""}`
      : added ? ` “${name}” is on your calendar. You can still answer the organizer.`
      : ` “${name}” isn't on a calendar here yet. Answer it here (the organizer gets your answer by email), or add it to your calendar without answering.`));
    bar.append(body);
    const actions = el("div", "cal-trip-actions");
    const answer = (what, label) => {
      const b = el("button", `cal-trip-btn ${what === "accept" ? "primary" : ""}`, label);
      b.addEventListener("click", async () => {
        const note = document.createElement("textarea");
        note.rows = 3;
        note.placeholder = "A note to the organizer (optional)";
        note.style.cssText = "width:100%;box-sizing:border-box;margin-bottom:10px";
        const go = await ask({
          title: `${label}: ${name}`,
          text: `Your answer goes to the organizer by email, and their calendar records it.${what !== "decline" && !added ? " The meeting is added to your calendar too." : ""}`,
          content: note,
          buttons: [{ label: "Cancel", value: false }, { label: "Send the answer", value: true, primary: true }],
        });
        if (!go) return;
        b.disabled = true;
        try {
          const said = await invoke("invitation_answer", { id: m.id, answer: what, comment: note.value.trim() || null });
          status(`${said}.`);
          showInviteBar(m);
        } catch (err) {
          b.disabled = false;
          showError("Answering the invitation", err);
        }
      });
      actions.append(b);
    };
    if (found.answered !== "accept") answer("accept", "Accept");
    if (found.answered !== "tentative") answer("tentative", "Tentative");
    if (found.answered !== "decline") answer("decline", "Decline");
    bar.append(actions);
    if (!added && !found.answered) {
      const add = el("button", "cal-trip-btn", "Add to my calendar");
      add.addEventListener("click", async () => {
        add.disabled = true;
        try {
          await invoke("invitation_add", { id: m.id, calendar: null });
          store.set(`inviteAdded:${m.id}`, true);
          status(`“${name}” is on your calendar.`);
          showInviteBar(m);
        } catch (err) {
          add.disabled = false;
          showError("Adding the meeting", err);
        }
      });
      actions.append(add);
    }
  } else {
    const item = el("div", "cal-trip-item");
    const mark = el("div", "cal-trip-icon");
    mark.append(icon("calendar-days"));
    item.append(mark, ...calInviteParts(found.invite, () => showInviteBar(m), true));
    bar.append(item);
  }
  bar.hidden = false;
}

/** The Trips button's count: reservations waiting for an answer (said once in the status bar). */
let calTripsTold = false;
async function calTripsCount() {
  let list;
  try { list = await invoke("reservations_list"); } catch { return; }
  const n = list.filter((r) => r.status === "suggested").length;
  const badge = $("cal-trips-count");
  badge.textContent = n ? String(n) : "";
  badge.hidden = !n;
  if (n && !calTripsTold) {
    calTripsTold = true;
    status(`${n} ${n === 1 ? "hotel stay" : "hotel stays"} from your travel mail ${n === 1 ? "isn't" : "aren't"} on your calendar: Calendar ▸ Trips.`);
  }
}
if (!READER) {
  setTimeout(calTripsCount, 20_000);
  setInterval(calTripsCount, 600_000);
}

// ------------------------------------------------------------------ Settings ▸ Speed and maintenance
const gb = (bytes) => `${(bytes / 1024 ** 3).toFixed(bytes % 1024 ** 3 ? 1 : 0)} GB`;
async function memoryPrefsFill() {
  try {
    const m = await invoke("memory_get");
    $("p-memory").value = m.choice;
    // the window keeps two connections and the download one: each its own cache; the mapped
    // part is shared through the system's file cache
    const uses = (c) => m.sizes[c].cache * 3 + m.sizes[c].mapped;
    for (const o of $("p-memory").options) {
      const fits = !m.total || uses(o.value) <= m.total / 2;
      o.textContent = `${{ normal: "Normal", more: "More", most: "Most" }[o.value]} (up to ${gb(uses(o.value))})${!fits ? " — more than this computer can spare" : ""}`;
    }
    $("p-memory-note").textContent = m.total
      ? `This computer has ${gb(m.total)} of memory. ${uses("most") <= m.total / 2 ? "Most fits comfortably." : uses("more") <= m.total / 2 ? "More fits comfortably; Most would leave little for other programs." : "Normal suits it best."}`
      : "";
  } catch { /* (left as it was) */ }
}
$("prefs-open")?.addEventListener("click", () => setTimeout(memoryPrefsFill, 0));
$("p-memory").addEventListener("change", async () => {
  try {
    await invoke("memory_set", { choice: $("p-memory").value });
    status(`Memory for speed: ${$("p-memory").selectedOptions[0].textContent}. In effect now; the background download restarts with it.`);
    memoryPrefsFill();
  } catch (err) { showError("Changing the memory for speed", err); }
});

// Maintenance: when it runs by itself, Run Maintenance Now, its last run
for (let h = 0; h < 24; h++) {
  $("p-maint-hour").append(new Option(new Date(2000, 0, 1, h).toLocaleTimeString([], { hour: "numeric" }), String(h)));
}
function maintShowParts() {
  const when = $("p-maint-when").value;
  $("p-maint-day-label").hidden = when !== "weekly";
  $("p-maint-hour-label").hidden = when === "off";
  $("p-maint-idle").closest("label").hidden = when === "off";
}
async function maintenancePrefsFill() {
  try {
    const m = await invoke("maintenance_get");
    $("p-maint-when").value = m.schedule.when;
    $("p-maint-day").value = String(m.schedule.weekday);
    $("p-maint-hour").value = String(m.schedule.hour);
    $("p-maint-idle").checked = m.schedule.idle_only;
    maintShowParts();
    const l = m.last;
    $("p-maint-last").textContent = m.waiting
      ? "Asked for: the background download starts it within seconds (the status bar shows each step)."
      : l?.at
        ? `Last run ${new Date(l.at * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })}, ${l.took < 60 ? `${l.took} s` : `${Math.round(l.took / 60)} min`}: ${l.said}.`
        : "Not run yet.";
  } catch { /* (left as it was) */ }
}
$("prefs-open")?.addEventListener("click", () => setTimeout(maintenancePrefsFill, 0));
for (const id of ["p-maint-when", "p-maint-day", "p-maint-hour", "p-maint-idle"]) {
  $(id).addEventListener("change", async () => {
    maintShowParts();
    const schedule = { when: $("p-maint-when").value, hour: Number($("p-maint-hour").value), weekday: Number($("p-maint-day").value), idle_only: $("p-maint-idle").checked };
    try { await invoke("maintenance_set", { schedule }); } catch (err) { showError("Changing when maintenance runs", err); }
  });
}
$("p-maint-now").addEventListener("click", async () => {
  try {
    await invoke("maintenance_now");
    status("Maintenance asked for: the background download starts it within seconds. The status bar shows each step; click it for the details.");
    maintenancePrefsFill();
  } catch (err) { showError("Running maintenance", err); }
});
