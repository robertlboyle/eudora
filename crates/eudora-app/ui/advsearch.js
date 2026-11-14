// Advanced Search (classic Eudora's Find Messages window).
//
// Classic 7.1 (SearchView.cpp, IDR_SEARCH_WND): up to five criteria rows of [category] [compare]
// [value], More / Fewer, Match All / Match Any, a Mailboxes tab of what to search, the results in
// a list. Here: any number of rows, + / − on each, More / Fewer, Match all / any, the accounts to
// search, and the search text the rows make, shown (so people learn it) and editable. Search puts
// it in the search box: the results are the usual search results.
//
// Every row becomes words the search already understands (aqs.rs, search_marks.rs, header search),
// so nothing here searches by itself. Accessibility: every control is named by its row ("Condition
// 2: field"), adding or removing a row is announced, and focus goes where the work continues.

(() => {
  "use strict";

  /** The fields, in classic's spirit (its Anywhere, Headers, Body, Attachment Name(s), Status,
   *  Priority, Label, Date, Size, Personality, Mailbox Name, To, From, Subject, CC, BCC, Any
   *  Recipient) with today's additions. `kind` decides the tests and the value's control. */
  const FIELDS = [
    { key: "anywhere", label: "Anywhere", kind: "text", q: "" },
    { key: "from", label: "From", kind: "address", q: "from" },
    { key: "to", label: "To", kind: "address", q: "to" },
    { key: "cc", label: "Cc", kind: "address", q: "cc" },
    { key: "bcc", label: "Bcc", kind: "address", q: "bcc" },
    { key: "recipients", label: "Any recipient (To, Cc, Bcc)", kind: "address", q: "recipient" },
    { key: "subject", label: "Subject", kind: "text", q: "subject" },
    { key: "body", label: "Body (the message's text)", kind: "text", q: "body" },
    { key: "attachment", label: "Attachment name", kind: "text", q: "attachment" },
    { key: "hasattachment", label: "Has attachments", kind: "yesno" },
    { key: "date", label: "Date (received or sent)", kind: "date" },
    { key: "size", label: "Size", kind: "size" },
    { key: "folder", label: "Mailbox (folder)", kind: "folder" },
    { key: "account", label: "Account", kind: "account" },
    { key: "label", label: "Label", kind: "label" },
    { key: "status", label: "Status", kind: "status" },
    { key: "priority", label: "Priority", kind: "priority" },
    { key: "header", label: "Header named…", kind: "header" },
  ];

  /** The tests each kind offers (classic's words). */
  const VERBS = {
    text: [["contains", "contains"], ["not", "doesn't contain"], ["is", "is exactly"], ["isnt", "isn't exactly"], ["starts", "starts with"]],
    address: [["contains", "contains"], ["not", "doesn't contain"], ["is", "is"], ["isnt", "isn't"], ["starts", "starts with"], ["ends", "ends with (a domain)"]],
    header: [["contains", "contains"], ["not", "doesn't contain"], ["present", "is present"], ["absent", "isn't present"]],
    yesno: [["is", "is"]],
    date: [["on", "is on"], ["noton", "isn't on"], ["after", "is after"], ["before", "is before"], ["between", "is between"], ["within", "is within"]],
    size: [["greater", "is greater than"], ["less", "is less than"], ["between", "is between"]],
    folder: [["is", "is"], ["isnt", "isn't"]],
    account: [["is", "is"], ["isnt", "isn't"]],
    label: [["is", "is"], ["isnt", "isn't"]],
    status: [["is", "is"], ["isnt", "isn't"]],
    priority: [["is", "is"], ["isnt", "isn't"]],
  };

  const PERIODS = [["today", "today"], ["yesterday", "yesterday"], ["this week", "this week"], ["last week", "last week"], ["this month", "this month"], ["last month", "last month"], ["this year", "this year"], ["last year", "last year"]];
  const STATUSES = [["unread", "Unread"], ["read", "Read"], ["replied", "Replied"], ["forwarded", "Forwarded"], ["redirected", "Redirected"], ["flagged", "Flagged (To Do)"]];
  const PRIORITIES = [["highest", "Highest"], ["high", "High"], ["normal", "Normal"], ["low", "Low"], ["lowest", "Lowest"], ["none", "None set"]];
  /** Every message (a group of a "doesn't" test beside OR needs something to take it from). */
  const EVERY = "received:>=1970-01-01";

  const blank = () => ({ field: "anywhere", verb: "contains", value: "", value2: "", unit: "KB", name: "" });
  let saved = null;
  try { saved = JSON.parse(localStorage.getItem("eudora.advSearch") || "null"); } catch { saved = null; }
  const model = {
    rows: Array.isArray(saved?.rows) && saved.rows.length ? saved.rows : [blank()],
    any: !!saved?.any,
    accounts: Array.isArray(saved?.accounts) ? saved.accounts : [],
    edited: false,
  };
  const keep = () => {
    try { localStorage.setItem("eudora.advSearch", JSON.stringify({ rows: model.rows, any: model.any, accounts: model.accounts })); } catch { /* (a private window) */ }
  };

  const box = () => document.getElementById("adv-search");
  const fieldOf = (key) => FIELDS.find((f) => f.key === key) || FIELDS[0];
  const make = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };
  const say = (text) => (typeof announce === "function" ? announce(text) : undefined);
  const selectOf = (pairs, value, name) => {
    const s = make("select");
    for (const [v, t] of pairs) {
      const o = make("option", "", t);
      o.value = v;
      s.append(o);
    }
    s.value = pairs.some(([v]) => v === value) ? value : pairs[0]?.[0] ?? "";
    s.setAttribute("aria-label", name);
    return s;
  };

  // ---------------------------------------------------------------- the words each row makes

  /** A value as one search word: quoted when it has spaces or signs the search reads. */
  const word = (v) => (/^[\p{L}\p{N}@._'-]+$/u.test(v) ? v : `"${v.replace(/"/g, "")}"`);
  const phrase = (v) => `"${v.replace(/"/g, "")}"`;

  /** The search words for one row (null: it says nothing yet), and whether they're a "doesn't". */
  function rowTerm(r) {
    const f = fieldOf(r.field);
    const v = String(r.value ?? "").trim();
    const p = f.q ? `${f.q}:` : "";
    switch (f.kind) {
      case "text":
      case "address": {
        if (!v) return null;
        switch (r.verb) {
          case "not": return { t: `-${p}${word(v)}`, neg: true };
          case "is": return { t: `${p}${phrase(v)}` };
          case "isnt": return { t: `-${p}${phrase(v)}`, neg: true };
          case "starts": {
            const plain = v.replace(/[^\p{L}\p{N}\s]/gu, " ").trim();
            if (!plain) return null;
            // one word: word*; several: a phrase whose last word is a start
            return { t: /\s/.test(plain) ? `${p}"${plain}"*` : `${p}${plain}*` };
          }
          case "ends": return { t: `${p}${phrase(v.replace(/^@/, ""))}` };
          default: return { t: `${p}${word(v)}` };
        }
      }
      case "header": {
        const name = String(r.name ?? "").trim().replace(/:$/, "");
        if (!/^[A-Za-z][A-Za-z0-9_-]*$/.test(name)) return null;
        if (r.verb === "present") return { t: `${name}:*` };
        if (r.verb === "absent") return { t: `-${name}:*`, neg: true, mark: true };
        if (!v) return null;
        return r.verb === "not" ? { t: `-${name}:${word(v)}`, neg: true, mark: true } : { t: `${name}:${word(v)}` };
      }
      case "yesno":
        return { t: `hasattachment:${v === "no" ? "no" : "yes"}` };
      case "date": {
        if (r.verb === "within") return { t: `received:${/\s/.test(v || "today") ? phrase(v) : v || "today"}` };
        if (!v) return null;
        if (r.verb === "between") {
          const w = String(r.value2 ?? "").trim();
          if (!w) return null;
          const [a, b] = v <= w ? [v, w] : [w, v];
          return { t: `received:${a}..${b}` };
        }
        const t = { on: v, noton: v, after: `>${v}`, before: `<${v}` }[r.verb] ?? v;
        return r.verb === "noton" ? { t: `-received:${t}`, neg: true } : { t: `received:${t}` };
      }
      case "size": {
        const n = Number(v);
        if (!v || !(n >= 0)) return null;
        const u = r.unit === "MB" ? "MB" : "KB";
        if (r.verb === "between") {
          const m = Number(String(r.value2 ?? "").trim());
          if (!(m >= 0) || String(r.value2 ?? "").trim() === "") return null;
          return { t: `size:${Math.min(n, m)}${u}..${Math.max(n, m)}${u}` };
        }
        return { t: `size:${r.verb === "less" ? "<" : ">"}${n}${u}` };
      }
      case "folder":
        if (!v) return null;
        return r.verb === "isnt" ? { t: `-folder:${word(v)}`, neg: true } : { t: `folder:${word(v)}` };
      case "account":
        if (!v) return null;
        return r.verb === "isnt" ? { t: `-account:${word(v)}`, neg: true, mark: true } : { t: `account:${word(v)}` };
      case "label": {
        if (!v) return null;
        const t = `label:${word(v)}`;
        return r.verb === "isnt" ? { t: `-${t}`, neg: true, mark: true } : { t };
      }
      case "status":
        return r.verb === "isnt" ? { t: `-is:${v || "unread"}`, neg: true, mark: true } : { t: `is:${v || "unread"}` };
      case "priority":
        return r.verb === "isnt" ? { t: `-priority:${v || "high"}`, neg: true, mark: true } : { t: `priority:${v || "high"}` };
      default:
        return null;
    }
  }

  /** The whole search the rows make. Match all: the rows side by side (every one must hold).
   *  Match any: joined by OR; a "doesn't" row then gets a group of its own ("every message
   *  except…"), as the search reads `a OR -b` as "a, and not b". The accounts chosen are
   *  required beside either. */
  function buildQuery() {
    const terms = model.rows.map(rowTerm).filter(Boolean);
    let main = "";
    if (model.any && terms.length > 1) {
      main = terms.map((x) => (x.neg && !x.mark ? `( ${EVERY} ${x.t} )` : x.t)).join(" OR ");
    } else {
      main = terms.map((x) => x.t).join(" ");
    }
    const accounts = model.accounts.filter(Boolean).map((a) => `account:${word(a)}`);
    const scope = accounts.length > 1 ? `( ${accounts.join(" OR ")} )` : accounts[0] || "";
    if (scope && model.any && terms.length > 1) main = `( ${main} )`;
    return [main, scope].filter(Boolean).join(" ");
  }

  // ---------------------------------------------------------------- the window

  /** The value control(s) for a row, by its field. */
  function valueControls(r, n, onChange) {
    const f = fieldOf(r.field);
    const name = (what) => `Condition ${n}: ${what}`;
    const text = (prop, what, type = "text", placeholder = "") => {
      const i = make("input");
      i.type = type;
      i.value = r[prop] ?? "";
      i.placeholder = placeholder;
      i.spellcheck = false;
      i.autocomplete = "off";
      i.setAttribute("aria-label", name(what));
      i.addEventListener("input", () => { r[prop] = i.value; onChange(); });
      return i;
    };
    const pick = (pairs, what, fallback) => {
      if (!pairs.some(([v]) => v === r.value)) r.value = fallback ?? pairs[0]?.[0] ?? "";
      const s = selectOf(pairs, r.value, name(what));
      s.addEventListener("change", () => { r.value = s.value; onChange(); });
      return s;
    };
    switch (f.kind) {
      case "text":
        return [text("value", "words", "text", r.field === "attachment" ? "a file name, like invoice.pdf" : "words or a phrase")];
      case "address":
        return [text("value", "name or address", "text", r.verb === "ends" ? "example.com" : "a name or an address")];
      case "header": {
        const nameBox = text("name", "header's name", "text", "List-Id");
        nameBox.setAttribute("list", "adv-headers");
        const out = [nameBox];
        if (r.verb === "contains" || r.verb === "not") out.push(text("value", "header's words", "text", "words in it"));
        return out;
      }
      case "yesno":
        return [pick([["yes", "Yes"], ["no", "No"]], "has attachments", "yes")];
      case "date":
        if (r.verb === "within") return [pick(PERIODS, "period", "this week")];
        if (r.verb === "between") {
          const and = make("span", "as-and", "and");
          and.setAttribute("aria-hidden", "true");
          return [text("value", "from date", "date"), and, text("value2", "to date", "date")];
        }
        return [text("value", "date", "date")];
      case "size": {
        const unit = selectOf([["KB", "KB"], ["MB", "MB"]], r.unit || "KB", name("unit"));
        unit.addEventListener("change", () => { r.unit = unit.value; onChange(); });
        const first = text("value", r.verb === "between" ? "smallest size" : "size", "number");
        first.min = "0";
        if (r.verb !== "between") return [first, unit];
        const second = text("value2", "largest size", "number");
        second.min = "0";
        const and = make("span", "as-and", "and");
        and.setAttribute("aria-hidden", "true");
        return [first, and, second, unit];
      }
      case "folder": {
        const i = text("value", "mailbox's name", "text", "Inbox, or a path like Bulk/Marketing");
        i.setAttribute("list", "adv-folders");
        return [i];
      }
      case "account": {
        const accounts = (state.accounts?.accounts || []).map((a) => [a.address || a.name, a.label ? `${a.label} (${a.address || a.name})` : a.address || a.name]);
        return [accounts.length ? pick(accounts, "account") : text("value", "account's address")];
      }
      case "label": {
        const labels = (state.labelDefs || []).map((l) => [l.name, l.name]);
        return [pick([["any", "Any label"], ...labels, ["none", "No label"]], "label", labels[0]?.[0] ?? "any")];
      }
      case "status":
        return [pick(STATUSES, "status", "unread")];
      case "priority":
        return [pick(PRIORITIES, "priority", "high")];
      default:
        return [];
    }
  }

  function describe(r) {
    const f = fieldOf(r.field);
    const verb = (VERBS[f.kind].find(([v]) => v === r.verb) || VERBS[f.kind][0])[1];
    return `${f.label} ${verb}`;
  }

  /** Draws the rows; `focusRow` / `focusWhat` = where the keyboard goes after. */
  function render(focusRow, focusWhat = "field") {
    const list = document.getElementById("as-rows");
    list.replaceChildren();
    const total = model.rows.length;
    model.rows.forEach((r, i) => {
      const n = i + 1;
      const f = fieldOf(r.field);
      if (!VERBS[f.kind].some(([v]) => v === r.verb)) r.verb = VERBS[f.kind][0][0];
      const row = make("div", "as-row");
      row.setAttribute("role", "group");
      row.setAttribute("aria-label", `Condition ${n} of ${total}`);
      const join = make("span", "as-join", i === 0 ? "" : model.any ? "or" : "and");
      join.setAttribute("aria-hidden", "true");
      const field = selectOf(FIELDS.map((x) => [x.key, x.label]), r.field, `Condition ${n}: field`);
      field.className = "as-field";
      field.addEventListener("change", () => {
        r.field = field.value;
        r.value = "";
        r.value2 = "";
        r.verb = VERBS[fieldOf(r.field).kind][0][0];
        render(i, "field");
        changed();
      });
      const verb = selectOf(VERBS[f.kind], r.verb, `Condition ${n}: test`);
      verb.className = "as-verb";
      verb.addEventListener("change", () => {
        const before = r.verb;
        r.verb = verb.value;
        // (between / within and the header's tests change the value's controls)
        if ([before, r.verb].some((v) => ["between", "within", "present", "absent"].includes(v))) render(i, "verb");
        changed();
      });
      const values = make("span", "as-values");
      values.append(...valueControls(r, n, changed));
      const add = make("button", "as-add", "+");
      add.type = "button";
      add.setAttribute("aria-label", `Add a condition after condition ${n}`);
      add.title = "Add a condition after this one";
      add.addEventListener("click", () => addRow(i + 1));
      const remove = make("button", "as-remove", "−");
      remove.type = "button";
      remove.setAttribute("aria-label", `Remove condition ${n}`);
      remove.title = "Remove this condition";
      remove.disabled = total === 1;
      remove.addEventListener("click", () => removeRow(i));
      row.append(join, field, verb, values, add, remove);
      list.append(row);
    });
    document.getElementById("as-fewer").disabled = total === 1;
    if (focusRow !== undefined) {
      const row = list.children[Math.min(focusRow, total - 1)];
      const target = focusWhat === "verb" ? row?.querySelector(".as-verb") : focusWhat === "value" ? row?.querySelector(".as-values input, .as-values select") : row?.querySelector(".as-field");
      target?.focus();
    }
  }

  function addRow(at) {
    const last = model.rows[at - 1];
    // (a new row starts like the one before it: several "From contains" rows are common)
    const r = blank();
    if (last) { r.field = last.field; r.verb = last.verb; }
    model.rows.splice(at, 0, r);
    render(at, "value");
    changed();
    say(`Condition ${at + 1} added: ${describe(r)}. ${model.rows.length} conditions.`);
  }

  function removeRow(i) {
    if (model.rows.length === 1) return;
    const [r] = model.rows.splice(i, 1);
    render(Math.max(0, i - 1), "field");
    changed();
    say(`Condition ${i + 1} removed (${describe(r)}). ${model.rows.length} ${model.rows.length === 1 ? "condition" : "conditions"} left.`);
  }

  /** The rows changed: the search text follows them, unless typed over. */
  function changed() {
    keep();
    if (model.edited) return;
    const q = buildQuery();
    document.getElementById("as-query").value = q;
    document.querySelectorAll("#as-rows .as-join").forEach((j, i) => { j.textContent = i === 0 ? "" : model.any ? "or" : "and"; });
    document.getElementById("as-go").disabled = !q;
  }

  function renderAccounts() {
    const list = document.getElementById("as-accounts");
    list.replaceChildren();
    const accounts = state.accounts?.accounts || [];
    for (const a of accounts) {
      const key = a.address || a.name;
      const label = make("label", "as-account");
      const c = make("input");
      c.type = "checkbox";
      c.checked = model.accounts.includes(key);
      c.addEventListener("change", () => {
        model.accounts = c.checked ? [...new Set([...model.accounts, key])] : model.accounts.filter((x) => x !== key);
        document.getElementById("as-scope-some").checked = model.accounts.length > 0;
        document.getElementById("as-scope-all").checked = model.accounts.length === 0;
        changed();
      });
      label.append(c, ` ${a.label ? `${a.label} (${key})` : key}`);
      if (a.kind === "graph-shared") label.title = "A shared mailbox";
      list.append(label);
    }
    const some = model.accounts.length > 0;
    document.getElementById("as-scope-all").checked = !some;
    document.getElementById("as-scope-some").checked = some;
    list.hidden = !some;
  }

  function fillLists() {
    const folders = document.getElementById("adv-folders");
    const names = new Set();
    for (const f of state.folders || []) {
      const shown = typeof shownPath === "function" ? shownPath(f.path) : f.path;
      names.add(shown);
      if (names.size > 3000) break;
    }
    folders.replaceChildren(...[...names].sort((a, b) => a.localeCompare(b)).map((n) => { const o = make("option"); o.value = n; return o; }));
    const headers = document.getElementById("adv-headers");
    headers.replaceChildren(...["List-Id", "Reply-To", "Return-Path", "Message-ID", "Importance", "X-Priority", "X-Mailer", "User-Agent", "Delivered-To", "Precedence", "Organization", "Authentication-Results", "In-Reply-To"].map((n) => { const o = make("option"); o.value = n; return o; }));
  }

  function open() {
    const d = box();
    if (!d) return;
    const classic = document.body.classList.contains("classic");
    document.getElementById("as-title").textContent = classic ? "Find Messages" : "Advanced Search";
    fillLists();
    document.getElementById(model.any ? "as-any" : "as-all").checked = true;
    renderAccounts();
    model.edited = false;
    document.getElementById("as-edited").hidden = true;
    render();
    changed();
    if (!d.open) d.showModal();
    d.querySelector(".as-row .as-values input, .as-row .as-values select, .as-row .as-field")?.focus();
  }

  function run() {
    const q = document.getElementById("as-query").value.trim();
    if (!q) return;
    $("query").value = q;
    box().close();
    runSearch();
    say(`Searching: ${q}`);
  }

  function wire() {
    const d = box();
    if (!d || d.dataset.wired) return;
    d.dataset.wired = "1";
    for (const id of ["as-all", "as-any"]) {
      document.getElementById(id).addEventListener("change", () => {
        model.any = document.getElementById("as-any").checked;
        changed();
      });
    }
    document.getElementById("as-scope-all").addEventListener("change", () => {
      model.accounts = [];
      renderAccounts();
      changed();
    });
    document.getElementById("as-scope-some").addEventListener("change", () => {
      const list = document.getElementById("as-accounts");
      list.hidden = false;
      list.querySelector("input")?.focus();
    });
    document.getElementById("as-more").addEventListener("click", () => addRow(model.rows.length));
    document.getElementById("as-fewer").addEventListener("click", () => removeRow(model.rows.length - 1));
    document.getElementById("as-clear").addEventListener("click", () => {
      model.rows = [blank()];
      model.accounts = [];
      model.edited = false;
      document.getElementById("as-edited").hidden = true;
      renderAccounts();
      render(0, "field");
      changed();
      say("Cleared: one empty condition.");
    });
    const query = document.getElementById("as-query");
    query.addEventListener("input", () => {
      model.edited = query.value.trim() !== buildQuery();
      document.getElementById("as-edited").hidden = !model.edited;
      document.getElementById("as-go").disabled = !query.value.trim();
    });
    query.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); run(); }
    });
    document.getElementById("as-rebuild").addEventListener("click", () => {
      model.edited = false;
      document.getElementById("as-edited").hidden = true;
      changed();
      query.focus();
    });
    document.getElementById("as-go").addEventListener("click", run);
    document.getElementById("as-close").addEventListener("click", () => d.close());
    document.getElementById("as-help").addEventListener("click", () => invoke("open_help", { topic: "advanced-search" }).catch(() => {}));
    // Enter in a row's box searches (as classic's default Search button)
    document.getElementById("as-rows").addEventListener("keydown", (e) => {
      if (e.key === "Enter" && e.target.tagName === "INPUT") { e.preventDefault(); run(); }
    });
    // (the dialog's own form: nothing is submitted)
    d.addEventListener("submit", (e) => e.preventDefault());
  }

  window.openAdvancedSearch = () => { wire(); open(); };
  /** For tests and the manual: the words a set of rows makes. */
  window.advancedSearchQuery = (rows, any = false, accounts = []) => {
    const was = { rows: model.rows, any: model.any, accounts: model.accounts };
    Object.assign(model, { rows, any, accounts });
    try { return buildQuery(); } finally { Object.assign(model, was); }
  };

  document.getElementById("adv-search-btn")?.addEventListener("click", () => window.openAdvancedSearch());
})();
