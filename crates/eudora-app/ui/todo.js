// To Do. In place of the list and the reader when To Do is chosen in the sidebar: an Add box, then
// the tasks by when they're due; a task about a message opens it. Kept in Microsoft To Do (the
// primary account), so every device shares it.

const todoState = { on: false, list: null, count: null, lists: [] };
const todoDay = new Intl.DateTimeFormat("en-US", { weekday: "short", month: "short", day: "numeric" });

/** Midnight UTC of a day picked here (tasks have dates, not times). */
function todoDate(d) {
  return Math.floor(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate()) / 1000);
}
/** A due date (midnight UTC) as this computer's date. */
function todoLocal(due) {
  const u = new Date(due * 1000);
  return new Date(u.getUTCFullYear(), u.getUTCMonth(), u.getUTCDate());
}

async function todoLoad() {
  try { todoState.list = await invoke("todos_list"); } catch (err) { console.warn("todo", err); todoState.list = todoState.list || []; }
  try { todoState.lists = await invoke("todo_lists"); } catch { /* (none known yet) */ }
  todoState.count = todoState.list.filter((t) => !t.done).length;
}

/** The sidebar's To Do line (under the calendar), with how many are open. */
function todoSidebar() {
  if (todoState.list === null) {
    todoState.list = [];
    todoLoad().then(renderSidebar);
  }
  const lists = todoState.lists || [];
  const open = sideItem({
    key: "todo:all", depth: 0, name: "To Do", iconName: "list-check", count: todoState.count || 0, cls: "accounts", children: lists.length > 0,
    tip: "Your tasks, and messages flagged as To Do (Ctrl+Shift+G): kept in Microsoft To Do. Open the arrow to show or hide each list.",
  });
  if (todoState.on) $("folders").lastElementChild.classList.add("on");
  if (!open) return;
  // each list, shown or hidden like a calendar: lists others shared start hidden
  for (const l of lists) {
    sideItem({
      key: `todo:list:${l.id}`, depth: 1, name: l.name, iconName: l.flagged ? "flag" : l.owner ? "list-check" : "users", count: l.shown ? l.open : 0,
      tip: `${l.name}: ${l.owner ? (l.shared ? "your list, shared with others" : "your list") : "shared with you by someone else"}. ${l.shown ? "Shown" : "Hidden"}: click to ${l.shown ? "hide" : "show"} it. (Microsoft To Do keeps it either way.)`,
    });
    // (as a calendar: its color filled when shown, an outline when hidden; right-click for colors)
    const line = $("folders").lastElementChild;
    const swatch = el("span", "cal-swatch");
    swatch.style.background = l.shown ? l.color : "transparent";
    swatch.style.borderColor = l.color;
    line.querySelector(".icon")?.replaceWith(swatch);
    if (!l.shown) line.classList.add("cal-hidden");
  }
}

/** Right-click on a To Do list in the sidebar: its color and Show / Hide, as a calendar's. */
function todoContext(key, e) {
  if (!key.startsWith("todo:list:")) return false;
  e.preventDefault();
  const l = todoState.lists.find((x) => `todo:list:${x.id}` === key);
  if (!l) return true;
  colorMenu(e, {
    name: l.name, color: l.color, own: l.own_color, ownLabel: "Its first color", shown: l.shown, noun: "list",
    pick: async (color) => {
      try {
        await invoke("todo_list_color", { id: l.id, color });
        await todoLoad();
        renderSidebar();
        if (todoState.on) todoRender();
        status(color ? `${l.name} is shown in its new color.` : `${l.name} has its first color again.`);
      } catch (err) { showError("Changing the list's color", err); }
    },
    toggle: () => todoClick(key, { target: document.body }),
  });
  return true;
}

function todoClick(key, e) {
  if (!key.startsWith("todo:")) return false;
  if (e?.target.closest(".twist")) { toggle(key); return true; }
  const list = key.startsWith("todo:list:") ? todoState.lists.find((l) => `todo:list:${l.id}` === key) : null;
  if (list) {
    invoke("todo_list_shown", { id: list.id, shown: !list.shown })
      .then(async () => {
        await todoLoad();
        renderSidebar();
        if (todoState.on) todoRender();
        status(`${list.name} is ${list.shown ? "hidden" : "shown"} in To Do.`);
      })
      .catch((err) => showError("Showing the list", err));
    return true;
  }
  openTodo();
  return true;
}

