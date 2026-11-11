# Building Eudora

This guide is for people and for AI coding agents. It covers setting up a computer to build Eudora,
building and testing it on Windows, the Mac, and Linux (and the Android and iOS versions), and the
few settings a build of your own needs before it can sign in to Microsoft or Google or update
itself.

If you're an AI agent: run the setup script for the platform, then the checks in
[Check your setup](#check-your-setup), before building. Every command here is meant to run as
written, from the repository's top folder.

## What Eudora is made of

| Folder | What |
|---|---|
| `crates/eudora-core` | The core library, in Rust: downloading (Microsoft Graph, Gmail API, IMAP, POP3), storage (SQLite + compressed message packs), search (Tantivy), the spam filter, rules, sync, backups, the local API and plugins (wasmtime). No user interface. |
| `crates/eudora-app` | The window: Tauri 2 with a plain HTML / CSS / JavaScript front end in `ui/` (no npm, no bundler). `ui/phone/` is the phone version's interface; `gen/android` and `gen/apple` are the phone projects. |
| `crates/eudora-cli` | The `eudora-cli` command line: every function of the window, for scripts. The window also runs it in the background to download mail. |
| `crates/eudora-release` | Signs release files for Eudora's self-updating (`keygen`, `publish`). |
| `crates/eudora-web` | A web interface (not developed further at the moment). |
| `crates/eudora-testkit` | Makes synthetic mail for tests. |
| `packaging/` | The Windows installer (Inno Setup), the Mac's entitlements, the Linux build container. |
| `scripts/` | Setup, build, and packaging scripts (below). |
| `docs/` | The local API (`API.md`), plugins (`PLUGINS.md`), and how classic Eudora's commands map to this one (`CLASSIC-UI.md`). |

The user manual is in `crates/eudora-app/ui/help/` and opens from Help in the program (F1).

## Set up a computer

Each script checks what's already installed and installs only what's missing, asking first.
Most installs need an administrator's approval (Windows asks, the Mac asks for your password,
Linux uses `sudo`).

**Windows 10 or 11** (PowerShell):

```powershell
.\scripts\setup-windows.ps1              # the desktop version for this PC
.\scripts\setup-windows.ps1 -Arm64       # also build for Windows on ARM
.\scripts\setup-windows.ps1 -Installer   # also make the Windows installer (Inno Setup)
.\scripts\setup-windows.ps1 -Android     # also the Android version
```

It installs, through `winget`: Git, Visual Studio 2022 Build Tools with the C++ workload and the
Windows SDK (Rust's linker), Rust (rustup, stable), and, as asked, the ARM64 compilers and LLVM
(clang, needed by the `ring` library on ARM64), Inno Setup 6, OpenJDK 17 and Android Studio.

**macOS 11 or newer** (Terminal):

```bash
bash scripts/setup-mac.sh          # the Mac version (Apple silicon and Intel)
bash scripts/setup-mac.sh --ios    # also the iOS version
```

It installs Apple's command-line tools, Rust and its Intel target (for one app that runs on both
kinds of Mac), and with `--ios` the iOS targets, Tauri's command line, CocoaPods, and XcodeGen.
The iOS version also needs Xcode from the App Store.

**Linux** (Debian / Ubuntu, Fedora, and Arch; on others it says what to install):

```bash
bash scripts/setup-linux.sh
```

It installs a C compiler, `pkg-config`, WebKitGTK 4.1, GTK 3, the tray-icon library, and Rust.

### Check your setup

```bash
rustc --version      # 1.99 or newer (edition 2024)
cargo --version
```

On Windows, also `rustup show` should say the host is `x86_64-pc-windows-msvc` (or
`aarch64-pc-windows-msvc` on an ARM64 PC).

## Build and run

```bash
cargo build --release -p eudora-app -p eudora-cli
```

Then start the window: `target/release/eudora` (`target\release\eudora.exe` on Windows). The
command line is `target/release/eudora-cli`; `eudora-cli --help` lists its commands. The first build takes
several minutes; `Cargo.lock` pins every library to the tested version.

A new library (where Eudora keeps the mail) is made in the system's usual place for program data
the first time: `%LOCALAPPDATA%\Eudora\Library` on Windows, `~/Library/Application Support/Eudora/Library`
on the Mac, `~/.local/share/eudora/library` on Linux. Name another with
`eudora <folder>` or the `EUDORA_LIBRARY` variable. **Never put a library in Dropbox, OneDrive,
or another syncing folder:** their file locks damage a live database.

## Test

```bash
cargo test --workspace
```

About 270 tests; the core's run in under a minute after the build. `cargo test -p eudora-core`
runs only the core's. A few tests are marked `#[ignore]` (they need a real account or files of
your own) and are skipped.

## Packages

| Platform | Command | Result |
|---|---|---|
| Windows (x64) | `.\scripts\build-installer.ps1` | `dist\Eudora-Setup-<version>.exe` |
| Windows on ARM | `.\scripts\build-installer.ps1 -Arch arm64` | `dist\Eudora-Setup-<version>-arm64.exe` |
| Mac | `bash scripts/build-mac.sh` | `dist/Eudora-<version>.dmg` (one app for Apple silicon and Intel) |
| Linux | `python3 scripts/build_linux.py --arch x86_64` (or `arm64`, `both`) | AppImage, .deb, and .rpm in `dist/`, built in Docker |
| Android | `.\scripts\build-android.ps1` | `dist\Eudora-<version>-android.apk` |
| iOS | `cargo tauri ios build` in `crates/eudora-app` | (needs an Apple developer account to install on a phone) |

The Mac package is signed with a "Developer ID Application" certificate when the keychain has
one, and notarized when a `notarytool` profile is set up (see the top of `scripts/build-mac.sh`);
without them it's built unsigned. The Android package is signed with the key named in
`scripts/build-android.ps1`; make your own with `keytool` for your builds. The Linux build needs
Docker (on the Mac, Colima or Docker Desktop).

