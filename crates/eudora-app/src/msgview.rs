//! The .msg viewer.
//!
//! An Outlook .msg or an .eml opened (Explorer, Open with, `eudora file.msg`) starts this: a program of its
//! own with one small window (`ui/msgview.html`), never the Eudora window, the library, its
//! download, or the window's lock. The file is read and converted (`msg_read::to_eml`) on a thread
//! of its own while the window starts, so the two take as long as the slower of them. What the
//! library knows about the sender (the whitelist, the user's own domains, the always-show list for
//! pictures) is read from its database read-only, if it can be read at once; else nobody is
//! trusted and pictures ask. The message is shown with the main window's safety: no scripts,
//! nothing from the network, a fraud's links dead, programs never opened, files saved marked as
//! downloaded from the Internet. Reply, Reply All, and Forward hand the message to the Eudora
//! window (started if it isn't running) as an .eml in `answers_dir`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use eudora_core::outgoing::MessageView;
use serde::Serialize;

use crate::Reply;

/// The file opened, converted and read.
struct Opened {
    file: String,
    eml: Vec<u8>,
    view: MessageView,
    /// Pictures from the web load by themselves (the sender is on the always-show list).
    pictures: bool,
}

#[derive(Default)]
struct Job {
    done: Mutex<Option<Result<Arc<Opened>, String>>>,
    ready: Condvar,
}

/// The viewer's state.
struct Viewer {
    job: Arc<Job>,
    started: Instant,
}

impl Viewer {
    fn opened(&self) -> Result<Arc<Opened>, String> {
        let mut done = self.job.done.lock().unwrap_or_else(|e| e.into_inner());
        while done.is_none() {
            done = self.job.ready.wait(done).unwrap_or_else(|e| e.into_inner());
        }
        done.clone().unwrap_or_else(|| Err("the file wasn't read".into()))
    }
}

/// The message files among those given (.msg, and .eml); the rest.
pub fn split(files: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    files.into_iter().partition(|f| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("msg") || e.eq_ignore_ascii_case("eml")))
}

/// Starts a viewer of its own for a .msg file (another copy of this program).
pub fn spawn(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe).arg(path).spawn().map(|_| ()).map_err(|e| format!("Couldn't start the viewer: {e}"))
}

/// Where Reply / Forward leave the message for the Eudora window (removed after a day).
pub fn answers_dir() -> PathBuf {
    std::env::temp_dir().join("Eudora msg")
}

/// Where attachments opened from a viewer are written (removed after a day).
fn opened_dir() -> PathBuf {
    answers_dir().join(format!("open-{}", std::process::id()))
}