async function openTodo() {
  window.closeCalendar?.();
  window.closeContacts?.();
  todoState.on = true;
  document.body.classList.add("todo-mode");
  $("todo").hidden = false;
  await todoLoad();
  renderSidebar();
  todoRender();
  $("todo-new").focus();
}

function closeTodo() {
  if (!todoState.on) return;
  todoState.on = false;
  document.body.classList.remove("todo-mode");
  $("todo").hidden = true;
}

/** Brought up to date (after a flag, or now and then). */
async function todoRefresh() {
  await todoLoad();
  renderSidebar();
  if (todoState.on) todoRender();
}

function todoGroup(t, today) {
  if (t.done) return "Done";
  if (t.due == null) return t.list === "Flagged email" || t.message ? "Flagged messages and others" : "Someday";
  const d = todoLocal(t.due);
  if (d < today) return "Overdue";
  if (d.getTime() === today.getTime()) return "Today";
  if (d < calAddDays(today, 7)) return "This week";
  return "Later";
}

function todoRender() {
  const box = $("todo-list");
  const today = calMidnight(new Date());
  const order = ["Overdue", "Today", "This week", "Later", "Flagged messages and others", "Someday", "Done"];
  const groups = new Map(order.map((g) => [g, []]));
  for (const t of todoState.list) groups.get(todoGroup(t, today)).push(t);
  const out = [];
  for (const [name, tasks] of groups) {
    if (!tasks.length) continue;
    out.push(el("h3", `todo-group${name === "Overdue" ? " overdue" : ""}`, `${name} (${tasks.length})`));
    for (const t of tasks) out.push(todoRow(t, today));
  }
  if (!out.length) out.push(el("p", "p-note", "Nothing to do. Add a task above, or flag a message (Ctrl+Shift+G, or right-click ▸ Flag as To Do)."));
  box.replaceChildren(...out);
  $("todo-sync").textContent = todoState.list.some((t) => !t.synced && t.list !== "Flagged email")
    ? "Some tasks are waiting to reach Microsoft To Do."
    : "Kept in Microsoft To Do: Outlook, the To Do app and Eudora on your other computers show the same tasks.";
}

