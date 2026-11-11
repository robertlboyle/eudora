//! A library: one folder holding the database, the packs, the dictionaries and the index.
//!
//! ```text
//! <library>/eudora.db     SQLite (WAL)
//! <library>/packs/        raw messages
//! <library>/dicts/        zstd dictionaries
//! <library>/index/        Tantivy
//! <library>/imports/      import reports
//! <library>/images/       pictures from the web kept on this computer (when the user chose so)
//! <library>/status/       what a running download is doing
//! <library>/library.lock  held while a copy of Eudora writes to it
//! ```
//! Only one copy of Eudora may write to a library at a time (the lock); readers don't lock.

use std::fs::File;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db;
use crate::error::{Error, IoContext, Result};
use crate::index::{IndexDoc, SearchIndex, SearchResult, SortOrder};
use crate::pack::{Compression, DEFAULT_PACK_LIMIT, Location, PackStore};
use crate::parse::{self, DateHints};
use crate::progress::Progress;

pub const ZSTD_LEVEL: i32 = 3;
/// Index writer memory (shared by its threads) for imports and rebuilds.
pub const INDEX_MEMORY: usize = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

pub struct Library {
    pub root: PathBuf,
    pub(crate) conn: Connection,
    pub(crate) packs: PackStore,
    pub(crate) index: SearchIndex,
    /// Search by any header (built in the background; None until the writer has made it).
    pub(crate) headers: Option<crate::header_index::HeaderIndex>,
    pub(crate) access: Access,
    /// The messages a search leaves out (search_scope.rs), remembered with what they were worked
    /// out from.
    pub(crate) search_hidden: std::sync::Mutex<Option<crate::search_scope::HiddenCache>>,
    _lock: Option<File>,
}

/// One row of a message list.
#[derive(Clone, Debug)]
pub struct MessageRow {
    pub id: i64,
    pub date: i64,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub size: u64,
    pub attachments: u64,
    pub message_id: String,
    pub list_id: Option<String>,
    /// When it came into the library (unix seconds).
    pub added: i64,
    /// Status and priority (`marks`: UNREAD, REPLIED…, the priority set by hand).
    pub flags: i64,
    /// Its labels (Outlook categories), in the order given.
    pub labels: Vec<String>,
}

/// A text value as sorted: lowercase, without quotes and leading spaces, a subject without its
/// Re: / Fwd: prefixes, and only its start (millions are kept in memory while sorting).
fn sort_text(key: &str, value: &str) -> String {
    let lead = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<');
    let mut s = value.trim_start_matches(lead);
    if key == "subject" {
        // Re: / Fwd: … in any case, as often as they're there
        'prefixes: loop {
            for p in ["re:", "fw:", "fwd:", "aw:", "wg:", "sv:", "re :", "fw :"] {
                if s.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)) {
                    s = s[p.len()..].trim_start_matches(lead);
                    continue 'prefixes;
                }
            }
            break;
        }
    }
    let mut out = String::with_capacity(36);
    for c in s.chars().flat_map(char::to_lowercase).take(32) {
        out.push(c);
    }
    if out.trim_end().is_empty() {
        return SORT_EMPTY.into();
    }
    out
}

/// An empty value while sorting (kept last).
const SORT_EMPTY: &str = "\u{10FFFF}";

/// Where a folder view continues: the last row shown.
#[derive(Clone, Copy, Debug)]
pub struct PageCursor {
    pub date: i64,
    pub id: i64,
}

#[derive(Clone, Debug)]
pub struct FolderInfo {
    pub id: i64,
    pub path: String,
    pub messages: u64,
    /// The account's name in the folder pane ("On This Computer", "pat@example.com").
    pub account: String,
    pub account_id: i64,
}

/// One original copy of a message (see `copies` in the database).
#[derive(Clone, Debug, serde::Serialize)]
pub struct CopyInfo {
    pub id: i64,
    /// mime, exchange-export
    pub format: String,
    pub size: u64,
    pub server_id: Option<String>,
    /// The folder it came from, its account's name, and the account's type.
    pub folder: Option<String>,
    pub account: Option<String>,
    pub kind_name: Option<&'static str>,
}

/// An account: wherever its mail comes from (Microsoft 365, Gmail, IMAP, POP3, a classic
/// Eudora folder, other old mailboxes), it's shown the same way.
#[derive(Clone, Debug)]
pub struct AccountInfo {
    pub id: i64,
    /// graph, gmail, imap, pop3, eudora, mbox, local
    pub kind: String,
    /// What the kind is called ("Microsoft 365", "classic Eudora").
    pub kind_name: &'static str,
    pub address: Option<String>,
    /// The user's own name for it.
    pub label: Option<String>,
    /// The name the provider or the old program had (e.g. the person's name).
    pub name: String,
    /// Where its mail came from ("C:\OldMail (personalities: …)").
    pub source: Option<String>,
    /// Messages in it, each once.
    pub messages: u64,
}

impl AccountInfo {
    /// What the folder pane shows: the user's name for it, else its address.
    pub fn shown(&self) -> String {
        self.label.clone().or_else(|| self.address.clone()).unwrap_or_else(|| self.name.clone())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub messages: u64,
    pub locations: u64,
    pub folders: u64,
    pub raw_bytes: u64,
    pub pack_bytes: u64,
    pub packs: u64,
    pub db_bytes: u64,
    pub index_bytes: u64,
    pub indexed_docs: u64,
    pub attachments: u64,
    pub attachment_refs: u64,
    pub addresses: u64,
    pub dictionary: Option<u32>,
    pub oldest: Option<i64>,
    pub newest: Option<i64>,
}

/// Where the library goes when none is named: the system's place for an app's local data,
/// never a synced folder (Windows: `%LOCALAPPDATA%\Eudora 27\Library`; the Mac:
/// `~/Library/Application Support/Eudora 27/Library`; Linux: `$XDG_DATA_HOME/eudora27/library`
/// or `~/.local/share/eudora27/library`).
pub fn default_library_path() -> Option<PathBuf> {
    // a phone app's own storage, as its system names it (set_app_data_dir)
    if let Some(dir) = APP_DATA.get() {
        return Some(dir.join("Library"));
    }
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("LOCALAPPDATA").map(|d| d.join("Eudora 27").join("Library"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library").join("Application Support").join("Eudora 27").join("Library"))
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local").join("share"))).map(|d| d.join("eudora27").join("library"))
    }
}

static APP_DATA: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Eudora's own folder as the system gives it to an app (Android, iOS: the app's private
/// storage, from Tauri), in place of the desktops' usual places: the library is `<it>/Library`,
/// updates, the remembered library and the window's lock beside it. Set once, at the start.
pub fn set_app_data_dir(dir: PathBuf) {
    let _ = APP_DATA.set(dir);
}

/// The file that remembers the library last opened by name (beside the usual library's folder),
/// so Eudora started with no folder named (the installer's shortcut) opens that one.
fn remembered_file() -> Option<PathBuf> {
    default_library_path().and_then(|p| p.parent().map(|d| d.join("last-library.txt")))
}

/// The library last opened by name, when it's still a library.
pub fn remembered_library() -> Option<PathBuf> {
    let text = std::fs::read_to_string(remembered_file()?).ok()?;
    let path = PathBuf::from(text.trim());
    if path.join("eudora.db").exists() {
        return Some(path);
    }
    // (an iOS app's folder moves when the app is updated: the same name beside the usual library)
    let beside = default_library_path()?.with_file_name(path.file_name()?);
    beside.join("eudora.db").exists().then_some(beside)
}

/// Remembers a library opened by name (see `remembered_library`).
pub fn remember_library(path: &Path) {
    let Some(file) = remembered_file() else { return };
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, path.to_string_lossy().as_bytes());
}

/// The sync service whose folder holds `path` (Dropbox, OneDrive, Google Drive, iCloud Drive,
/// Box), if any: a live library there gets corrupted by its file locks.
pub fn in_sync_folder(path: &Path) -> Option<&'static str> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    path.components().find_map(|c| {
        let name = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("dropbox") || name.ends_with(" dropbox") {
            Some("Dropbox")
        } else if name.starts_with("onedrive") {
            Some("OneDrive")
        } else if name == "google drive" || name.starts_with("googledrive") || name == "my drive" {
            Some("Google Drive")
        } else if name == "icloud drive" || name == "iclouddrive" || name == "mobile documents" {
            Some("iCloud Drive")
        } else if name == "box" || name == "box sync" {
            Some("Box")
        } else {
            None
        }
    })
}

/// Where Restore puts a library unless told otherwise: the usual place when it holds no library
/// yet (a new computer), else a folder beside it ("Library (restored 2026-10-27)").
pub fn restore_target() -> Option<PathBuf> {
    let usual = default_library_path()?;
    let empty = |p: &Path| !p.exists() || std::fs::read_dir(p).map(|mut d| d.next().is_none()).unwrap_or(false);
    if empty(&usual) {
        return Some(usual);
    }
    let name = usual.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Library".into());
    let base = format!("{name} (restored {})", chrono::Local::now().format("%Y-%m-%d"));
    (1..1000).map(|n| usual.with_file_name(if n == 1 { base.clone() } else { format!("{base} {n}") })).find(|p| empty(p))
}

/// Restores a backup as Eudora's library (File ▸ Restore Eudora, `eudora restore`,
/// `v1.library.restore`): into `dest` (else [`restore_target`]), never inside a sync service's
/// folder; the search index built when the backup has none; and the folder **becomes the library
/// Eudora opens** from then on. `replace`: the user agreed to replace the Eudora library already
/// in that folder (`backup::empty_for_restore`).
pub fn restore_as_library(backup: &Path, dest: Option<&Path>, password: Option<&str>, replace: bool, progress: &dyn Progress) -> Result<(PathBuf, crate::backup::RestoreReport)> {
    let dest = match dest.filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => std::path::absolute(d).unwrap_or_else(|_| d.to_path_buf()),
        None => restore_target().ok_or_else(|| Error::Other("This computer has no usual place for Eudora's library: name a folder.".into()))?,
    };
    if let Some(service) = in_sync_folder(&dest) {
        return Err(Error::Other(format!(
            "{} is inside a {service} folder. A library there gets damaged while {service} copies its files: choose a folder outside it (the usual place is {}).",
            dest.display(),
            default_library_path().map(|p| p.display().to_string()).unwrap_or_default()
        )));
    }
    if replace {
        crate::backup::empty_for_restore(&dest, progress)?;
    }
    let report = crate::backup::restore(backup, &dest, password, progress)?;
    if report.rebuild_indexes {
        Library::open(&dest, Access::Write, progress)?.close(progress)?;
    }
    remember_library(&dest);
    Ok((dest, report))
}

/// Restoring over the library Eudora has open: the backup goes into a folder beside it, and the
/// next start swaps it into the library's place before opening it. The open library is never
/// emptied first, so a restore that fails or is stopped leaves it as it was.
const REPLACE_FILE: &str = "replace-library.txt";
/// A replaced library still being deleted (or left by a start that ended before it was gone).
const REPLACED_FILE: &str = "replaced-library.txt";

/// The folder beside `target` a restore over it goes into first.
pub fn staging_for(target: &Path) -> PathBuf {
    let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Library".into());
    target.with_file_name(format!("{name} (restoring)"))
}

/// Notes that `staged` (a finished restore) takes `target`'s place at the next start.
pub fn replace_at_next_start(staged: &Path, target: &Path) -> Result<()> {
    let dir = app_data_dir().ok_or_else(|| Error::Other("This computer has no folder for Eudora's own files.".into()))?;
    note_replacement(&dir, staged, target)?;
    remember_library(target);
    Ok(())
}