/// Earlier viewers' files, a day old or more.
fn tidy() {
    let Ok(entries) = std::fs::read_dir(answers_dir()) else { return };
    let day = std::time::Duration::from_secs(24 * 3600);
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > day);
        if old {
            let path = e.path();
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

fn prepare(path: &Path) -> Result<Opened, String> {
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let size = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if size > eudora_core::library::OPEN_FILE_MAX {
        return Err(format!("{file} is {} MB, too big to be a message", size / 1_048_576));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // (an .eml, or a ".msg" that's really one, as some programs save: shown as it is)
    let eml = if eudora_core::msg_read::looks_like_msg(&bytes) { eudora_core::msg_read::to_eml(&bytes).map_err(|e| e.to_string())? } else { bytes };
    let from = eudora_core::outgoing::sender_of(&eml);
    let trust = eudora_core::outgoing::SenderTrust::of_library(eudora_core::library::usual_library().as_deref(), &from);
    let view = eudora_core::outgoing::file_message_view(-1, &eml, &trust);
    Ok(Opened { file, pictures: trust.show_pictures && view.warning.is_none(), eml, view })
}

/// Notes a step's time for measuring (`EUDORA_VIEW_TIMING` names the file), else nothing.
fn note(started: Instant, what: &str) {
    let Some(file) = std::env::var_os("EUDORA_VIEW_TIMING") else { return };
    use std::io::Write;
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| writeln!(f, "{what} {}", started.elapsed().as_millis()));
}

/// The viewer: reads the file while its window starts; ends when the window closes.
pub fn run(context: tauri::Context<tauri::Wry>, path: PathBuf) {
    let started = Instant::now();
    let job = Arc::new(Job::default());
    {
        let job = job.clone();
        let path = path.clone();
        std::thread::spawn(move || {
            let result = prepare(&path).map(Arc::new);
            note(started, "read");
            *job.done.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            job.ready.notify_all();
            tidy();
        });
    }
    let title = format!("{} – Eudora", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    tauri::Builder::default()
        .manage(Viewer { job, started })
        .invoke_handler(tauri::generate_handler![
            msgview_message,
            msgview_shown,
            msgview_source,
            msgview_pictures,
            msgview_open_link,
            msgview_attachment_open,
            msgview_attachment_save,
            msgview_attachments_save_all,
            msgview_save_eml,
            msgview_answer,
            msgview_file_icon,
            msgview_theme
        ])
        .setup(move |app| {
            note(started, "setup");
            // (the Eudora window's WebView2 folder: when it's open, its browser is joined, quicker
            // than starting one; measured 2026-11-11, a folder of its own took 200 ms longer)
            tauri::WebviewWindowBuilder::new(app, "msgview", tauri::WebviewUrl::App("msgview.html".into()))
                .title(title)
                .inner_size(900.0, 760.0)
                .min_inner_size(420.0, 300.0)
                .build()?;
            note(started, "window");
            Ok(())
        })
        .run(context)
        .expect("error while starting the Eudora message viewer");
}

#[derive(Serialize)]
struct Shown {
    file: String,
    view: MessageView,
    pictures: bool,
}

fn opened(state: &tauri::State<'_, Viewer>) -> Result<Arc<Opened>, String> {
    state.opened()
}

/// The message (waits for the file to be read, which began before the window).
#[tauri::command]
async fn msgview_message(state: tauri::State<'_, Viewer>) -> Reply<Shown> {
    let job = state.job.clone();
    let started = state.started;
    note(started, "asked");
    let o = tauri::async_runtime::spawn_blocking(move || Viewer { job, started }.opened()).await.map_err(crate::text)??;
    Ok(Shown { file: o.file.clone(), view: o.view.clone(), pictures: o.pictures })
}

/// The message is on screen: how long since the program started, in milliseconds (also written
/// to the file `EUDORA_VIEW_TIMING` names, for measuring); None unless measuring.
#[tauri::command]
fn msgview_shown(state: tauri::State<'_, Viewer>) -> Option<u128> {
    std::env::var_os("EUDORA_VIEW_TIMING")?;
    note(state.started, "shown");
    Some(state.started.elapsed().as_millis())
}

/// The message as an internet message (what the .msg became), as text.
#[tauri::command]
fn msgview_source(state: tauri::State<'_, Viewer>) -> Reply<String> {
    Ok(String::from_utf8_lossy(&opened(&state)?.eml).into_owned())
}

/// The message's pictures from the web, fetched by Eudora (the message's frame stays offline):
/// never a fraud's.
#[tauri::command]
async fn msgview_pictures(state: tauri::State<'_, Viewer>) -> Reply<Vec<eudora_core::remote_images::Picture>> {
    let o = opened(&state)?;
    if o.view.warning.is_some() {
        return Err("This message looks like a fraud: its pictures aren't loaded.".into());
    }
    let urls = o.view.web_pictures.clone();
    tauri::async_runtime::spawn_blocking(move || eudora_core::remote_images::fetch(&urls, None)).await.map_err(crate::text)
}

/// A link in the message: a web link in the browser, a mailto: link a new message in Eudora;
/// nothing else, and nothing from a fraud.
#[tauri::command]
fn msgview_open_link(state: tauri::State<'_, Viewer>, url: String) -> Reply<()> {
    if opened(&state)?.view.warning.is_some() {
        return Err("Links in a message that looks like a fraud are turned off.".into());
    }
    let link = url.trim();
    if link.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")) && !link.chars().any(char::is_control) {
        let exe = std::env::current_exe().map_err(crate::text)?;
        return std::process::Command::new(exe).arg(link).spawn().map(|_| ()).map_err(|e| format!("Couldn't start Eudora: {e}"));
    }
    let Some(link) = crate::web_link(link) else {
        return Err(format!("Eudora only opens web links (http, https), not “{}”", url.chars().take(80).collect::<String>()));
    };
    eudora_core::graph::open_browser(link).map_err(|e| format!("Couldn't open the browser: {e}"))
}

/// An attachment's name and bytes.
fn attachment(o: &Opened, part: &str) -> Result<(String, Vec<u8>), String> {
    let name = o.view.attachments.iter().find(|a| a.part == part).map(|a| a.name.clone()).ok_or("that attachment isn't in the message")?;
    let (_, bytes) = eudora_core::compose::part_bytes(&o.eml, part).ok_or("that attachment couldn't be read")?;
    Ok((name, bytes))
}

/// Opens an attachment with the system's program (written to a folder of this viewer's, marked
/// as downloaded); never a program, never a fraud's.
#[tauri::command]
async fn msgview_attachment_open(state: tauri::State<'_, Viewer>, part: String) -> Reply<String> {
    let o = opened(&state)?;
    if o.view.warning.is_some() {
        return Err("This message looks like a fraud: its attachments aren't opened. Save one only if you're sure it's safe.".into());
    }
    let (name, bytes) = attachment(&o, &part)?;
    if eudora_core::saving::runnable(&name) {
        return Err(format!("“{name}” can run a program on your computer, so Eudora doesn't open it. Save it, and open it yourself only if you trust who sent it."));
    }
    let path = eudora_core::saving::write_file(&opened_dir(), &name, &bytes).map_err(crate::text)?;
    let shown = path.to_string_lossy().into_owned();
    // (the system's program starts on a thread of its own: the viewer never waits for it)
    std::thread::spawn(move || {
        let _ = eudora_core::graph::open_browser(&path.to_string_lossy());
    });
    Ok(shown)
}

/// Asks for a folder (the system's dialog); None when canceled.
async fn folder(window: tauri::Window, title: &str) -> Reply<Option<PathBuf>> {
    Ok(crate::pick_with(window, eudora_core::pick::Pick::Folder, title.to_string(), &[]).await?.into_iter().next())
}

/// Saves one attachment into a folder the user chooses; where it went (None: canceled).
#[tauri::command]
async fn msgview_attachment_save(window: tauri::Window, state: tauri::State<'_, Viewer>, part: String) -> Reply<Option<String>> {
    let o = opened(&state)?;
    let (name, bytes) = attachment(&o, &part)?;
    let Some(dir) = folder(window, &format!("Save “{name}” in")).await? else { return Ok(None) };
    let path = eudora_core::saving::write_file(&dir, &name, &bytes).map_err(crate::text)?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Saves every attachment into a folder the user chooses.
#[tauri::command]
async fn msgview_attachments_save_all(window: tauri::Window, state: tauri::State<'_, Viewer>) -> Reply<Option<Vec<String>>> {
    let o = opened(&state)?;
    let Some(dir) = folder(window, "Save the attachments in").await? else { return Ok(None) };
    let mut saved = Vec::new();
    for a in &o.view.attachments {
        let (name, bytes) = attachment(&o, &a.part)?;
        saved.push(eudora_core::saving::write_file(&dir, &name, &bytes).map_err(crate::text)?.to_string_lossy().into_owned());
    }
    Ok(Some(saved))
}

/// The system's icon for a kind of file, as a data: URL (as the window's `file_icon`).
#[tauri::command]
async fn msgview_file_icon(name: String) -> Option<String> {
    use base64::Engine;
    let png = tauri::async_runtime::spawn_blocking(move || eudora_core::file_icons::icon_png(&name)).await.ok()??;
    Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

/// Light or dark as Eudora's window was set (as the window's `theme_set`).
#[tauri::command]
fn msgview_theme(handle: tauri::AppHandle, theme: Option<String>) {
    handle.set_theme(match theme.as_deref() {
        Some("light") => Some(tauri::Theme::Light),
        Some("dark") => Some(tauri::Theme::Dark),
        _ => None,
    });
}

/// A name for the message's file: its subject, made safe.
fn eml_name(o: &Opened) -> String {
    let stem: String = o.view.subject.chars().take(120).collect::<String>().trim().trim_end_matches(['.', ' ']).to_string();
    eudora_core::saving::safe_file_name(&format!("{}.eml", if stem.is_empty() { "(no subject)" } else { &stem }))
}

/// Saves the message as an .eml in a folder the user chooses.
#[tauri::command]
async fn msgview_save_eml(window: tauri::Window, state: tauri::State<'_, Viewer>) -> Reply<Option<String>> {
    let o = opened(&state)?;
    let Some(dir) = folder(window, "Save the message as an .eml file in").await? else { return Ok(None) };
    let path = eudora_core::saving::write_file(&dir, &eml_name(&o), &o.eml).map_err(crate::text)?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Reply, Reply All, Forward: the message handed to the Eudora window (started when it isn't
/// open), which opens a new message from it.
#[tauri::command]
fn msgview_answer(state: tauri::State<'_, Viewer>, mode: String) -> Reply<String> {
    if !["reply", "replyAll", "forward"].contains(&mode.as_str()) {
        return Err(format!("“{mode}” isn't a way to answer a message"));
    }
    let o = opened(&state)?;
    let dir = answers_dir();
    std::fs::create_dir_all(&dir).map_err(crate::text)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = dir.join(format!("{stamp}-{}.eml", std::process::id()));
    std::fs::write(&path, &o.eml).map_err(crate::text)?;
    let exe = std::env::current_exe().map_err(crate::text)?;
    std::process::Command::new(exe).arg(format!("{}{mode}:{}", crate::opening::COMPOSE_PREFIX, path.display())).spawn().map_err(|e| format!("Couldn't start Eudora: {e}"))?;
    Ok("Eudora".into())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn message_files_go_to_the_viewer() {
        let files = vec![PathBuf::from("a.MSG"), PathBuf::from("b.ics"), PathBuf::from("c.eml"), PathBuf::from("mailto:x@example.com")];
        let (msgs, rest) = super::split(files);
        assert_eq!(msgs, [PathBuf::from("a.MSG"), PathBuf::from("c.eml")]);
        assert_eq!(rest, [PathBuf::from("b.ics"), PathBuf::from("mailto:x@example.com")]);
    }
}