## Continuous integration

`.github/workflows/build.yml` runs on GitHub Actions for every push and pull request: the tests on
Windows, the Mac, and Linux, and every package (the Windows installers for x64 and ARM64, the Mac
disk image, and the Linux AppImage, .deb, and .rpm for x86-64 and ARM64). Each run keeps the
packages as downloadable artifacts (the run's page ▸ Artifacts). They're unsigned and carry no
sign-in registrations or update server: for trying the code, not for release.

## Settings for a build of your own

The published source signs in nowhere and updates from nowhere until you give it your own
identities. Set them as variables when building (no file to edit), or in
`crates/eudora-core/src/official.rs`:

| Variable | What | Without it |
|---|---|---|
| `EUDORA_MICROSOFT_CLIENT_ID` | Your Microsoft app registration's Application (client) ID | Microsoft 365 and Outlook.com accounts can't be added; IMAP, POP3, and Gmail still work |
| `EUDORA_GOOGLE_CLIENT` | The path of your Google OAuth client's JSON file ("Desktop app") | Gmail works through IMAP with an app password only |
| `EUDORA_GOOGLE_IOS_CLIENT` | Your Google iOS OAuth client's `.plist` (iOS only) | No Google sign-in on iOS |
| `EUDORA_UPDATE_URL_DEFAULT` | The web folder your signed update files are published in | The program doesn't update itself |
| `EUDORA_DOWNLOAD_PAGE` | The page to download Eudora by hand | The update check names no page |
| `EUDORA_MAC_TEAM` | Your Apple team ID (the Mac only installs updates signed by it) | The Mac version doesn't install updates itself |
| `FONTAWESOME_PRO` | The folder of a Font Awesome Pro package (needs a Pro license) | The Font Awesome Free icons in `ui/icons/free/` are used |

For example, on Windows:

```powershell
$env:EUDORA_MICROSOFT_CLIENT_ID = '00000000-0000-0000-0000-000000000000'
cargo build --release -p eudora-app -p eudora-cli
```

### A Microsoft app registration

In the [Azure portal](https://portal.azure.com) ▸ Microsoft Entra ID ▸ App registrations ▸ New
registration:

1. Supported account types: **Accounts in any organizational directory and personal Microsoft
   accounts**.
2. Redirect URI: platform **Public client / native (mobile & desktop)**, `http://localhost`. For
   iOS, also `net.planet.eudora://auth` (or your own app identifier's scheme).
3. Authentication ▸ **Allow public client flows: Yes**. There is no client secret: Eudora signs in
   with the authorization code and PKCE.
4. API permissions ▸ Microsoft Graph ▸ Delegated: `offline_access`, `User.Read`,
   `User.ReadBasic.All`, `Mail.ReadWrite`, `Mail.ReadWrite.Shared`, `Mail.Send`, `Mail.Send.Shared`,
   `MailboxSettings.ReadWrite`, `Contacts.ReadWrite`, `Contacts.ReadWrite.Shared`,
   `Calendars.ReadWrite`, `Calendars.ReadWrite.Shared`, `Tasks.ReadWrite`; for the Online Archive
   also `MailboxFolder.ReadWrite`, `MailboxItem.ImportExport`, `MailboxItem.ReadWrite`. The full
   list Eudora asks for is `SIGN_IN_SCOPES` in `crates/eudora-core/src/graph.rs`.
5. Copy the **Application (client) ID** into `EUDORA_MICROSOFT_CLIENT_ID`.

### A Google OAuth client

In the [Google Cloud console](https://console.cloud.google.com): make a project, enable the Gmail,
People, and Google Calendar APIs, set up the OAuth consent screen, then Credentials ▸ Create
credentials ▸ OAuth client ID ▸ **Desktop app**, and download its JSON. Point
`EUDORA_GOOGLE_CLIENT` at that file. The build reads it into the program; it never goes into the
source. (Google treats a desktop app's client secret as not secret, but keep the file out of your
repository anyway.)

### Your own update server

```bash
cargo run -p eudora-release -- keygen          # a new signing key (keep the private half safe)
cargo run -p eudora-release -- publish --notes "What's new"
```

`keygen` writes the private key outside the project (`EUDORA_SIGNING_KEY` names where) and the
public half into `crates/eudora-core/update_key.pub`, which the program carries: it installs only
updates signed by that key. `publish` writes `dist/update-site/` (the installer,
`latest-<platform>.json`, and its signature). Upload those files to the folder named by
`EUDORA_UPDATE_URL_DEFAULT`: the installer first, the `.json` and `.sig` last.

## Rules the code follows

- **Cross-platform first:** one implementation in Rust for every platform; code for one system
  only where nothing cross-platform does the job, kept small and behind one function or trait
  (`#[cfg(windows)]` and the like).
- **Dependencies:** few, pinned in `Cargo.lock`, and only under permissive licenses (MIT, BSD,
  Apache-2.0, ISC, zlib, MPL-2.0). No GPL or AGPL code.
- **Nothing is dropped silently:** a message that can't be read is still stored as it came,
  searchable as text, and reported.
- **Long jobs report real numbers** ("Importing message 120,000 of 1,412,000"), never only
  "Working…".
- **Every feature is in the manual** (`crates/eudora-app/ui/help/`), the command line, and the local
  API (`docs/API.md`). A test checks that every command of the window is in the API or has a reason
  not to be.
- **US spelling**, and the Oxford comma, in code, the interface, and documents.

## Questions and fixes

Send bug reports and suggestions to **software@planet.net**. Eudora comes with no warranty and no
support, but the intention is to fix reported bugs quickly.
