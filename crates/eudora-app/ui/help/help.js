// Eudora Help: the user manual (Help ▸ Eudora Help, F1). Plain JS, no libraries.
//
// The manual is a set of chapter files (CHAPTERS), each a list of <section id="…" data-title="…"
// data-index="term; term">. One section shows at a time (by the address's #id); the contents on
// the left, Previous / Next at the bottom, a search over every section's words, and an A to Z
// index made from each section's title and its data-index terms.

"use strict";

const CHAPTERS = [
  ["start.html", "Getting started"],
  ["reading.html", "Reading mail"],
  ["search.html", "Searching"],
  ["writing.html", "Writing and sending"],
  ["organizing.html", "Organizing mail"],
  ["sorting.html", "Sorting, spam, and rules"],
  ["calendar.html", "Calendar, contacts, and To Do"],
  ["background.html", "Downloading and background work"],
  ["keeping.html", "Backups, sync, and upkeep"],
  ["phone.html", "Eudora on your phone"],
  ["access.html", "Accessibility"],
  ["power.html", "Command line, API, and plugins"],
  ["reference.html", "Reference"],
  ["trouble.html", "Troubleshooting"],
  ["more.html", "About Eudora"],
  ["whats-new.html", "What's New"],
];

const page = document.getElementById("page");
const toc = document.getElementById("toc");
const q = document.getElementById("q");
/** Every section in reading order: {id, title, part, el, text}. */
let sections = [];
const byId = new Map();

async function load() {
  const parts = await Promise.all(
    CHAPTERS.map(async ([file, part]) => {
      const reply = await fetch(file);
      if (!reply.ok) throw new Error(`${file}: ${reply.status}`);
      return [part, await reply.text()];
    }),
  );
  for (const [part, html] of parts) {
    const holder = document.createElement("div");
    holder.innerHTML = html;
    for (const el of holder.querySelectorAll("section[id]")) {
      const s = { id: el.id, title: el.dataset.title || el.querySelector("h1")?.textContent || el.id, part, el, text: "" };
      // (a space where each tag was: table cells and list items mustn't run together)
      const plain = document.createElement("textarea");
      plain.innerHTML = el.innerHTML.replace(/<[^>]+>/g, " ");
      s.text = plain.value.replace(/\s+/g, " ").replace(/ ([.,;:)])/g, "$1").trim();
      sections.push(s);
      byId.set(s.id, s);
    }
  }
  sections.push(indexSection());
  byId.set("index", sections[sections.length - 1]);
  buildToc();
  window.addEventListener("hashchange", route);
  route();
}

function buildToc() {
  let part = "";
  for (const s of sections) {
    if (s.part !== part) {
      part = s.part;
      const h = document.createElement("div");
      h.className = "part";
      h.textContent = part;
      toc.append(h);
    }
    const a = document.createElement("a");
    a.href = "#" + s.id;
    a.textContent = s.title;
    a.dataset.id = s.id;
    toc.append(a);
  }
}

/** Shows the section the address names (else the first), or the search results. */
function route() {
  document.body.classList.remove("toc-open");
  const id = decodeURIComponent(location.hash.slice(1));
  if (id.startsWith("search=")) {
    const words = id.slice(7);
    if (q.value !== words) q.value = words;
    showResults(words);
    return;
  }
  const s = byId.get(id) || sections[0];
  page.replaceChildren(s.el.cloneNode(true));
  const i = sections.indexOf(s);
  const pager = document.createElement("div");
  pager.className = "pager";
  const prev = sections[i - 1];
  const next = sections[i + 1];
  pager.innerHTML =
    (prev ? `<a href="#${prev.id}">← ${escape(prev.title)}</a>` : "<span></span>") +
    (next ? `<a href="#${next.id}">${escape(next.title)} →</a>` : "<span></span>");
  page.append(pager);
  for (const a of toc.querySelectorAll("a")) {
    a.classList.toggle("here", a.dataset.id === s.id);
    if (a.dataset.id === s.id) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
  }
  pager.setAttribute("role", "navigation");
  pager.setAttribute("aria-label", "Previous and next page");
  toc.querySelector("a.here")?.scrollIntoView({ block: "nearest" });
  page.scrollTop = 0;
  document.title = `${s.title} – Eudora Help`;
  arrived(s.title);
}

/** A page or results shown: said to a screen reader, and (after the first) the keyboard moved to
 *  the top of what's shown, so reading starts there. */
let firstShown = true;
function arrived(what) {
  const said = document.getElementById("said");
  if (said) { said.textContent = ""; setTimeout(() => { said.textContent = what; }, 50); }
  if (firstShown) { firstShown = false; return; }
  if (document.activeElement !== q) page.focus({ preventScroll: true });
}

