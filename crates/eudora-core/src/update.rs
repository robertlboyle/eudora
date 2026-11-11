//! Updates: Eudora looks on its
//! web server for a newer version, downloads it in the background, checks it, and installs it
//! when the user says (now, or when Eudora closes).
//!
//! **On the server** (one folder, made by `cargo run -p eudora-release -- publish`):
//!
//! * `latest-<platform>.json`: `{"app": "Eudora", "platform": "windows", "version": "0.2.0",
//!   "file": "Eudora-Setup-0.2.0.exe", "size": …, "sha256": "…", "notes": "…",
//!   "published": "2026-10-27"}`
//! * `latest-<platform>.json.sig`: an Ed25519 signature of those exact bytes (base64). Only the
//!   holder of the private key (kept apart from the source, never in the project or on the
//!   server) can make one; Eudora carries the public key (`update_key.pub`). A changed or stolen
//!   server can't push a program: an unsigned or wrongly signed file is refused, and so is a
//!   download whose size or SHA-256 differs from the signed file.
//! * the file itself: on Windows the Setup program (Inno Setup), run without its pages
//!   (`/SILENT`) in the way Eudora was installed (for this user or for everyone), so the
//!   uninstaller, shortcuts, file types and PATH stay right. Unlike PlanetPDF (a launcher and a
//!   folder per version, for terminal servers), Eudora is one program per user, and Setup
//!   already closes a running Eudora and replaces it.
//!
//! The same checks run on every system. Installing: Windows runs Setup; the Mac swaps
//! `Eudora.app` for the one in the disk image; **Linux, as PlanetPDF** (2026-11-06): each package
//! has its own manifest (`latest-linux-<x86_64|arm64>-<appimage|deb|rpm>.json`), an AppImage
//! replaces its own file as Eudora closes, and a .deb / .rpm is installed by `pkexec eudora
//! apply-update` (the system asks for an administrator's password; the update is checked again
//! as root before `dpkg` / `rpm` sees it). A development build only says a newer version exists
//! and where to get it.
//!
//! Nothing about the user is sent: the requests carry only Eudora's name and version.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// Where Eudora looks (`official`).
pub const UPDATE_URL: &str = crate::official::UPDATE_URL;
/// Where to get Eudora by hand.
pub const DOWNLOAD_PAGE: &str = crate::official::DOWNLOAD_PAGE;
/// Overrides the address (tests set it empty: no test may reach the real server).
pub const URL_ENV_VAR: &str = "EUDORA_UPDATE_URL";
/// `{"url": "https://…/"}` beside the program: another server (set by IT).
pub const CONFIG_FILE: &str = "update.json";
/// This copy's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Eudora's public update key (base64 of the 32 raw bytes), made by `eudora-release keygen`.
const PUBLIC_KEY: &str = include_str!("../update_key.pub");
const TIMEOUT_SECS: u64 = 30;
const MANIFEST_LIMIT: u64 = 1 << 20;

/// "windows", "windows-arm64", "macos", "linux-<x86_64|arm64>-<appimage|deb|rpm>" (a development build on
/// Linux: "linux-<arch>", which the server has no update for), "android" or "ios": which build
/// fits this copy. (Phones say only that a newer version exists: `installed`.)
pub fn platform() -> String {
    let base = base_platform();
    match linux_kind() {
        Some(kind) => format!("{base}-{kind}"),
        None => base.to_string(),
    }
}

/// The system and processor, without how a Linux copy was installed.
pub fn base_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        // Windows on ARM: an installer and update channel of its own
        "windows-arm64"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x86_64"
    }
}

/// Linux only: how this copy was installed: "appimage" (running from an AppImage: `$APPIMAGE`),
/// or "deb" / "rpm" (the `package-kind` file the package put beside the program).
pub fn linux_kind() -> Option<&'static str> {
    if !cfg!(target_os = "linux") || cfg!(target_os = "android") {
        return None;
    }
    if std::env::var_os("APPIMAGE").is_some() {
        return Some("appimage");
    }
    let exe = std::env::current_exe().ok()?;
    match std::fs::read_to_string(exe.with_file_name(KIND_FILE)).ok()?.trim() {
        "deb" => Some("deb"),
        "rpm" => Some("rpm"),
        _ => None,
    }
}

/// Beside the program in a .deb / .rpm (`/opt/eudora/package-kind`): how it was installed.
pub const KIND_FILE: &str = "package-kind";

pub fn manifest_name(platform: &str) -> String {
    format!("latest-{platform}.json")
}

/// A release as the signed manifest describes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub published: String,
}

