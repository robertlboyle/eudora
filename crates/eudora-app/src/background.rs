//! The download, run by the window while it's open.
//!
//! The window only reads the library; the download (`eudora-cli graph download`) is its one
//! writer. So the window starts that program itself: hidden, at below-normal priority, beside
//! its own exe, and stops it when the window closes. When another copy already holds the
//! library's write lock (`download.cmd` started by hand), it leaves it alone and looks again
//! every half minute: when that one ends, the window takes over. A download that stops with an
//! error is started again after 30 seconds (it resumes where it was), as `download.cmd` does.
//! Its record goes to `<library>/logs/download.log`; its progress to the status file the
//! toolbar badge already shows.

use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(desktop)]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The setting: whether the window runs the download, and what it includes.
pub const SETTING: &str = "background-download";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub on: bool,
    /// The Microsoft 365 Online Archive too.
    pub archive: bool,
    /// The shared mailboxes added.
    pub shared: bool,
}

impl Default for Options {
    fn default() -> Options {
        // shared mailboxes: brought up to date when opened, not read in full in the background
        // phones: not the Online Archive (older mail; a phone downloads what's recent first)
        Options { on: true, archive: cfg!(desktop), shared: false }
    }
}

impl Options {
    pub fn read(json: Option<String>) -> Options {
        json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }
}

/// What the background download is doing, for Settings.
#[derive(Clone, Debug, Default, Serialize)]
pub struct State {
    /// "running", "elsewhere" (another copy has the library), "off", "restarting", "missing",
    /// "starting".
    pub state: String,
    pub pid: Option<u32>,
    pub note: String,
    pub log: String,
}

#[derive(Clone)]
pub struct Background {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    pub state: Arc<Mutex<State>>,
    /// Set when the options change: the running download is stopped and started again with them.
    restart: Arc<AtomicBool>,
    /// Set while something needs the library to itself (a backup): the download is stopped
    /// and waits; cleared, it starts again.
    held: Arc<AtomicBool>,
}

/// The download program: `eudora-cli` beside this one (both are built and installed together).
#[cfg(desktop)]
pub fn program() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let exe = me.with_file_name(if cfg!(windows) { "eudora-cli.exe" } else { "eudora-cli" });
    exe.is_file().then_some(exe)
}

/// The copy of the download program that runs: `<library>/bin/`, refreshed whenever the original
/// changes (size or time). A running program can't be replaced on Windows, so running the
/// original would block a new build or an update for as long as Eudora is open.
#[cfg(desktop)]
fn running_copy(original: &Path, library: &Path) -> std::io::Result<PathBuf> {
    let dir = library.join("bin");
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(original.file_name().unwrap_or_default());
    let meta = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
    if meta(original) != meta(&copy) {
        let fresh = dir.join("eudora-new.part");
        std::fs::copy(original, &fresh)?;
        let _ = std::fs::File::options().write(true).open(&fresh).and_then(|f| f.set_modified(std::fs::metadata(original)?.modified()?));
        std::fs::rename(&fresh, &copy)?;
    }
    Ok(copy)
}

/// Whether another copy holds the library's write lock right now.
pub fn writer_elsewhere(library: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(library.join("library.lock")) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(_) => true,
    }
}

