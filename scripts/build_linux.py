"""Build Eudora for Linux: the AppImage, the .deb, and the .rpm, for x86-64 and ARM64, as
PlanetPDF's scripts/build_linux.py.

    python3 scripts/build_linux.py --arch arm64      # from the Mac (Colima) or any Docker host
    python3 scripts/build_linux.py --arch x86_64     # (x86-64 runs through Rosetta on the Mac)
    python3 scripts/build_linux.py --arch both
    python3 scripts/build_linux.py --arch both --publish --notes "What's new"

Builds in the build computer packaging/linux/Dockerfile (Ubuntu 24.04, made the first time and
cached), one processor at a time: the command line, then the window through Tauri (which also
makes the AppImage, carrying WebKitGTK with it), then the .deb and the .rpm made here, as
PlanetPDF's are. Results in dist/linux/:

* ``Eudora-<version>-<aarch64|x86_64>.AppImage``: runs on most distributions without
  installing; make it executable and start it. It replaces itself when it updates.
* ``eudora_<version>_<arm64|amd64>.deb``: Ubuntu 24.04 / Debian 13 or newer.
* ``eudora-<version>-1.<aarch64|x86_64>.rpm``: Fedora 40 / openSUSE or newer (signed with the
  repositories' key when this computer has it: scripts/linux_repo.py).

The packages install into /opt/eudora (``eudora27`` the window, ``eudora`` the command line, both
also in /usr/bin), with a menu entry, "Open With" for .eml and .ics files, and ``package-kind``
saying how they were installed, so Eudora updates each its own way (core/update.rs): the AppImage
replaces its file; a .deb / .rpm is installed through pkexec with an administrator's password.
Installing a .deb / .rpm the first time also adds Eudora's apt / dnf repository, so the system's
updater offers new versions too; removing it removes the repository.

With --publish, the signed update files for every package are made in dist/update-site/ (on the
host, where the update-signing key is): latest-linux-<x86_64|arm64>-<appimage|deb|rpm>.json.
"""

from __future__ import annotations

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCKERFILE = ROOT / "packaging" / "linux" / "Dockerfile"
IMAGE = "eudora-linux-build"
ARCHES = {  # our name -> (Docker, Rust / AppImage / rpm, deb)
    "arm64": ("linux/arm64", "aarch64", "arm64"),
    "x86_64": ("linux/amd64", "x86_64", "amd64"),
}
INSTALL_DIR = "/opt/eudora"
KIND_FILE = "package-kind"  # (core/update.rs: how this copy was installed)
PUBLIC_KEY = ROOT / "packaging" / "linux" / "eudora-linux.asc"  # (the repositories' key)
REPO_KEY = Path("/key/linux-repo-key.asc")  # (its private half, mounted when this computer has it)
APT_KEYRING = "/usr/share/keyrings/eudora-archive-keyring.gpg"
RPM_KEY = "/usr/share/pki/rpm-gpg/RPM-GPG-KEY-eudora"
REPO_URL = os.environ.get("EUDORA_LINUX_REPO_URL", "")
SUMMARY = "Eudora: a fast email client for huge mailboxes"
DESCRIPTION = ("Eudora reads, searches, sorts, and writes mail from Microsoft 365, Gmail, IMAP, and POP3 "
               "accounts in one library, with a calendar, contacts, and To Do. Free to use, with no ads "
               "and no data collection. From Planet Networks.")
HOMEPAGE = os.environ.get("EUDORA_HOMEPAGE", "https://github.com/robertlboyle/eudora")
DESKTOP = """[Desktop Entry]
Type=Application
Name=Eudora
GenericName=Email Client
Comment=Read, search, sort, and write your mail
Exec={exec} %U
Icon=eudora
Terminal=false
Categories=Network;Email;Office;
MimeType=message/rfc822;text/calendar;x-scheme-handler/mailto;
StartupWMClass=eudora27
"""
STEPS = 6


def step(n: int, text: str) -> None:
    print(f"\nStep {n} of {STEPS}: {text}", flush=True)


def run(*args, **kw) -> subprocess.CompletedProcess:
    return subprocess.run([str(a) for a in args], check=True, **kw)


def version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    return re.search(r'^version = "([^"]+)"', text, re.M).group(1)


def this_arch() -> str:
    return "arm64" if platform.machine().lower() in ("aarch64", "arm64") else "x86_64"


# --- on the Docker host -----------------------------------------------------------------------------

