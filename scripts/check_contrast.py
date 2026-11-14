"""Measures the color contrast of every look, light and dark, against WCAG 2.2 AA: text 4.5:1 (1.4.3), the parts of controls and
the focus ring 3:1 (1.4.11). Reads the looks' colors from their stylesheets
(crates/eudora-app/ui/*.css: each look's light block and its prefers-color-scheme: dark block).

    py -3 scripts/check_contrast.py          # every pair, the failures marked
    py -3 scripts/check_contrast.py --fail   # exit 1 when any pair fails (for tests)

Standard library only.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

UI = Path(__file__).resolve().parent.parent / "crates" / "eudora-app" / "ui"
LOOKS = ["eudora.css", "thunderbird.css", "lookout-classic.css", "lookout-new.css", "gmail.css", "yahoo.css"]
FOCUS = {"light": "#1a5fd0", "dark": "#7fb0ff"}
# looks whose toolbar is a colored bar with its own (white) text: that text is measured there
TOOLBAR_TEXT = {"lookout-new.css": "#ffffff", "yahoo.css": "#ffffff"}

# (foreground, background(s), the least ratio, what it is)
TEXT_ON = ["window", "sidebar", "list", "reader", "field", "button", "head", "toolbar", "pane-title"]
PAIRS = (
    [("text", bg, 4.5, "text") for bg in TEXT_ON]
    + [("muted", bg, 4.5, "gray text") for bg in TEXT_ON]
    + [
        ("selected-text", "selected", 4.5, "the chosen row's text"),
        ("text", "selected-blur", 4.5, "the chosen row's text, list not focused"),
        ("text", "hover", 4.5, "text under the pointer"),
        ("note-text", "note", 4.5, "a note's text"),
        ("accent", "list", 4.5, "links and accent text"),
        ("accent", "reader", 4.5, "links and accent text"),
        ("line-strong", "field", 3.0, "a field's border"),
        ("line-strong", "button", 3.0, "a button's border"),
    ]
)


def blocks(css: str) -> tuple[str, str]:
    """The look's light block (the first holding --text) and the one inside its dark media query."""
    dark_at = css.find("prefers-color-scheme: dark")
    light = css[: dark_at if dark_at >= 0 else len(css)]
    dark = css[dark_at:] if dark_at >= 0 else ""
    return light, dark


def variables(css: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for name, value in re.findall(r"--([a-z0-9-]+)\s*:\s*([^;]+);", css):
        out.setdefault(name, value.strip())
    return out


def rgb(value: str, under: tuple[float, float, float] | None = None) -> tuple[float, float, float] | None:
    m = re.fullmatch(r"#([0-9a-fA-F]{3,8})", value.strip())
    if not m:
        return None
    h = m.group(1)
    if len(h) in (3, 4):
        h = "".join(c * 2 for c in h)
    r, g, b = (int(h[i : i + 2], 16) / 255 for i in (0, 2, 4))
    if len(h) == 8 and under:
        a = int(h[6:8], 16) / 255
        r, g, b = (a * c + (1 - a) * u for c, u in zip((r, g, b), under))
    return (r, g, b)


def luminance(c: tuple[float, float, float]) -> float:
    def lin(v: float) -> float:
        return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4
    r, g, b = (lin(v) for v in c)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def ratio(a: tuple[float, float, float], b: tuple[float, float, float]) -> float:
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


def shade(color: str, backs: list[tuple[float, float, float]], least: float) -> str:
    """`color` made darker (on light backgrounds) or lighter (on dark ones), keeping its hue,
    until it reaches `least` (plus a little) against every background."""
    c = rgb(color)
    dark_backs = sum(luminance(b) < 0.2 for b in backs) > len(backs) / 2
    for step in range(0, 101):
        t = step / 100
        target = (1.0, 1.0, 1.0) if dark_backs else (0.0, 0.0, 0.0)
        mixed = tuple(ch + (tg - ch) * t for ch, tg in zip(c, target))
        if all(ratio(mixed, b) >= least + 0.15 for b in backs):
            return "#" + "".join(f"{round(v * 255):02x}" for v in mixed)
    return color


def fix_file(path: Path, mode: str, name: str, value: str) -> None:
    css = path.read_text(encoding="utf-8")
    dark_at = css.find("prefers-color-scheme: dark")
    start, end = (0, dark_at if dark_at >= 0 else len(css)) if mode == "light" else (dark_at, len(css))
    part = css[start:end]
    m = re.search(rf"(--{name}\s*:\s*)([^;]+);", part)
    if not m:
        # (a dark block that doesn't set it: added at its first declaration block)
        brace = part.find("{", part.find("{") + 1) if mode == "dark" else -1
        if brace < 0:
            return
        part = part[: brace + 1] + f"\n    --{name}: {value};" + part[brace + 1 :]
    else:
        part = part[: m.start(2)] + value + part[m.end(2) :]
    path.write_text(css[:start] + part + css[end:], encoding="utf-8", newline="\n")


def main() -> int:
    failures = 0
    wanted: dict[tuple[str, str, str], tuple[str, list, float]] = {}
    for name in LOOKS:
        css = (UI / name).read_text(encoding="utf-8")
        light, dark = blocks(css)
        base = variables(light)
        for mode, block in (("light", light), ("dark", dark)):
            v = dict(base)
            if mode == "dark":
                if not dark:
                    continue
                v.update(variables(dark))
            pairs = list(PAIRS) + [(None, bg, 3.0, "the focus ring") for bg in ("window", "list", "reader", "field")]
            for fg, bg, least, what in pairs:
                if bg not in v or (fg and fg not in v):
                    continue
                back = rgb(v[bg], rgb(v.get("window", "#ffffff")))
                if bg == "toolbar" and name in TOOLBAR_TEXT and fg in ("text", "muted"):
                    if fg == "muted":
                        continue
                    fore = rgb(TOOLBAR_TEXT[name])
                else:
                    fore = rgb(FOCUS[mode]) if fg is None else rgb(v[fg], back)
                if not back or not fore:
                    continue
                r = ratio(fore, back)
                bad = r < least
                failures += bad
                if bad and fg and "--fix" in sys.argv:
                    key = (name, mode, fg)
                    color, backs, need = wanted.get(key, (v[fg], [], least))
                    wanted[key] = (color, backs + [back], max(need, least))
                if bad or "--all" in sys.argv:
                    print(f"{'FAIL' if bad else 'ok  '} {name:20} {mode:5} {fg or 'focus':14} on {bg:14} {r:5.2f} (needs {least}) {what}")
    print(f"{failures} pairs below WCAG 2.2 AA")
    for (name, mode, fg), (color, backs, least) in wanted.items():
        # (measured against every background it's used on, not only the failing ones)
        css = (UI / name).read_text(encoding="utf-8")
        light, dark = blocks(css)
        v = variables(light)
        if mode == "dark":
            v.update(variables(dark))
        on = [b for f, b, _, _ in PAIRS if f == fg and b in v]
        all_backs = [rgb(v[b], rgb(v.get("window", "#ffffff"))) for b in on if rgb(v[b], rgb(v.get("window", "#ffffff")))]
        if name in TOOLBAR_TEXT:
            all_backs = [b for b, n in zip(all_backs, on) if n != "toolbar"]
        new = shade(color, all_backs or backs, least)
        print(f"fixed {name} {mode} --{fg}: {color} -> {new}")
        fix_file(UI / name, mode, fg, new)
    return 1 if failures and "--fail" in sys.argv else 0


if __name__ == "__main__":
    raise SystemExit(main())