function todoRow(t, today) {
  const row = el("div", `todo-row${t.done ? " done" : ""}`);
  row.dataset.id = t.id;
  const tick = el("input");
  tick.type = "checkbox";
  tick.checked = t.done;
  tick.title = t.done ? "Not done yet" : "Done";
  tick.setAttribute("aria-label", `Done: ${t.title}`);
  tick.addEventListener("change", () => todoChange(t.id, { done: tick.checked }, tick.checked ? `Done: ${t.title}` : `Open again: ${t.title}`));
  const star = el("button", `todo-star${t.important ? " on" : ""}`, t.important ? "★" : "☆");
  star.title = t.important ? "Not important" : "Important";
  star.setAttribute("aria-label", `Important: ${t.title}`);
  star.setAttribute("aria-pressed", String(!!t.important));
  star.addEventListener("click", () => todoChange(t.id, { important: !t.important }));
  const main = el("div", "todo-main");
  main.append(el("div", "todo-title", t.title));
  // its list's color down the left edge, as an event's calendar color
  const list = (todoState.lists || []).find((l) => l.name === t.list);
  if (list) row.style.setProperty("--list", list.color);
  const meta = el("div", "todo-meta");
  if (t.due != null) {
    const d = todoLocal(t.due);
    meta.append(el("span", `todo-due${!t.done && d < today ? " late" : ""}`, d.getTime() === today.getTime() ? "Today" : todoDay.format(d)));
  }
  if (t.message) {
    const link = el("button", "todo-message", "✉ Open Email");
    link.title = "Show the message this task is about";
    link.addEventListener("click", (e) => { e.stopPropagation(); openInnerReader(t.message, t.title); });
    meta.append(link);
  } else if (t.message_id || t.list === "Flagged email") {
    // not found here by its id: looked for by its subject (a flagged email's task is named after it)
    const find = el("button", "todo-message", "✉ Find Email");
    find.title = "Search your mail for this message (its subject)";
    find.addEventListener("click", (e) => { e.stopPropagation(); todoFindMessage(t); });
    meta.append(find);
  }
  if (t.list && t.list !== "Tasks") meta.append(el("span", "todo-note", t.list));
  if (t.notes) meta.append(el("span", "todo-note", t.notes.split("\n")[0].slice(0, 120)));
  main.append(meta);
  row.append(tick, star, main);
  row.addEventListener("dblclick", () => (t.message ? openInnerReader(t.message, t.title) : todoEdit(t)));
  // the keyboard and screen readers: Tab reaches each task, read with its due day and list;
  // Enter opens it (its message, else the editor), Shift+F10 / the Menu key its menu
  row.tabIndex = 0;
  row.setAttribute("role", "group");
  row.setAttribute("aria-label", [t.title, t.done ? "done" : "", t.important ? "important" : "", t.due != null ? `due ${todoDay.format(todoLocal(t.due))}` : "", t.list && t.list !== "Tasks" ? t.list : "", t.message ? "about an email" : ""].filter(Boolean).join(", "));
  row.addEventListener("keydown", (e) => {
    if (e.target !== row) return;
    if (e.key === "Enter") { e.preventDefault(); t.message ? openInnerReader(t.message, t.title) : todoEdit(t); }
    else if (e.key === " ") { e.preventDefault(); tick.click(); }
    else if ((e.key === "F10" && e.shiftKey) || e.key === "ContextMenu") { e.preventDefault(); window.openContextMenu?.(row); }
  });
  return row;
}

/** A task whose message isn't known here: a search for it, by its Message-ID when the task has
 *  one, else its subject. */
function todoFindMessage(t) {
  closeTodo();
  const subject = t.title.replace(/"/g, " ").trim();
  $("query").value = t.message_id ? `message-id:"${t.message_id.replace(/[<>"]/g, "")}"` : `subject:"${subject}"`;
  runSearch();
  if (t.message_id) {
    // (no Message-ID match: the subject instead)
    setTimeout(() => { if (state.list?.kind === "search" && !state.list.total) { $("query").value = `subject:"${subject}"`; runSearch(); } }, 1500);
  }
}

async function todoChange(id, args, said) {
  try {
    await invoke("todo_update", { id, ...args });
    if (said) status(said);
    await todoRefresh();
  } catch (err) { showError("Changing the task", err); }
}

/** Edit: title, notes, due date. */
async function todoEdit(t) {
  const box = el("div", "todo-edit");
  const title = el("input");
  title.value = t.title;
  const due = el("input");
  due.type = "date";
  if (t.due != null) {
    const d = todoLocal(t.due);
    due.value = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
  }
  const notes = el("textarea");
  notes.value = t.notes;
  notes.rows = 5;
  const line = (label, input) => { const l = el("label", "todo-edit-line"); l.append(el("span", "", label), input); return l; };
  box.append(line("Task", title), line("Due", due), line("Notes", notes));
  const answer = await ask({ title: "Edit the task", text: "", content: box, buttons: [{ label: "Cancel", value: null }, { label: "Save", value: "save", primary: true }] });
  if (answer !== "save") return;
  const [y, m, d] = due.value.split("-").map(Number);
  await todoChange(t.id, { title: title.value, notes: notes.value, due: due.value ? todoDate(new Date(y, m - 1, d)) : null, clearDue: !due.value }, "Task saved.");
}

// the Add box: a title, then Enter (a due date from the picker beside it)
async function todoAddNow() {
  const title = $("todo-new").value.trim();
  if (!title) return;
  const when = $("todo-new-due").value;
  const today = calMidnight(new Date());
  const due = { today: todoDate(today), tomorrow: todoDate(calAddDays(today, 1)), week: todoDate(calAddDays(today, 7)) }[when] ?? null;
  try {
    await invoke("todo_add", { title, due });
    $("todo-new").value = "";
    await todoRefresh();
  } catch (err) { showError("Adding the task", err); }
}
$("todo-add").addEventListener("click", todoAddNow);
$("todo-new").addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); todoAddNow(); } });