def private_dev() -> Path | None:
    """The developer's private folder (Font Awesome Pro, Google's client): ``EUDORA_DEV``, as
    build.rs finds it."""
    if os.environ.get("EUDORA_DEV"):
        return Path(os.environ["EUDORA_DEV"])
    return None


def in_docker(arch: str, out: Path) -> None:
    """Make (or reuse) the build image for that processor, then run this script inside it."""
    docker_platform = ARCHES[arch][0]
    tag = f"{IMAGE}:{arch}"
    print(f"Preparing the {arch} build image (cached after the first time)…", flush=True)
    run("docker", "buildx", "build", "--platform", docker_platform, "--load", "-t", tag,
        "-f", DOCKERFILE, DOCKERFILE.parent)
    out.mkdir(parents=True, exist_ok=True)
    sys.path.insert(0, str(ROOT / "scripts"))
    try:
        from linux_repo import private_key
    except ImportError:
        # (the published source has no package repositories: unsigned packages)
        def private_key():
            return None

    mounts = []
    key = private_key()
    if key and key.is_file():
        mounts += ["-v", f"{key}:{REPO_KEY}:ro"]
    # Only the two folders of the private folder the build uses: the build scripts ask cargo to
    # watch it (a Google key put there later is noticed), and cargo then reads all of it each build.
    dev = private_dev()
    if dev and (dev / "Google").is_dir():
        mounts += ["-v", f"{dev / 'Google'}:/private-dev/Google:ro", "-e", "EUDORA_DEV=/private-dev"]
    if dev and (dev / "fontawesome").is_dir():
        mounts += ["-v", f"{dev / 'fontawesome'}:/fontawesome:ro", "-e", "FONTAWESOME_PRO=/fontawesome"]
    # The project's own files only (Git's list, with changes not yet committed), copied to
    # target/linux-src: the working folder also holds build output (the iPhone build alone is
    # over a gigabyte), which Docker's shared folder made cargo crawl through.
    src = ROOT / "target" / "linux-src"
    listed = run("git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", cwd=ROOT,
                 capture_output=True).stdout.decode("utf-8").split("\0")
    keep = {name for name in listed if name and (ROOT / name).is_file()}
    for old in (src.rglob("*") if src.is_dir() else []):
        if old.is_file() and old.relative_to(src).as_posix() not in keep and not old.relative_to(src).as_posix().startswith("crates/eudora-app/ui/icons/"):
            old.unlink()
    for name in sorted(keep):
        source, copy = ROOT / name, src / name
        if not copy.is_file() or copy.stat().st_mtime < source.stat().st_mtime or copy.stat().st_size != source.stat().st_size:
            copy.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, copy)
    print(f"Copied the project's {len(keep):,} files to build from.", flush=True)
    # (the build's own files stay in Docker volumes: the cargo cache shared, a target per processor)
    run("docker", "run", "--rm", "--platform", docker_platform,
        # (one level down: the core's build script also watches the folder three above it, which
        # at /src would be the whole file system)
        "-v", f"{src}:/work/src", "-v", f"{out}:/out",
        "-v", "eudora-cargo-registry:/opt/cargo/registry", "-v", f"eudora-target-{arch}:/build/target",
        "-e", "CARGO_TARGET_DIR=/build/target",
        # (no debugging symbols in what's shipped: a quarter of the size)
        "-e", "CARGO_PROFILE_RELEASE_DEBUG=false", "-e", "CARGO_PROFILE_RELEASE_STRIP=symbols", *mounts, tag,
        "python3", "/work/src/scripts/build_linux.py", "--inside", "--out", "/out")


# --- inside the build image -------------------------------------------------------------------------

