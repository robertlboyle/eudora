"""Builds Eudora 27's icon files from the vector originals in crates/eudora-app/icons/.

The SVGs are rendered by a headless Chromium-family browser (Edge, Chrome or Chromium: whichever
this computer has, on Windows, the Mac or Linux) with a transparent background (no SVG library
needed), then scaled down: the full design for 32 px and up, the simple envelope-and-pencil
design for 16-24 px (the people turn to mush that small, as classic Eudora knew). Writes:
  icons/icon.ico        every Windows size (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
  icons/icon.png        1024 px (macOS, Linux and anything else that wants a big picture)
  icons/32x32.png, 128x128.png, 128x128@2x.png
  icons/tray-newmail.png, tray-nomail.png   (64 px, for the tray once Eudora has one)
Run: python scripts/make_icons.py   (needs Pillow)
"""
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent.parent / "crates" / "eudora-app" / "icons"


def browser() -> Path:
    """A Chromium-family browser on this computer (EUDORA_BROWSER overrides)."""
    if os.environ.get("EUDORA_BROWSER"):
        return Path(os.environ["EUDORA_BROWSER"])
    places = [
        Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Microsoft/Edge/Application/msedge.exe",
        Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Microsoft/Edge/Application/msedge.exe",
        Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Google/Chrome/Application/chrome.exe",
        Path("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        Path("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ]
    places += [Path(p) for p in (shutil.which(n) for n in ("microsoft-edge", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser")) if p]
    found = next((p for p in places if p.is_file()), None)
    if not found:
        sys.exit("No Edge, Chrome or Chromium found (or set EUDORA_BROWSER to one)")
    return found


def stop(proc: subprocess.Popen) -> None:
    """Stops the browser and the helper processes it started."""
    if os.name == "nt":
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(proc.pid)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


def render(svg: Path, size: int, work: Path) -> Image.Image:
    """The SVG at size x size, transparent where it's empty."""
    edge = browser()
    page = work / f"{svg.stem}-{size}.html"
    page.write_text(
        f"<!doctype html><html><body style='margin:0;background:transparent'>"
        f"<img src='{svg.as_uri()}' width={size} height={size} style='display:block'></body></html>",
        encoding="utf-8",
    )
    out = work / f"{svg.stem}-{size}.png"
    # the browser leaves helper processes behind: never wait for it to exit (they keep it
    # "running"), wait for the picture, then stop it and its helpers. A fresh profile each time.
    edge_proc = subprocess.Popen(
        [str(edge), "--headless", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=1",
         "--default-background-color=00000000", f"--user-data-dir={work / f'profile-{svg.stem}-{size}'}",
         f"--screenshot={out}", f"--window-size={size},{size}", page.as_uri()],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        **({} if os.name == "nt" else {"start_new_session": True}),
    )
    deadline = time.time() + 60
    last = -1
    while time.time() < deadline:
        size_now = out.stat().st_size if out.exists() else -1
        if size_now > 0 and size_now == last:
            break
        last = size_now
        time.sleep(0.5)
    stop(edge_proc)
    if not out.exists():
        sys.exit(f"The browser didn't render {svg.name}")
    return Image.open(out).convert("RGBA").crop((0, 0, size, size))


def main() -> None:
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as tmp:
        work = Path(tmp)
        big = render(HERE / "eudora.svg", 1024, work)
        small = render(HERE / "eudora-small.svg", 256, work)
        if big.getpixel((0, 0))[3] != 0:
            sys.exit("the render isn't transparent (Edge ignored the background color)")
        sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
        frames = [(small if s <= 24 else big).resize((s, s), Image.LANCZOS) for s in sizes]
        frames[-1].save(HERE / "icon.ico", format="ICO", sizes=[(s, s) for s in sizes], append_images=frames[:-1])
        big.save(HERE / "icon.png")
        big.resize((32, 32), Image.LANCZOS).save(HERE / "32x32.png")
        big.resize((128, 128), Image.LANCZOS).save(HERE / "128x128.png")
        big.resize((256, 256), Image.LANCZOS).save(HERE / "128x128@2x.png")
        for tray in ("tray-newmail", "tray-nomail"):
            render(HERE / f"{tray}.svg", 256, work).resize((64, 64), Image.LANCZOS).save(HERE / f"{tray}.png")
    check = Image.open(HERE / "icon.ico")
    print("icon.ico sizes:", sorted(check.info.get("sizes", [])))


if __name__ == "__main__":
    main()