/// The release with the exact manifest bytes and signature (kept beside the download, so it's
/// checked again before it's run).
#[derive(Clone, Debug)]
pub struct Signed {
    pub release: Release,
    pub manifest: Vec<u8>,
    pub signature: Vec<u8>,
}

/// The numbers of a version ("0.2.10" → [0, 2, 10]).
pub fn parse_version(text: &str) -> Result<Vec<u64>> {
    let parts: Vec<u64> = text.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect();
    if parts.is_empty() {
        return Err(Error::Other(format!("“{text}” isn't a version number.")));
    }
    Ok(parts)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (Ok(mut a), Ok(mut b)) = (parse_version(candidate), parse_version(current)) else { return false };
    let width = a.len().max(b.len());
    a.resize(width, 0);
    b.resize(width, 0);
    a > b
}

// --- signatures -------------------------------------------------------------------------------

fn public_key() -> Result<Vec<u8>> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(PUBLIC_KEY.trim())
        .map_err(|_| Error::Other("Eudora's update key is damaged, so it can't check that an update is genuine.".into()))?;
    if raw.len() != 32 {
        return Err(Error::Other("Eudora's update key is damaged, so it can't check that an update is genuine.".into()));
    }
    Ok(raw)
}

/// The release the manifest describes, if `key` signed it and it's for this program and system.
pub fn verify_with(manifest: &[u8], signature: &[u8], key: &[u8], platform: &str) -> Result<Release> {
    use base64::Engine;
    let sig = base64::engine::general_purpose::STANDARD.decode(String::from_utf8_lossy(signature).trim()).unwrap_or_default();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(manifest, &sig)
        .map_err(|_| Error::Other("The update on the server isn't signed with Eudora's update key, so it wasn't used.".into()))?;
    let unreadable = |why: String| Error::Other(format!("The update information on the server can't be read ({why})."));
    let data: serde_json::Value = serde_json::from_slice(manifest).map_err(|e| unreadable(e.to_string()))?;
    if data["app"] != "Eudora" {
        return Err(unreadable(format!("it's for {}", data["app"])));
    }
    if data["platform"].as_str() != Some(platform) {
        return Err(unreadable(format!("it's for {}, not {platform}", data["platform"])));
    }
    let release: Release = serde_json::from_value(data).map_err(|e| unreadable(e.to_string()))?;
    let f = &release.file;
    if f.is_empty() || f.contains(['/', '\\', ':']) || f.starts_with('.') {
        return Err(unreadable(format!("odd file name {f:?}")));
    }
    if release.sha256.len() != 64 || !release.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unreadable("no SHA-256".into()));
    }
    parse_version(&release.version)?;
    Ok(Release { sha256: release.sha256.to_ascii_lowercase(), ..release })
}

pub fn verify(manifest: &[u8], signature: &[u8]) -> Result<Release> {
    verify_with(manifest, signature, &public_key()?, &platform())
}

// --- the server -------------------------------------------------------------------------------

/// Where to look: `EUDORA_UPDATE_URL`, then `update.json` beside the program, then [`UPDATE_URL`].
pub fn configured_url() -> String {
    if let Ok(url) = std::env::var(URL_ENV_VAR) {
        return url.trim().to_string();
    }
    let beside = std::env::current_exe().ok().and_then(|exe| std::fs::read(exe.with_file_name(CONFIG_FILE)).ok());
    if let Some(url) = beside.and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|j| j["url"].as_str().map(str::trim).map(String::from)) {
        if !url.is_empty() {
            return url;
        }
    }
    UPDATE_URL.to_string()
}

fn join(base: &str, name: &str) -> String {
    format!("{}/{name}", base.trim_end_matches('/'))
}

fn agent() -> ureq::Agent {
    // (certificates checked by the system, as for every connection Eudora makes)
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(std::time::Duration::from_secs(TIMEOUT_SECS)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(TIMEOUT_SECS)))
        .user_agent(concat!("Eudora/", env!("CARGO_PKG_VERSION"), " (updates)"))
        .build()
        .new_agent()
}

fn open(url: &str) -> Result<ureq::Body> {
    if !url.to_ascii_lowercase().starts_with("https://") {
        return Err(Error::Other(format!("Updates are only fetched over HTTPS (not “{url}”).")));
    }
    let response = agent()
        .get(url)
        .header("Cache-Control", "no-cache")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => Error::Other(format!("The update server has no Eudora update for this system yet (HTTP {code}).")),
            e => Error::Network(format!("Eudora couldn't reach the update server ({e}).")),
        })?;
    Ok(response.into_body())
}