def stage_install(window: Path, cli: Path, stage: Path) -> None:
    """The files as the .deb and .rpm install them."""
    shutil.rmtree(stage, ignore_errors=True)
    app = stage / INSTALL_DIR.lstrip("/")
    app.mkdir(parents=True)
    for program in (window, cli):
        shutil.copy2(program, app / program.name)
        (app / program.name).chmod(0o755)
    (stage / "usr" / "bin").mkdir(parents=True)
    for name in ("eudora27", "eudora"):
        (stage / "usr" / "bin" / name).symlink_to(f"{INSTALL_DIR}/{name}")
    apps = stage / "usr" / "share" / "applications"
    apps.mkdir(parents=True)
    (apps / "eudora.desktop").write_text(DESKTOP.format(exec="eudora27"), encoding="utf-8")
    for size, source in ((32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png")):
        icons = stage / "usr" / "share" / "icons" / "hicolor" / f"{size}x{size}" / "apps"
        icons.mkdir(parents=True)
        shutil.copy2(ROOT / "crates" / "eudora-app" / "icons" / source, icons / "eudora.png")
    # the repositories' public key, for apt (binary keyring) and dnf / zypper (armored)
    if PUBLIC_KEY.is_file():
        keyring = stage / APT_KEYRING.lstrip("/")
        keyring.parent.mkdir(parents=True)
        run("gpg", "--batch", "--yes", "--dearmor", "--output", keyring, PUBLIC_KEY)
        keyring.chmod(0o644)
        rpm_key = stage / RPM_KEY.lstrip("/")
        rpm_key.parent.mkdir(parents=True)
        shutil.copy2(PUBLIC_KEY, rpm_key)


# Installing the .deb / .rpm the first time adds Eudora's repository (as PlanetPDF's), so the
# system's updater offers new versions; upgrades never add it again (an administrator who removed
# it keeps it removed); removing the package removes it.
APT_SOURCE = "/etc/apt/sources.list.d/eudora.sources"
REFRESH = ("command -v update-desktop-database >/dev/null && update-desktop-database -q || true\n"
           "command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q /usr/share/icons/hicolor || true\n")
DEB_POSTINST = f"""#!/bin/sh
set -e
{REFRESH}if [ "$1" = configure ] && [ -z "$2" ] && [ -d /etc/apt/sources.list.d ] && [ ! -e {APT_SOURCE} ] && [ -e {APT_KEYRING} ]; then
    cat > {APT_SOURCE} <<'END'
# Eudora's package repository, added when Eudora was installed: Software Updater and
# "apt upgrade" then offer new versions. Delete this file to stop it.
Types: deb
URIs: {REPO_URL}/apt
Suites: stable
Components: main
Architectures: @ARCH@
Signed-By: {APT_KEYRING}
END
fi
"""
DEB_POSTRM = f"""#!/bin/sh
set -e
{REFRESH}if [ "$1" = remove ] || [ "$1" = purge ]; then
    rm -f {APT_SOURCE}
fi
"""
RPM_REPOS = {"/etc/yum.repos.d": "", "/etc/zypp/repos.d": "type=rpm-md\nautorefresh=1\n"}
RPM_POST = "\n".join(f"""if [ $1 -eq 1 ] && [ -d {folder} ] && [ ! -e {folder}/eudora.repo ] && [ -e {RPM_KEY} ]; then
cat > {folder}/eudora.repo <<'END'
# Eudora's package repository, added when Eudora was installed: the system's updates then
# offer new versions. Delete this file to stop it.
[eudora]
name=Eudora
baseurl={REPO_URL}/rpm/$basearch
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=file://{RPM_KEY}
{extra}END
fi""" for folder, extra in RPM_REPOS.items())
RPM_POSTUN = "\n".join(f"[ $1 -eq 0 ] && rm -f {folder}/eudora.repo || :" for folder in RPM_REPOS)


def deb_depends(programs: list[Path], work: Path) -> str:
    """The libraries' packages, from the programs themselves (dpkg-shlibdeps)."""
    here = work / "shlibdeps"
    shutil.rmtree(here, ignore_errors=True)
    (here / "debian").mkdir(parents=True)
    (here / "debian" / "control").write_text("Source: eudora\n\nPackage: eudora\nArchitecture: any\n", encoding="utf-8")
    found = run("dpkg-shlibdeps", "-O", *[f"-e{p}" for p in programs], cwd=here, capture_output=True, text=True).stdout
    shutil.rmtree(here)
    return found.strip().split("=", 1)[1]


def make_deb(stage: Path, work: Path, v: str, arch: str, depends: str, target: Path) -> None:
    root = work / "deb"
    shutil.rmtree(root, ignore_errors=True)
    shutil.copytree(stage, root, symlinks=True)
    (root / INSTALL_DIR.lstrip("/") / KIND_FILE).write_text("deb\n", encoding="utf-8")
    size = sum(p.stat().st_size for p in root.rglob("*") if p.is_file() and not p.is_symlink()) // 1024
    (root / "DEBIAN").mkdir()
    (root / "DEBIAN" / "control").write_text(
        f"Package: eudora\nVersion: {v}\nArchitecture: {ARCHES[arch][2]}\n"
        f"Maintainer: Planet Networks <software@planet.net>\nInstalled-Size: {size}\n"
        f"Depends: {depends}, libayatana-appindicator3-1\nRecommends: xdg-utils, gnome-keyring | kwalletmanager\n"
        f"Section: mail\nPriority: optional\nHomepage: {HOMEPAGE}\nDescription: {SUMMARY}\n {DESCRIPTION}\n",
        encoding="utf-8")
    for name, text in (("postinst", DEB_POSTINST.replace("@ARCH@", ARCHES[arch][2])), ("postrm", DEB_POSTRM)):
        script = root / "DEBIAN" / name
        script.write_text(text, encoding="utf-8")
        script.chmod(0o755)
    run("dpkg-deb", "--root-owner-group", "-Zxz", "--build", root, target, capture_output=True)
    shutil.rmtree(root)


def make_rpm(stage: Path, work: Path, v: str, arch: str, target: Path) -> None:
    top = work / "rpm"
    shutil.rmtree(top, ignore_errors=True)
    rpm_arch = ARCHES[arch][1]
    root = work / "rpm-stage"
    shutil.rmtree(root, ignore_errors=True)
    shutil.copytree(stage, root, symlinks=True)
    (root / INSTALL_DIR.lstrip("/") / KIND_FILE).write_text("rpm\n", encoding="utf-8")
    keys = f"{APT_KEYRING}\n{RPM_KEY}\n" if (root / RPM_KEY.lstrip("/")).is_file() else ""
    (top / "SPECS").mkdir(parents=True)
    spec = top / "SPECS" / "eudora.spec"
    # (the libraries it needs are found from the programs by rpm itself: by soname, so Fedora and
    # openSUSE both resolve them)
    spec.write_text(f"""\
Name: eudora
Version: {v}
Release: 1
Summary: {SUMMARY}
License: MIT OR Apache-2.0
URL: {HOMEPAGE}
BuildArch: {rpm_arch}
Recommends: xdg-utils
# (the tray icon's library is loaded while running, so rpm can't find it in the programs)
Requires: libayatana-appindicator3.so.1()(64bit)
%define debug_package %{{nil}}
%define __os_install_post %{{nil}}
%define _build_id_links none

%description
{DESCRIPTION}

%install
cp -a {root}/. %{{buildroot}}/

%post
{REFRESH.replace('|| true', '|| :')}{RPM_POST}

%postun
{REFRESH.replace('|| true', '|| :')}{RPM_POSTUN}

%files
%defattr(-,root,root,-)
{INSTALL_DIR}
/usr/bin/eudora27
/usr/bin/eudora
/usr/share/applications/eudora.desktop
/usr/share/icons/hicolor/32x32/apps/eudora.png
/usr/share/icons/hicolor/128x128/apps/eudora.png
/usr/share/icons/hicolor/256x256/apps/eudora.png
{keys}""", encoding="utf-8")
    run("rpmbuild", "-bb", "--define", f"_topdir {top}", "--target", rpm_arch, spec, capture_output=True)
    built = next((top / "RPMS").rglob("*.rpm"))
    shutil.move(built, target)
    shutil.rmtree(top)
    shutil.rmtree(root)


def build(out: Path) -> None:
    arch, v = this_arch(), version()
    triple = f"{ARCHES[arch][1]}-unknown-linux-gnu"
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    work = Path("/tmp/eudora-linux")
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    print(f"Eudora {v} for Linux ({arch})", flush=True)

    step(1, "Building the command line (release)")
    run("cargo", "build", "--release", "--locked", "-p", "eudora-cli", cwd=ROOT)
    cli = target_dir / "release" / "eudora"

    step(2, "Building the window and the AppImage (Tauri)")
    sidecar = work / "sidecar"
    sidecar.mkdir()
    shutil.copy2(cli, sidecar / f"eudora-{triple}")
    config = work / "linux.conf.json"
    config.write_text(
        '{"bundle": {"active": true, "targets": ["appimage"], "externalBin": ["' + str(sidecar / "eudora") + '"],'
        ' "category": "Productivity", "shortDescription": "' + SUMMARY + '",'
        ' "linux": {"appimage": {"bundleMediaFramework": false}}}}', encoding="utf-8")
    # Tauri fetches linuxdeploy's AppImage plugin and runs it; under x86-64 emulation (the Mac's
    # Rosetta) an AppImage's marker in its ELF header ("AI\x02" at byte 8) makes the kernel refuse
    # it (Exec format error). Tauri clears it on linuxdeploy itself; the plugin is fetched here
    # with it cleared too (the marker means nothing to running it).
    plugin = Path.home() / ".cache" / "tauri" / "linuxdeploy-plugin-appimage.AppImage"
    if not plugin.is_file():
        plugin.parent.mkdir(parents=True, exist_ok=True)
        run("curl", "-fsSLo", plugin, "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/"
            f"continuous/linuxdeploy-plugin-appimage-{ARCHES[arch][1]}.AppImage")
        with open(plugin, "r+b") as f:
            f.seek(8)
            f.write(b"\0\0\0")
        plugin.chmod(0o755)
    run("cargo", "tauri", "build", "--bundles", "appimage", "--config", config, "--", "--locked",
        cwd=ROOT / "crates" / "eudora-app")
    window = target_dir / "release" / "eudora27"
    appimage = next((target_dir / "release" / "bundle" / "appimage").glob(f"*_{v}_*.AppImage"))

    step(3, "Checking the programs")
    for program in (cli, window):
        missing = [line for line in run("ldd", program, capture_output=True, text=True).stdout.splitlines() if "not found" in line]
        if missing:
            sys.exit(f"{program.name} needs libraries the system doesn't have: {missing}")
    said = run(cli, "api", "methods", capture_output=True, text=True).stdout
    if "v1.mail.search" not in said:
        sys.exit("The command line didn't answer as it should (eudora api methods).")
    print(f"Both programs load; the command line answers ({said.count('v1.')} API methods).")

    stage = work / "stage"
    stage_install(window, cli, stage)
    rpm_arch, deb_arch = ARCHES[arch][1], ARCHES[arch][2]
    out.mkdir(parents=True, exist_ok=True)
    made = []

    step(4, "The AppImage")
    target = out / f"Eudora-{v}-{rpm_arch}.AppImage"
    shutil.copy2(appimage, target)
    target.chmod(0o755)
    made.append(target)

    step(5, "The .deb")
    target = out / f"eudora_{v}_{deb_arch}.deb"
    make_deb(stage, work, v, arch, deb_depends([window, cli], work), target)
    made.append(target)

    step(6, "The .rpm")
    target = out / f"eudora-{v}-1.{rpm_arch}.rpm"
    make_rpm(stage, work, v, arch, target)
    if REPO_KEY.is_file():
        sys.path.insert(0, str(ROOT / "scripts"))
        from linux_repo import sign_rpm

        sign_rpm(target, REPO_KEY)
        print("Signed with the repositories' key.")
    else:
        print("Not signed: this computer doesn't have the repositories' key (scripts/linux_repo.py keygen).")
    made.append(target)
    for path in made:
        print(f"  {path.name}  {path.stat().st_size / 1e6:.1f} MB")


# --- the update files (on the host) ---------------------------------------------------------------

def packages(out: Path, arch: str) -> list[tuple[str, Path]]:
    v, (_, rpm_arch, deb_arch) = version(), ARCHES[arch]
    return [("appimage", out / f"Eudora-{v}-{rpm_arch}.AppImage"), ("deb", out / f"eudora_{v}_{deb_arch}.deb"),
            ("rpm", out / f"eudora-{v}-1.{rpm_arch}.rpm")]


def publish(out: Path, arches: list[str], notes: str) -> None:
    for arch in arches:
        for kind, path in packages(out, arch):
            if not path.is_file():
                sys.exit(f"{path.name} isn't there: build it first.")
            args = ["cargo", "run", "-q", "-p", "eudora-release", "--", "publish", "--platform",
                    f"linux-{arch}-{kind}", "--file", str(path)]
            if notes:
                args += ["--notes", notes]
            run(*args, cwd=ROOT)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--arch", choices=["arm64", "x86_64", "both"], default="both")
    parser.add_argument("--out", default=str(ROOT / "dist" / "linux"))
    parser.add_argument("--publish", action="store_true", help="also make the signed update files")
    parser.add_argument("--publish-only", action="store_true", help="only make the update files")
    parser.add_argument("--notes", default="")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    out = Path(args.out)
    if args.inside:
        build(out)
        return 0
    arches = ["arm64", "x86_64"] if args.arch == "both" else [args.arch]
    if not args.publish_only:
        for arch in arches:
            in_docker(arch, out)
    if args.publish or args.publish_only:
        publish(out, arches, args.notes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