function escape(text) {
  return text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

/** Words of a search, lowercase, without punctuation around them. */
function words(text) {
  return text.toLowerCase().split(/\s+/).map((w) => w.replace(/^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$/gu, "")).filter((w) => w.length > 1);
}

/** Sections holding every word searched for, best first: in the title, then in the index terms,
 *  then how often in the text. */
function search(text) {
  const want = words(text);
  if (!want.length) return [];
  const hits = [];
  for (const s of sections) {
    if (s.id === "index") continue;
    const body = s.text.toLowerCase();
    if (!want.every((w) => body.includes(w))) continue;
    const title = s.title.toLowerCase();
    const terms = (s.el.dataset.index || "").toLowerCase();
    let score = 0;
    for (const w of want) {
      if (title.includes(w)) score += 20;
      if (terms.includes(w)) score += 10;
      score += Math.min(body.split(w).length - 1, 10);
    }
    hits.push({ s, score });
  }
  return hits.sort((a, b) => b.score - a.score).map((h) => h.s);
}

/** A line of the section's text around the first word found, the words marked. */
function snippet(s, want) {
  const lower = s.text.toLowerCase();
  let at = Math.max(0, lower.indexOf(want[0]) - 80);
  // (from the start of a word)
  if (at > 0) at = s.text.indexOf(" ", at) + 1 || at;
  let piece = (at > 0 ? "…" : "") + s.text.slice(at, at + 220) + (at + 220 < s.text.length ? "…" : "");
  piece = escape(piece);
  for (const w of want) {
    piece = piece.replace(new RegExp(`(${w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")})`, "gi"), "<mark>$1</mark>");
  }
  return piece;
}

function showResults(text) {
  const want = words(text);
  const found = search(text);
  const out = document.createElement("div");
  const h = document.createElement("h1");
  h.textContent = `Search: ${text}`;
  out.append(h);
  if (!want.length) {
    out.insertAdjacentHTML("beforeend", "<p>Type a word or two to search the manual.</p>");
  } else if (!found.length) {
    out.insertAdjacentHTML(
      "beforeend",
      `<p>No results.</p><p class="see">Try fewer words, or look in the <a href="#index">Index</a>. Features classic Eudora had that are gone are listed in <a href="#removed">Removed features</a>.</p>`,
    );
  } else {
    out.insertAdjacentHTML("beforeend", `<p class="see">${found.length} ${found.length === 1 ? "page" : "pages"}</p>`);
    for (const s of found) {
      out.insertAdjacentHTML("beforeend", `<div class="hit"><a href="#${s.id}">${escape(s.title)}</a> <span class="see">· ${escape(s.part)}</span><p>${snippet(s, want)}</p></div>`);
    }
  }
  page.replaceChildren(out);
  arrived(!want.length ? "Type a word or two to search the manual." : found.length ? `${found.length} ${found.length === 1 ? "page" : "pages"} found` : "No results");
  for (const a of toc.querySelectorAll("a")) a.classList.remove("here");
  page.scrollTop = 0;
  document.title = "Search – Eudora Help";
}

/** The A to Z index: each section's title and its data-index terms, linking to the section. */
function indexSection() {
  const entries = [];
  for (const s of sections) {
    entries.push([s.title, s]);
    for (const term of (s.el.dataset.index || "").split(";")) {
      const t = term.trim();
      if (t) entries.push([t, s]);
    }
  }
  entries.sort((a, b) => a[0].localeCompare(b[0], "en", { sensitivity: "base" }));
  const el = document.createElement("section");
  el.id = "index";
  el.innerHTML = "<h1>Index</h1><p class=\"see\">Every topic in the manual, A to Z. The search box at the top finds any word.</p>";
  let letter = "";
  let list = null;
  const seen = new Set();
  for (const [term, s] of entries) {
    const key = term.toLowerCase() + "→" + s.id;
    if (seen.has(key)) continue;
    seen.add(key);
    const first = /[a-z]/i.test(term[0]) ? term[0].toUpperCase() : "#";
    if (first !== letter) {
      letter = first;
      el.insertAdjacentHTML("beforeend", `<h2 class="index-letter">${letter}</h2>`);
      list = document.createElement("ul");
      list.className = "index-list";
      el.append(list);
    }
    const li = document.createElement("li");
    li.innerHTML = term === s.title ? `<a href="#${s.id}">${escape(term)}</a>` : `<a href="#${s.id}">${escape(term)}</a> <span class="see">(${escape(s.title)})</span>`;
    list.append(li);
  }
  return { id: "index", title: "Index", part: "Index", el, text: "" };
}

let typing = 0;
q.addEventListener("input", () => {
  clearTimeout(typing);
  typing = setTimeout(() => {
    const text = q.value.trim();
    if (text) location.hash = "search=" + encodeURIComponent(text);
    else location.hash = "welcome";
  }, 200);
});
q.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    const first = page.querySelector(".hit a");
    if (first) location.hash = first.getAttribute("href").slice(1);
  }
  if (e.key === "Escape") {
    q.value = "";
    location.hash = "welcome";
  }
});
document.addEventListener("keydown", (e) => {
  const mod = e.ctrlKey || e.metaKey;
  if ((mod && e.key.toLowerCase() === "f") || (e.key === "/" && document.activeElement !== q)) {
    e.preventDefault();
    q.focus();
    q.select();
  }
});
document.getElementById("menu-btn").addEventListener("click", () => document.body.classList.toggle("toc-open"));

load().catch((e) => {
  page.innerHTML = `<h1>Eudora Help</h1><p>The manual couldn't be loaded (${escape(String(e.message || e))}).</p>`;
});
