"""Finds form fields a screen reader can't name: every <input>, <select>, and <textarea> in the window's pages needs a name: inside
a <label>, a <label for=its id>, aria-label, aria-labelledby, or (last resort) a title.

    py -3 scripts/check_labels.py           # lists the fields without a name
    py -3 scripts/check_labels.py --fail    # exit 1 when there are any

Standard library only. Fields made by the scripts (app.js…) aren't seen here: they're named where
they're made.
"""

from __future__ import annotations

import sys
from html.parser import HTMLParser
from pathlib import Path

UI = Path(__file__).resolve().parent.parent / "crates" / "eudora-app" / "ui"
PAGES = ["index.html", "compose.html", "msgview.html", "ics.html", "help/index.html", "phone/index.html"]
SKIP_TYPES = {"hidden", "submit", "button", "reset", "image"}


class Fields(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.fields: list[tuple[int, str, dict]] = []
        self.label_for: set[str] = set()
        self.in_label = 0

    def handle_starttag(self, tag: str, attrs: list) -> None:
        a = {k: (v or "") for k, v in attrs}
        if tag == "label":
            self.in_label += 1
            if a.get("for"):
                self.label_for.add(a["for"])
        if tag in ("input", "select", "textarea"):
            if tag == "input" and a.get("type", "text") in SKIP_TYPES:
                return
            self.fields.append((self.getpos()[0], tag, {**a, "_in_label": bool(self.in_label)}))

    def handle_endtag(self, tag: str) -> None:
        if tag == "label" and self.in_label:
            self.in_label -= 1


def main() -> int:
    missing = 0
    for page in PAGES:
        path = UI / page
        if not path.exists():
            continue
        p = Fields()
        p.feed(path.read_text(encoding="utf-8"))
        for line, tag, a in p.fields:
            named = a["_in_label"] or a.get("id") in p.label_for or a.get("aria-label") or a.get("aria-labelledby") or a.get("title")
            if not named:
                missing += 1
                print(f"{page}:{line}: <{tag} id={a.get('id') or '?'} type={a.get('type', '')}> has no name for a screen reader")
    print(f"{missing} fields without a name")
    return 1 if missing and "--fail" in sys.argv else 0


if __name__ == "__main__":
    raise SystemExit(main())