fn note_replacement(dir: &Path, staged: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
    let file = dir.join(REPLACE_FILE);
    std::fs::write(&file, format!("{}\n{}\n", staged.display(), target.display())).context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// At start, before a library opens: puts a restore waiting to replace a library in its place
/// (`replace_at_next_start`), and goes on deleting a library a swap replaced. The old library
/// is first renamed aside (quick, and undone if the restored one can't move in), then deleted on
/// a thread of its own (~100 GB takes a while). Waits up to 2 minutes for the Eudora that asked
/// for it (and its download) to let the library go. Ok(None): nothing waited.
pub fn finish_replacement() -> Result<Option<PathBuf>> {
    match app_data_dir() {
        Some(dir) => finish_replacement_in(&dir),
        None => Ok(None),
    }
}

fn finish_replacement_in(dir: &Path) -> Result<Option<PathBuf>> {
    let dir = dir.to_path_buf();
    delete_replaced(&dir);
    let file = dir.join(REPLACE_FILE);
    let Ok(text) = std::fs::read_to_string(&file) else { return Ok(None) };
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let (Some(staged), Some(target)) = (lines.next().map(PathBuf::from), lines.next().map(PathBuf::from)) else {
        let _ = std::fs::remove_file(&file);
        return Ok(None);
    };
    if !staged.join("eudora.db").is_file() {
        let _ = std::fs::remove_file(&file);
        return Err(Error::Other(format!("The restored library to put in place of {} isn't there any more ({}).", target.display(), staged.display())));
    }
    // the library must be free: the Eudora that restarted, and its download, are closing
    let lock_path = target.join("library.lock");
    let started = std::time::Instant::now();
    if lock_path.exists() {
        loop {
            let free = std::fs::OpenOptions::new().write(true).open(&lock_path).map(|f| f.try_lock().is_ok()).unwrap_or(true);
            if free {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(120) {
                return Err(Error::Other(format!("{} is still in use (another Eudora or its download), so the restored library couldn't take its place yet. Close every Eudora and start it again.", target.display())));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    // a rename can be refused for a moment (an antivirus or the search indexer reading a file)
    let rename = |from: &Path, to: &Path| -> std::io::Result<()> {
        let mut tries = 0;
        loop {
            match std::fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(_) if tries < 60 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => return Err(e),
            }
        }
    };
    let old = if target.exists() {
        let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Library".into());
        let base = format!("{name} (replaced {})", chrono::Local::now().format("%Y-%m-%d %H%M%S"));
        let aside = (1..1000).map(|n| target.with_file_name(if n == 1 { base.clone() } else { format!("{base} {n}") })).find(|p| !p.exists()).unwrap_or_else(|| target.with_file_name(base));
        rename(&target, &aside).context(|| format!("moving {} aside", target.display()))?;
        Some(aside)
    } else {
        None
    };
    if let Err(e) = rename(&staged, &target) {
        if let Some(old) = &old {
            let _ = rename(old, &target); // (the old library back where it was)
        }
        return Err(Error::Other(format!("The restored library couldn't take the place of {}: {e}. It's still in {}.", target.display(), staged.display())));
    }
    let _ = std::fs::remove_file(&file);
    if app_data_dir().as_deref() == Some(dir.as_path()) {
        remember_library(&target);
    }
    if let Some(old) = old {
        let _ = std::fs::write(dir.join(REPLACED_FILE), old.display().to_string());
        delete_replaced(&dir);
    }
    Ok(Some(target))
}

/// Deletes a library a swap replaced, on a thread of its own (only a folder that is a library).
fn delete_replaced(dir: &Path) {
    let file = dir.join(REPLACED_FILE);
    let Ok(old) = std::fs::read_to_string(&file) else { return };
    let old = PathBuf::from(old.trim());
    std::thread::spawn(move || {
        if old.join("eudora.db").exists() || old.join("packs").is_dir() {
            if std::fs::remove_dir_all(&old).is_err() {
                return; // (tried again at the next start)
            }
        }
        let _ = std::fs::remove_file(file);
    });
}

/// Eudora's own folder for this user (beside the usual library): the remembered library, the
/// window's lock, files handed to a running window.
pub fn app_data_dir() -> Option<PathBuf> {
    default_library_path().and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Messages opened from files (an .eml double-clicked), shown without adding them to the
/// library: kept in memory by a number below zero for as long as the program runs, so a
/// message window (Reply, Forward, attachments, Print) works on them as on any message.
static OPENED: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

/// The largest message file opened (a message bigger than this isn't a message).
pub const OPEN_FILE_MAX: u64 = 200 * 1024 * 1024;

/// Keeps a message read from a file; its number (below zero) for the message window.
pub fn open_message_file(bytes: Vec<u8>) -> i64 {
    let mut opened = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    opened.push(bytes);
    -(opened.len() as i64)
}

fn opened_file(id: i64) -> Option<Vec<u8>> {
    let opened = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    usize::try_from(-id).ok().and_then(|n| n.checked_sub(1)).and_then(|i| opened.get(i).cloned())
}

/// The library to use when none is named: the one last opened by name, else the usual place.
pub fn usual_library() -> Option<PathBuf> {
    remembered_library().or_else(default_library_path)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Library {
    /// Opens the library at `root`; with `Access::Write`, creates it when the folder is new
    /// or empty, takes the write lock, repairs a torn pack write and brings the index up to
    /// date with the database.
    pub fn open(root: &Path, access: Access, progress: &dyn Progress) -> Result<Library> {
        let db_path = root.join("eudora.db");
        if !db_path.exists() {
            let empty = !root.exists()
                || std::fs::read_dir(root).context(|| format!("reading {}", root.display()))?.next().is_none();
            if access == Access::Read || !empty {
                return Err(Error::NotALibrary(root.to_path_buf()));
            }
            std::fs::create_dir_all(root).context(|| format!("creating {}", root.display()))?;
        }
        let lock = if access == Access::Write {
            let path = root.join("library.lock");
            let file = File::create(&path).context(|| format!("creating {}", path.display()))?;
            if file.try_lock().is_err() {
                return Err(Error::Locked(root.to_path_buf()));
            }
            Some(file)
        } else {
            None
        };
        let conn = db::open(&db_path, now())?;
        let packs = PackStore::open(&root.join("packs"), &root.join("dicts"), DEFAULT_PACK_LIMIT, ZSTD_LEVEL)?;
        let index = SearchIndex::open(&root.join("index"))?;
        let headers = crate::header_index::HeaderIndex::open(&root.join("header-index"), access == Access::Write)?;
        std::fs::create_dir_all(root.join("imports")).context(|| format!("creating {}", root.join("imports").display()))?;
        let library = Library { root: root.to_path_buf(), conn, packs, index, headers, access, search_hidden: Default::default(), _lock: lock };
        if access == Access::Write {
            library.packs.start_writing()?;
            if let Some(h) = &library.headers {
                h.start_writing()?;
            }
            library.index.start_writing(index_threads(), INDEX_MEMORY)?;
            library.catch_up_index(progress)?;
            progress.report(0, 0, "Checking that every message's original copy is recorded…");
            db::fill_first_copies(&library.conn)?;
        }
        Ok(library)
    }

    /// Repairs stored text that isn't valid UTF-8 (`text_repair`), saying what it repaired.
    pub fn repair_text(&self, progress: &dyn Progress) -> Result<u64> {
        progress.report(0, 0, "Checking that all stored text is valid…");
        let repaired = crate::text_repair::repair(&self.conn)?;
        let n: u64 = repaired.iter().map(|(_, _, n)| n).sum();
        if n > 0 {
            let said: Vec<String> = repaired.iter().map(|(t, c, n)| format!("{n} in {t}.{c}")).collect();
            progress.event(&format!("Repaired {n} stored text values that weren't valid ({}): only their broken bytes were replaced.", said.join(", ")));
        }
        Ok(n)
    }

    /// Every original copy of a message (its own first), with where each came from.
    pub fn copies_of(&self, id: i64) -> Result<Vec<CopyInfo>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.id, c.format, c.size, c.server_id, f.path, COALESCE(a.label, a.address, a.name), a.kind
             FROM copies c LEFT JOIN folders f ON f.id = c.folder LEFT JOIN accounts a ON a.id = f.account
             WHERE c.message = ?1 ORDER BY c.id",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            let kind: Option<String> = r.get(6)?;
            Ok(CopyInfo {
                id: r.get(0)?,
                format: r.get(1)?,
                size: r.get::<_, i64>(2)? as u64,
                server_id: r.get(3)?,
                folder: r.get(4)?,
                account: r.get(5)?,
                kind_name: kind.as_deref().map(db::kind_name),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One copy's original bytes.
    pub fn copy_bytes(&self, copy: i64) -> Result<Vec<u8>> {
        let location = self
            .conn
            .query_row("SELECT pack, pack_offset, pack_len FROM copies WHERE id = ?1", params![copy], |r| Self::location(r, 0))
            .optional()?
            .ok_or_else(|| Error::Other(format!("there's no copy {copy}")))?;
        Ok(self.packs.read(location)?.raw)
    }

    pub fn compression(&self) -> Compression {
        Compression { level: ZSTD_LEVEL, dict: self.packs.newest_dictionary() }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Lets the index finish its background merges (call before exiting after big writes).
    pub fn close(self, progress: &dyn Progress) -> Result<()> {
        if self.access == Access::Write {
            progress.report(0, 0, "Finishing the search index (merging its segments)…");
            self.index.finish_writing()?;
        }
        Ok(())
    }

    pub(crate) fn location(row: &rusqlite::Row, at: usize) -> rusqlite::Result<Location> {
        Ok(Location { pack: row.get::<_, i64>(at)? as u32, offset: row.get::<_, i64>(at + 1)? as u64, len: row.get::<_, i64>(at + 2)? as u32 })
    }

    /// The message's original bytes.
    /// One read-only query, every value as text (the command line's `sql`, for diagnosis). Only
    /// a single SELECT / WITH statement; the connection is also put in query-only mode for it.
    pub fn query_text(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        let head = sql.trim_start().to_ascii_lowercase();
        if !(head.starts_with("select") || head.starts_with("with")) || sql.trim().trim_end_matches(';').contains(';') {
            return Err(Error::Other("only one SELECT (or WITH … SELECT) query".into()));
        }
        self.conn.execute_batch("PRAGMA query_only = ON")?;
        let result = (|| -> Result<Vec<Vec<String>>> {
            let mut stmt = self.conn.prepare(sql)?;
            let n = stmt.column_count();
            let rows = stmt.query_map([], |r| {
                (0..n)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => "NULL".to_string(),
                            rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                            rusqlite::types::ValueRef::Real(v) => v.to_string(),
                            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                            rusqlite::types::ValueRef::Blob(b) => format!("<{} bytes>", b.len()),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<String>>>()
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })();
        self.conn.execute_batch("PRAGMA query_only = OFF")?;
        result
    }

    pub fn raw_message(&self, id: i64) -> Result<Vec<u8>> {
        if id < 0 {
            return opened_file(id).ok_or_else(|| Error::Other(format!("there's no message {id}")));
        }
        let location = self
            .conn
            .query_row("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1", params![id], |r| Self::location(r, 0))
            .optional()?
            .ok_or_else(|| Error::Other(format!("there's no message {id}")))?;
        Ok(self.packs.read(location)?.raw)
    }

    fn row(r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
        Ok(MessageRow {
            id: r.get(0)?,
            date: r.get(1)?,
            from: r.get(2)?,
            subject: r.get(3)?,
            snippet: r.get(4)?,
            size: r.get::<_, i64>(5)? as u64,
            attachments: r.get::<_, i64>(6)? as u64,
            message_id: r.get(7)?,
            list_id: r.get(8)?,
            added: r.get(9)?,
            flags: r.get(10)?,
            labels: Vec::new(),
        })
    }

    /// Rows for these ids, in the same order.
    pub fn rows(&self, ids: &[i64]) -> Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, date, from_display, subject, snippet, size, attachment_count, message_id, list_id, added, flags FROM messages WHERE id = ?1",
        )?;
        let mut labels = self.conn.prepare_cached("SELECT label FROM categories WHERE message = ?1 ORDER BY rowid")?;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(mut row) = stmt.query_row(params![id], Self::row).optional()? {
                row.labels = labels.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// A folder's messages, newest first, one page at a time (keyset paging: page 10,000 is
    /// as fast as page 1).
    pub fn list_folder(&self, folder: i64, after: Option<PageCursor>, limit: usize) -> Result<Vec<MessageRow>> {
        let mut ids = Vec::with_capacity(limit);
        match after {
            None => {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT message FROM locations WHERE folder = ?1 ORDER BY date DESC, message DESC LIMIT ?2",
                )?;
                for id in stmt.query_map(params![folder, limit as i64], |r| r.get::<_, i64>(0))? {
                    ids.push(id?);
                }
            }
            Some(c) => {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT message FROM locations WHERE folder = ?1 AND (date, message) < (?2, ?3)
                     ORDER BY date DESC, message DESC LIMIT ?4",
                )?;
                for id in stmt.query_map(params![folder, c.date, c.id, limit as i64], |r| r.get::<_, i64>(0))? {
                    ids.push(id?);
                }
            }
        }
        self.rows(&ids)
    }

    /// Messages in any of `folders` (each once), or every message when `folders` is empty
    /// (All Mail), newest first, one page at a time (keyset paging).
    pub fn list(&self, folders: &[i64], after: Option<PageCursor>, limit: usize) -> Result<Vec<MessageRow>> {
        if folders.len() == 1 {
            return self.list_folder(folders[0], after, limit);
        }
        let (date, id) = after.map_or((i64::MAX, i64::MAX), |c| (c.date, c.id));
        let ids: Vec<i64> = if folders.is_empty() {
            let mut stmt = self.conn.prepare_cached("SELECT id FROM messages WHERE (date, id) < (?1, ?2) ORDER BY date DESC, id DESC LIMIT ?3")?;
            stmt.query_map(params![date, id, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        } else {
            let marks = vec!["?"; folders.len()].join(",");
            let sql = format!(
                "SELECT message FROM locations WHERE folder IN ({marks}) AND (date, message) < (?, ?)
                 GROUP BY message ORDER BY date DESC, message DESC LIMIT ?"
            );
            let mut values: Vec<i64> = folders.to_vec();
            values.extend([date, id, limit as i64]);
            let mut stmt = self.conn.prepare_cached(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(values), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        self.rows(&ids)
    }

    /// How many messages `list(folders)` holds (each once).
    pub fn count(&self, folders: &[i64]) -> Result<u64> {
        let n: i64 = match folders {
            [] => self.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?,
            [one] => self.conn.query_row("SELECT COUNT(*) FROM locations WHERE folder = ?1", params![one], |r| r.get(0))?,
            many => {
                let marks = vec!["?"; many.len()].join(",");
                self.conn.query_row(
                    &format!("SELECT COUNT(DISTINCT message) FROM locations WHERE folder IN ({marks})"),
                    rusqlite::params_from_iter(many.iter()),
                    |r| r.get(0),
                )?
            }
        };
        Ok(n as u64)
    }

    /// The cursor `skip` rows into `list(folders)` (a jump down the scroll bar).
    pub fn cursor_in(&self, folders: &[i64], skip: u64) -> Result<Option<PageCursor>> {
        match folders {
            [one] => self.cursor_at(*one, skip),
            [] => Ok(self
                .conn
                .query_row("SELECT date, id FROM messages ORDER BY date DESC, id DESC LIMIT 1 OFFSET ?1", params![skip as i64], |r| {
                    Ok(PageCursor { date: r.get(0)?, id: r.get(1)? })
                })
                .optional()?),
            many => {
                let marks = vec!["?"; many.len()].join(",");
                let sql = format!(
                    "SELECT date, message FROM locations WHERE folder IN ({marks}) GROUP BY message
                     ORDER BY date DESC, message DESC LIMIT 1 OFFSET ?"
                );
                let mut values: Vec<i64> = many.to_vec();
                values.push(skip as i64);
                Ok(self
                    .conn
                    .query_row(&sql, rusqlite::params_from_iter(values), |r| Ok(PageCursor { date: r.get(0)?, id: r.get(1)? }))
                    .optional()?)
            }
        }
    }

    /// Each message's To and Cc ("Name <address>" or the address), for the list's columns.
    pub fn recipients_of(&self, ids: &[i64]) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.role, m.name, a.address FROM message_addresses m JOIN addresses a ON a.id = m.address
             WHERE m.message = ?1 AND m.role IN (1, 2) ORDER BY m.role, m.position",
        )?;
        ids.iter()
            .map(|id| {
                let (mut to, mut cc) = (Vec::new(), Vec::new());
                for row in stmt.query_map(params![id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
                    let (role, name, address) = row?;
                    let shown = if name.trim().is_empty() { address } else { format!("{} <{address}>", name.trim()) };
                    if role == 1 { to.push(shown) } else { cc.push(shown) }
                }
                Ok((to.join(", "), cc.join(", ")))
            })
            .collect()
    }

    /// Every address the user's own: their accounts' addresses (not shared mailboxes) and old
    /// Eudora personalities.
    pub fn own_addresses(&self) -> Result<Vec<String>> {
        // (not shared mailboxes: they're other people's, or a team's)
        let mut stmt = self.conn.prepare("SELECT address, source FROM accounts WHERE kind <> 'graph-shared'")?;
        let mut out = Vec::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))? {
            let (address, source) = row?;
            out.extend(address.filter(|a| a.contains('@')));
            // classic Eudora personalities, from where its mail came from ("C:\OldMail (personalities: a, b)")
            if let Some(list) = source.as_deref().and_then(|s| s.split_once("(personalities: ")).map(|(_, l)| l.trim_end_matches(')').to_string()) {
                out.extend(list.split(", ").filter(|p| p.contains('@')).map(str::to_string));
            }
        }
        out.sort_by_key(|a| a.to_lowercase());
        out.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        Ok(out)
    }

    /// `folder:name` / `in:name` (Outlook's and Gmail's): every folder of that name (or path) in
    /// any account, as the index's folder ids.
    fn resolve_folders(&self, query: &str) -> Result<String> {
        let mut out = Vec::new();
        for term in crate::aqs::terms(query) {
            let (neg, body) = match term.strip_prefix('-') {
                Some(b) => (true, b.to_string()),
                None => (false, term.clone()),
            };
            let folder = body.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("folder") || k.eq_ignore_ascii_case("in")).map(|(_, v)| v.trim_matches('"').to_lowercase());
            let Some(name) = folder.filter(|n| !n.is_empty() && !n.chars().all(|c| c.is_ascii_digit())) else {
                out.push(term);
                continue;
            };
            let mut stmt = self.conn.prepare_cached("SELECT id FROM folders WHERE lower(name) = ?1 OR lower(path) = ?1 OR lower(path) LIKE '%/' || ?1")?;
            let ids: Vec<i64> = stmt.query_map(params![name], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            out.push(match (ids.is_empty(), neg) {
                (true, false) => "folder:0".to_string(), // no such folder: nothing
                (true, true) => String::new(),
                (false, false) => format!("({})", ids.iter().map(|i| format!("folder:{i}")).collect::<Vec<_>>().join(" OR ")),
                (false, true) => ids.iter().map(|i| format!("-folder:{i}")).collect::<Vec<_>>().join(" "),
            });
        }
        Ok(out.join(" "))
    }

    /// The ids of these addresses in the library (the user's own, for `hide_from`).
    pub fn address_ids(&self, addresses: &[String]) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT id FROM addresses WHERE address = ?1")?;
        let mut ids = Vec::new();
        for a in addresses {
            if let Some(id) = stmt.query_row(params![a.trim().to_lowercase()], |r| r.get(0)).optional()? {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// All Mail without the messages sent from any of `hide_from` (address ids: the user's own,
    /// so what they sent, wherever it's filed), newest first, one page at a time.
    pub fn list_all_except_from(&self, hide_from: &[i64], after: Option<PageCursor>, limit: usize) -> Result<Vec<MessageRow>> {
        if hide_from.is_empty() {
            return self.list(&[], after, limit);
        }
        let (date, id) = after.map_or((i64::MAX, i64::MAX), |c| (c.date, c.id));
        let marks = vec!["?"; hide_from.len()].join(",");
        let sql = format!(
            "SELECT m.id FROM messages m WHERE (m.date, m.id) < (?, ?) AND NOT EXISTS
             (SELECT 1 FROM message_addresses a WHERE a.message = m.id AND a.role = 0 AND a.address IN ({marks}))
             ORDER BY m.date DESC, m.id DESC LIMIT ?"
        );
        let mut values = vec![date, id];
        values.extend_from_slice(hide_from);
        values.push(limit as i64);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let ids: Vec<i64> = stmt.query_map(rusqlite::params_from_iter(values), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        self.rows(&ids)
    }

    /// The folders All Mail - Filtered leaves out: Spam / Junk, Bulk, Notifications, Lists,
    /// Trash / Deleted Items, Drafts, Out, Sent, and their subfolders, in the mailbox or its
    /// Online Archive.
    pub fn filtered_out_folders(&self) -> Result<Vec<i64>> {
        const OUT: [&str; 14] = [
            "spam", "junk", "junk email", "junk e-mail", "bulk", "notifications", "lists", "deleted items", "trash", "drafts", "outbox", "sent", "sent items", "old sent items",
        ];
        let mut stmt = self.conn.prepare_cached("SELECT id, path FROM folders")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name) = row?;
            let name = name.strip_prefix("Online Archive/").unwrap_or(&name);
            let top = name.split('/').next().unwrap_or("").trim().to_lowercase();
            if OUT.contains(&top.as_str()) || top.starts_with("sent ") {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// All Mail - Filtered's condition on `m`: in a folder other than those, not judged spam,
    /// marketing, a notification or a mailing list by the spam filter, not from the user.
    fn filtered_where(out: &[i64], hide_from: &[i64]) -> String {
        let mut w = format!(
            "EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id{})
             AND NOT EXISTS (SELECT 1 FROM triage t WHERE t.message = m.id AND t.category IN ('spam', 'blocked', 'bulk', 'notification', 'list'))",
            if out.is_empty() { String::new() } else { format!(" AND l.folder NOT IN ({})", out.iter().map(i64::to_string).collect::<Vec<_>>().join(",")) }
        );
        if !hide_from.is_empty() {
            w.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM message_addresses a WHERE a.message = m.id AND a.role = 0 AND a.address IN ({}))",
                hide_from.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
            ));
        }
        w
    }

    /// All Mail - Filtered, newest first, a page after `after`.
    pub fn list_filtered(&self, hide_from: &[i64], after: Option<PageCursor>, limit: usize) -> Result<Vec<MessageRow>> {
        let (date, id) = after.map_or((i64::MAX, i64::MAX), |c| (c.date, c.id));
        let sql = format!("SELECT m.id FROM messages m WHERE (m.date, m.id) < (?1, ?2) AND {} ORDER BY m.date DESC, m.id DESC LIMIT ?3", Self::filtered_where(&self.filtered_out_folders()?, hide_from));
        let ids: Vec<i64> = self.conn.prepare(&sql)?.query_map(params![date, id, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        self.rows(&ids)
    }

    /// How many messages All Mail - Filtered holds (reads them all: seconds; counted lazily).
    pub fn count_filtered(&self, hide_from: &[i64]) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM messages m WHERE {}", Self::filtered_where(&self.filtered_out_folders()?, hide_from));
        Ok(self.conn.query_row(&sql, [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Where All Mail - Filtered's row `skip` is (for a page reached without the ones before).
    pub fn cursor_filtered(&self, hide_from: &[i64], skip: u64) -> Result<Option<PageCursor>> {
        let sql = format!("SELECT m.date, m.id FROM messages m WHERE {} ORDER BY m.date DESC, m.id DESC LIMIT 1 OFFSET ?1", Self::filtered_where(&self.filtered_out_folders()?, hide_from));
        Ok(self.conn.query_row(&sql, params![skip as i64], |r| Ok(PageCursor { date: r.get(0)?, id: r.get(1)? })).optional()?)
    }

    /// Every message of All Mail - Filtered with a folder it's in (Select All, then act on it).
    pub fn list_ids_filtered(&self, hide_from: &[i64]) -> Result<Vec<(i64, i64)>> {
        let sql = format!(
            "SELECT m.id, (SELECT MIN(l.folder) FROM locations l WHERE l.message = m.id) FROM messages m WHERE {}",
            Self::filtered_where(&self.filtered_out_folders()?, hide_from)
        );
        Ok(self.conn.prepare(&sql)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Recent Mail's condition on `m`: received since `since`, filed by the spam filter or the
    /// rules out of the Inbox (or in the folders they sort into: Bulk, Notifications, Lists),
    /// never judged spam or blocked, not in Spam / Junk or a Trash, not from the user.
    fn recent_where(&self, since: i64, hide_from: &[i64]) -> Result<String> {
        let mut sorted_into = Vec::new();
        let mut never = Vec::new();
        let mut stmt = self.conn.prepare_cached("SELECT id, path FROM folders")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, name) = row?;
            let name = name.strip_prefix("Online Archive/").unwrap_or(&name);
            let top = name.split('/').next().unwrap_or("").trim().to_lowercase();
            if ["bulk", "notifications", "lists"].contains(&top.as_str()) {
                sorted_into.push(id);
            } else if ["spam", "junk", "junk email", "junk e-mail", "deleted items", "trash", "deleted messages", "drafts", "outbox", "inbox"].contains(&top.as_str()) || top.starts_with("sent") || top == "old sent items" {
                never.push(id);
            }
        }
        let ids = |v: &[i64]| v.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let mut w = format!(
            "m.date >= {since}
             AND NOT EXISTS (SELECT 1 FROM triage t WHERE t.message = m.id AND t.category IN ('spam', 'blocked'))
             AND EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id{never}
                 AND ({into} EXISTS (SELECT 1 FROM triage t WHERE t.message = m.id AND t.applied = 1 AND t.category NOT IN ('priority', 'people', 'other'))))",
            never = if never.is_empty() { String::new() } else { format!(" AND l.folder NOT IN ({})", ids(&never)) },
            into = if sorted_into.is_empty() { String::new() } else { format!("l.folder IN ({}) OR", ids(&sorted_into)) },
        );
        if !hide_from.is_empty() {
            w.push_str(&format!(" AND NOT EXISTS (SELECT 1 FROM message_addresses a WHERE a.message = m.id AND a.role = 0 AND a.address IN ({}))", ids(hide_from)));
        }
        Ok(w)
    }

    /// Recent Mail, newest first, a page after `after`.
    pub fn list_recent(&self, since: i64, hide_from: &[i64], after: Option<PageCursor>, limit: usize) -> Result<Vec<MessageRow>> {
        let (date, id) = after.map_or((i64::MAX, i64::MAX), |c| (c.date, c.id));
        let sql = format!("SELECT m.id FROM messages m WHERE (m.date, m.id) < (?1, ?2) AND {} ORDER BY m.date DESC, m.id DESC LIMIT ?3", self.recent_where(since, hide_from)?);
        let ids: Vec<i64> = self.conn.prepare(&sql)?.query_map(params![date, id, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        self.rows(&ids)
    }

    /// How many messages Recent Mail holds.
    pub fn count_recent(&self, since: i64, hide_from: &[i64]) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM messages m WHERE {}", self.recent_where(since, hide_from)?);
        Ok(self.conn.query_row(&sql, [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Where Recent Mail's row `skip` is.
    pub fn cursor_recent(&self, since: i64, hide_from: &[i64], skip: u64) -> Result<Option<PageCursor>> {
        let sql = format!("SELECT m.date, m.id FROM messages m WHERE {} ORDER BY m.date DESC, m.id DESC LIMIT 1 OFFSET ?1", self.recent_where(since, hide_from)?);
        Ok(self.conn.query_row(&sql, params![skip as i64], |r| Ok(PageCursor { date: r.get(0)?, id: r.get(1)? })).optional()?)
    }

    /// Every message of Recent Mail with a folder it's in.
    pub fn list_ids_recent(&self, since: i64, hide_from: &[i64]) -> Result<Vec<(i64, i64)>> {
        let sql = format!("SELECT m.id, (SELECT MIN(l.folder) FROM locations l WHERE l.message = m.id) FROM messages m WHERE {}", self.recent_where(since, hide_from)?);
        Ok(self.conn.prepare(&sql)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// How many messages `list_all_except_from` holds (from the address index: no full scan).
    pub fn count_all_except_from(&self, hide_from: &[i64]) -> Result<u64> {
        let all = self.count(&[])?;
        if hide_from.is_empty() {
            return Ok(all);
        }
        let marks = vec!["?"; hide_from.len()].join(",");
        let sent: i64 = self.conn.query_row(
            &format!("SELECT COUNT(DISTINCT message) FROM message_addresses WHERE address IN ({marks}) AND role = 0"),
            rusqlite::params_from_iter(hide_from.iter()),
            |r| r.get(0),
        )?;
        Ok(all.saturating_sub(sent as u64))
    }

    /// Every message in a list with a folder it's in (its own in a folder's list): for acting on
    /// a whole list at once (Select All, then Delete, Transfer…). `hide_from` as in
    /// `list_all_except_from` (All Mail only).
    pub fn list_ids(&self, folders: &[i64], hide_from: &[i64]) -> Result<Vec<(i64, i64)>> {
        let not_mine = if hide_from.is_empty() || !folders.is_empty() {
            String::new()
        } else {
            format!(
                " WHERE NOT EXISTS (SELECT 1 FROM message_addresses a WHERE a.message = l.message AND a.role = 0 AND a.address IN ({}))",
                vec!["?"; hide_from.len()].join(",")
            )
        };
        let (sql, values): (String, Vec<i64>) = match folders {
            [] => (format!("SELECT l.message, MIN(l.folder) FROM locations l{not_mine} GROUP BY l.message"), hide_from.to_vec()),
            [one] => ("SELECT message, folder FROM locations WHERE folder = ?".into(), vec![*one]),
            many => (format!("SELECT message, MIN(folder) FROM locations WHERE folder IN ({}) GROUP BY message", vec!["?"; many.len()].join(",")), many.to_vec()),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// These messages sorted by a list column: "date", "added", "size", "attachments", "status",
    /// "priority", "subject" (without Re: / Fwd:), "from", "to", "cc" (the first one's name, else
    /// address), "preview", "list", "msgid". Ties: newest first. Read 500 at a time, so a list of
    /// millions is sorted in seconds (the window keeps the result for its pages).
    pub fn sort_ids(&self, ids: &[i64], key: &str, ascending: bool) -> Result<Vec<i64>> {
        let first_person = |role: u8| {
            format!(
                "COALESCE((SELECT CASE WHEN ma.name <> '' THEN ma.name ELSE a.address END FROM message_addresses ma JOIN addresses a ON a.id = ma.address
                  WHERE ma.message = m.id AND ma.role = {role} ORDER BY ma.position LIMIT 1), '')"
            )
        };
        let (value, text) = match key {
            "date" => ("m.date".to_string(), false),
            "added" => ("m.added".into(), false),
            "size" => ("m.size".into(), false),
            "attachments" => ("m.attachment_count".into(), false),
            // unread first, then replied, forwarded, redirected (classic's status column)
            "status" => ("(m.flags & 15)".into(), false),
            "priority" => ("((m.flags >> 8) & 7)".into(), false),
            "subject" => ("m.subject".into(), true),
            "from" => ("m.from_display".into(), true),
            "to" => (first_person(1), true),
            "cc" => (first_person(2), true),
            "preview" => ("m.snippet".into(), true),
            "list" => ("COALESCE(m.list_id, '')".into(), true),
            "msgid" => ("m.message_id".into(), true),
            other => return Err(Error::Other(format!("the list can't be sorted by {other}"))),
        };
        enum Value {
            Number(i64),
            Text(String),
        }
        let mut keyed: Vec<(Value, i64, i64)> = Vec::with_capacity(ids.len());
        // (the text borrowed from SQLite, one allocation a message: millions of them)
        let read = |r: &rusqlite::Row| -> rusqlite::Result<(Value, i64, i64)> {
            let id: i64 = r.get(0)?;
            let date: i64 = r.get(1)?;
            let v = if text { Value::Text(sort_text(key, r.get_ref(2)?.as_str().unwrap_or(""))) } else { Value::Number(r.get(2)?) };
            Ok((v, date, id))
        };
        if ids.len() > if cfg!(test) { 2 } else { 20_000 } {
            // a big list (All Mail: millions): one pass over every message, keeping the list's
            // (asking 500 at a time took 19 s for 2.4 million); which are in it: a table by id
            let top = ids.iter().copied().max().unwrap_or(0).max(0) as usize;
            let mut wanted = vec![false; top + 1];
            for &id in ids {
                if id >= 0 {
                    wanted[id as usize] = true;
                }
            }
            let mut stmt = self.conn.prepare(&format!("SELECT m.id, m.date, {value} FROM messages m WHERE m.id <= {top}"))?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                if id >= 0 && wanted[id as usize] {
                    keyed.push(read(r)?);
                }
            }
        } else {
            for chunk in ids.chunks(500) {
                let sql = format!("SELECT m.id, m.date, {value} FROM messages m WHERE m.id IN ({})", chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(","));
                let mut stmt = self.conn.prepare(&sql)?;
                for row in stmt.query_map([], read)? {
                    keyed.push(row?);
                }
            }
        }
        // (unstable is faster, and nothing is equal: ties go by date, then id)
        keyed.sort_unstable_by(|(a, ad, ai), (b, bd, bi)| {
            let order = match (a, b) {
                (Value::Number(x), Value::Number(y)) => x.cmp(y),
                // empty values last, whichever way
                (Value::Text(x), Value::Text(y)) if (x == SORT_EMPTY) != (y == SORT_EMPTY) => return (x == SORT_EMPTY).cmp(&(y == SORT_EMPTY)),
                (Value::Text(x), Value::Text(y)) => x.cmp(y),
                _ => std::cmp::Ordering::Equal,
            };
            let order = if ascending { order } else { order.reverse() };
            // the same value: newest first
            order.then_with(|| (bd, bi).cmp(&(ad, ai)))
        });
        Ok(keyed.into_iter().map(|(_, _, id)| id).collect())
    }

    /// The cursor `skip` rows into `list_all_except_from` (a jump down the scroll bar).
    pub fn cursor_all_except_from(&self, hide_from: &[i64], skip: u64) -> Result<Option<PageCursor>> {
        if hide_from.is_empty() {
            return self.cursor_in(&[], skip);
        }
        let marks = vec!["?"; hide_from.len()].join(",");
        let sql = format!(
            "SELECT m.date, m.id FROM messages m WHERE NOT EXISTS
             (SELECT 1 FROM message_addresses a WHERE a.message = m.id AND a.role = 0 AND a.address IN ({marks}))
             ORDER BY m.date DESC, m.id DESC LIMIT 1 OFFSET ?"
        );
        let mut values: Vec<i64> = hide_from.to_vec();
        values.push(skip as i64);
        Ok(self.conn.query_row(&sql, rusqlite::params_from_iter(values), |r| Ok(PageCursor { date: r.get(0)?, id: r.get(1)? })).optional()?)
    }

    /// The folders each message is in (id, name), for showing them as labels.
    pub fn labels_of(&self, ids: &[i64]) -> Result<Vec<Vec<(i64, String)>>> {
        let mut stmt = self.conn.prepare_cached("SELECT f.id, f.name FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1 ORDER BY f.name")?;
        ids.iter()
            .map(|id| Ok(stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?))
            .collect()
    }

    /// Every account, with how many messages it holds (each once).
    pub fn accounts(&self) -> Result<Vec<AccountInfo>> {
        self.accounts_counted(true)
    }

    /// Every account; `counted` false leaves `messages` at 0 (counting reads every location:
    /// seconds on a large library, so the window counts after it's shown the mail).
    pub fn accounts_counted(&self, counted: bool) -> Result<Vec<AccountInfo>> {
        let mut counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
        if counted {
            let mut stmt = self.conn.prepare(
                "SELECT f.account, COUNT(DISTINCT l.message) FROM locations l JOIN folders f ON f.id = l.folder GROUP BY f.account",
            )?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (account, n) = row?;
                counts.insert(account, n as u64);
            }
        }
        let mut stmt = self.conn.prepare("SELECT id, kind, address, label, name, source FROM accounts ORDER BY position, id")?;
        let rows = stmt.query_map([], |r| {
            let id: i64 = r.get(0)?;
            let kind: String = r.get(1)?;
            Ok(AccountInfo {
                id,
                kind_name: db::kind_name(&kind),
                kind,
                address: r.get(2)?,
                label: r.get(3)?,
                name: r.get(4)?,
                source: r.get(5)?,
                messages: counts.get(&id).copied().unwrap_or(0),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The account's Trash (its "Deleted Items" / "Trash" folder; made when it has none).
    pub fn trash_folder(&self, account: i64) -> Result<i64> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM folders WHERE account = ?1 AND (role = 'trash' OR lower(path) IN ('deleted items', 'trash', 'deleted messages'))
                 ORDER BY role IS NULL, id LIMIT 1",
                params![account],
                |r| r.get(0),
            )
            .optional()?;
        match found {
            Some(id) => Ok(id),
            None => db::ensure_folder(&self.conn, account, "Trash"),
        }
    }

    /// The same folder on the other side of a Microsoft 365 account: "Inbox" ↔ "Online
    /// Archive/Inbox" (the window shows the two as one folder).
    pub fn archive_twin(&self, folder: i64) -> Result<Option<i64>> {
        let (account, path): (i64, String) = self.conn.query_row("SELECT account, path FROM folders WHERE id = ?1", params![folder], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let other = match path.strip_prefix("Online Archive/") {
            Some(rest) => rest.to_string(),
            None if path == "Online Archive" => return Ok(None),
            None => format!("Online Archive/{path}"),
        };
        Ok(self.conn.query_row("SELECT id FROM folders WHERE account = ?1 AND path = ?2", params![account, other], |r| r.get(0)).optional()?)
    }

    /// Moves messages from folder `from` to folder `to` (Transfer; Delete = to the Trash). In
    /// this library at once; within a Microsoft 365 account the move is also queued for the
    /// server (`pending_ops` "move", carried out by the download: `server_changes`). Moves of a
    /// message still waiting there are joined into one (Inbox → A → B = Inbox → B; moved back =
    /// nothing to do). The Online Archive is never changed on the server: moves into or out of
    /// it stay in this library. A folder shown as one with its archive twin ("Inbox" + "Online
    /// Archive/Inbox") takes a message on its own side: archive mail into the archive's, the
    /// mailbox's into the mailbox's. Between accounts it's a copy: the original stays where it
    /// is on its server. Returns how many moved.
    pub fn move_messages(&self, ids: &[i64], from: i64, to: i64) -> Result<u64> {
        // `from` and `to` may each be shown as one with an archive twin: each message moves from
        // the side it's filed on (Undo finds it there) to its own side: archive mail to the
        // archive's twin, the mailbox's to the mailbox's
        let (from_twin, to_twin) = (self.archive_twin(from)?, self.archive_twin(to)?);
        let to_is_archive = in_online_archive(&self.conn.query_row("SELECT path FROM folders WHERE id = ?1", params![to], |r| r.get::<_, String>(0))?);
        let mut stmt = self.conn.prepare_cached("SELECT 1 FROM locations WHERE message = ?1 AND folder = ?2")?;
        let mut groups: std::collections::BTreeMap<(i64, i64), Vec<i64>> = Default::default();
        for id in ids {
            let src = match from_twin {
                Some(twin) if !stmt.exists(params![id, from])? && stmt.exists(params![id, twin])? => twin,
                _ => from,
            };
            let target = match to_twin {
                Some(twin) if self.from_archive(*id)? != to_is_archive => twin,
                _ => to,
            };
            groups.entry((src, target)).or_default().push(*id);
        }
        drop(stmt);
        let mut moved = 0;
        for ((src, target), ids) in groups {
            moved += self.move_from(&ids, src, target)?;
        }
        Ok(moved)
    }

    /// Whether a message is in a Microsoft 365 account's Online Archive on the server (and not
    /// also in the mailbox itself): it stays there whatever folder it's filed in here.
    pub fn from_archive(&self, message: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM server_items s JOIN folders f ON f.id = s.folder WHERE s.message = ?1 AND f.path LIKE 'Online Archive/%')
               AND NOT EXISTS (SELECT 1 FROM server_items s JOIN folders f ON f.id = s.folder JOIN accounts a ON a.id = f.account
                               WHERE s.message = ?1 AND a.kind LIKE 'graph%' AND f.path NOT LIKE 'Online Archive/%')",
            params![message],
            |r| r.get(0),
        )?)
    }

    /// `move_messages` for messages that are in `from` itself, to `to` itself.
    fn move_from(&self, ids: &[i64], from: i64, to: i64) -> Result<u64> {
        if from == to || ids.is_empty() {
            return Ok(0);
        }
        let account_of = |folder: i64| -> Result<(i64, String, String)> {
            Ok(self.conn.query_row(
                "SELECT a.id, a.kind, f.path FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
                params![folder],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        };
        let ((from_account, kind, from_path), (to_account, _, to_path)) = (account_of(from)?, account_of(to)?);
        let same_account = from_account == to_account;
        let graph = same_account && kind.starts_with("graph");
        // IMAP accounts whose server Eudora changes (on unless the user switched it off)
        let imap = same_account && kind == "imap" && self.imap_writable()?.contains(&from_account);
        // Gmail (the Gmail API): a move there is a change of labels (`gmail_changes`)
        let gmail = same_account && kind == "gmail";
        let queued = graph || imap || gmail;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<u64> {
            let mut moved = 0;
            for id in ids {
                let server_id: Option<String> = self
                    .conn
                    .query_row("SELECT server_id FROM locations WHERE message = ?1 AND folder = ?2", params![id, from], |r| r.get(0))
                    .optional()?
                    .flatten();
                let added = self.conn.execute(
                    "INSERT OR IGNORE INTO locations (message, folder, date) SELECT id, ?2, date FROM messages WHERE id = ?1",
                    params![id, to],
                )?;
                if same_account {
                    self.conn.execute("DELETE FROM locations WHERE message = ?1 AND folder = ?2", params![id, from])?;
                }
                // a move of this message to `from` still waiting for the server
                let waiting: Option<(i64, serde_json::Value)> = if queued {
                    self.conn
                        .query_row(
                            "SELECT id, payload FROM pending_ops WHERE kind = 'move' AND json_extract(payload, '$.message') = ?1
                             AND json_extract(payload, '$.to') = ?2 ORDER BY id DESC LIMIT 1",
                            params![id, from],
                            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                        )
                        .optional()?
                        .map(|(op, payload)| Ok::<_, Error>((op, serde_json::from_str(&payload)?)))
                        .transpose()?
                } else {
                    None
                };
                let keep_here = |sid: Option<&str>| -> Result<()> {
                    if let Some(sid) = sid {
                        self.conn.execute("UPDATE locations SET server_id = ?3 WHERE message = ?1 AND folder = ?2", params![id, to, sid])?;
                    }
                    Ok(())
                };
                // filing no server holds: noted for the account's filing log (filing sync);
                // back in the folder where the server has it = the filing undone
                let note = |undone: bool| -> Result<()> {
                    let home: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM server_items WHERE folder = ?1 AND message = ?2)", params![to, id], |r| r.get(0))?;
                    let folder = if undone || home { crate::filing_sync::AS_ON_SERVER } else { to_path.as_str() };
                    self.conn.execute("INSERT INTO filing_pending (account, message, folder, at) VALUES (?1, ?2, ?3, ?4)", params![to_account, id, folder, now()])?;
                    Ok(())
                };
                if graph && (self.from_archive(*id)? || in_online_archive(&to_path)) {
                    // stays where it is on the server (archive mail never leaves the archive;
                    // the mailbox's mail isn't moved into it): only filed here, keeping its
                    // server id, and a move still waiting is called off
                    let sid = server_id.clone().or_else(|| waiting.as_ref().and_then(|(_, p)| p["server_id"].as_str().map(str::to_string)));
                    if let Some((op, _)) = &waiting {
                        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                    }
                    keep_here(sid.as_deref())?;
                    note(false)?;
                } else if queued {
                    if graph && in_online_archive(&from_path) {
                        note(true)?; // a mailbox message leaving an archive-only folder: the server moves it now
                    }
                    match (waiting, server_id) {
                        (Some((op, mut payload)), _) => {
                            if payload["from"].as_i64() == Some(to) {
                                self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // back where it is on the server
                                keep_here(payload["server_id"].as_str())?;
                            } else {
                                payload["to"] = to.into();
                                self.conn.execute("UPDATE pending_ops SET payload = ?2, attempts = 0, last_error = NULL WHERE id = ?1", params![op, payload.to_string()])?;
                            }
                        }
                        (None, Some(server_id)) => {
                            let payload = serde_json::json!({ "account": from_account, "message": id, "server_id": server_id, "from": from, "to": to });
                            self.conn.execute(
                                "INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'move', ?2)",
                                params![now(), payload.to_string()],
                            )?;
                        }
                        (None, None) => note(false)?, // not from the server (a copy Eudora made): only filed here
                    }
                } else if same_account {
                    note(false)?; // IMAP with server changes off, POP3, archives with no server
                }
                self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![id])?;
                moved += (added > 0 || same_account) as u64;
            }
            Ok(moved)
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result
    }

    /// Copy To: the messages are also in folder `to` (in this library; a copy into a server
    /// account's folder isn't sent to the server yet). Returns how many were added.
    pub fn copy_messages(&self, ids: &[i64], to: i64) -> Result<u64> {
        let mut n = 0;
        for id in ids {
            n += self.conn.execute(
                "INSERT OR IGNORE INTO locations (message, folder, date) SELECT id, ?2, date FROM messages WHERE id = ?1",
                params![id, to],
            )? as u64;
            self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![id])?;
        }
        Ok(n)
    }

    /// Changes waiting to be applied on servers (moves, …), by kind.
    pub fn pending_changes(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare("SELECT kind, COUNT(*) FROM pending_ops WHERE kind != 'draft' GROUP BY kind ORDER BY kind")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Saves a draft (JSON); `id` None = a new one. Returns its id.
    pub fn save_draft(&self, id: Option<i64>, draft: &str) -> Result<i64> {
        match id {
            Some(id) if self.conn.execute("UPDATE pending_ops SET payload = ?2, created = ?3 WHERE id = ?1 AND kind = 'draft'", params![id, draft, now()])? > 0 => Ok(id),
            _ => {
                self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'draft', ?2)", params![now(), draft])?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    /// The saved drafts (id, saved at, JSON), newest first.
    pub fn drafts(&self) -> Result<Vec<(i64, i64, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, created, payload FROM pending_ops WHERE kind = 'draft' ORDER BY created DESC")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_draft(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1 AND kind = 'draft'", params![id])?;
        Ok(())
    }

    /// Rebuilds the people for address completion from the mail: everyone the user wrote to
    /// (`mine`: the user's addresses, as senders) and everyone who wrote to them. A table of its
    /// own (written by a reader too), so typing never searches every message.
    pub fn rebuild_contacts(&self, mine: &[String]) -> Result<u64> {
        self.conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS my_addresses (address TEXT PRIMARY KEY); DELETE FROM temp.my_addresses;")?;
        for m in mine {
            self.conn.execute("INSERT OR IGNORE INTO temp.my_addresses VALUES (lower(?1))", params![m])?;
        }
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS from_me (message INTEGER PRIMARY KEY);
             DELETE FROM temp.from_me;
             INSERT OR IGNORE INTO temp.from_me
               SELECT ma.message FROM message_addresses ma JOIN addresses a ON a.id = ma.address
               WHERE ma.role = 0 AND a.address IN (SELECT address FROM temp.my_addresses);",
        )?;
        // the long part (every address of every message) into a table of this connection's own,
        // with no lock on the library: the download writes meanwhile (2026-10-18: holding the
        // write lock for all of it stopped the download for minutes after each window start)
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS contacts_new (address TEXT PRIMARY KEY, name TEXT, sent INTEGER, received INTEGER, last INTEGER);
             DELETE FROM temp.contacts_new;
             INSERT INTO temp.contacts_new (address, name, sent, received, last)
             SELECT address, MAX(name), SUM(sent), SUM(received), MAX(date) FROM (
                 SELECT a.address, ma.name, 1 AS sent, 0 AS received, m.date
                 FROM temp.from_me f JOIN message_addresses ma ON ma.message = f.message AND ma.role IN (1, 2, 3)
                 JOIN addresses a ON a.id = ma.address JOIN messages m ON m.id = f.message
                 UNION ALL
                 SELECT a.address, ma.name, 0, 1, m.date
                 FROM message_addresses ma JOIN addresses a ON a.id = ma.address JOIN messages m ON m.id = ma.message
                 WHERE ma.role = 0 AND ma.message NOT IN (SELECT message FROM temp.from_me)
             )
             WHERE address LIKE '%@%' AND address NOT IN (SELECT address FROM temp.my_addresses)
             GROUP BY address;",
        )?;
        // then the swap, quickly
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.conn.execute_batch(
            "DELETE FROM contacts;
             INSERT INTO contacts (address, name, sent, received, last) SELECT address, name, sent, received, last FROM temp.contacts_new;",
        );
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        let _ = self.conn.execute_batch("DELETE FROM temp.contacts_new;");
        result?;
        Ok(self.conn.query_row("SELECT COUNT(*) FROM contacts", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// People whose address or name starts with what's typed (any word of the name), from
    /// those the user wrote to (`sent`) and / or who wrote to them (`received`): the ones
    /// written to most first, then the most recent.
    pub fn complete_address(&self, typed: &str, sent: bool, received: bool, limit: usize) -> Result<Vec<(String, String)>> {
        let typed = typed.trim().to_lowercase().replace(['%', '_'], "");
        if typed.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<(String, String)> = Vec::new();
        if sent || received {
            let mut stmt = self.conn.prepare_cached(
                "SELECT name, address FROM contacts
                 WHERE (address LIKE ?1 || '%' OR name LIKE ?1 || '%' OR name LIKE '% ' || ?1 || '%')
                   AND ((?2 AND sent > 0) OR (?3 AND received > 0))
                 ORDER BY (CASE WHEN ?2 THEN sent ELSE 0 END) * 5 + (CASE WHEN ?3 THEN received ELSE 0 END) DESC, last DESC
                 LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![typed, sent, received, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
            out = rows.collect::<rusqlite::Result<_>>()?;
        }
        // then the Address Book
        if out.len() < limit {
            for (name, address) in self.address_book_matches(&typed, limit * 3)? {
                if out.len() >= limit {
                    break;
                }
                if !out.iter().any(|(_, a)| a.eq_ignore_ascii_case(&address)) {
                    out.push((name, address));
                }
            }
        }
        Ok(out)
    }

    /// Address Book entries whose name (any word of it) or an address starts with `typed`
    /// (lowercase): (name, address), one row per address.
    pub fn address_book_matches(&self, typed: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached("SELECT card FROM cards WHERE instr(lower(card), ?1) > 0 LIMIT 400")?;
        let cards = stmt.query_map(params![typed], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        let mut out = Vec::new();
        for json in cards {
            let Ok(card) = serde_json::from_str::<crate::cards::Card>(&json) else { continue };
            let name = card.shown();
            let name_hit = name.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(typed)) || name.to_lowercase().starts_with(typed);
            for e in &card.emails {
                let address = e.value.trim();
                if address.contains('@') && (name_hit || address.to_lowercase().starts_with(typed)) {
                    out.push((if name == address { String::new() } else { name.clone() }, address.to_string()));
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// A setting (JSON text), one item per setting group (`settings_items`, ready for syncing
    /// between computers).
    /// The memory choice (`db::MEMORY`) put in effect on this connection now.
    pub fn apply_memory(&self) -> Result<()> {
        crate::db::apply_memory(&self.conn)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings_items WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
    }

    /// Stores a setting (its version goes up by one). A setting, not mail: a reader may write it.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings_items (key, version, device, modified, value) VALUES (?1, 1, 'this', ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET version = version + 1, device = 'this', modified = ?2, value = ?3",
            params![key, now(), value],
        )?;
        Ok(())
    }

    /// The account a message belongs to (its first location's).
    pub fn message_account(&self, id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT f.account FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1 ORDER BY l.id LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Where pictures from the web are kept when the user chose to keep them.
    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    /// Adds a mailbox shared with the user (Microsoft 365) as an account of its own; downloads
    /// with `shared` bring its mail in. A setting, not mail: a reader may add it.
    pub fn add_shared_mailbox(&self, address: &str) -> Result<i64> {
        db::graph_account(&self.conn, "graph-shared", address, "", now())
    }

    /// A Microsoft account just signed in (Microsoft 365 or Outlook.com): its account, made now
    /// so it shows under Accounts at once (the download fills it; the same row if it's there).
    pub fn add_microsoft_account(&self, address: &str, name: &str) -> Result<i64> {
        db::graph_account(&self.conn, "graph", address, name, now())
    }

    /// A Google account signed in (Gmail through the Gmail API): its row, at once.
    pub fn add_google_account(&self, address: &str) -> Result<i64> {
        db::graph_account(&self.conn, "gmail", address, "", now())
    }

    /// What the user calls an account (None or "": its address again). A setting, not mail:
    /// a reader may change it while a download writes.
    pub fn set_account_label(&self, account: i64, label: Option<&str>) -> Result<()> {
        let label = label.map(str::trim).filter(|l| !l.is_empty());
        self.conn.execute("UPDATE accounts SET label = ?2 WHERE id = ?1", params![account, label])?;
        Ok(())
    }

    /// Changes the address of an account made from files (a classic Eudora archive, mailbox
    /// files): e.g. an archive that was really one of its personalities' (
    /// old Eudora files are pat@example.net, kept apart from the pat@example.org IMAP
    /// account, so rules can tell them apart). In the personalities listed in its source the
    /// new address gives way to the old one. Accounts on a server keep the address they sign
    /// in with. Returns the old address.
    pub fn set_account_address(&self, account: i64, address: &str) -> Result<String> {
        let address = address.trim().to_lowercase();
        if !address.contains('@') || address.contains(char::is_whitespace) {
            return Err(Error::Other(format!("{address} isn't an email address")));
        }
        let (kind, old, source): (String, Option<String>, Option<String>) =
            self.conn.query_row("SELECT kind, address, source FROM accounts WHERE id = ?1", params![account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if !matches!(kind.as_str(), "eudora" | "local" | "mbox") {
            return Err(Error::Other(format!("a {} account keeps the address it signs in with", db::kind_name(&kind))));
        }
        let old = old.unwrap_or_default();
        let source = source.map(|s| if old.is_empty() { s } else { s.replace(&address, &old) });
        self.conn.execute("UPDATE accounts SET address = ?2, source = ?3 WHERE id = ?1", params![account, address, source])?;
        Ok(old)
    }

    /// Gives the "imported files" account an owner (for libraries made before imports
    /// recognized classic Eudora folders): its kind, address, name and source; folders under
    /// `root` ("Eudora 2005") move to the account's top. Settings only: packs and index are
    /// untouched, so it's safe while a download writes.
    pub fn adopt_imported(&self, kind: &str, address: &str, name: &str, source: &str, root: Option<&str>) -> Result<i64> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<i64> {
            let account: i64 = self
                .conn
                .query_row("SELECT id FROM accounts WHERE kind = 'local' ORDER BY id LIMIT 1", [], |r| r.get(0))
                .optional()?
                .ok_or_else(|| Error::Other("there's no imported-files account to adopt".into()))?;
            self.conn.execute(
                "UPDATE accounts SET kind = ?2, address = ?3, name = ?4, source = ?5 WHERE id = ?1",
                params![account, kind, address, name, source],
            )?;
            if let Some(root) = root.map(|r| r.trim_matches('/')).filter(|r| !r.is_empty()) {
                let root_id: Option<i64> =
                    self.conn.query_row("SELECT id FROM folders WHERE account = ?1 AND path = ?2", params![account, root], |r| r.get(0)).optional()?;
                let cut = root.len() as i64 + 2;
                self.conn.execute(
                    "UPDATE folders SET path = substr(path, ?3) WHERE account = ?1 AND path LIKE ?2 || '/%'",
                    params![account, root, cut],
                )?;
                if let Some(root_id) = root_id {
                    self.conn.execute("UPDATE folders SET parent = NULL WHERE parent = ?1", params![root_id])?;
                    let used: i64 = self.conn.query_row("SELECT COUNT(*) FROM locations WHERE folder = ?1", params![root_id], |r| r.get(0))?;
                    if used == 0 {
                        self.conn.execute("DELETE FROM folders WHERE id = ?1", params![root_id])?;
                    }
                }
            }
            Ok(account)
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result
    }

    /// Removes a folder whose every message is also in another folder (nothing is lost: the
    /// messages stay where else they are). Their search entries are brought up to date the
    /// next time the library is opened for writing. Returns how many messages it held.
    pub fn remove_folder_of_copies(&self, folder: i64) -> Result<u64> {
        let only_here: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM locations l WHERE l.folder = ?1
             AND NOT EXISTS (SELECT 1 FROM locations o WHERE o.message = l.message AND o.folder != l.folder)",
            params![folder],
            |r| r.get(0),
        )?;
        if only_here > 0 {
            return Err(Error::Other(format!("{only_here} messages are only in this folder: it can't be removed without losing them")));
        }
        let children: i64 = self.conn.query_row("SELECT COUNT(*) FROM folders WHERE parent = ?1", params![folder], |r| r.get(0))?;
        if children > 0 {
            return Err(Error::Other("the folder has folders inside it".into()));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<u64> {
            self.conn.execute("INSERT OR IGNORE INTO index_pending (message) SELECT message FROM locations WHERE folder = ?1", params![folder])?;
            let n = self.conn.execute("DELETE FROM locations WHERE folder = ?1", params![folder])?;
            self.conn.execute("DELETE FROM server_items WHERE folder = ?1", params![folder])?;
            // the folder, then any parents left empty
            let mut next = Some(folder);
            while let Some(id) = next {
                let parent: Option<i64> = self.conn.query_row("SELECT parent FROM folders WHERE id = ?1", params![id], |r| r.get(0)).optional()?.flatten();
                let busy: i64 = self.conn.query_row(
                    "SELECT (SELECT COUNT(*) FROM locations WHERE folder = ?1) + (SELECT COUNT(*) FROM folders WHERE parent = ?1)",
                    params![id],
                    |r| r.get(0),
                )?;
                if busy > 0 {
                    break;
                }
                self.conn.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
                next = parent;
            }
            Ok(n as u64)
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result
    }

    /// The cursor `skip` rows into a folder (for benchmarks of deep pages).
    pub fn cursor_at(&self, folder: i64, skip: u64) -> Result<Option<PageCursor>> {
        Ok(self
            .conn
            .query_row(
                "SELECT date, message FROM locations WHERE folder = ?1 ORDER BY date DESC, message DESC LIMIT 1 OFFSET ?2",
                params![folder, skip as i64],
                |r| Ok(PageCursor { date: r.get(0)?, id: r.get(1)? }),
            )
            .optional()?)
    }

    /// Brings a reader up to date with what a writer (download, import) committed since it
    /// opened: the database sees new rows by itself, the search index needs this.
    pub fn refresh(&self) -> Result<()> {
        self.index.reload()
    }

    pub fn folders(&self) -> Result<Vec<FolderInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.path, COALESCE((SELECT fc.messages FROM folder_counts fc WHERE fc.folder = f.id), 0), COALESCE(a.label, a.address, a.name), a.id
             FROM folders f JOIN accounts a ON a.id = f.account ORDER BY a.position, a.id, f.path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FolderInfo { id: r.get(0)?, path: r.get(1)?, messages: r.get::<_, i64>(2)? as u64, account: r.get(3)?, account_id: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn find_folder(&self, path: &str) -> Result<Option<i64>> {
        db::find_folder(&self.conn, path)
    }

    /// Searches: words and fields in the main index, `name:value` parts for any other header
    /// in the header index (`header_index::split_query`), both together when there are both.
    pub fn search(&self, query: &str, limit: usize, order: SortOrder) -> Result<(SearchResult, Vec<MessageRow>)> {
        self.search_scoped(query, limit, order, false)
    }

    /// `search`, leaving out messages only in mailboxes left out of search (search_scope.rs)
    /// unless `everywhere` (the search box's Include all mailboxes).
    pub fn search_scoped(&self, query: &str, limit: usize, order: SortOrder, everywhere: bool) -> Result<(SearchResult, Vec<MessageRow>)> {
        let _timed = crate::speed::Timer::new("Search");
        // repairs and suggestions for what isn't quite right (search_help), then Outlook's
        // search words (hasattachment:, received:, size:…), then folder names
        let today = chrono::Local::now().date_naive();
        let help = crate::search_help::check(query, today);
        if help.fixed.trim().is_empty() {
            let result = SearchResult { total: 0, ids: Vec::new(), took: std::time::Duration::ZERO, notes: help.notes, suggestions: help.suggestions, hidden: 0 };
            return Ok((result, Vec::new()));
        }
        let without = if everywhere { None } else { self.search_hidden()? };
        let (result, rows) = self.search_checked(&help.fixed, today, limit, order, without)?;
        let mut result = result;
        let mut notes = help.notes;
        notes.append(&mut result.notes);
        result.notes = notes;
        result.suggestions = help.suggestions;
        Ok((result, rows))
    }

    fn search_checked(&self, query: &str, today: chrono::NaiveDate, limit: usize, order: SortOrder, without: Option<std::sync::Arc<crate::index::IdSet>>) -> Result<(SearchResult, Vec<MessageRow>)> {
        let query = crate::aqs::translate(query, today);
        let query = self.resolve_folders(&query)?;
        match self.search_translated(&query, limit, order, without.clone()) {
            // only exclusions ("-folder:inbox", "hasattachment:no"): the index needs something to
            // take them from, so every message
            Err(Error::Query(tantivy::query::QueryParserError::AllButQueryForbidden)) => {
                self.search_translated(&format!("date:[1970-01-01T00:00:00Z TO 2200-01-01T00:00:00Z] {query}"), limit, order, without)
            }
            other => other,
        }
    }

    fn search_translated(&self, query: &str, limit: usize, order: SortOrder, without: Option<std::sync::Arc<crate::index::IdSet>>) -> Result<(SearchResult, Vec<MessageRow>)> {
        let (rest, clauses) = crate::header_index::split_query(query);
        let result = if clauses.is_empty() {
            self.index.search_filtered(query, limit, order, None, without)?
        } else if without.is_some() {
            // headers and mailboxes left out: the header index's matches, through the main index
            // (every date when nothing else was asked for), leaving those out
            let hx = self.headers.as_ref().ok_or_else(|| Error::Other("searching by header isn't ready yet: the download prepares it (it starts the next time the download runs)".into()))?;
            hx.reload()?;
            let within = hx.matching_ids(&clauses)?;
            let words = if rest.trim().is_empty() { "date:[1970-01-01T00:00:00Z TO 2200-01-01T00:00:00Z]".to_string() } else { rest.clone() };
            self.index.search_filtered(&words, limit, order, Some(&within), without)?
        } else {
            let hx = self.headers.as_ref().ok_or_else(|| Error::Other("searching by header isn't ready yet: the download prepares it (it starts the next time the download runs)".into()))?;
            hx.reload()?;
            let started = std::time::Instant::now();
            if rest.trim().is_empty() {
                let (total, ids) = hx.search(&clauses, limit, order != SortOrder::Oldest)?;
                SearchResult { total, ids, took: started.elapsed(), notes: Vec::new(), suggestions: Vec::new(), hidden: 0 }
            } else {
                let within = hx.matching_ids(&clauses)?;
                self.index.search_within(&rest, limit, order, &within)?
            }
        };
        let ids: Vec<i64> = result.ids.iter().map(|id| *id as i64).collect();
        let rows = self.rows(&ids)?;
        Ok((result, rows))
    }

    pub fn stats(&self) -> Result<Stats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0)) };
        let db_bytes = ["eudora.db", "eudora.db-wal"]
            .iter()
            .filter_map(|n| std::fs::metadata(self.root.join(n)).ok())
            .map(|m| m.len())
            .sum();
        Ok(Stats {
            messages: one("SELECT COUNT(*) FROM messages")? as u64,
            locations: one("SELECT COUNT(*) FROM locations")? as u64,
            folders: one("SELECT COUNT(*) FROM folders")? as u64,
            raw_bytes: one("SELECT SUM(size) FROM messages")? as u64,
            pack_bytes: self.packs.disk_size()?,
            packs: self.packs.pack_ids()?.len() as u64,
            db_bytes,
            index_bytes: SearchIndex::disk_size(&self.root.join("index")),
            indexed_docs: self.index.num_docs(),
            attachments: one("SELECT COUNT(*) FROM attachments")? as u64,
            attachment_refs: one("SELECT COUNT(*) FROM attachment_refs")? as u64,
            addresses: one("SELECT COUNT(*) FROM addresses")? as u64,
            dictionary: self.packs.newest_dictionary().map(|d| d.id),
            oldest: self.conn.query_row("SELECT MIN(date) FROM messages WHERE date > 0", [], |r| r.get(0))?,
            newest: self.conn.query_row("SELECT MAX(date) FROM messages", [], |r| r.get(0))?,
        })
    }

    /// The oldest and newest message dates (from the date index: instant on any library).
    pub fn date_range(&self) -> Result<(Option<i64>, Option<i64>)> {
        Ok((
            self.conn.query_row("SELECT MIN(date) FROM messages WHERE date > 0", [], |r| r.get(0))?,
            self.conn.query_row("SELECT MAX(date) FROM messages", [], |r| r.get(0))?,
        ))
    }

    /// The paths of the folders a message is in.
    pub fn folder_paths(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1 ORDER BY f.path",
        )?;
        let paths = stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(paths)
    }

    /// Folder ids a message is in.
    pub(crate) fn message_folders(&self, id: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT folder FROM locations WHERE message = ?1 ORDER BY folder")?;
        let folders = stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(folders)
    }

    /// `catch_up_index` for the library's own maintenance (Optimize Library).
    pub(crate) fn catch_up_index_now(&self, progress: &dyn Progress) -> Result<()> {
        self.catch_up_index(progress)
    }

    /// Adds index entries the database has but the last index commit didn't cover.
    fn catch_up_index(&self, progress: &dyn Progress) -> Result<()> {
        let committed = self.index.committed_max_id()?;
        let max: i64 = self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?;
        let pending: i64 = self.conn.query_row("SELECT COUNT(*) FROM index_pending", [], |r| r.get(0))?;
        if max <= committed && pending == 0 {
            return Ok(());
        }
        let mut ids: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM messages WHERE id > ?1 UNION SELECT message FROM index_pending ORDER BY 1")?;
            stmt.query_map(params![committed], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        ids.dedup();
        let replace_from = committed;
        self.reindex_ids(&ids, |id| id <= replace_from, "Bringing the search index up to date", progress)?;
        self.index.commit(max)?;
        self.conn.execute("DELETE FROM index_pending", [])?;
        Ok(())
    }

    /// Messages stored before 2026-10-19 counted the pictures their HTML shows (logos) as
    /// attachments, so the list showed a paperclip for them: those with a picture are read
    /// again, up to `limit` a call, and their count (and search entry) set to the files really
    /// attached. Returns how many were looked at (0: done). The writer only (the download).
    pub fn recount_attachments(&self, limit: usize) -> Result<u64> {
        if self.access != Access::Write {
            return Ok(0);
        }
        let mark: i64 = self.setting("inline-count-mark")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if mark < 0 {
            return Ok(0); // done
        }
        let rows: Vec<(i64, i64, i64, Location)> = {
            let mut stmt = self.conn.prepare(
                "SELECT m.id, m.date, m.attachment_count, m.pack, m.pack_offset, m.pack_len FROM messages m
                 WHERE m.id > ?1 AND m.attachment_count > 0
                   AND EXISTS (SELECT 1 FROM attachment_refs r JOIN attachments a ON a.id = r.attachment WHERE r.message = m.id AND a.mime LIKE 'image/%')
                 ORDER BY m.id LIMIT ?2",
            )?;
            stmt.query_map(params![mark, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, Library::location(r, 3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let Some(&(last, ..)) = rows.last() else {
            self.set_setting("inline-count-mark", "-1")?;
            return Ok(0);
        };
        for (id, date, count, at) in &rows {
            let Ok(record) = self.packs.read(*at) else { continue };
            let m = parse::parse(&record.raw, parse::DateHints::default(), now());
            if m.attached() as i64 == *count || m.attachments.is_empty() {
                continue;
            }
            self.conn.execute("UPDATE messages SET attachment_count = ?2 WHERE id = ?1", params![id, m.attached() as i64])?;
            let folders = self.message_folders(*id)?;
            self.index.replace(&index_text(&m).as_doc(*id, *date, m.raw_len, m.attached() as u64, &folders))?;
        }
        self.set_setting("inline-count-mark", &last.to_string())?;
        Ok(rows.len() as u64)
    }

    /// Messages stored with no text (their plain-text version empty beside the real HTML one:
    /// before 2026-10-17 such mail had no preview and no searchable text) are read again, up to
    /// `limit` a call: their preview and search entry made from the HTML. Returns how many were
    /// looked at (0: done). The writer only (the download).
    pub fn refill_empty_text(&self, limit: usize) -> Result<u64> {
        if self.access != Access::Write {
            return Ok(0);
        }
        let mark: i64 = self.setting("refill-text-mark")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let rows: Vec<(i64, i64, Location)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, date, pack, pack_offset, pack_len FROM messages WHERE id > ?1 AND snippet = '' AND size > 0 ORDER BY id LIMIT ?2",
            )?;
            stmt.query_map(params![mark, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, Library::location(r, 2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let Some(&(last, _, _)) = rows.last() else { return Ok(0) };
        for (id, date, at) in &rows {
            let Ok(record) = self.packs.read(*at) else { continue };
            let m = parse::parse(&record.raw, parse::DateHints::default(), now());
            if m.snippet.is_empty() {
                continue; // really no text
            }
            self.conn.execute("UPDATE messages SET snippet = ?2 WHERE id = ?1", params![id, m.snippet])?;
            let folders = self.message_folders(*id)?;
            self.index.replace(&index_text(&m).as_doc(*id, *date, m.raw_len, m.attached() as u64, &folders))?;
        }
        self.set_setting("refill-text-mark", &last.to_string())?;
        Ok(rows.len() as u64)
    }

    /// Throws the index away and builds it again from the packs and the database.
    pub fn rebuild_index(&self, progress: &dyn Progress) -> Result<()> {
        if self.access != Access::Write {
            return Err(Error::Other("open the library for writing to rebuild its index".into()));
        }
        self.index.delete_all()?;
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM messages ORDER BY id")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let max = ids.last().copied().unwrap_or(0);
        self.reindex_ids(&ids, |_| false, "Rebuilding the search index", progress)?;
        self.index.commit(max)?;
        self.conn.execute("DELETE FROM index_pending", [])?;
        Ok(())
    }

    /// Reads, parses and indexes these messages again (in parallel), replacing entries when
    /// `replace(id)` says one may exist already.
    fn reindex_ids(&self, ids: &[i64], replace: impl Fn(i64) -> bool, what: &str, progress: &dyn Progress) -> Result<()> {
        let total = ids.len() as u64;
        let mut done = 0u64;
        for chunk in ids.chunks(2000) {
            if progress.canceled() {
                return Err(Error::Canceled);
            }
            progress.report(done, total, what);
            let mut stmt = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len, date FROM messages WHERE id = ?1")?;
            let mut work = Vec::with_capacity(chunk.len());
            for id in chunk {
                // a message removed meanwhile (Empty Trash): out of the index too
                let Some((location, date)) = stmt.query_row(params![id], |r| Ok((Self::location(r, 0)?, r.get::<_, i64>(3)?))).optional()? else {
                    self.index.delete(*id as u64)?;
                    continue;
                };
                work.push((*id, location, date, self.message_folders(*id)?));
            }
            let packs = &self.packs;
            let parsed: Vec<Result<(i64, i64, Vec<i64>, parse::ParsedMessage)>> = work
                .into_par_iter()
                .map(|(id, location, date, folders)| {
                    let raw = packs.read(location)?.raw;
                    Ok((id, date, folders, parse::parse(&raw, DateHints::default(), now())))
                })
                .collect();
            let mut extracted = self.conn.prepare_cached("SELECT text FROM extracted WHERE message = ?1 ORDER BY part")?;
            for item in parsed {
                let (id, date, folders, mut m) = item?;
                // text from inside its attachments (PDFs, pictures read by OCR): found by search too
                for text in extracted.query_map(params![id], |r| r.get::<_, String>(0))? {
                    m.body_text.push_str("\n\n");
                    m.body_text.push_str(&text?);
                }
                let doc = index_text(&m);
                let d = doc.as_doc(id, date, m.raw_len, m.attached() as u64, &folders);
                if replace(id) { self.index.replace(&d)? } else { self.index.add(&d)? }
            }
            done += chunk.len() as u64;
        }
        progress.report(done, total, what);
        Ok(())
    }
}

pub fn index_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).clamp(1, 8)
}

/// The text fields of an index entry, joined from a parsed message.
pub(crate) struct IndexText<'a> {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub attachments: String,
    pub m: &'a parse::ParsedMessage,
}

pub(crate) fn index_text(m: &parse::ParsedMessage) -> IndexText<'_> {
    let join = |list: &[parse::Addr]| list.iter().map(|a| a.display()).collect::<Vec<_>>().join(", ");
    IndexText {
        from: join(&m.from),
        to: join(&m.to),
        cc: join(&m.cc),
        bcc: join(&m.bcc),
        attachments: m.attachments.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join("\n"),
        m,
    }
}

impl IndexText<'_> {
    pub fn as_doc<'b>(&'b self, id: i64, date: i64, size: u64, attachments: u64, folders: &'b [i64]) -> IndexDoc<'b> {
        IndexDoc {
            id: id as u64,
            from: &self.from,
            to: &self.to,
            cc: &self.cc,
            bcc: &self.bcc,
            subject: &self.m.subject,
            body: &self.m.body_text,
            attachment_names: &self.attachments,
            date,
            size,
            attachments,
            folders,
        }
    }
}

/// A folder of a Microsoft 365 account's Online Archive (stays as it is on the server).
pub fn in_online_archive(path: &str) -> bool {
    path == "Online Archive" || path.starts_with("Online Archive/")
}

/// How a folder's path is shown: an Online Archive folder as part of the mailbox ("Online
/// Archive/Inbox_2021" → "Inbox_2021"); "" for the archive's own top (holds no mail).
pub fn shown_path(path: &str) -> &str {
    match path.strip_prefix("Online Archive/") {
        Some(rest) => rest,
        None if path == "Online Archive" => "",
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Silent;

    #[test]
    fn a_restore_over_the_open_library_takes_its_place_at_the_next_start() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Eudora 27");
        let target = app.join("Library");
        let staged = staging_for(&target);
        assert_eq!(staged.file_name().unwrap(), "Library (restoring)");
        for (dir, text) in [(&target, "old"), (&staged, "restored")] {
            std::fs::create_dir_all(dir.join("packs")).unwrap();
            std::fs::write(dir.join("eudora.db"), text).unwrap();
        }
        // nothing noted: nothing happens
        assert!(finish_replacement_in(&app).unwrap().is_none());
        note_replacement(&app, &staged, &target).unwrap();
        // the library still held (an Eudora closing): the swap waits for it
        let lock = std::fs::File::create(target.join("library.lock")).unwrap();
        lock.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(700));
            drop(lock);
        });
        assert_eq!(finish_replacement_in(&app).unwrap(), Some(target.clone()));
        release.join().unwrap();
        assert_eq!(std::fs::read_to_string(target.join("eudora.db")).unwrap(), "restored");
        assert!(!staged.exists());
        assert!(!app.join(REPLACE_FILE).exists());
        // the old library goes (on a thread of its own)
        for _ in 0..100 {
            if !app.join(REPLACED_FILE).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let left: Vec<_> = std::fs::read_dir(&app).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left, vec!["Library".to_string()]);
        // a restored copy gone missing: the library stays, said so, the note dropped
        note_replacement(&app, &staged, &target).unwrap();
        assert!(finish_replacement_in(&app).unwrap_err().to_string().contains("isn't there any more"));
        assert_eq!(std::fs::read_to_string(target.join("eudora.db")).unwrap(), "restored");
        assert!(!app.join(REPLACE_FILE).exists());
    }

    #[test]
    fn a_library_is_never_restored_into_a_sync_folder() {
        // (a backslash separates folders only on Windows: elsewhere it's part of a name)
        let windows = |_: &(&str, Option<&str>)| cfg!(windows);
        let cases = [
            ("/Users/ann/Dropbox/Mail", Some("Dropbox")),
            ("/Users/ann/Library/CloudStorage/OneDrive-Contoso/Mail", Some("OneDrive")),
            ("/Users/ann/Library/Mobile Documents/com~apple~CloudDocs/Mail", Some("iCloud Drive")),
            ("/Users/ann/Library/Application Support/Eudora 27/Library", None),
        ];
        let windows_cases = [
            (r"C:\Users\Ann\Dropbox\Ann\Eudora", Some("Dropbox")),
            (r"C:\Users\Ann\Dropbox\Mail", Some("Dropbox")),
            (r"C:\Users\Ann\OneDrive - Contoso\Mail", Some("OneDrive")),
            (r"G:\My Drive\Mail", Some("Google Drive")),
            (r"C:\Users\Ann\AppData\Local\Eudora 27\Library", None),
            (r"C:\EudoraTest\library", None),
        ];
        for (path, service) in cases.into_iter().chain(windows_cases.into_iter().filter(windows)) {
            assert_eq!(in_sync_folder(Path::new(path)), service, "{path}");
        }
    }

    #[test]
    fn a_list_is_sorted_by_any_column_either_way() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |n: u32, from: &str, to: &str, subject: &str, body: usize| {
            let p = src.join(format!("{n}.eml"));
            let text = "x".repeat(body);
            std::fs::write(&p, format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: Mon, {n} Jan 2024 10:00:00 +0000\r\nMessage-ID: <{n}@x>\r\n\r\n{text}\r\n")).unwrap();
            p
        };
        let files = vec![
            mail(1, "Zoe <zoe@x.com>", "Bob <bob@y.com>", "Re: banana", 10),
            mail(2, "\"Adam\" <adam@x.com>", "carl@y.com", "apple", 3000),
            mail(3, "Mia <mia@x.com>", "Al <al@y.com>", "", 500),
        ];
        crate::import(&mut library, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        let ids: Vec<i64> = library.list_ids(&[], &[]).unwrap().into_iter().map(|(id, _)| id).collect();
        let subject = |id: i64| library.rows(&[id]).unwrap()[0].subject.clone();
        let order = |key: &str, asc: bool| library.sort_ids(&ids, key, asc).unwrap().into_iter().map(subject).collect::<Vec<_>>();
        // "Re:" left out of the order; no subject last, either way
        assert_eq!(order("subject", true), vec!["apple", "Re: banana", ""]);
        assert_eq!(order("subject", false), vec!["Re: banana", "apple", ""]);
        assert_eq!(order("from", true), vec!["apple", "", "Re: banana"], "Adam (quotes left out), Mia, Zoe");
        assert_eq!(order("to", true), vec!["", "Re: banana", "apple"], "Al, Bob, carl@y.com");
        assert_eq!(order("size", false), vec!["apple", "", "Re: banana"]);
        assert_eq!(order("date", true), vec!["Re: banana", "apple", ""]);
        assert!(library.sort_ids(&ids, "folders", true).is_err());
        // a list of some of them (read in one pass over every message when it's big)
        let two = vec![ids[0], ids[2]];
        assert_eq!(library.sort_ids(&two, "subject", true).unwrap().len(), 2);
    }
}

#[cfg(test)]
mod recent_mail_tests {
    use super::*;
    use crate::progress::Silent;

    #[test]
    fn recent_mail_is_what_was_sorted_away_lately_without_spam() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // 1-5 this year, 6 long ago
        let files: Vec<_> = (1..=6)
            .map(|n| {
                let p = src.join(format!("{n}.eml"));
                let year = if n == 6 { 2001 } else { 2026 };
                std::fs::write(&p, format!("From: a{n}@x.com
Subject: s{n}
Date: Mon, {n} Nov {year} 10:00:00 +0000
Message-ID: <{n}@x>

hi
")).unwrap();
                p
            })
            .collect();
        crate::import(&mut library, &files, &crate::ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let id = |n: u32| -> i64 { library.conn.query_row("SELECT id FROM messages WHERE subject = ?1", [format!("s{n}")], |r| r.get(0)).unwrap() };
        let (m1, m2, m3, m4, m5, m6) = (id(1), id(2), id(3), id(4), id(5), id(6));
        let inbox: i64 = library.conn.query_row("SELECT folder FROM locations LIMIT 1", [], |r| r.get(0)).unwrap();
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        let folder = |name: &str| crate::db::ensure_folder(&library.conn, account, name).unwrap();
        let (marketing, spam, clients) = (folder("Bulk/Marketing"), folder("Spam"), folder("Clients"));
        library.move_messages(&[m1, m6], inbox, marketing).unwrap(); // sorted into Bulk
        library.move_messages(&[m2], inbox, spam).unwrap(); // spam
        library.move_messages(&[m3], inbox, clients).unwrap(); // filed by the filter's rule
        let judge = |m: i64, category: &str| {
            library.conn.execute("INSERT INTO triage (message, category, reasons, decided, applied) VALUES (?1, ?2, '[]', 0, 1)", params![m, category]).unwrap();
        };
        judge(m2, "spam");
        judge(m3, "rule");
        judge(m4, "bulk"); // judged bulk, but put back in the Inbox: not sorted away
        // m5 stays in the Inbox untouched
        let since = 1_761_955_200; // 2025-11-01
        let ids: Vec<i64> = library.list_recent(since, &[], None, 50).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![m3, m1]);
        assert_eq!(library.count_recent(since, &[]).unwrap(), 2);
        assert_eq!(library.list_ids_recent(since, &[]).unwrap().len(), 2);
        // a page after the first row
        let first = library.cursor_recent(since, &[], 0).unwrap().unwrap();
        assert_eq!(library.list_recent(since, &[], Some(first), 50).unwrap().into_iter().map(|r| r.id).collect::<Vec<_>>(), vec![m1]);
        // the user's own mail left out
        let sender: i64 = library.conn.query_row("SELECT address FROM message_addresses WHERE message = ?1 AND role = 0", [m1], |r| r.get(0)).unwrap();
        assert_eq!(library.count_recent(since, &[sender]).unwrap(), 1);
        let _ = (m5, m6);
    }
}

#[cfg(test)]
mod folder_count_tests {
    use super::*;
    use crate::progress::Silent;

    #[test]
    fn folder_counts_follow_every_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files: Vec<_> = (1..=3)
            .map(|n| {
                let p = src.join(format!("{n}.eml"));
                std::fs::write(&p, format!("From: a@x.com\r\nSubject: s{n}\r\nDate: Mon, {n} Jan 2024 10:00:00 +0000\r\nMessage-ID: <{n}@x>\r\n\r\nhi\r\n")).unwrap();
                p
            })
            .collect();
        crate::import(&mut library, &files, &crate::ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let ids: Vec<i64> = library.list_ids(&[], &[]).unwrap().into_iter().map(|(id, _)| id).collect();
        let inbox: i64 = library.conn.query_row("SELECT folder FROM locations LIMIT 1", [], |r| r.get(0)).unwrap();
        let counts = |l: &Library, f: i64| -> (i64, i64) { l.conn.query_row("SELECT messages, unread FROM folder_counts WHERE folder = ?1", [f], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or((0, 0)) };
        let recount = |l: &Library, f: i64| -> (i64, i64) {
            l.conn.query_row("SELECT COUNT(*), COALESCE(SUM(m.flags & 1), 0) FROM locations lo JOIN messages m ON m.id = lo.message WHERE lo.folder = ?1", [f], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(counts(&library, inbox), recount(&library, inbox));
        // read / unread
        library.set_status(&ids, crate::marks::Status::Unread).unwrap();
        assert_eq!(counts(&library, inbox), (3, 3));
        library.set_status(&ids[..1], crate::marks::Status::Read).unwrap();
        assert_eq!(counts(&library, inbox), (3, 2));
        // moved to another folder: both follow
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        let other = crate::db::ensure_folder(&library.conn, account, "Other").unwrap();
        library.move_messages(&ids[1..2], inbox, other).unwrap();
        assert_eq!((counts(&library, inbox), counts(&library, other)), ((2, 1), (1, 1)));
        assert_eq!((counts(&library, inbox), counts(&library, other)), (recount(&library, inbox), recount(&library, other)));
        // a location removed
        library.conn.execute("DELETE FROM locations WHERE folder = ?1 AND message = ?2", [other, ids[1]]).unwrap();
        assert_eq!(counts(&library, other), (0, 0));
        assert_eq!(library.unread_by_folder().unwrap(), vec![(inbox, 1)]);
    }
}