fn read_small(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    open(url)?.into_reader().take(limit + 1).read_to_end(&mut data).map_err(|e| Error::Network(format!("The update information didn't arrive ({e}).")))?;
    if data.len() as u64 > limit {
        return Err(Error::Other("The update information on the server is far too large.".into()));
    }
    Ok(data)
}

/// The newer release on the server, or None when this is the latest.
pub fn check(base: &str, current: &str) -> Result<Option<Signed>> {
    if base.trim().is_empty() {
        return Err(Error::Other("No update server address is set.".into()));
    }
    let name = manifest_name(&platform());
    let manifest = read_small(&join(base, &name), MANIFEST_LIMIT)?;
    let signature = read_small(&join(base, &format!("{name}.sig")), 4096)?;
    let release = verify(&manifest, &signature)?;
    Ok(is_newer(&release.version, current).then_some(Signed { release, manifest, signature }))
}

fn matches(path: &Path, release: &Release) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if meta.len() != release.size {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => digest.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex(&digest.finalize()) == release.sha256
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The release's file in `folder`, checked against the signed size and SHA-256. `progress(done,
/// total)` counts bytes; returning false stops the download.
pub fn download(base: &str, signed: &Signed, folder: &Path, mut progress: impl FnMut(u64, u64) -> bool) -> Result<PathBuf> {
    let release = &signed.release;
    std::fs::create_dir_all(folder).map_err(|source| Error::Io { context: format!("making {}", folder.display()), source })?;
    let target = folder.join(&release.file);
    if !matches(&target, release) {
        let part = folder.join(format!("{}.part", release.file));
        let result = (|| -> Result<()> {
            let mut body = open(&join(base, &release.file))?.into_reader();
            let mut out = std::fs::File::create(&part).map_err(|source| Error::Io { context: format!("writing {}", part.display()), source })?;
            let mut digest = Sha256::new();
            let mut buf = vec![0u8; 256 * 1024];
            let mut done = 0u64;
            loop {
                let n = body.read(&mut buf).map_err(|e| Error::Network(format!("The download didn't finish ({e}).")))?;
                if n == 0 {
                    break;
                }
                done += n as u64;
                if done > release.size {
                    return Err(Error::Other("The download is larger than the update information says: it was thrown away.".into()));
                }
                digest.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(|source| Error::Io { context: format!("writing {}", part.display()), source })?;
                if !progress(done, release.size) {
                    return Err(Error::Canceled);
                }
            }
            out.sync_all().map_err(|source| Error::Io { context: format!("writing {}", part.display()), source })?;
            if done != release.size || hex(&digest.finalize()) != release.sha256 {
                return Err(Error::Other("The downloaded update doesn't match its checksum: it was thrown away. Eudora tries again later.".into()));
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &target).map_err(|source| Error::Io { context: format!("keeping {}", target.display()), source })?;
    }
    progress(release.size, release.size);
    let name = manifest_name(&platform());
    let write = |file: String, bytes: &[u8]| std::fs::write(folder.join(&file), bytes).map_err(|source| Error::Io { context: format!("writing {file}"), source });
    write(name.clone(), &signed.manifest)?;
    write(format!("{name}.sig"), &signed.signature)?;
    // older downloads go
    if let Ok(entries) = std::fs::read_dir(folder) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n != release.file && n != name && n != format!("{name}.sig") && n != STATE_FILE && n != LOG_FILE {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(target)
}

/// An update downloaded earlier and still good (signed, newer than `current`, checksum), else None.
pub fn downloaded(folder: &Path, current: &str) -> Option<(Signed, PathBuf)> {
    let name = manifest_name(&platform());
    let manifest = std::fs::read(folder.join(&name)).ok()?;
    let signature = std::fs::read(folder.join(format!("{name}.sig"))).ok()?;
    let release = verify(&manifest, &signature).ok()?;
    let path = folder.join(&release.file);
    (is_newer(&release.version, current) && matches(&path, &release)).then(|| (Signed { release, manifest, signature }, path))
}

// --- choices and state (per computer, beside the usual library: never in a library) ----------

const STATE_FILE: &str = "state.json";
const LOG_FILE: &str = "update.log";

/// `%LOCALAPPDATA%\Eudora\updates` (the Mac's and Linux's own place for app data).
pub fn folder() -> Option<PathBuf> {
    crate::library::app_data_dir().map(|d| d.join("updates"))
}

/// What the user chose (Settings ▸ Updates) and what Eudora remembers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// A quiet check 20 s after Eudora starts.
    pub on_start: bool,
    /// Checks every this many days while Eudora runs; 0 = never by itself.
    pub every_days: u32,
    /// When it last looked (Unix seconds).
    pub last_check: Option<i64>,
    /// What it found then ("" = this is the latest).
    pub last_found: Option<String>,
    /// Skip This Version.
    pub skipped: Option<String>,
    /// When I Close Eudora: install this version as it closes.
    pub on_close: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        State { on_start: true, every_days: 14, last_check: None, last_found: None, skipped: None, on_close: None }
    }
}

impl State {
    pub fn read() -> State {
        folder().and_then(|f| std::fs::read(f.join(STATE_FILE)).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn write(&self) -> Result<()> {
        let folder = folder().ok_or_else(|| Error::Other("This computer has no folder for Eudora's own files.".into()))?;
        std::fs::create_dir_all(&folder).map_err(|source| Error::Io { context: format!("making {}", folder.display()), source })?;
        let tmp = folder.join(format!("{STATE_FILE}.new"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).map_err(|source| Error::Io { context: "writing the update choices".into(), source })?;
        std::fs::rename(&tmp, folder.join(STATE_FILE)).map_err(|source| Error::Io { context: "writing the update choices".into(), source })
    }

    /// Whether a quiet check is due: on start (`starting`), or `every_days` since the last.
    pub fn due(&self, now: i64, starting: bool) -> bool {
        if starting {
            return self.on_start;
        }
        self.every_days > 0 && self.last_check.is_none_or(|last| now - last >= i64::from(self.every_days) * 86_400)
    }
}

/// A line in `updates/update.log` (what happened, for when an update didn't go as expected).
pub fn log(line: &str) {
    let Some(folder) = folder() else { return };
    let _ = std::fs::create_dir_all(&folder);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(folder.join(LOG_FILE)) {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

// --- installing -------------------------------------------------------------------------------

/// How this copy can be updated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Installed {
    /// Installed by Setup (its uninstaller beside the program): Setup updates it in place.
    Setup { folder: PathBuf, everyone: bool },
    /// The Mac: `Eudora.app` in a folder the user may write (Applications): the new app from
    /// the downloaded disk image takes its place.
    MacApp { bundle: PathBuf },
    /// Linux, an AppImage in a folder the user may write: the new one takes its file's place.
    AppImage { file: PathBuf },
    /// Linux, a .deb or .rpm ("deb" / "rpm"): installed with an administrator's OK (pkexec).
    LinuxPackage { package: String, folder: PathBuf },
    /// A development build or a system Eudora can't update itself on: it only says what's new.
    Manual { why: String },
}

pub fn installed() -> Installed {
    let Ok(exe) = std::env::current_exe() else { return Installed::Manual { why: "Eudora can't tell where it's running from.".into() } };
    let Some(folder) = exe.parent().map(Path::to_path_buf) else { return Installed::Manual { why: "Eudora can't tell where it's running from.".into() } };
    if cfg!(target_os = "macos") {
        // …/Eudora.app/Contents/MacOS/eudora
        let bundle = folder.parent().and_then(Path::parent).map(Path::to_path_buf);
        return match bundle {
            Some(b) if b.extension().is_some_and(|x| x == "app") => match b.parent() {
                Some(dir) if writable(dir) => Installed::MacApp { bundle: b },
                _ => Installed::Manual { why: format!("Eudora can't replace itself in {} (no permission there): drag the new one into it from the download.", b.parent().unwrap_or(&b).display()) },
            },
            _ => Installed::Manual { why: "This copy isn't an app in Applications (a development build), so it can't update itself.".into() },
        };
    }
    if cfg!(target_os = "linux") {
        return match linux_kind() {
            Some("appimage") => {
                let file = PathBuf::from(std::env::var_os("APPIMAGE").unwrap_or_default());
                match file.parent() {
                    Some(dir) if file.is_file() && writable(dir) => Installed::AppImage { file },
                    _ => Installed::Manual { why: format!("Eudora can't replace {} (no permission there): download the new AppImage.", file.display()) },
                }
            }
            Some(kind) if pkexec().is_some() => Installed::LinuxPackage { package: kind.into(), folder },
            Some(_) => Installed::Manual { why: "This system has no pkexec to ask for an administrator's OK: update Eudora with the system's updater (Software Updater, apt, or dnf).".into() },
            None => Installed::Manual { why: "This copy wasn't installed from Eudora's AppImage, .deb, or .rpm (a development build), so it can't update itself.".into() },
        };
    }
    if !cfg!(windows) {
        return Installed::Manual { why: "Updating itself isn't built for this system.".into() };
    }
    if !folder.join("unins000.exe").is_file() {
        return Installed::Manual { why: "This copy wasn't installed by Eudora's Setup (a development build), so it can't update itself.".into() };
    }
    // installed for this user (in their own app data) or for everyone (Program Files)
    let mine = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).is_some_and(|l| folder.starts_with(l));
    Installed::Setup { folder, everyone: !mine }
}

/// Runs the downloaded Setup (checked again first: it trusts nothing it's handed) without its
/// pages, in the way Eudora was installed, keeping the shortcuts and PATH as they are; with
/// `restart`, Setup starts Eudora again when it's done. Setup closes a running Eudora itself;
/// installed for everyone, Windows asks for an administrator's OK.
pub fn install(folder: &Path, restart: bool) -> Result<String> {
    match installed() {
        Installed::MacApp { bundle } => return install_mac(folder, &bundle, restart),
        Installed::AppImage { file } => return install_appimage(folder, &file, restart),
        Installed::LinuxPackage { package, folder: app } => return install_package(folder, &package, &app, restart),
        _ => {}
    }
    let Installed::Setup { folder: app, everyone } = installed() else {
        return Err(Error::Other("This copy of Eudora can't update itself.".into()));
    };
    let (signed, setup) = downloaded(folder, CURRENT).ok_or_else(|| Error::Other("No checked update is waiting (it's downloaded again at the next check).".into()))?;
    let mut command = std::process::Command::new(&setup);
    command.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/TASKS=", if everyone { "/ALLUSERS" } else { "/CURRENTUSER" }]);
    command.arg(format!("/DIR={}", app.display()));
    command.arg(format!("/LOG={}", folder.join("setup.log").display()));
    if restart {
        command.arg("/relaunch=1");
    }
    log(&format!("installing {} over {} ({}, restart {restart})", signed.release.version, CURRENT, if everyone { "everyone" } else { "this user" }));
    command.spawn().map_err(|source| Error::Io { context: format!("starting {}", setup.display()), source })?;
    Ok(signed.release.version)
}

/// Whether this user may make files in the folder.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".eudora-update-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The Apple team the new app must be signed by, or it isn't installed (`official`).
const MAC_TEAM: &str = crate::official::MAC_TEAM;

/// The Mac: once Eudora has quit, a small script mounts the downloaded disk image (checked
/// again: size and checksum), checks the app in it is signed by Planet Networks' team and
/// notarized, copies it beside the running one, swaps the two, removes the old one and, with
/// `restart`, opens it. Anything wrong leaves the app as it was (said in `update.log`).
fn install_mac(folder: &Path, bundle: &Path, restart: bool) -> Result<String> {
    if MAC_TEAM.is_empty() {
        return Err(Error::Other("This build of Eudora names no Apple team to check updates against, so it doesn't install them itself: download the new version by hand.".into()));
    }
    let (signed, dmg) = downloaded(folder, CURRENT).ok_or_else(|| Error::Other("No checked update is waiting (it's downloaded again at the next check).".into()))?;
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let log_file = folder.join("update.log");
    let mount = folder.join("mount");
    let new = bundle.with_extension("app.new");
    let old = bundle.with_extension("app.old");
    let script = format!(
        r#"exec >>{log} 2>&1
while kill -0 {pid} 2>/dev/null; do sleep 1; done
echo "$(date): installing {version}"
mkdir -p {mount}
hdiutil attach -nobrowse -readonly -noautoopen -mountpoint {mount} {dmg} || exit 1
app={mount}/Eudora.app
if codesign --verify --deep --strict "$app" && codesign -dv "$app" 2>&1 | grep -q "TeamIdentifier={team}" && spctl -a -t exec "$app"; then
  rm -rf {new} && ditto "$app" {new} && rm -rf {old} && mv {bundle} {old} && mv {new} {bundle} && rm -rf {old} && echo "installed {version}"
else
  echo "not installed: the app in the update isn't signed and notarized by Planet Networks"
fi
hdiutil detach {mount} -quiet
{reopen}
"#,
        log = quote(&log_file),
        pid = std::process::id(),
        version = signed.release.version,
        mount = quote(&mount),
        dmg = quote(&dmg),
        team = MAC_TEAM,
        new = quote(&new),
        old = quote(&old),
        bundle = quote(bundle),
        reopen = if restart { format!("open {}", quote(bundle)) } else { String::new() },
    );
    log(&format!("installing {} over {} as Eudora quits (restart {restart})", signed.release.version, CURRENT));
    std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|source| Error::Io { context: "starting the update".into(), source })?;
    Ok(signed.release.version)
}

fn quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// pkexec (polkit), which asks for an administrator's password in a window of the desktop's own.
fn pkexec() -> Option<PathBuf> {
    ["/usr/bin/pkexec", "/bin/pkexec"].iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Linux, an AppImage: once Eudora has quit, the downloaded AppImage (checked: size and
/// checksum) is copied beside the running one, made runnable, and renamed over it (a rename
/// never leaves half a file); with `restart` it's started again.
fn install_appimage(folder: &Path, file: &Path, restart: bool) -> Result<String> {
    let (signed, new) = downloaded(folder, CURRENT).ok_or_else(|| Error::Other("No checked update is waiting (it's downloaded again at the next check).".into()))?;
    let staged = file.with_extension("AppImage.new");
    let script = format!(
        r#"exec >>{log} 2>&1
while kill -0 {pid} 2>/dev/null; do sleep 1; done
echo "$(date): installing {version}"
cp {new} {staged} && chmod 755 {staged} && mv -f {staged} {file} && echo "installed {version}" || {{ rm -f {staged}; echo "not installed"; }}
{reopen}
"#,
        log = quote(&folder.join(LOG_FILE)),
        pid = std::process::id(),
        version = signed.release.version,
        new = quote(&new),
        staged = quote(&staged),
        file = quote(file),
        reopen = if restart { format!("setsid {} >/dev/null 2>&1 &", quote(file)) } else { String::new() },
    );
    log(&format!("installing {} over {} as Eudora quits (AppImage, restart {restart})", signed.release.version, CURRENT));
    spawn_script(&script)?;
    Ok(signed.release.version)
}

/// Linux, a .deb / .rpm: once Eudora has quit, `pkexec <program folder>/eudora-cli apply-update
/// <updates folder>` (the system asks for an administrator's password), which checks the update
/// again as root ([`apply_update`]) and installs it; with `restart` Eudora starts again.
fn install_package(folder: &Path, kind: &str, app: &Path, restart: bool) -> Result<String> {
    let (signed, _) = downloaded(folder, CURRENT).ok_or_else(|| Error::Other("No checked update is waiting (it's downloaded again at the next check).".into()))?;
    let pkexec = pkexec().ok_or_else(|| Error::Other("This system has no pkexec.".into()))?;
    let script = format!(
        r#"exec >>{log} 2>&1
while kill -0 {pid} 2>/dev/null; do sleep 1; done
echo "$(date): installing {version} ({kind})"
{pkexec} {cli} apply-update {folder}
case $? in 0) ;; 126|127) echo "not installed: the administrator's password wasn't given" ;; *) echo "not installed" ;; esac
{reopen}
"#,
        log = quote(&folder.join(LOG_FILE)),
        pid = std::process::id(),
        version = signed.release.version,
        pkexec = quote(&pkexec),
        cli = quote(&app.join("eudora-cli")),
        folder = quote(folder),
        reopen = if restart { format!("setsid {} >/dev/null 2>&1 &", quote(&app.join("eudora"))) } else { String::new() },
    );
    log(&format!("installing {} over {} as Eudora quits ({kind}, restart {restart})", signed.release.version, CURRENT));
    spawn_script(&script)?;
    Ok(signed.release.version)
}

fn spawn_script(script: &str) -> Result<()> {
    std::process::Command::new("/bin/sh")
        .args(["-c", script])
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|source| Error::Io { context: "starting the update".into(), source })
}

/// `eudora-cli apply-update FOLDER`, run as root by pkexec (Linux, a .deb / .rpm): copies the
/// downloaded package and its signed manifest out of the user's folder into one only root can
/// write, checks them there (Eudora's signature, this system and package kind, size, SHA-256,
/// newer than this copy), and installs that copy with `dpkg -i` or `rpm -U`. Nothing in the
/// user's folder is trusted: it's checked after it can no longer change.
pub fn apply_update(from: &Path) -> Result<String> {
    let kind = linux_kind().ok_or_else(|| Error::Other("This copy wasn't installed from Eudora's .deb or .rpm.".into()))?;
    if kind == "appimage" {
        return Err(Error::Other("An AppImage updates itself without an administrator.".into()));
    }
    let safe = std::env::temp_dir().join(format!("eudora-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&safe);
    {
        #[cfg_attr(not(unix), allow(unused_mut))] // (only Unix sets its mode)
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&safe).map_err(|source| Error::Io { context: format!("making {}", safe.display()), source })?;
    }
    let result = (|| {
        let name = manifest_name(&platform());
        for file in [name.clone(), format!("{name}.sig")] {
            std::fs::copy(from.join(&file), safe.join(&file)).map_err(|source| Error::Io { context: format!("reading {file}"), source })?;
        }
        let manifest = std::fs::read(safe.join(&name)).map_err(|source| Error::Io { context: format!("reading {name}"), source })?;
        let signature = std::fs::read(safe.join(format!("{name}.sig"))).map_err(|source| Error::Io { context: format!("reading {name}.sig"), source })?;
        let release = verify(&manifest, &signature)?;
        if !is_newer(&release.version, CURRENT) {
            return Err(Error::Other(format!("{} isn't newer than this Eudora ({CURRENT}).", release.version)));
        }
        let package = safe.join(&release.file);
        std::fs::copy(from.join(&release.file), &package).map_err(|source| Error::Io { context: format!("reading {}", release.file), source })?;
        if !matches(&package, &release) {
            return Err(Error::Other("The downloaded update doesn't match its signed checksum: not installed.".into()));
        }
        let mut command = if kind == "deb" { std::process::Command::new("dpkg") } else { std::process::Command::new("rpm") };
        if kind == "deb" {
            command.arg("-i");
        } else {
            command.args(["-U", "--replacepkgs"]);
        }
        let done = command.arg(&package).status().map_err(|source| Error::Io { context: format!("starting {}", if kind == "deb" { "dpkg" } else { "rpm" }), source })?;
        if !done.success() {
            return Err(Error::Other(format!("{} couldn't install the update ({done}).", if kind == "deb" { "dpkg" } else { "rpm" })));
        }
        Ok(release.version)
    })();
    let _ = std::fs::remove_dir_all(&safe);
    result
}

// --- what the window and the API do -----------------------------------------------------------

/// What a check found.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    /// Not looked: the next quiet check isn't due.
    NotDue,
    /// This is the latest.
    Latest { current: String },
    /// A newer version the user chose to skip (said only to a quiet check).
    Skipped { version: String },
    /// Downloaded and checked: ready to install (`on_close`: already set to go in as Eudora closes).
    Ready { current: String, version: String, notes: String, on_close: bool },
    /// Newer, but this copy can't update itself: why, and where to get it.
    Manual { current: String, version: String, notes: String, why: String, page: String },
}

/// The version, the choices, how this copy updates, the server, and an update waiting.
pub fn status() -> serde_json::Value {
    let waiting = folder().and_then(|f| downloaded(&f, CURRENT)).map(|(s, _)| s.release);
    serde_json::json!({ "current": CURRENT, "choices": State::read(), "installed": installed(), "url": configured_url(), "waiting": waiting })
}

/// Settings ▸ Updates: check on start; every N days (0 = never by itself).
pub fn set_choices(on_start: bool, every_days: u32) -> Result<State> {
    if every_days > 365 {
        return Err(Error::Other("Every 0 to 365 days.".into()));
    }
    let state = State { on_start, every_days, ..State::read() };
    state.write()?;
    Ok(state)
}

/// Looks for a newer version (`force`: now, whatever the choices; else only when due, with
/// `starting` = the check after Eudora starts) and downloads it when this copy can install it.
/// `progress(done, total)` counts the download's bytes.
pub fn check_now(force: bool, starting: bool, progress: impl FnMut(u64, u64) -> bool) -> Result<Outcome> {
    let mut state = State::read();
    let now = chrono::Utc::now().timestamp();
    if !force && !state.due(now, starting) {
        return Ok(Outcome::NotDue);
    }
    let base = configured_url();
    let found = check(&base, CURRENT);
    state.last_check = Some(now);
    let found = match found {
        Ok(f) => f,
        Err(e) => {
            log(&format!("check failed: {e}"));
            let _ = state.write();
            return Err(e);
        }
    };
    state.last_found = Some(found.as_ref().map(|s| s.release.version.clone()).unwrap_or_default());
    let _ = state.write();
    let Some(signed) = found else { return Ok(Outcome::Latest { current: CURRENT.into() }) };
    let r = &signed.release;
    if !force && state.skipped.as_deref() == Some(r.version.as_str()) {
        return Ok(Outcome::Skipped { version: r.version.clone() });
    }
    if let Installed::Manual { why } = installed() {
        return Ok(Outcome::Manual { current: CURRENT.into(), version: r.version.clone(), notes: r.notes.clone(), why, page: DOWNLOAD_PAGE.into() });
    }
    let dir = folder().ok_or_else(|| Error::Other("This computer has no folder for Eudora's own files.".into()))?;
    download(&base, &signed, &dir, progress).inspect_err(|e| log(&format!("download of {} failed: {e}", r.version)))?;
    log(&format!("{} downloaded and checked", r.version));
    Ok(Outcome::Ready { current: CURRENT.into(), version: r.version.clone(), notes: r.notes.clone(), on_close: state.on_close.as_deref() == Some(r.version.as_str()) })
}

/// The user's answer to a ready update: "now" (Setup starts, and starts Eudora again when done:
/// the caller then closes Eudora), "close" (installed as Eudora closes), or "skip" (this version
/// isn't offered again by the quiet checks). Returns the version.
pub fn answer(when: &str) -> Result<String> {
    let dir = folder().ok_or_else(|| Error::Other("This computer has no folder for Eudora's own files.".into()))?;
    let (signed, _) = downloaded(&dir, CURRENT).ok_or_else(|| Error::Other("No checked update is waiting (it's downloaded again at the next check).".into()))?;
    let version = signed.release.version;
    let mut state = State::read();
    match when {
        "now" => {
            state.on_close = None;
            state.write()?;
            install(&dir, true)
        }
        "close" => {
            state.on_close = Some(version.clone());
            state.write()?;
            Ok(version)
        }
        "skip" => {
            state.skipped = Some(version.clone());
            state.on_close = None;
            state.write()?;
            Ok(version)
        }
        other => Err(Error::Other(format!("“{other}”: now, close or skip."))),
    }
}

/// As Eudora closes: the update the user chose to install then, if it's still there and good.
pub fn install_on_close() {
    let mut state = State::read();
    let Some(version) = state.on_close.take() else { return };
    let _ = state.write();
    match folder().map(|dir| install(&dir, false)) {
        Some(Ok(v)) => log(&format!("installing {v} as Eudora closes")),
        Some(Err(e)) => log(&format!("{version} wasn't installed as Eudora closed: {e}")),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> (ring::signature::Ed25519KeyPair, Vec<u8>) {
        use ring::signature::KeyPair;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = pair.public_key().as_ref().to_vec();
        (pair, public)
    }

    fn sign(pair: &ring::signature::Ed25519KeyPair, manifest: &[u8]) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(pair.sign(manifest).as_ref()).into_bytes()
    }

    fn manifest(platform: &str, file: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "app": "Eudora", "platform": platform, "version": "0.2.0", "file": file,
            "size": 3, "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "notes": "New."
        }))
        .unwrap()
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("nonsense", "0.1.0"));
    }

    #[test]
    fn only_a_good_signature_for_this_system_is_believed() {
        let (pair, public) = keypair();
        let good = manifest("windows", "Eudora-Setup-0.2.0.exe");
        let release = verify_with(&good, &sign(&pair, &good), &public, "windows").unwrap();
        assert_eq!(release.version, "0.2.0");
        // a changed manifest, another key, another system, a file name with a path
        let mut changed = good.clone();
        changed[10] ^= 1;
        assert!(verify_with(&changed, &sign(&pair, &good), &public, "windows").is_err());
        let (other, _) = keypair();
        assert!(verify_with(&good, &sign(&other, &good), &public, "windows").is_err());
        assert!(verify_with(&good, &sign(&pair, &good), &public, "macos").is_err());
        let sneaky = manifest("windows", "..\\evil.exe");
        assert!(verify_with(&sneaky, &sign(&pair, &sneaky), &public, "windows").is_err());
        assert!(verify_with(&good, b"not base64", &public, "windows").is_err());
    }

    #[test]
    fn the_built_in_key_reads() {
        assert_eq!(public_key().unwrap().len(), 32);
    }

    #[test]
    fn a_download_must_match_its_checksum() {
        let dir = std::env::temp_dir().join(format!("eudora-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let release = Release { version: "0.2.0".into(), file: "x.exe".into(), size: 3, sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(), notes: String::new(), published: String::new() };
        std::fs::write(dir.join("x.exe"), b"abc").unwrap();
        assert!(matches(&dir.join("x.exe"), &release));
        std::fs::write(dir.join("x.exe"), b"abd").unwrap();
        assert!(!matches(&dir.join("x.exe"), &release));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checks_come_when_due() {
        let s = State::default();
        assert!(s.due(1_000_000, true));
        assert!(s.due(1_000_000, false), "never checked");
        let s = State { last_check: Some(1_000_000), ..State::default() };
        assert!(!s.due(1_000_000 + 13 * 86_400, false));
        assert!(s.due(1_000_000 + 14 * 86_400, false));
        let s = State { every_days: 0, on_start: false, last_check: Some(0), ..State::default() };
        assert!(!s.due(i64::MAX / 2, false));
        assert!(!s.due(0, true));
    }

    #[test]
    fn only_https() {
        assert!(open("http://software.planet.net/Eudora/latest-windows.json").is_err());
    }
}
