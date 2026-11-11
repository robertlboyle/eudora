// Edit ▸ Find Text (classic Eudora's Find): finds words in the message being read or written,
// marks every match, steps through them (Enter / F3, Shift+Enter / Shift+F3). Shared by the
// main window and the message window. Matches are marked with the CSS highlight API, so the
// text itself (a message being written) is never changed.

/** `places()`: where to look now, [{ doc, root }] (a message's body may be in a frame). */
function textFinder(places) {
  let matches = [];
  let at = -1;

  function styled(doc) {
    if (doc.getElementById("eudora-find-style")) return;
    const s = doc.createElement("style");
    s.id = "eudora-find-style";
    s.textContent = "::highlight(eudora-find){background:#ffe066;color:#000}::highlight(eudora-find-now){background:#ff9632;color:#000}";
    (doc.head || doc.documentElement).append(s);
  }

  function paint() {
    const docs = new Set(matches.map((m) => m.doc));
    for (const { doc } of places()) docs.add(doc);
    for (const doc of docs) {
      const win = doc.defaultView;
      if (!win?.CSS?.highlights || !win.Highlight) continue;
      styled(doc);
      const mine = matches.filter((m, i) => m.doc === doc && i !== at).map((m) => m.range);
      const now = matches.filter((m, i) => m.doc === doc && i === at).map((m) => m.range);
      win.CSS.highlights.set("eudora-find", new win.Highlight(...mine));
      win.CSS.highlights.set("eudora-find-now", new win.Highlight(...now));
    }
  }

  return {
    /** Finds every match of `query` (any case); returns how many. */
    search(query) {
      matches = [];
      at = -1;
      const needle = query.toLowerCase();
      if (needle) {
        for (const { doc, root } of places()) {
          if (!root) continue;
          const walker = doc.createTreeWalker(root, NodeFilter.SHOW_TEXT);
          for (let node = walker.nextNode(); node; node = walker.nextNode()) {
            const text = node.nodeValue.toLowerCase();
            for (let i = text.indexOf(needle); i >= 0; i = text.indexOf(needle, i + needle.length)) {
              const range = doc.createRange();
              range.setStart(node, i);
              range.setEnd(node, i + needle.length);
              matches.push({ doc, range });
            }
          }
        }
      }
      paint();
      return matches.length;
    },
    /** Moves to the next (`step` 1) or previous (−1) match; returns its number (1-based). */
    go(step) {
      if (!matches.length) return 0;
      at = (at + step + matches.length) % matches.length;
      paint();
      const { range } = matches[at];
      (range.startContainer.parentElement || range.startContainer).scrollIntoView({ block: "center" });
      return at + 1;
    },
    clear() {
      matches = [];
      at = -1;
      paint();
    },
    get count() { return matches.length; },
  };
}

/** Wires a find bar (input, count, ↑ ↓ ✕) to a finder. Returns { open() }. */
function findBar(ids, finder, onClose = () => {}) {
  const $f = (id) => document.getElementById(id);
  const say = (n) => {
    const q = $f(ids.input).value;
    $f(ids.count).textContent = !q ? "" : finder.count ? `${n || 0} of ${finder.count}` : "Not found";
  };
  $f(ids.input).addEventListener("input", () => { finder.search($f(ids.input).value); say(finder.go(1)); });
  $f(ids.input).addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === "F3") { e.preventDefault(); say(finder.go(e.shiftKey ? -1 : 1)); }
    if (e.key === "Escape") { e.preventDefault(); close(); }
  });
  $f(ids.next).addEventListener("click", () => say(finder.go(1)));
  $f(ids.prev).addEventListener("click", () => say(finder.go(-1)));
  $f(ids.close).addEventListener("click", () => close());
  function close() {
    $f(ids.bar).hidden = true;
    finder.clear();
    onClose();
  }
  return {
    open() {
      $f(ids.bar).hidden = false;
      $f(ids.input).focus();
      $f(ids.input).select();
      if ($f(ids.input).value) { finder.search($f(ids.input).value); say(finder.go(1)); }
    },
    again(step = 1) {
      if ($f(ids.bar).hidden || !$f(ids.input).value) { this.open(); return; }
      if (!finder.count) finder.search($f(ids.input).value);
      say(finder.go(step));
    },
  };
}
