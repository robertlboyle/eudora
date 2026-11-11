//! Files opened with Eudora: a double-clicked .eml shows in a message window (Reply, Forward,
//! attachments, Print), without being added to the library; an .ics shows its events, each of
//! which can be added to one of the user's calendars.
//!
//! One Eudora window at a time: the first holds a lock in Eudora's own folder; a second start
//! (Windows starts the program again for each file opened) hands its files to the first, by a
//! file in that folder the window looks for, and ends. The lock is a file lock (the standard
//! library's, every OS); the Mac hands files to the running program itself (RunEvent::Opened).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::Manager;

/// The kinds of file Eudora opens.
const KINDS: [&str; 2] = ["eml", "ics"];

fn kind_of(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    KINDS.iter().find(|k| **k == ext).copied()
}

/// A `mailto:` link handed to Eudora as the system's mail program (it travels with the files).
fn is_mailto(path: &Path) -> bool {
    path.to_str().is_some_and(|s| s.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")))
}

/// The command line: the library folder named (if any) and the files (and mailto: links) to open.
pub fn sort_args() -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut library = None;
    let mut files = Vec::new();
    for arg in std::env::args_os().skip(1).map(PathBuf::from) {
        // (Linux passes files as file:// addresses too)
        let arg = match arg.to_str().and_then(|s| s.strip_prefix("file://")) {
            Some(rest) => PathBuf::from(eudora_core_url_path(rest)),
            None => arg,
        };
        if is_mailto(&arg) {
            files.push(arg);
        } else if kind_of(&arg).is_some() && arg.is_file() {
            files.push(std::path::absolute(&arg).unwrap_or(arg));
        } else if library.is_none() {
            library = Some(arg);
        }
    }
    (library, files)
}

/// A file:// address's path (%XX decoded).
fn eudora_core_url_path(rest: &str) -> String {
    let b = rest.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a second start leaves its files for the window already open.
fn requests_dir() -> Option<PathBuf> {
    eudora_core::library::app_data_dir().map(|d| d.join("open-requests"))
}

/// The window's lock, held while it runs (desktops: a phone runs one copy of an app by itself).
#[cfg(desktop)]
static LOCK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

/// Takes the window's lock; when another Eudora window holds it, hands it the files (or, with
/// none, asks it to come to the front) and says so: this start should end. A library named on
/// the command line always starts a window of its own (another library).
#[cfg(desktop)]
pub fn hand_over(files: &[PathBuf], library_named: bool) -> bool {
    let Some(dir) = eudora_core::library::app_data_dir() else { return false };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("window.lock")) else { return false };
    match file.try_lock() {
        Ok(()) => {
            let _ = LOCK.set(file);
            false
        }
        Err(_) if library_named && files.is_empty() => false,
        Err(_) => {
            let Some(requests) = requests_dir() else { return false };
            let _ = std::fs::create_dir_all(&requests);
            let name = format!("{}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0), std::process::id());
            let text: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
            let tmp = requests.join(format!("{name}.tmp"));
            // (written whole, then named for the window to take: never read half-written)
            std::fs::write(&tmp, text.join("\n")).is_ok() && std::fs::rename(&tmp, requests.join(format!("{name}.txt"))).is_ok()
        }
    }
}

/// The window: opens the files it was started with, then those later starts hand it.
pub fn watch(handle: tauri::AppHandle, files: Vec<PathBuf>) {
    std::thread::spawn(move || {
        for f in files {
            open_or_say(&handle, &f);
        }
        let Some(dir) = requests_dir() else { return };
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "txt") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let _ = std::fs::remove_file(&path);
                let asked: Vec<PathBuf> = text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                if asked.is_empty() {
                    if let Some(main) = handle.get_webview_window("main") {
                        #[cfg(desktop)]
                        let _ = main.unminimize();
                        let _ = main.show();
                        let _ = main.set_focus();
                    }
                }
                for f in asked {
                    open_or_say(&handle, &f);
                }
            }
        }
    });
}

/// Opens a file; a problem is said in the main window's status bar.
pub fn open_or_say(handle: &tauri::AppHandle, path: &Path) {
    use tauri::Emitter;
    if let Err(why) = open_file(handle, path) {
        let name = if is_mailto(path) { "the mail link".to_string() } else { path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() };
        let _ = handle.emit_to("main", "outbox", vec![format!("Couldn't open {name}: {why}")]);
    }
}

fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > max {
        return Err(format!("it's {} MB, too big for this kind of file", size / 1_048_576));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

/// Calendar files opened, by number (for their window).
static ICS: Mutex<Vec<(String, eudora_core::ics::IcsFile)>> = Mutex::new(Vec::new());

fn open_file(handle: &tauri::AppHandle, path: &Path) -> Result<(), String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    if is_mailto(path) {
        return crate::open_mailto_now(handle, &path.to_string_lossy());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match kind_of(path) {
        Some("eml") => {
            let bytes = read_limited(path, eudora_core::library::OPEN_FILE_MAX)?;
            let subject = eudora_core::parse::display(&bytes, eudora_core::library::now()).parsed.subject;
            let id = eudora_core::library::open_message_file(bytes);
            let title: String = if subject.trim().is_empty() { name } else { subject.trim().to_string() };
            let title: String = title.chars().filter(|c| !c.is_control()).take(120).collect();
            tauri::WebviewWindowBuilder::new(handle, format!("reader-file-{n}"), tauri::WebviewUrl::App(format!("index.html?message={id}").into()))
                .title(format!("{title} – Eudora"))
                .inner_size(900.0, 760.0)
                .min_inner_size(420.0, 300.0)
                .build()
                .map_err(|e| e.to_string())?;
        }
        Some("ics") => {
            let bytes = read_limited(path, eudora_core::ics::ICS_MAX)?;
            let file = eudora_core::ics::read(&String::from_utf8_lossy(&bytes));
            let number = {
                let mut list = ICS.lock().unwrap_or_else(|e| e.into_inner());
                list.push((name.clone(), file));
                list.len() - 1
            };
            tauri::WebviewWindowBuilder::new(handle, format!("ics-{n}"), tauri::WebviewUrl::App(format!("ics.html?file={number}").into()))
                .title(format!("{name} – Eudora"))
                .inner_size(720.0, 640.0)
                .min_inner_size(420.0, 300.0)
                .build()
                .map_err(|e| e.to_string())?;
        }
        _ => return Err("Eudora opens .eml and .ics files".into()),
    }
    Ok(())
}

/// A calendar file opened: its name, what it's for, and its events.
#[tauri::command]
pub async fn ics_file(file: usize) -> Result<serde_json::Value, String> {
    let list = ICS.lock().unwrap_or_else(|e| e.into_inner());
    let (name, f) = list.get(file).ok_or("that calendar file isn't open any more")?;
    Ok(serde_json::json!({ "name": name, "method": f.method, "events": f.events }))
}

/// Adds one of a calendar file's events to a calendar. Never with its people: adding it here
/// mustn't send them invitations (it's the user's own copy).
#[tauri::command]
pub async fn ics_add(handle: tauri::AppHandle, file: usize, event: usize, calendar: i64) -> Result<i64, String> {
    let e = {
        let list = ICS.lock().unwrap_or_else(|e| e.into_inner());
        list.get(file).and_then(|(_, f)| f.events.get(event).cloned()).ok_or("that event isn't in the file any more")?
    };
    let new = eudora_core::meetings::NewEvent::from_ics(calendar, &e);
    tauri::async_runtime::spawn_blocking(move || handle.state::<crate::App>().with(|l| l.new_appointment(&new)))
        .await
        .map_err(|e| e.to_string())?
}