impl Background {
    /// Starts watching: `options` is read each time a download is about to start.
    pub fn start(library: PathBuf, options: impl Fn() -> Options + Send + 'static) -> Background {
        let bg = Background {
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(State { state: "starting".into(), ..Default::default() })),
            restart: Arc::new(AtomicBool::new(false)),
            held: Arc::new(AtomicBool::new(false)),
        };
        let (stop, child, state, restart, held) = (bg.stop.clone(), bg.child.clone(), bg.state.clone(), bg.restart.clone(), bg.held.clone());
        std::thread::spawn(move || run(library, options, stop, child, state, restart, held));
        bg
    }

    /// No library open: nothing to download into.
    pub fn idle(note: &str) -> Background {
        Background {
            stop: Arc::new(AtomicBool::new(true)),
            child: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(State { state: "off".into(), note: note.into(), ..Default::default() })),
            restart: Arc::new(AtomicBool::new(false)),
            held: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Stops the download and keeps it stopped (`true`, e.g. for a backup), or lets it start
    /// again (`false`).
    pub fn hold(&self, on: bool) {
        self.held.store(on, Ordering::Relaxed);
        if on {
            if let Some(mut c) = self.child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    /// The options changed: stop the running download; the watcher starts it again with them.
    pub fn options_changed(&self) {
        self.restart.store(true, Ordering::Relaxed);
    }

    /// The window is closing: stop the download it started (it resumes next time).
    pub fn shut_down(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn set(state: &Mutex<State>, what: &str, pid: Option<u32>, note: String, log: &Path) {
    *state.lock().unwrap() = State { state: what.into(), pid, note, log: log.display().to_string() };
}

/// Sleeps in short steps, so stopping is quick.
fn pause(stop: &AtomicBool, restart: &AtomicBool, secs: u64) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until && !stop.load(Ordering::Relaxed) && !restart.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn run(
    library: PathBuf,
    options: impl Fn() -> Options,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    state: Arc<Mutex<State>>,
    restart: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
) {
    let logs = library.join("logs");
    let log = logs.join("download.log");
    // (a phone runs no download program of its own)
    #[cfg(mobile)]
    let _ = &child;
    while !stop.load(Ordering::Relaxed) {
        restart.store(false, Ordering::Relaxed);
        if held.load(Ordering::Relaxed) {
            set(&state, "held", None, "Paused while Eudora is backed up; it starts again when the backup is done.".into(), &log);
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let opts = options();
        if !opts.on {
            set(&state, "off", None, "Downloading while Eudora is open is switched off (Settings ▸ Downloading and sync).".into(), &log);
            pause(&stop, &restart, 5);
            continue;
        }
        // a phone app can't start another program: the same download runs in this thread
        #[cfg(mobile)]
        {
            let _ = std::fs::create_dir_all(&logs);
            set(&state, "running", None, "Eudora is keeping the library up to date while it's open.".into(), &log);
            let result = in_process(&library, &opts, &stop, &restart, &held, &log);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if let Err(e) = result {
                set(&state, "restarting", None, format!("The download stopped ({e}); starting it again in 30 seconds. Its log: {}", log.display()), &log);
                pause(&stop, &restart, 30);
            }
            continue;
        }
        #[cfg(desktop)]
        {
        let Some(exe) = program() else {
            set(&state, "missing", None, "The download program (eudora-cli) isn't beside Eudora's own program.".into(), &log);
            pause(&stop, &restart, 30);
            continue;
        };
        if writer_elsewhere(&library) {
            set(&state, "elsewhere", None, "Another copy of the download (download.cmd) is keeping the library up to date; Eudora takes over when it ends.".into(), &log);
            pause(&stop, &restart, 30);
            continue;
        }
        let _ = std::fs::create_dir_all(&logs);
        let exe = match running_copy(&exe, &library) {
            Ok(copy) => copy,
            Err(e) => {
                set(&state, "restarting", None, format!("Couldn't copy the download program into the library ({e}); trying again in 30 seconds."), &log);
                pause(&stop, &restart, 30);
                continue;
            }
        };
        let mut cmd = Command::new(&exe);
        cmd.arg("-l").arg(&library).arg("--log").arg(&log).args(["graph", "download", "--watch", "5"]);
        if opts.archive {
            cmd.arg("--archive");
        }
        // (which shared mailboxes: those marked active, `graph_sync::SHARED_ACTIVE`; the window
        // makes that list when it opens a library, so none is downloaded until chosen)
        cmd.arg("--shared");
        let _ = opts.shared;
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }
        let spawned = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                set(&state, "restarting", None, format!("The download couldn't start ({e}); trying again in 30 seconds."), &log);
                pause(&stop, &restart, 30);
                continue;
            }
        };
        let pid = spawned.id();
        set(&state, "running", Some(pid), "Eudora is keeping the library up to date while it's open.".into(), &log);
        *child.lock().unwrap() = Some(spawned);
        // wait for it to end, or for the window to close / the options to change
        let code = loop {
            if stop.load(Ordering::Relaxed) || restart.load(Ordering::Relaxed) || held.load(Ordering::Relaxed) {
                if let Some(mut c) = child.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                break None;
            }
            let mut guard = child.lock().unwrap();
            let Some(c) = guard.as_mut() else { break None };
            match c.try_wait() {
                Ok(Some(status)) => {
                    guard.take();
                    break Some(status);
                }
                Ok(None) => {}
                Err(_) => break None,
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(500));
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(status) = code {
            // (with --watch it doesn't end by itself; an end is an error, a kill, or a lock race)
            let how = status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "it was stopped".into());
            set(&state, "restarting", None, format!("The download stopped ({how}); starting it again in 30 seconds. Its log: {}", log.display()), &log);
            pause(&stop, &restart, 30);
        }
        }
    }
    set(&state, "off", None, "Eudora is closing.".into(), &log);
}

/// The download in this thread (phones): the core's loop (`watch::run`, as `eudora-cli graph
/// download --watch 5` runs it on a desktop), stopped by the window closing, new options or a
/// hold; its events go to the download log as the desktop's program writes them.
#[cfg(mobile)]
fn in_process(library: &Path, opts: &Options, stop: &AtomicBool, restart: &AtomicBool, held: &AtomicBool, log: &Path) -> eudora_core::Result<()> {
    struct Here<'a> {
        flags: [&'a AtomicBool; 3],
        log: Mutex<Option<std::fs::File>>,
    }
    impl eudora_core::Progress for Here<'_> {
        fn report(&self, _done: u64, _total: u64, _stage: &str) {}
        fn canceled(&self) -> bool {
            self.flags.iter().any(|f| f.load(Ordering::Relaxed))
        }
        fn event(&self, text: &str) {
            use std::io::Write;
            if let Some(f) = self.log.lock().unwrap().as_mut() {
                let _ = writeln!(f, "{}  {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
            }
        }
    }
    let here = Here { flags: [stop, restart, held], log: Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(log).ok()) };
    let mut lib = eudora_core::Library::open(library, eudora_core::Access::Write, &here)?;
    // nothing downloads until the phone was told how much mail to download (asked before its
    // first account, and at start when an account came before the question)
    let mut said = false;
    while eudora_core::graph_sync::download_period(&lib).is_none() {
        if eudora_core::Progress::canceled(&here) {
            let _ = lib.close(&eudora_core::Silent);
            return Ok(());
        }
        if !said {
            eudora_core::Progress::event(&here, "Waiting for how much mail to download (asked on the phone).");
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let result = eudora_core::watch::run(&mut lib, &eudora_core::watch::Watch::background(opts.archive), &here);
    let _ = lib.close(&eudora_core::Silent);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_to_everything_on() {
        assert_eq!(Options::read(None), Options { on: true, archive: true, shared: false });
        assert_eq!(Options::read(Some(r#"{"on":false}"#.into())), Options { on: false, archive: true, shared: false });
        assert_eq!(Options::read(Some("not json".into())), Options::default());
    }

    #[test]
    fn the_running_copy_follows_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("eudora-cli.exe");
        std::fs::write(&original, b"version 1").unwrap();
        let library = dir.path().join("library");
        let copy = running_copy(&original, &library).unwrap();
        assert_eq!(copy, library.join("bin").join("eudora-cli.exe"));
        assert_eq!(std::fs::read(&copy).unwrap(), b"version 1");
        std::fs::write(&original, b"version 2, longer").unwrap();
        running_copy(&original, &library).unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"version 2, longer", "a new build is picked up");
    }

    #[test]
    fn a_held_write_lock_means_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!writer_elsewhere(dir.path()));
        let held = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.path().join("library.lock")).unwrap();
        held.lock().unwrap();
        assert!(writer_elsewhere(dir.path()));
        held.unlock().unwrap();
        assert!(!writer_elsewhere(dir.path()));
    }
}