// right-click a task
let todoMenuBox = null;
function todoCloseMenu() { todoMenuBox?.remove(); todoMenuBox = null; }
$("todo-list").addEventListener("contextmenu", (e) => {
  const row = e.target.closest(".todo-row");
  if (!row) return;
  e.preventDefault();
  todoCloseMenu();
  const t = todoState.list.find((x) => x.id === Number(row.dataset.id));
  if (!t) return;
  const today = calMidnight(new Date());
  const item = (label, act) => { const b = el("button", "", label); b.addEventListener("click", () => { todoCloseMenu(); act(); }); return b; };
  const due = (label, d) => item(label, () => todoChange(t.id, d == null ? { clearDue: true } : { due: todoDate(d) }, d == null ? "Due date taken off." : `Due ${todoDay.format(d)}.`));
  const box = el("div", "popup-menu todo-menu");
  if (t.message) box.append(item("Open Email", () => openInnerReader(t.message, t.title)));
  box.append(item("Edit…", () => todoEdit(t)), item(t.done ? "Not done yet" : "Done", () => todoChange(t.id, { done: !t.done })), el("hr"));
  box.append(due("Due today", today), due("Due tomorrow", calAddDays(today, 1)), due("Due next week", calAddDays(today, 7)), due("No due date", null), el("hr"));
  box.append(item("Send to Twos", async () => {
    try { await invoke("todo_twos", { id: t.id }); status(`Sent to Twos: ${t.title}`); } catch (err) { showError("Sending to Twos", err); }
  }));
  box.append(el("hr"), item("Delete…", async () => {
    const text = t.list === "Flagged email" ? `Delete “${t.title}”? The message loses its flag (in Outlook too).` : `Delete “${t.title}”? It's deleted from Microsoft To Do too.`;
    if (!(await askYesNo("Delete the task", text, "Delete"))) return;
    try { await invoke("todo_delete", { id: t.id }); status("Task deleted."); await todoRefresh(); } catch (err) { showError("Deleting the task", err); }
  }));
  document.body.append(box);
  box.style.left = `${Math.min(e.clientX, innerWidth - box.offsetWidth - 8)}px`;
  box.style.top = `${Math.min(e.clientY, innerHeight - box.offsetHeight - 8)}px`;
  todoMenuBox = box;
});
document.addEventListener("click", (e) => { if (todoMenuBox && !todoMenuBox.contains(e.target)) todoCloseMenu(); });

// Settings ▸ To Do ▸ Twos: a user id and a token (kept in this computer's credential store)
(async () => {
  if (READER) return;
  try {
    const [user, token] = await invoke("twos_get");
    $("p-twos-user").value = user || "";
    $("p-twos-state").textContent = token ? "A token is kept on this computer." : "No token yet.";
  } catch { /* no library yet */ }
})();
$("p-twos-save").addEventListener("click", async () => {
  try {
    await invoke("twos_set", { user: $("p-twos-user").value, token: $("p-twos-token").value || null });
    $("p-twos-token").value = "";
    $("p-twos-state").textContent = "Saved. The token is kept in this computer's credential store.";
  } catch (err) { showError("Saving Twos' sign-in", err); }
});
$("p-twos-test").addEventListener("click", async () => {
  if (!(await askYesNo("Try Twos", "Add a test line (“Eudora can add to Twos”) to your Twos today list?", "Add it"))) return;
  try { await invoke("twos_test"); $("p-twos-state").textContent = "It worked: look in Twos' today list."; } catch (err) { $("p-twos-state").textContent = String(err); }
});

// up to date now and then (tasks changed in Outlook or on the phone come in with the download)
if (!READER) setInterval(() => { if (!document.querySelector("dialog[open]")) todoRefresh(); }, 120_000);
