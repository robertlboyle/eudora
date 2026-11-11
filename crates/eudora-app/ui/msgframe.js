// The frame a message is shown in, shared by the main window (app.js), the calendar, and the
// .msg viewer (msgview.js): a sandbox with no scripts and nothing from the network, the email's
// colors in dark mode, scrolling by wheel.

/** HTML mail goes in a sandbox: no scripts, nothing loaded from the network (pictures from
 * the web stay blank); a click on a link goes to `openLink`. */
function sandboxed(html) {
  const policy = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:";
  const head = `<meta http-equiv="Content-Security-Policy" content="${policy}">` +
    `<style>body{font:14px/1.45 system-ui,-apple-system,"Segoe UI",sans-serif;margin:12px 16px;color:#1d1d1f;background:#fff;word-wrap:break-word}img{max-width:100%;height:auto}` +
    `@media screen and (prefers-color-scheme: dark){html{background:${DARK_PAGE}}body{color:${DARK_TEXT};background:${DARK_PAGE}}a{color:#8ab4f8}}</style>`;
  return `<!doctype html><html><head>${head}</head><body>${html}</body></html>`;
}

const DARK_PAGE = "#1e1d1b";
const DARK_TEXT = "#e6e4e0";

/** A message in dark mode, as Outlook shows it: the email's own colors flipped, light backgrounds made dark and dark
 *  text made light, each keeping its hue (links stay blue, highlights their color); pictures as
 *  they are. Done on the shown copy only, after it loads. */
function darkenMessage(doc) {
  const win = doc.defaultView;
  if (!win || !win.matchMedia("(prefers-color-scheme: dark)").matches) return;
  const rgba = (c) => {
    const m = /rgba?\(([\d.]+),\s*([\d.]+),\s*([\d.]+)(?:,\s*([\d.]+))?\)/.exec(c || "");
    return m ? [+m[1] / 255, +m[2] / 255, +m[3] / 255, m[4] === undefined ? 1 : +m[4]] : null;
  };
  const hsl = ([r, g, b]) => {
    const max = Math.max(r, g, b), min = Math.min(r, g, b), l = (max + min) / 2;
    if (max === min) return [0, 0, l];
    const d = max - min, s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    const h = max === r ? (g - b) / d + (g < b ? 6 : 0) : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
    return [h * 60, s, l];
  };
  const css = ([h, s, l], a) => `hsla(${h.toFixed(0)}, ${(s * 100).toFixed(0)}%, ${(l * 100).toFixed(0)}%, ${a})`;
  const elements = doc.body ? [doc.body, ...doc.body.querySelectorAll("*")].slice(0, 6000) : [];
  // read all first, then write (one layout, however long the message)
  const changes = [];
  for (const el of elements) {
    const cs = win.getComputedStyle(el);
    const bg = rgba(cs.backgroundColor);
    if (bg && bg[3] > 0.05) {
      const [h, s, l] = hsl(bg);
      // a light background goes dark (white: the page's own dark; light tints: dark tints)
      if (l > 0.5) changes.push([el, "background-color", css([h, s * 0.6, 0.11 + (1 - l) * 0.3], bg[3])]);
    }
    const fg = rgba(cs.color);
    if (fg) {
      const [h, s, l] = hsl(fg);
      // dark text goes light (black: the page's own light; colored text: a light version)
      if (l < 0.6) changes.push([el, "color", css([h, s, 0.9 - l * 0.35], fg[3])]);
    }
    if (el.hasAttribute("bgcolor")) el.removeAttribute("bgcolor");
  }
  for (const [el, prop, value] of changes) el.style.setProperty(prop, value, "important");
}

/** A message shown in a frame scrolls with the wheel and a touchpad's two fingers, done by
 *  Eudora itself: the box under the
 *  pointer that can still move that way (a wide table…), else the message. Ctrl+wheel (zoom) is
 *  left alone; Shift+wheel goes sideways. */
function scrollByWheel(doc) {
  const win = doc.defaultView;
  if (!win) return;
  doc.addEventListener("wheel", (e) => {
    if (e.ctrlKey) return;
    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? win.innerHeight : 1;
    let dx = e.deltaX * unit;
    let dy = e.deltaY * unit;
    if (e.shiftKey && !dx) { dx = dy; dy = 0; }
    const room = (n) => {
      const s = win.getComputedStyle(n);
      const down = dy && /auto|scroll/.test(s.overflowY) && (dy < 0 ? n.scrollTop > 0 : n.scrollTop + n.clientHeight < n.scrollHeight - 1);
      const across = dx && /auto|scroll/.test(s.overflowX) && (dx < 0 ? n.scrollLeft > 0 : n.scrollLeft + n.clientWidth < n.scrollWidth - 1);
      return down || across;
    };
    let target = doc.scrollingElement || doc.documentElement;
    for (let n = e.target?.nodeType === 1 ? e.target : e.target?.parentElement; n && n !== doc.body && n !== doc.documentElement; n = n.parentElement) {
      if (room(n)) { target = n; break; }
    }
    e.preventDefault();
    target.scrollBy(dx, dy);
  }, { passive: false });
}
