//! Eudora 27's window. For now a read-only viewer of a library: folders, message lists (pages
//! by keyset, only the rows on screen), search and messages, to check imported and converted
//! mail. The front end is `ui/` (plain HTML / CSS / JS); these commands are all it calls.
//!
//! The library comes from the first argument or `EUDORA_LIBRARY`.
//!
//! A library (`run`), so the same window is the desktop program (`main.rs`) and the phone apps
//! (Tauri's mobile builds). What only a
//! desktop has (the menu bar, the tray, dragging files out) is `#[cfg(desktop)]`.

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "ios")]
mod ios;
mod api_host;
mod background;
mod opening;
#[cfg(desktop)]
mod tray;
use eudora_core::firewall;

/// Changes when the icon files do (build.rs), so a new icon is compiled into the program.
const _ICON_SUM: &str = env!("EUDORA_ICON_SUM");

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

use eudora_core::{Access, Library, MessageRow, PageCursor, Silent, SortOrder};
use serde::Serialize;

/// Message bytes shown by "Show Source", at most (the rest is cut, and says so).
const SOURCE_LIMIT: usize = 4 * 1024 * 1024;

struct App {
    path: Option<PathBuf>,
    library: Mutex<Option<Library>>,
    problem: Mutex<Option<String>>,
    /// The library is still being opened (in the background, so the window shows at once);
    /// `with` waits for it.
    opening: Mutex<bool>,
    opened: Condvar,
    /// The last row of each page loaded, (folder, page) → cursor, so the next page continues
    /// from it without counting rows.
    page_ends: Mutex<HashMap<(String, u64), PageCursor>>,
    /// The user's own addresses' ids (for hiding what they sent from All Mail), looked up
    /// again after a minute (an account may have been added).
    own: Mutex<Option<(std::time::Instant, Vec<i64>)>>,
    /// A second connection, opened on first use, for the slow questions (accounts' counts, the
    /// Views', statistics, subscriptions, the address book), so a click on a folder or a
    /// message never waits behind one of them (2026-10-18: switching folders took seconds).
    slow: Mutex<Option<Library>>,
    /// A third connection for the message being read only: the preview never waits behind a list
    /// page, a count, a sort or the read-ahead on the other two.
    reader: Mutex<Option<Library>>,
    /// A fifth for the message lists a click opens: the rows of a folder, a View or All Mail
    /// never wait behind the folder refresh, counts, marks or moves on the main connection.
    lister: Mutex<Option<Library>>,
    /// A fourth for searches only: never behind the folder refresh, a list page or a sort; its
    /// index reloaded at most every 10 s (new mail).
    searcher: Mutex<Option<(Library, std::time::Instant)>>,
    /// The spam filter deciding for many messages: its count and Stop (status bar, Activity).
    sort_job: Mutex<Option<std::sync::Arc<eudora_core::progress::Job>>>,
    /// The open list sorted by a column (its key: the list, the column, the way, its count),
    /// so each page after the first is read from it at once.
    sorted: Mutex<Option<SortedList>>,
    /// One sort at a time: the pages asked for meanwhile wait for it, then read from it.
    sorting: tauri::async_runtime::Mutex<()>,
}

impl App {
    fn own_ids(&self) -> Reply<Vec<i64>> {
        let mut own = self.own.lock().map_err(text)?;
        if let Some((at, ids)) = own.as_ref() {
            if at.elapsed() < std::time::Duration::from_secs(60) {
                return Ok(ids.clone());
            }
        }
        let ids = self.with(|l| l.address_ids(&l.own_addresses()?))?;
        *own = Some((std::time::Instant::now(), ids.clone()));
        Ok(ids)
    }
}

type Reply<T> = Result<T, String>;

fn text<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl App {
    fn with<T>(&self, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        self.wait_opened();
        let guard = self.library.lock().map_err(text)?;
        let library = guard.as_ref().ok_or_else(|| self.problem().unwrap_or_else(|| "No library is open.".into()))?;
        f(library).map_err(text)
    }

    /// `with` on whichever connection is free this moment (the main one, else the second),
    /// for a quick read the user is waiting on, e.g. a reply's window; when both are busy,
    /// the main one.
    fn with_free<T>(&self, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        self.wait_opened();
        if let Ok(guard) = self.library.try_lock() {
            let library = guard.as_ref().ok_or_else(|| self.problem().unwrap_or_else(|| "No library is open.".into()))?;
            return f(library).map_err(text);
        }
        if let (Some(path), Ok(mut guard)) = (&self.path, self.slow.try_lock()) {
            if guard.is_none() {
                *guard = Some(Library::open(path, Access::Read, &Silent).map_err(text)?);
            }
            return f(guard.as_ref().expect("opened just now")).map_err(text);
        }
        self.with(f)
    }

    /// `with` on the second connection (the slow questions).
    fn with_slow<T>(&self, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        self.wait_opened();
        if let Some(problem) = self.problem() {
            return Err(problem);
        }
        let Some(path) = &self.path else { return Err("No library is open.".into()) };
        let mut guard = self.slow.lock().map_err(text)?;
        if guard.is_none() {
            *guard = Some(Library::open(path, Access::Read, &Silent).map_err(text)?);
        }
        f(guard.as_ref().expect("opened just now")).map_err(text)
    }

    /// `with` on the searching connection (searches only), its index brought up to date when it
    /// hasn't been for 10 s.
    fn with_searcher<T>(&self, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        self.wait_opened();
        if let Some(problem) = self.problem() {
            return Err(problem);
        }
        let Some(path) = &self.path else { return Err("No library is open.".into()) };
        let mut guard = self.searcher.lock().map_err(text)?;
        if guard.is_none() {
            *guard = Some((Library::open(path, Access::Read, &Silent).map_err(text)?, std::time::Instant::now()));
        }
        let (library, fresh) = guard.as_mut().expect("opened just now");
        if fresh.elapsed() >= std::time::Duration::from_secs(10) {
            library.refresh().map_err(text)?;
            *fresh = std::time::Instant::now();
        }
        f(library).map_err(text)
    }

    /// `with` on the reading connection (the message shown, nothing else).
    /// `f` on a read-only connection of its own, opened for it: a long job (writing hundreds of
    /// messages out) that no click ever waits behind.
    fn with_own<T>(&self, f: impl FnOnce(&Library) -> Reply<T>) -> Reply<T> {
        self.wait_opened();
        if let Some(problem) = self.problem() {
            return Err(problem);
        }
        let Some(path) = &self.path else { return Err("No library is open.".into()) };
        let library = Library::open(path, Access::Read, &Silent).map_err(text)?;
        f(&library)
    }

    fn with_reader<T>(&self, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        self.wait_opened();
        if let Some(problem) = self.problem() {
            return Err(problem);
        }
        let Some(path) = &self.path else { return Err("No library is open.".into()) };
        let mut guard = self.reader.lock().map_err(text)?;
        if guard.is_none() {
            *guard = Some(Library::open(path, Access::Read, &Silent).map_err(text)?);
        }
        f(guard.as_ref().expect("opened just now")).map_err(text)
    }

    /// `with` on the listing connection (the rows of the list shown, nothing else).
    fn with_lister<T>(&self, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        self.wait_opened();
        if let Some(problem) = self.problem() {
            return Err(problem);
        }
        let Some(path) = &self.path else { return Err("No library is open.".into()) };
        let mut guard = self.lister.lock().map_err(text)?;
        if guard.is_none() {
            *guard = Some(Library::open(path, Access::Read, &Silent).map_err(text)?);
        }
        f(guard.as_ref().expect("opened just now")).map_err(text)
    }

    /// A list's rows: on the listing connection (a click), or the background one (the warm-up).
    fn with_list<T>(&self, background: bool, f: impl FnOnce(&Library) -> eudora_core::Result<T>) -> Reply<T> {
        if background { self.with_slow(f) } else { self.with_lister(f) }
    }

    /// Waits until the library has been opened (or failed to open).
    fn wait_opened(&self) {
        let mut opening = self.opening.lock().unwrap_or_else(|e| e.into_inner());
        while *opening {
            opening = self.opened.wait(opening).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn problem(&self) -> Option<String> {
        self.problem.lock().ok().and_then(|p| p.clone())
    }

    /// Opens the library (run on a thread of its own at startup).
    fn open(&self) {
        if let Some(path) = &self.path {
            // the first run (a new or empty folder): the library is made, then opened as always
            // (the window only reads it; the download writes)
            if !path.join("eudora.db").exists() {
                if let Err(e) = Library::open(path, Access::Write, &Silent).and_then(|l| l.close(&Silent)) {
                    *self.problem.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("Couldn't make the library in {}: {e}", path.display()));
                }
            }
            match Library::open(path, Access::Read, &Silent) {
                Ok(l) => {
                    // every Microsoft account signed in on this computer has its account (one
                    // signed in elsewhere, or before this was done at sign-in, shows at once)
                    for address in eudora_core::graph::GraphClient::signed_in_accounts() {
                        let _ = l.add_microsoft_account(&address, "");
                    }
                    // shared mailboxes start inactive: none is downloaded until marked active
                    if l.setting(eudora_core::graph_sync::SHARED_ACTIVE).ok().flatten().is_none() {
                        let _ = l.set_setting(eudora_core::graph_sync::SHARED_ACTIVE, "[]");
                    }
                    *self.library.lock().unwrap_or_else(|e| e.into_inner()) = Some(l)
                }
                Err(e) => *self.problem.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("Couldn't open the library: {e}")),
            }
        }
        *self.opening.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.opened.notify_all();
        // the reading connection opened now, in the background, so the first preview doesn't wait for it
        let _ = self.with_reader(|_| Ok(()));
        let _ = self.with_searcher(|_| Ok(()));
    }
}

#[derive(Serialize)]
struct Info {
    path: Option<String>,
    problem: Option<String>,
    oldest: Option<i64>,
    newest: Option<i64>,
    /// Said once after a start that put a restored library in place of this one (the window
    /// then forgets what it showed of the old one) …
    replaced: Option<String>,
    /// … or couldn't (the old library opens, untouched).
    replace_failed: Option<String>,
}

/// What this start did with a restore waiting to replace the library (`finish_replacement`).
static REPLACEMENT: Mutex<Option<Result<String, String>>> = Mutex::new(None);

#[tauri::command]
async fn info(app: tauri::State<'_, App>) -> Reply<Info> {
    // only what the window shows at once: the full `stats` reads the whole database (30 s on
    // a large library from a cold disk, with the window blank meanwhile)
    let (oldest, newest) = app.with(|l| l.date_range()).unwrap_or_default();
    let replacement = REPLACEMENT.lock().ok().and_then(|mut r| r.take());
    let (replaced, replace_failed) = match replacement {
        Some(Ok(done)) => (Some(done), None),
        Some(Err(why)) => (None, Some(why)),
        None => (None, None),
    };
    Ok(Info { path: app.path.as_ref().map(|p| p.display().to_string()), problem: app.problem(), oldest, newest, replaced, replace_failed })
}

#[derive(Serialize)]
struct Folder {
    id: i64,
    path: String,
    messages: u64,
    account: String,
    account_id: i64,
}

/// The folders with their counts. `refresh`: first catch up with what a download or import
/// committed since (the search index; also forgets where pages ended, as rows may have moved).
#[tauri::command]
async fn folders(handle: tauri::AppHandle, refresh: Option<bool>) -> Reply<Vec<Folder>> {
    use tauri::Manager;
    let app = handle.state::<App>();
    let _timed = eudora_core::speed::Timer::new("Folder list");
    // (no search index reload here any more: lists don't use it, and searches reload their own
    // on their own connection; it held every click behind it while the library was busy)
    if refresh.unwrap_or(false) {
        app.page_ends.lock().map_err(text)?.clear();
    }
    // the 10-second refresh on the background connection: a click on a list never waits behind
    // it
    let list = if refresh.unwrap_or(false) {
        let handle = handle.clone();
        tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.folders())).await.map_err(text)??
    } else {
        app.with(|l| l.folders())?
    };
    Ok(list
        .into_iter()
        .map(|f| Folder { id: f.id, path: f.path, messages: f.messages, account: f.account, account_id: f.account_id })
        .collect())
}

#[derive(Serialize)]
struct Row {
    id: i64,
    date: i64,
    from: String,
    subject: String,
    snippet: String,
    size: u64,
    attachments: u64,
    /// The folders it's in (id, name): shown as labels.
    labels: Vec<(i64, String)>,
    to: String,
    cc: String,
    message_id: String,
    list_id: Option<String>,
    added: i64,
    /// Status and priority (`marks`).
    flags: i64,
    /// Its labels (Outlook categories).
    categories: Vec<String>,
}

fn rows_with_labels(l: &Library, rows: Vec<MessageRow>) -> eudora_core::Result<Vec<Row>> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let labels = l.labels_of(&ids)?;
    let recipients = l.recipients_of(&ids)?;
    Ok(rows
        .into_iter()
        .zip(labels)
        .zip(recipients)
        .map(|((r, labels), (to, cc))| Row {
            id: r.id,
            date: r.date,
            from: r.from,
            subject: r.subject,
            snippet: r.snippet,
            size: r.size,
            attachments: r.attachments,
            labels,
            to,
            cc,
            message_id: r.message_id,
            list_id: r.list_id,
            added: r.added,
            flags: r.flags,
            categories: r.labels,
        })
        .collect())
}

/// A key for a list: its folders ("" = All Mail).
fn list_key(folders: &[i64]) -> String {
    folders.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// One page of a list, newest first: one folder, several (a label: the same name in every
/// account) or none (All Mail). Page n continues from page n-1's last row when that page
/// was loaded; otherwise (a jump down the scroll bar) it counts rows once to find it.
#[tauri::command]
async fn page(app: tauri::State<'_, App>, folders: Vec<i64>, page: u64, size: usize, hide_sent: Option<bool>, view: Option<String>, accounts: Option<Vec<i64>>, filtered: Option<bool>, recent: Option<i64>, background: Option<bool>) -> Reply<Vec<Row>> {
    // (`background`: the window's warm-up of lists not open yet, on the background connection)
    let bg = background.unwrap_or(false);
    let _timed = eudora_core::speed::Timer::new("A page of a message list");
    let size = size.clamp(1, 1000);
    if let Some(since) = recent {
        // Recent Mail: what was sorted away lately, not spam
        let hide = app.own_ids()?;
        let key = format!("recent:{since}");
        let known = if page == 0 { None } else { app.page_ends.lock().map_err(text)?.get(&(key.clone(), page - 1)).copied() };
        let rows = app.with_list(bg, |l| {
            let after = match (page, known) {
                (0, _) => None,
                (_, Some(c)) => Some(c),
                (_, None) => match l.cursor_recent(since, &hide, page * size as u64 - 1)? {
                    Some(c) => Some(c),
                    None => return Ok(Vec::new()),
                },
            };
            rows_with_labels(l, l.list_recent(since, &hide, after, size)?)
        })?;
        if let Some(last) = rows.last() {
            app.page_ends.lock().map_err(text)?.insert((key, page), PageCursor { date: last.date, id: last.id });
        }
        return Ok(rows);
    }
    if filtered.unwrap_or(false) {
        // All Mail - Filtered: what the filter leaves for the Inbox, everywhere
        let hide = app.own_ids()?;
        let key = "filtered".to_string();
        let known = if page == 0 { None } else { app.page_ends.lock().map_err(text)?.get(&(key.clone(), page - 1)).copied() };
        let rows = app.with_list(bg, |l| {
            let after = match (page, known) {
                (0, _) => None,
                (_, Some(c)) => Some(c),
                (_, None) => match l.cursor_filtered(&hide, page * size as u64 - 1)? {
                    Some(c) => Some(c),
                    None => return Ok(Vec::new()),
                },
            };
            rows_with_labels(l, l.list_filtered(&hide, after, size)?)
        })?;
        if let Some(last) = rows.last() {
            app.page_ends.lock().map_err(text)?.insert((key, page), PageCursor { date: last.date, id: last.id });
        }
        return Ok(rows);
    }
    if let Some(view) = view {
        // a Synthetic View: in these mailboxes, or the ones chosen for the combined Views
        let accounts = match accounts {
            Some(a) => a,
            None => app.with_list(bg, |l| l.combined_view_accounts())?,
        };
        let key = format!("view:{view}:{accounts:?}");
        let known = if page == 0 { None } else { app.page_ends.lock().map_err(text)?.get(&(key.clone(), page - 1)).copied() };
        let rows = app.with_list(bg, |l| {
            let after = match (page, known) {
                (0, _) => None,
                (_, Some(c)) => Some(c),
                (_, None) => match l.cursor_view(&view, &accounts, page * size as u64 - 1)? {
                    Some(c) => Some(c),
                    None => return Ok(Vec::new()),
                },
            };
            rows_with_labels(l, l.list_view(&view, &accounts, after, size)?)
        })?;
        if let Some(last) = rows.last() {
            app.page_ends.lock().map_err(text)?.insert((key, page), PageCursor { date: last.date, id: last.id });
        }
        return Ok(rows);
    }
    // All Mail without what the user sent (View ▸ Hide Mail I Sent in All Mail)
    let hide: Vec<i64> = if folders.is_empty() && hide_sent.unwrap_or(false) { app.own_ids()? } else { Vec::new() };
    let key = format!("{}{}", list_key(&folders), if hide.is_empty() { "" } else { "/received" });
    let known = if page == 0 { None } else { app.page_ends.lock().map_err(text)?.get(&(key.clone(), page - 1)).copied() };
    let rows = app.with_list(bg, |l| {
        let after = match (page, known) {
            (0, _) => None,
            (_, Some(c)) => Some(c),
            (_, None) => match if hide.is_empty() { l.cursor_in(&folders, page * size as u64 - 1)? } else { l.cursor_all_except_from(&hide, page * size as u64 - 1)? } {
                Some(c) => Some(c),
                None => return Ok(Vec::new()),
            },
        };
        let rows = if hide.is_empty() { l.list(&folders, after, size)? } else { l.list_all_except_from(&hide, after, size)? };
        rows_with_labels(l, rows)
    })?;
    if let Some(last) = rows.last() {
        app.page_ends.lock().map_err(text)?.insert((key, page), PageCursor { date: last.date, id: last.id });
    }
    Ok(rows)
}

/// How many messages a list holds (each once).
#[tauri::command]
async fn count(handle: tauri::AppHandle, folders: Vec<i64>, hide_sent: Option<bool>, view: Option<String>, accounts: Option<Vec<i64>>, filtered: Option<bool>, recent: Option<i64>) -> Reply<u64> {
    use tauri::Manager;
    let app = handle.state::<App>();
    if let Some(since) = recent {
        let hide = app.own_ids()?;
        let handle = handle.clone();
        return tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.count_recent(since, &hide))).await.map_err(text)?;
    }
    if filtered.unwrap_or(false) {
        // reads every message: on the second connection, so the window never waits for it
        let hide = app.own_ids()?;
        let handle = handle.clone();
        return tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.count_filtered(&hide))).await.map_err(text)?;
    }
    if let Some(view) = view {
        // Views are recounted on a timer: on the second connection, never in a click's way
        let handle = handle.clone();
        return tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| {
            let accounts = match &accounts {
                Some(a) => a.clone(),
                None => l.combined_view_accounts()?,
            };
            l.count_view(&view, &accounts)
        })).await.map_err(text)?;
    }
    if folders.is_empty() && hide_sent.unwrap_or(false) {
        let own = app.own_ids()?;
        return app.with(|l| l.count_all_except_from(&own));
    }
    app.with(|l| l.count(&folders))
}

/// Every message in a list with a folder it's in (Select All on a list bigger than what's loaded).
#[tauri::command]
async fn list_ids(app: tauri::State<'_, App>, folders: Vec<i64>, hide_sent: Option<bool>, view: Option<String>, accounts: Option<Vec<i64>>, filtered: Option<bool>, recent: Option<i64>) -> Reply<Vec<(i64, i64)>> {
    if let Some(since) = recent {
        let hide = app.own_ids()?;
        return app.with(|l| l.list_ids_recent(since, &hide));
    }
    if filtered.unwrap_or(false) {
        let hide = app.own_ids()?;
        return app.with(|l| l.list_ids_filtered(&hide));
    }
    if let Some(view) = view {
        return app.with(|l| {
            let accounts = match &accounts {
                Some(a) => a.clone(),
                None => l.combined_view_accounts()?,
            };
            l.view_ids(&view, &accounts)
        });
    }
    let hide: Vec<i64> = if folders.is_empty() && hide_sent.unwrap_or(false) { app.own_ids()? } else { Vec::new() };
    app.with(|l| l.list_ids(&folders, &hide))
}

/// The open list sorted by a column, kept for its pages.
struct SortedList {
    /// The list, the column and the way.
    key: String,
    /// Its count when sorted, and when.
    total: u64,
    at: std::time::Instant,
    ids: std::sync::Arc<Vec<i64>>,
}

/// A page of a list sorted by a column. The whole list is sorted once (`list_ids`, then
/// `Library::sort_ids`) and kept for its pages, until the list, the column, the way or its count
/// changes. `total`: the count the window has (a different count sorts again: new mail).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn sorted_page(
    app: tauri::State<'_, App>,
    folders: Vec<i64>,
    hide_sent: Option<bool>,
    view: Option<String>,
    accounts: Option<Vec<i64>>,
    filtered: Option<bool>,
    recent: Option<i64>,
    sort: String,
    ascending: bool,
    total: u64,
    page: u64,
    size: usize,
) -> Reply<Vec<Row>> {
    let _timed = eudora_core::speed::Timer::new("A page of a sorted list");
    let size = size.clamp(1, 1000);
    let key = format!("{folders:?}|{hide_sent:?}|{view:?}|{accounts:?}|{filtered:?}|{recent:?}|{sort}|{ascending}");
    let _one_at_a_time = app.sorting.lock().await;
    // sorted again when the count changed (new mail); a big list (All Mail: seconds to sort) at
    // most every 5 minutes, so mail arriving doesn't sort millions every 10 seconds
    let kept = app.sorted.lock().map_err(text)?.as_ref().filter(|s| {
        s.key == key && (s.total == total || (s.ids.len() > 100_000 && s.at.elapsed() < std::time::Duration::from_secs(300)))
    }).map(|s| s.ids.clone());
    let ids = match kept {
        Some(ids) => ids,
        None => {
            let listed = list_ids(app.clone(), folders, hide_sent, view, accounts, filtered, recent).await?;
            let ids: Vec<i64> = listed.into_iter().map(|(id, _)| id).collect();
            let sorted = std::sync::Arc::new(app.with_free(|l| l.sort_ids(&ids, &sort, ascending))?);
            *app.sorted.lock().map_err(text)? = Some(SortedList { key, total, at: std::time::Instant::now(), ids: sorted.clone() });
            sorted
        }
    };
    let start = (page as usize).saturating_mul(size).min(ids.len());
    let slice = ids[start..(start + size).min(ids.len())].to_vec();
    app.with_free(|l| rows_with_labels(l, l.rows(&slice)?))
}

/// The values of these headers for these messages (the list's header columns: asked for the
/// rows on screen only, since each message is read for them).
#[tauri::command]
async fn header_values(app: tauri::State<'_, App>, ids: Vec<i64>, names: Vec<String>) -> Reply<Vec<Vec<String>>> {
    app.with(|l| l.header_values(&ids, &names))
}

#[derive(Serialize)]
struct Account {
    id: i64,
    kind: String,
    kind_name: String,
    address: Option<String>,
    label: Option<String>,
    name: String,
    source: Option<String>,
    messages: u64,
    shown: String,
}

#[derive(Serialize)]
struct Accounts {
    accounts: Vec<Account>,
    /// Every message in the library, each once.
    total: u64,
}

#[tauri::command]
async fn accounts(handle: tauri::AppHandle, counts: Option<bool>) -> Reply<Accounts> {
    use tauri::Manager;
    // counts: false = at once, without messages counts (the window fills them in after); with
    // them: the slow connection, on a thread of its own
    let counted = counts.unwrap_or(true);
    tauri::async_runtime::spawn_blocking(move || accounts_now(&handle.state::<App>(), counted)).await.map_err(text)?
}

fn accounts_now(app: &App, counted: bool) -> Reply<Accounts> {
    let run = |l: &Library| -> eudora_core::Result<Accounts> {
        let accounts = l
            .accounts_counted(counted)?
            .into_iter()
            .map(|a| Account {
                shown: a.shown(),
                id: a.id,
                kind_name: a.kind_name.to_string(),
                kind: a.kind,
                address: a.address,
                label: a.label,
                name: a.name,
                source: a.source,
                messages: a.messages,
            })
            .collect();
        Ok(Accounts { accounts, total: if counted { l.count(&[])? } else { 0 } })
    };
    if counted { app.with_slow(run) } else { app.with(run) }
}

/// The user's own name for an account ("" = its address again).
#[tauri::command]
async fn set_account_label(app: tauri::State<'_, App>, id: i64, label: String) -> Reply<()> {
    app.with(|l| l.set_account_label(id, Some(&label)))
}

#[derive(Serialize)]
struct Found {
    total: usize,
    millis: f64,
    rows: Vec<Row>,
    /// A search by header covers this much of the library so far (it's prepared in the background).
    header_coverage: Option<eudora_core::header_index::Coverage>,
    /// What was repaired or left out of the search.
    notes: Vec<String>,
    /// Corrected searches to offer ("Did you mean from:bob?").
    suggestions: Vec<eudora_core::search_help::Suggestion>,
    /// Matches left out: only in mailboxes left out of search.
    hidden: usize,
}

#[tauri::command]
async fn search(handle: tauri::AppHandle, query: String, limit: usize, order: String, everywhere: Option<bool>) -> Reply<Found> {
    use tauri::Manager;
    let order = match order.as_str() {
        "oldest" => SortOrder::Oldest,
        "relevance" => SortOrder::Relevance,
        _ => SortOrder::Newest,
    };
    // (its own connection and thread: never behind the folder refresh, a list page or a sort)
    let (result, rows, header_coverage) = tauri::async_runtime::spawn_blocking(move || {
        handle.state::<App>().with_searcher(|l| {
            let (result, rows) = l.search_scoped(&query, limit.clamp(1, 5000), order, everywhere.unwrap_or(false))?;
            let by_header = eudora_core::header_index::uses_headers(&query);
            let coverage = if by_header { Some(l.header_coverage()?) } else { None };
            Ok((result, rows_with_labels(l, rows)?, coverage))
        })
    })
    .await
    .map_err(text)??;
    Ok(Found { total: result.total, millis: result.took.as_secs_f64() * 1000.0, rows, header_coverage, notes: result.notes, suggestions: result.suggestions, hidden: result.hidden })
}

/// Which mailboxes searches leave out (search_scope.rs): the switches turned and the folders
/// left out, for the magnifying glasses in the folder tree.
#[tauri::command]
async fn search_scope_get(app: tauri::State<'_, App>) -> Reply<eudora_core::search_scope::ScopeState> {
    app.with_reader(|l| l.search_scope_state())
}

/// Works out, on the search's own connection, which messages searches leave out, so the next
/// search doesn't wait for it (after the window starts, and after a switch is turned).
#[tauri::command]
async fn search_warm(handle: tauri::AppHandle) -> Reply<usize> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_searcher(|l| Ok(l.search_hidden()?.map(|s| s.len()).unwrap_or(0))))
        .await
        .map_err(text)?
}

/// Turns one search switch (`key`: shared, accounts, account:<id>, folder:<id>; `searched`: on,
/// off, or None to follow the level above again).
#[tauri::command]
async fn search_scope_set(app: tauri::State<'_, App>, key: String, searched: Option<bool>) -> Reply<eudora_core::search_scope::ScopeState> {
    app.with(|l| l.set_search_scope(&key, searched))
}

/// A message as shown (`Library::message_view`, shared with the web interface).
#[tauri::command]
async fn message(handle: tauri::AppHandle, id: i64) -> Reply<eudora_core::outgoing::MessageView> {
    use tauri::Manager;
    let _timed = eudora_core::speed::Timer::new("Opening a message");
    // on its own connection and a thread of its own: never behind a list page, a count, or a
    // command waiting on a slow network
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_reader(|l| l.message_view(id))).await.map_err(text)?
}

/// Fetches a message's pictures from the web (Eudora fetches them: the message's sandbox never
/// goes to the network); `keep`: kept on this computer, else fetched every time.
#[tauri::command]
async fn web_pictures(app: tauri::State<'_, App>, urls: Vec<String>, keep: bool) -> Reply<Vec<eudora_core::remote_images::Picture>> {
    let dir = if keep { Some(app.with(|l| Ok(l.images_dir()))?) } else { None };
    // (a thread of its own: on a slow connection the fetch mustn't hold up the window's other
    // commands, the next message above all)
    tauri::async_runtime::spawn_blocking(move || eudora_core::remote_images::fetch(&urls, dir.as_deref())).await.map_err(text)
}

/// A setting (JSON text) / storing one.
#[tauri::command]
async fn setting(app: tauri::State<'_, App>, key: String) -> Reply<Option<String>> {
    app.with(|l| l.setting(&key))
}

#[tauri::command]
async fn set_setting(app: tauri::State<'_, App>, key: String, value: String) -> Reply<()> {
    app.with(|l| l.set_setting(&key, &value))
}

#[derive(Serialize)]
struct FoundMailbox {
    name: String,
    address: String,
    inbox: u64,
    /// Already an account in this library.
    added: bool,
}

/// Every mailbox in the user's Microsoft 365 organization they can open (shared mailboxes and
/// others'), for choosing which to bring in. Takes a minute on a large directory.
#[tauri::command]
async fn find_mailboxes(app: tauri::State<'_, App>) -> Reply<Vec<FoundMailbox>> {
    let found = tauri::async_runtime::spawn_blocking(|| -> Reply<_> {
        let mut client = eudora_core::graph::GraphClient::resume(None, eudora_core::graph::ALL_SCOPES).map_err(text)?;
        eudora_core::graph_sync::find_mailboxes(&mut client, &Silent).map_err(text)
    })
    .await
    .map_err(text)??;
    let have: Vec<String> = app.with(|l| Ok(l.accounts_counted(false)?.into_iter().filter_map(|a| a.address.map(|x| x.to_lowercase())).collect()))?;
    Ok(found
        .into_iter()
        .map(|m| FoundMailbox { added: have.contains(&m.address.to_lowercase()), name: m.name, address: m.address, inbox: m.inbox })
        .collect())
}

/// Adds the chosen mailboxes as accounts of their own (the download brings their mail in).
#[tauri::command]
async fn add_shared_mailboxes(app: tauri::State<'_, App>, addresses: Vec<String>) -> Reply<()> {
    app.with(|l| {
        for address in &addresses {
            l.add_shared_mailbox(address)?;
        }
        Ok(())
    })
}

/// Signs in to the account's mail server (IMAP or POP3) and its SMTP server with these
/// settings and lists its folders (nothing saved or sent). `smtp_password` empty = the same.
#[tauri::command]
async fn imap_test(config: eudora_core::imap_sync::ImapConfig, password: String, smtp_password: Option<String>) -> Reply<eudora_core::imap_sync::TestResult> {
    let smtp_password = smtp_password.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || eudora_core::imap_sync::test(&config, &password, &smtp_password))
        .await
        .map_err(|e| e.to_string())?
        .map_err(text)
}

/// Adds or updates an IMAP / POP3 account (passwords, when typed, go to Credential Manager).
#[tauri::command]
async fn imap_save(app: tauri::State<'_, App>, config: eudora_core::imap_sync::ImapConfig, password: Option<String>, smtp_password: Option<String>) -> Reply<i64> {
    app.with(|l| l.save_imap_account(&config, password.as_deref(), smtp_password.as_deref()))
}

#[tauri::command]
async fn imap_accounts(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::imap_sync::ImapConfig>> {
    app.with(|l| l.imap_accounts())
}

// ------------------------------------------------------------------ writing mail

#[tauri::command]
async fn identities(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::outgoing::Identity>> {
    app.with_reader(|l| l.identities())
}

/// What a new message, reply, reply to all or forward starts with.
#[tauri::command]
async fn compose_seed(handle: tauri::AppHandle, mode: String, id: Option<i64>, ids: Option<Vec<i64>>) -> Reply<eudora_core::outgoing::ComposeStart> {
    use tauri::Manager;
    // (the reading connection: free nearly always, never behind a refresh, a count or a sort)
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_reader(|l| l.compose_start(&mode, id, &ids.unwrap_or_default()))).await.map_err(text)?
}

/// Sends a message through the account that owns its From address (`Outgoing::send`); the
/// library is let go of while it goes out.
#[tauri::command]
async fn send_message(handle: tauri::AppHandle, _app: tauri::State<'_, App>, draft: eudora_core::compose::Draft, draft_id: Option<i64>) -> Reply<String> {
    let _timed = eudora_core::speed::Timer::new("Send (into Out)");
    // Send = into the Out mailbox, due now, and the window that wrote it closes at once; the Out
    // mailbox's sender goes at once (`SEND_NOW`), says how it went in the main window's status
    // bar, and tries again later when it can't (it stays in Out meanwhile). Working offline it
    // simply waits there. (made once here, no network: a message that can't go, a missing From
    // or a bad address, is said at once in its window, not later) (on the reading connection, on
    // a thread of its own: the others may be busy with a refresh, a count or a sort)
    {
        use tauri::Manager;
        let (h, d) = (handle.clone(), draft.clone());
        tauri::async_runtime::spawn_blocking(move || h.state::<App>().with_reader(|l| l.prepare_send(d).map(|_| ()))).await.map_err(text)??;
    }
    // putting it in Out is a write, which can wait for the download's writer: done behind the
    // window; a failure is said in the
    // main window's status bar
    std::thread::spawn(move || {
        use tauri::{Emitter, Manager};
        let app = handle.state::<App>();
        match app.with(|l| l.queue_message(&draft, eudora_core::library::now())) {
            Ok(_) => {
                send_now();
                // the autosaved copy goes only once the message is safely in Out
                if let Some(id) = draft_id {
                    let _ = app.with(|l| l.delete_draft(id));
                }
            }
            Err(e) => {
                let _ = handle.emit_to("main", "outbox", vec![format!("Couldn't put “{}” in the Out mailbox: {e}. It's still in Drafts.", draft.subject)]);
            }
        }
    });
    Ok("queued".into())
}

/// Wakes the Out mailbox's sender (it otherwise looks every 30 s).
static SEND_NOW: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());

fn send_now() {
    *SEND_NOW.0.lock().unwrap() = true;
    SEND_NOW.1.notify_all();
}

#[tauri::command]
async fn save_draft(app: tauri::State<'_, App>, id: Option<i64>, draft: eudora_core::compose::Draft) -> Reply<i64> {
    let json = serde_json::to_string(&draft).map_err(text)?;
    app.with(|l| l.save_draft(id, &json))
}

#[derive(Serialize)]
struct DraftRow {
    id: i64,
    saved: i64,
    draft: eudora_core::compose::Draft,
}

#[tauri::command]
async fn drafts(app: tauri::State<'_, App>) -> Reply<Vec<DraftRow>> {
    let rows = app.with(|l| l.drafts())?;
    Ok(rows.into_iter().filter_map(|(id, saved, json)| serde_json::from_str(&json).ok().map(|draft| DraftRow { id, saved, draft })).collect())
}

#[tauri::command]
async fn delete_draft(app: tauri::State<'_, App>, id: i64) -> Reply<()> {
    app.with(|l| l.delete_draft(id))
}

/// Opens a message window: `mode` new / reply / replyAll / forward / draft.
#[tauri::command]
async fn open_compose(handle: tauri::AppHandle, mode: String, id: Option<i64>, ids: Option<Vec<i64>>, to: Option<String>, subject: Option<String>, body: Option<String>) -> Reply<()> {
    compose_window(&handle, &mode, id, ids, &[("to", to), ("subject", subject), ("body", body)])
}

/// A `mailto:` link (clicked in a message, or handed to Eudora by the system as the mail
/// program): a new message with its addresses, subject, and body. Nothing is sent.
#[tauri::command]
async fn open_mailto(handle: tauri::AppHandle, url: String) -> Reply<()> {
    open_mailto_now(&handle, &url)
}

pub(crate) fn open_mailto_now(handle: &tauri::AppHandle, url: &str) -> Reply<()> {
    let m = eudora_core::compose::parse_mailto(url).ok_or_else(|| format!("“{}” isn't a mailto: link", url.chars().take(80).collect::<String>()))?;
    compose_window(handle, "new", None, None, &[("to", Some(m.to)), ("cc", Some(m.cc)), ("bcc", Some(m.bcc)), ("subject", Some(m.subject)), ("body", Some(m.body))])
}

/// A message window (`compose.html`) with the fields given filled in.
fn compose_window(handle: &tauri::AppHandle, mode: &str, id: Option<i64>, ids: Option<Vec<i64>>, fields: &[(&str, Option<String>)]) -> Reply<()> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let ids = ids.unwrap_or_default().iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut url = format!("compose.html?mode={mode}&id={}&ids={ids}", id.map(|i| i.to_string()).unwrap_or_default());
    // a mailto: link's addresses, subject, and text
    for (key, value) in fields {
        if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
            url.push_str(&format!("&{key}={}", query_escape(v)));
        }
    }
    let title = match mode {
        "reply" | "replyAll" => "Reply",
        "forward" | "forwardMany" => "Forward",
        "redirect" => "Redirect",
        "again" => "Send Again",
        _ => "New Message",
    };
    tauri::WebviewWindowBuilder::new(handle, format!("compose-{n}"), tauri::WebviewUrl::App(url.into()))
        .title(format!("{title} – Eudora"))
        .inner_size(880.0, 720.0)
        .min_inner_size(520.0, 400.0)
        // (files dropped on it reach the page, which attaches them)
        .disable_drag_drop_handler()
        .build()
        .map_err(text)?;
    Ok(())
}

/// A link clicked in a message: the message's frame is sandboxed with `<base target="_blank">`,
/// so its links ask for a new window. Windows' WebView2 lets the page's own click listener take
/// them first; the Mac's WebKit runs no listeners inside a frame without scripts, so the
/// request comes here: no window is made, and the page's `openLink` gets the address (web links
/// to the browser, mailto: a new message, a fraud's links refused).
fn links_go_to_eudora<'a, M: tauri::Manager<tauri::Wry>>(
    builder: tauri::WebviewWindowBuilder<'a, tauri::Wry, M>,
    handle: &tauri::AppHandle,
    label: &str,
) -> tauri::WebviewWindowBuilder<'a, tauri::Wry, M> {
    let (handle, label) = (handle.clone(), label.to_string());
    let (nav_handle, nav_label) = (handle.clone(), label.clone());
    builder
        .on_new_window(move |url, _| {
            let _ = tauri::Emitter::emit_to(&handle, label.as_str(), "link-clicked", url.to_string());
            tauri::webview::NewWindowResponse::Deny
        })
        // iOS's WebKit offers no new-window request at all: the phone's message frame goes to the
        // link instead, and that navigation (any frame's) is stopped here; Eudora's own pages
        // (and the frames' srcdoc / data:) go on
        .on_navigation(move |url| {
            let own = matches!(url.host_str(), Some("tauri.localhost" | "ipc.localhost" | "localhost"));
            let away = matches!(url.scheme(), "http" | "https") && !own || url.scheme() == "mailto";
            if away {
                let _ = tauri::Emitter::emit_to(&nav_handle, nav_label.as_str(), "link-clicked", url.to_string());
            }
            !away
        })
}

/// A message in a window of its own (the reader's Pop out): the main page showing only it.
#[tauri::command]
async fn open_reader(handle: tauri::AppHandle, id: i64, title: String) -> Reply<()> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let title: String = title.chars().filter(|c| !c.is_control()).take(120).collect();
    let label = format!("reader-{n}");
    links_go_to_eudora(tauri::WebviewWindowBuilder::new(&handle, &label, tauri::WebviewUrl::App(format!("index.html?message={id}").into())), &handle, &label)
        .title(format!("{} – Eudora", if title.trim().is_empty() { "Message" } else { title.trim() }))
        .inner_size(900.0, 760.0)
        .min_inner_size(420.0, 300.0)
        .build()
        .map_err(text)?;
    Ok(())
}

/// The user manual (Help ▸ Eudora Help, F1): one
/// window of its own (`ui/help/`), brought to the front when it's open already, at `topic` (a
/// section's id) when one is given.
fn show_help(handle: &tauri::AppHandle, topic: Option<&str>) -> tauri::Result<()> {
    use tauri::Manager;
    let topic: String = topic.unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(60).collect();
    if let Some(w) = handle.get_webview_window("help") {
        if !topic.is_empty() {
            let _ = w.eval(format!("location.hash = '{topic}'"));
        }
        #[cfg(desktop)]
        let _ = w.unminimize();
        return w.set_focus();
    }
    let page = if topic.is_empty() { "help/index.html".to_string() } else { format!("help/index.html#{topic}") };
    tauri::WebviewWindowBuilder::new(handle, "help", tauri::WebviewUrl::App(page.into()))
        .title("Eudora Help")
        .inner_size(1100.0, 780.0)
        .min_inner_size(420.0, 300.0)
        .build()?;
    Ok(())
}

/// Opens the manual at a topic (a "?" beside a setting, a message's link to the help).
#[tauri::command]
async fn open_help(handle: tauri::AppHandle, topic: Option<String>) -> Reply<()> {
    show_help(&handle, topic.as_deref()).map_err(text)
}

/// A value as a URL query part (letters, digits and -_.~ kept; everything else %XX of its UTF-8).
fn query_escape(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Whether a message's link may be opened in the browser: web links only (`http` / `https`;
/// mailto: is handled in the window: a new message). javascript:, file:, data: and custom
/// schemes that start programs are refused.
fn web_link(url: &str) -> Option<&str> {
    let link = url.trim();
    let scheme = link.split(':').next().unwrap_or("").to_ascii_lowercase();
    ((scheme == "http" || scheme == "https") && link.contains("://") && !link.chars().any(|c| c.is_control() || c == '"')).then_some(link)
}

/// Opens a link from a message in the computer's default browser; the window shows the real
/// address on hover before the click.
#[tauri::command]
async fn open_link(url: String) -> Reply<()> {
    let Some(link) = web_link(&url) else {
        return Err(format!("Eudora only opens web links (http, https), not “{}”", url.chars().take(80).collect::<String>()));
    };
    // (the same Windows / Mac / Linux opener the Microsoft sign-in uses)
    eudora_core::graph::open_browser(link).map_err(|e| format!("Couldn't open the browser: {e}"))
}

/// Drags an attachment out of Eudora as a file: written into the library's Attachments folder as
/// opening one does (marked as downloaded from the Internet; a fraud's or a program never), then the
/// system's own drag, started on the window's thread. Dropped in a message being written, it's
/// attached there.
#[tauri::command]
async fn attachment_drag(window: tauri::WebviewWindow, handle: tauri::AppHandle, id: i64, part: String) -> Reply<()> {
    use tauri::Manager;
    let path = tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.attachment_to_open(id, &part))).await.map_err(text)??;
    #[cfg(desktop)]
    {
        let icon = eudora_core::file_icons::icon_png(&path.to_string_lossy()).unwrap_or_else(|| include_bytes!("../icons/32x32.png").to_vec());
        let w = window.clone();
        window
            .run_on_main_thread(move || {
                // (Linux: the drag starts from the GTK window under Tauri's)
                #[cfg(target_os = "linux")]
                let Ok(w) = w.gtk_window() else { return };
                let _ = drag::start_drag(&w, drag::DragItem::Files(vec![path]), drag::Image::Raw(icon), |_, _| {}, drag::Options::default());
            })
            .map_err(text)
    }
    // (a phone has no dragging files out of a window)
    #[cfg(mobile)]
    {
        let _ = (window, path);
        Ok(())
    }
}

/// Where dragged-out messages are written before they're dropped: the system's temporary folder,
/// a folder per drag (earlier drags' folders removed after a day, long copied where they went).
fn drag_root() -> std::path::PathBuf {
    std::env::temp_dir().join("Eudora drag")
}

/// Whether a path is one of Eudora's own dragged-out files (the window can't drag or move others).
fn in_drag_root(path: &std::path::Path) -> bool {
    let Ok(root) = std::fs::canonicalize(drag_root()) else { return false };
    std::fs::canonicalize(path).is_ok_and(|p| p.starts_with(&root) && p.is_file())
}

/// What `messages_write` made: the files, and each message that couldn't be written (id, why).
#[derive(serde::Serialize)]
struct Written {
    paths: Vec<String>,
    failed: Vec<(i64, String)>,
}

/// Writes messages out as files for dragging. A task of its own on a library connection of its
/// own, so nothing the window does waits for it; "messages-written" {done, total} tells the
/// window how far it is. An .eml is the stored bytes exactly; a .msg as Outlook saves one
/// (`Library::message_msg`). A message that fails is said, never dropped.
#[tauri::command]
async fn messages_write(window: tauri::WebviewWindow, handle: tauri::AppHandle, ids: Vec<i64>, eml: bool) -> Reply<Written> {
    use tauri::{Emitter, Manager};
    let root = drag_root();
    if let Ok(old) = std::fs::read_dir(&root) {
        for entry in old.flatten() {
            if entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 86_400) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let dir = root.join(format!("{}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0), std::process::id()));
    let how = if eml { eudora_core::msg::SaveAs::Eml } else { eudora_core::msg::SaveAs::Msg };
    let label = window.label().to_string();
    tauri::async_runtime::spawn_blocking(move || {
        handle.state::<App>().with_own(|l| {
            let total = ids.len();
            let mut out = Written { paths: Vec::with_capacity(total), failed: Vec::new() };
            let mut told = std::time::Instant::now();
            for (n, &id) in ids.iter().enumerate() {
                match l.save_message(id, how, &dir) {
                    Ok(path) => out.paths.push(path.to_string_lossy().into_owned()),
                    Err(e) => out.failed.push((id, e.to_string())),
                }
                if told.elapsed().as_millis() >= 150 || n + 1 == total {
                    told = std::time::Instant::now();
                    let _ = handle.emit_to(&label, "messages-written", serde_json::json!({ "done": n + 1, "total": total }));
                }
            }
            Ok(out)
        })
    })
    .await
    .map_err(text)?
}

/// Starts the system's drag of files `messages_write` made (the mouse button still down).
#[tauri::command]
async fn files_drag(window: tauri::WebviewWindow, paths: Vec<String>) -> Reply<()> {
    let paths: Vec<std::path::PathBuf> = paths.into_iter().map(std::path::PathBuf::from).filter(|p| in_drag_root(p)).collect();
    if paths.is_empty() {
        return Err("There's nothing to drag.".into());
    }
    #[cfg(desktop)]
    {
        let icon = eudora_core::file_icons::icon_png(&paths[0].to_string_lossy()).unwrap_or_else(|| include_bytes!("../icons/32x32.png").to_vec());
        let w = window.clone();
        window
            .run_on_main_thread(move || {
                #[cfg(target_os = "linux")]
                let Ok(w) = w.gtk_window() else { return };
                let _ = drag::start_drag(&w, drag::DragItem::Files(paths), drag::Image::Raw(icon), |_, _| {}, drag::Options::default());
            })
            .map_err(text)?;
    }
    #[cfg(mobile)]
    let _ = (window, paths);
    Ok(())
}

/// Moves files `messages_write` made into a folder the user chose (the mouse was let go before
/// they were ready); a name already there gets " (2)" and so on, nothing is overwritten.
#[tauri::command]
async fn files_place(paths: Vec<String>, dir: String) -> Reply<Vec<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = std::path::PathBuf::from(dir.trim());
        let mut out = Vec::new();
        for p in paths.into_iter().map(std::path::PathBuf::from).filter(|p| in_drag_root(p)) {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let (to, there) = eudora_core::saving::place_for(&dir, &name, &bytes);
            if !there {
                // (a copy, then the temporary one removed: the folder may be on another drive)
                std::fs::write(&to, &bytes).map_err(|e| format!("{}: {e}", to.display()))?;
                eudora_core::saving::mark_downloaded(&to);
            }
            let _ = std::fs::remove_file(&p);
            out.push(to.to_string_lossy().into_owned());
        }
        Ok(out)
    })
    .await
    .map_err(text)?
}

/// Opens an attachment with the system's program for its kind (written into the library's
/// Attachments folder first; programs and a fraud's attachments refused: `attachment_to_open`).
#[tauri::command]
async fn attachment_open(window: tauri::WebviewWindow, handle: tauri::AppHandle, id: i64, part: String) -> Reply<String> {
    use tauri::Manager;
    // (the file is written out on the reading connection, never the main one: lists, marks, and
    // moves don't wait behind a big attachment, its fraud check, or the virus scanner reading the
    // new file)
    let reader = handle.clone();
    let path = tauri::async_runtime::spawn_blocking(move || reader.state::<App>().with_reader(|l| l.attachment_to_open(id, &part))).await.map_err(text)??;
    // (iOS has no default program for a file: Apple's Quick Look viewer, with its Share button)
    #[cfg(target_os = "ios")]
    {
        let _ = window;
        eudora_core::quicklook::preview(&path).map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    }
    #[cfg(not(target_os = "ios"))]
    launch_detached(window, path.clone());
    Ok(path.to_string_lossy().into_owned())
}

/// Starts the system's program for a file on a thread of its own and returns at once: however
/// long the program takes to start (or the system to ask which one), Eudora never waits for it.
/// A failure is told to the window that asked (event "attachment-open-failed").
#[cfg(not(target_os = "ios"))]
fn launch_detached(window: tauri::WebviewWindow, path: std::path::PathBuf) {
    use tauri::Emitter;
    std::thread::spawn(move || {
        if let Err(e) = eudora_core::graph::open_browser(&path.to_string_lossy()) {
            let _ = window.emit_to(window.label(), "attachment-open-failed", format!("Couldn't open {}: {e}", path.display()));
        }
    });
}

/// An attachment handed to the phone's share sheet (iOS: Save to Files, open in another app,
/// AirDrop…); elsewhere it opens as `attachment_open` does.
#[tauri::command]
async fn attachment_share(window: tauri::WebviewWindow, handle: tauri::AppHandle, id: i64, part: String) -> Reply<()> {
    use tauri::Manager;
    let reader = handle.clone();
    let path = tauri::async_runtime::spawn_blocking(move || reader.state::<App>().with_reader(|l| l.attachment_to_open(id, &part))).await.map_err(text)??;
    #[cfg(target_os = "ios")]
    {
        let _ = window;
        eudora_core::quicklook::share(&path).map_err(|e| format!("Couldn't share {}: {e}", path.display()))?;
    }
    #[cfg(not(target_os = "ios"))]
    launch_detached(window, path);
    Ok(())
}

/// The system's icon for a kind of file (an attachment's chip), as a data: URL; none when the
/// system gives none (the window keeps its paperclip).
#[tauri::command]
async fn file_icon(name: String) -> Option<String> {
    use base64::Engine;
    let png = tauri::async_runtime::spawn_blocking(move || eudora_core::file_icons::icon_png(&name)).await.ok()??;
    Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

/// Saves one attachment into a folder; returns where it went.
#[tauri::command]
async fn attachment_save(handle: tauri::AppHandle, id: i64, part: String, dir: String) -> Reply<String> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with(|l| l.save_attachment(id, &part, std::path::Path::new(&dir)).map(|p| p.to_string_lossy().into_owned())))
        .await
        .map_err(text)?
}

/// Save All: every attachment of a message into a folder: (where they went, classic files never found).
#[tauri::command]
async fn attachments_save_all(handle: tauri::AppHandle, id: i64, dir: String) -> Reply<(Vec<String>, Vec<String>)> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        handle.state::<App>().with(|l| {
            let (saved, missing) = l.save_all_attachments(id, std::path::Path::new(&dir))?;
            Ok((saved.iter().map(|p| p.to_string_lossy().into_owned()).collect(), missing))
        })
    })
    .await
    .map_err(text)?
}

/// Transfer: moves messages to another folder (see `Library::move_messages`).
#[tauri::command]
async fn move_messages(app: tauri::State<'_, App>, ids: Vec<i64>, from: i64, to: i64) -> Reply<u64> {
    let _timed = eudora_core::speed::Timer::new("Moving messages");
    app.with(|l| l.move_messages(&ids, from, to))
}

/// Delete: to the account's Trash. Returns the Trash folder (for Undo).
#[tauri::command]
async fn delete_messages(app: tauri::State<'_, App>, ids: Vec<i64>, from: i64) -> Reply<i64> {
    app.with(|l| {
        let account: i64 = l.connection().query_row("SELECT account FROM folders WHERE id = ?1", [from], |r| r.get(0))?;
        let trash = l.trash_folder(account)?;
        l.move_messages(&ids, from, trash)?;
        Ok(trash)
    })
}

// ------------------------------------------------------------------ status, priority, labels, Empty Trash

/// Change Status (unread, read, replied, forwarded, redirected). Returns how many changed.
#[tauri::command]
async fn set_status(app: tauri::State<'_, App>, ids: Vec<i64>, status: String) -> Reply<u64> {
    let _timed = eudora_core::speed::Timer::new("Marking read / unread");
    let status = eudora_core::marks::Status::parse(&status).ok_or_else(|| format!("there's no status \"{status}\""))?;
    app.with(|l| l.set_status(&ids, status))
}

/// Change Priority: 1 (highest) … 5 (lowest), 0 = as the message says; `shift` −1 / +1 raises
/// or lowers instead.
#[tauri::command]
async fn set_priority(app: tauri::State<'_, App>, ids: Vec<i64>, level: u8, shift: Option<i8>) -> Reply<u64> {
    app.with(|l| match shift {
        Some(by) => l.shift_priority(&ids, by),
        None => l.set_priority(&ids, (level > 0).then_some(level)),
    })
}

/// Change Label: a label on (`on`) or off; no label = every label off (None).
#[tauri::command]
async fn set_label(app: tauri::State<'_, App>, ids: Vec<i64>, label: Option<String>, on: bool) -> Reply<u64> {
    app.with(|l| match &label {
        Some(name) => l.set_label(&ids, name, on),
        None => l.clear_labels(&ids),
    })
}

#[tauri::command]
async fn label_list(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::marks::LabelDef>> {
    app.with(|l| l.label_list())
}

/// Settings ▸ Mail: the list, and labels renamed (old, new) on the messages that have them.
#[tauri::command]
async fn label_list_set(app: tauri::State<'_, App>, list: Vec<eudora_core::marks::LabelDef>, renamed: Vec<(String, String)>) -> Reply<()> {
    app.with(|l| l.set_label_list(&list, &renamed))
}

#[tauri::command]
async fn signatures(app: tauri::State<'_, App>) -> Reply<eudora_core::signatures::Signatures> {
    app.with_reader(|l| l.signatures())
}

#[tauri::command]
async fn signatures_set(app: tauri::State<'_, App>, signatures: eudora_core::signatures::Signatures) -> Reply<()> {
    app.with(|l| l.set_signatures(&signatures))
}

/// Check Spelling: the misspellings in each piece of text (the system's spell checker).
#[tauri::command]
async fn spell_check(texts: Vec<String>) -> Reply<Vec<Vec<eudora_core::spell::Misspelling>>> {
    tauri::async_runtime::spawn_blocking(move || eudora_core::spell::check(&texts)).await.map_err(|e| e.to_string())?.map_err(text)
}

/// The 🎤 button: the system's voice typing, into the text that has the keyboard.
#[tauri::command]
async fn dictate() -> Reply<()> {
    eudora_core::dictation::start().map_err(text)
}

#[tauri::command]
async fn spell_add(word: String) -> Reply<()> {
    tauri::async_runtime::spawn_blocking(move || eudora_core::spell::add_word(&word)).await.map_err(|e| e.to_string())?.map_err(text)
}

/// The rules' notices (the "notify" action) waiting for the window; the taskbar button
/// flashes when there are any (as classic Eudora's notify did).
#[tauri::command]
async fn notices(app: tauri::State<'_, App>, window: tauri::Window) -> Reply<Vec<serde_json::Value>> {
    let Some(root) = app.path.clone() else { return Ok(Vec::new()) };
    let found = eudora_core::rule_actions::take_notices(&root);
    // (a desktop's taskbar button flashes; a phone has none)
    #[cfg(desktop)]
    if !found.is_empty() {
        let _ = window.request_user_attention(Some(tauri::UserAttentionType::Informational));
    }
    #[cfg(mobile)]
    let _ = window;
    Ok(found)
}

#[tauri::command]
async fn trash_summary(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::trash::TrashSummary>> {
    app.with(|l| l.trash_summary())
}

#[tauri::command]
async fn empty_trash(app: tauri::State<'_, App>, accounts: Vec<i64>) -> Reply<u64> {
    app.with(|l| l.empty_trash(&accounts))
}

// ------------------------------------------------------------------ address completion

/// Which people completion offers (Settings; setting "completion").
#[derive(Serialize, serde::Deserialize)]
#[serde(default)]
struct Completion {
    /// People the user wrote to.
    sent: bool,
    /// People who wrote to the user.
    received: bool,
}

impl Default for Completion {
    fn default() -> Self {
        Completion { sent: true, received: true }
    }
}

fn completion_of(l: &Library) -> eudora_core::Result<Completion> {
    Ok(l.setting("completion")?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
}

#[derive(Serialize)]
struct Suggestion {
    name: String,
    address: String,
}

/// The organization's directory (Microsoft 365) for an address field, after the mail and the
/// Address Book found too few: asked in the background, the window adds what comes back. One
/// connection kept, answers kept for the session.
#[tauri::command]
async fn complete_directory(typed: String) -> Reply<Vec<Suggestion>> {
    static CLIENT: Mutex<Option<eudora_core::graph::GraphClient>> = Mutex::new(None);
    static SEEN: Mutex<Option<HashMap<String, Vec<(String, String)>>>> = Mutex::new(None);
    let key = typed.trim().to_lowercase();
    if key.chars().count() < 3 {
        return Ok(Vec::new());
    }
    if let Some(found) = SEEN.lock().map_err(text)?.as_ref().and_then(|m| m.get(&key).cloned()) {
        return Ok(found.into_iter().map(|(name, address)| Suggestion { name, address }).collect());
    }
    let found = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<(String, String)>, String> {
        let mut client = CLIENT.lock().map_err(text)?;
        if client.is_none() {
            *client = Some(eudora_core::graph::GraphClient::resume_best(None, eudora_core::graph::ALL_SCOPES).map_err(text)?);
        }
        let found = client.as_mut().map(|c| c.search_directory(&key, 8)).transpose().map_err(text)?.unwrap_or_default();
        SEEN.lock().map_err(text)?.get_or_insert_with(HashMap::new).insert(key, found.clone());
        Ok(found)
    })
    .await
    .map_err(text)??;
    Ok(found.into_iter().map(|(name, address)| Suggestion { name, address }).collect())
}

/// People matching what's typed in an address field.
#[tauri::command]
async fn complete_address(app: tauri::State<'_, App>, typed: String) -> Reply<Vec<Suggestion>> {
    app.with(|l| {
        let c = completion_of(l)?;
        Ok(l.complete_address(&typed, c.sent, c.received, 8)?.into_iter().map(|(name, address)| Suggestion { name, address }).collect())
    })
}

/// Rebuilds the people for completion from the mail (at start, in the background; it reads
/// every message's addresses once).
#[tauri::command]
async fn rebuild_contacts(app: tauri::State<'_, App>) -> Reply<u64> {
    let mine: Vec<String> = app.with(|l| Ok(l.identities()?.into_iter().map(|i| i.address).collect()))?;
    // its own connection: the viewer keeps working meanwhile
    let path = app.path.clone().ok_or("no library")?;
    let own = Library::open(&path, Access::Read, &Silent).map_err(text)?;
    own.rebuild_contacts(&mine).map_err(text)
}

// ------------------------------------------------------------------ calendar

/// Every calendar, with its color and whether it's shown.
#[tauri::command]
async fn calendars_list(handle: tauri::AppHandle) -> Reply<Vec<eudora_core::calendar::CalendarInfo>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.calendars_list())).await.map_err(text)?
}

/// Shows or hides a calendar.
#[tauri::command]
async fn calendar_shown(app: tauri::State<'_, App>, id: i64, shown: bool) -> Reply<()> {
    app.with(|l| l.set_calendar_shown(id, shown))
}

/// A calendar's color here ("#rrggbb"), or None for its own color again.
#[tauri::command]
async fn calendar_color(app: tauri::State<'_, App>, id: i64, color: Option<String>) -> Reply<()> {
    app.with(|l| l.set_calendar_color(id, color.as_deref()))
}

/// The events between two times (Unix seconds).
#[tauri::command]
async fn calendar_events(handle: tauri::AppHandle, from: i64, to: i64) -> Reply<Vec<eudora_core::calendar::EventRow>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.events_between(from, to))).await.map_err(text)?
}

/// One event whole.
#[tauri::command]
async fn calendar_event(handle: tauri::AppHandle, id: i64) -> Reply<eudora_core::calendar::EventView> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.event_get(id))).await.map_err(text)?
}

/// Events matching words (the calendar's search box).
#[tauri::command]
async fn calendar_search(handle: tauri::AppHandle, words: String) -> Reply<Vec<eudora_core::calendar::EventRow>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.search_events(&words, 300))).await.map_err(text)?
}

/// Answers an invitation: accept, tentative or decline.
#[tauri::command]
async fn event_respond(app: tauri::State<'_, App>, id: i64, answer: String, series: Option<bool>, comment: Option<String>, propose_start: Option<i64>, propose_end: Option<i64>) -> Reply<()> {
    let proposed = propose_start.zip(propose_end);
    app.with(|l| l.event_respond_with(id, &answer, series.unwrap_or(false), comment.as_deref().unwrap_or(""), proposed))
}

/// Changes an event's private mark, how it shows, its categories.
#[tauri::command]
async fn event_update(app: tauri::State<'_, App>, id: i64, private: Option<bool>, show_as: Option<String>, categories: Option<Vec<String>>) -> Reply<()> {
    app.with(|l| l.event_update(id, private, show_as.as_deref(), categories.as_deref()))
}

/// Deletes an event (declines someone else's meeting, cancels the user's own): what was done.
#[tauri::command]
async fn event_delete(app: tauri::State<'_, App>, id: i64, series: Option<bool>) -> Reply<String> {
    app.with(|l| l.event_delete_as(id, series.unwrap_or(false)).map(str::to_string))
}

/// Reservations ahead found in Travel mail (flights added, stays offered).
#[tauri::command]
async fn reservations_list(handle: tauri::AppHandle) -> Reply<Vec<eudora_core::travel::Reservation>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.reservations())).await.map_err(text)?
}

/// Light or dark for every window and the menu bar, whatever Windows / macOS is set to ("light",
/// "dark"; anything else: follow the system). The looks' dark versions are written for the
/// system's dark mode; this sets what the windows report as that.
#[tauri::command]
fn theme_set(handle: tauri::AppHandle, theme: Option<String>) {
    handle.set_theme(match theme.as_deref() {
        Some("light") => Some(tauri::Theme::Light),
        Some("dark") => Some(tauri::Theme::Dark),
        _ => None,
    });
}

/// Places matching what's typed in a new appointment's Location (Google Maps with the user's
/// key, else OpenStreetMap).
#[tauri::command]
async fn places_suggest(text: String) -> Reply<Vec<eudora_core::meetings::Place>> {
    tauri::async_runtime::spawn_blocking(move || eudora_core::meetings::suggest_places(&text)).await.map_err(crate::text)?.map_err(crate::text)
}

/// Whether a Google Maps key is set (the key itself never leaves the credential store).
#[tauri::command]
fn maps_key_present() -> bool {
    eudora_core::meetings::maps_key().is_some()
}

/// Sets (or removes, with none) the Google Maps key, typed by the user in Settings.
#[tauri::command]
fn maps_key_set(key: Option<String>) -> Reply<()> {
    eudora_core::meetings::set_maps_key(key.as_deref()).map_err(text)
}

/// An appointment for its editor, its whole text asked of Microsoft when only the preview is
/// here (the calendar comes with previews only).
#[tauri::command]
async fn appointment_get(handle: tauri::AppHandle, id: i64) -> Reply<eudora_core::meetings::Appointment> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        let mut a = handle.state::<App>().with_free(|l| l.appointment(id))?;
        if a.body.is_none() && a.on_server {
            // (off line or refused: the editor shows the preview, and leaves the text alone)
            if let Ok(mut client) = eudora_core::graph::GraphClient::resume_best(Some(&a.address), eudora_core::graph::ALL_SCOPES) {
                if let Ok(full) = client.event(&a.server_id) {
                    a.body = full["body"]["content"].as_str().map(str::to_string);
                }
            }
        }
        Ok(a)
    })
    .await
    .map_err(text)?
}

/// Saves an appointment's changes (`body_changed`: its text was edited).
#[tauri::command]
async fn appointment_edit(handle: tauri::AppHandle, id: i64, event: eudora_core::meetings::NewEvent, body_changed: bool) -> Reply<()> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with(|l| l.edit_appointment(id, &event, body_changed))).await.map_err(text)?
}

/// An event as an .ics file's text (to attach to a message).
#[tauri::command]
async fn appointment_ics(app: tauri::State<'_, App>, id: i64) -> Reply<String> {
    app.with_free(|l| {
        let a = l.appointment(id)?;
        let text = a.body.as_deref().map(|h| mail_parser::decoders::html::html_to_text(h).to_string()).unwrap_or_else(|| a.event.preview.clone());
        Ok(eudora_core::meetings::ics(&a.event, &text, eudora_core::library::now()))
    })
}

/// The Zoom app (its Client ID; whether a secret is kept) and whether Zoom is signed in.
#[tauri::command]
fn zoom_get() -> serde_json::Value {
    let app = eudora_core::zoom::app();
    serde_json::json!({ "id": app.as_ref().map(|a| a.id.clone()).unwrap_or_default(), "has_app": app.is_some(), "signed_in": eudora_core::zoom::signed_in(), "redirect": eudora_core::zoom::REDIRECT })
}

/// Keeps the Zoom app's Client ID and Client Secret (typed by the user), or forgets them.
#[tauri::command]
fn zoom_set(id: Option<String>, secret: Option<String>) -> Reply<()> {
    match (id, secret) {
        (Some(id), Some(secret)) => eudora_core::zoom::set_app(Some(&eudora_core::zoom::ZoomApp { id, secret })).map_err(text),
        _ => eudora_core::zoom::set_app(None).map_err(text),
    }
}

/// Signs in to Zoom in the browser.
#[tauri::command]
async fn zoom_sign_in() -> Reply<()> {
    tauri::async_runtime::spawn_blocking(|| eudora_core::zoom::sign_in(&|url| { let _ = eudora_core::graph::open_browser(url); })).await.map_err(text)?.map_err(text)
}

/// Signs out of Zoom on this computer.
#[tauri::command]
fn zoom_sign_out() {
    eudora_core::zoom::sign_out();
}

/// Which online meetings can go in an event of this calendar.
#[tauri::command]
async fn conference_options(app: tauri::State<'_, App>, calendar: i64) -> Reply<eudora_core::meetings::ConferenceOptions> {
    app.with_free(|l| l.conference_options(calendar))
}

/// Makes a new appointment (its Meet asked of Google first, which takes a moment: on a thread of
/// its own); Microsoft sends its people the invitations. Returns its id.
#[tauri::command]
async fn appointment_new(handle: tauri::AppHandle, event: eudora_core::meetings::NewEvent) -> Reply<i64> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with(|l| l.new_appointment(&event))).await.map_err(text)?
}

/// The invitations ahead waiting for an answer, with their conflicts (the calendar's Invites).
#[tauri::command]
async fn invites_list(handle: tauri::AppHandle) -> Reply<Vec<eudora_core::invites::Invite>> {
    use tauri::Manager;
    // invitations from blocked senders leave the calendar quietly first
    tauri::async_runtime::spawn_blocking(move || {
        handle.state::<App>().with(|l| {
            l.remove_blocked_invites()?;
            l.invites()
        })
    })
    .await
    .map_err(text)?
}

/// An invitation reported as spam: no answer of any kind; the meeting off the calendar, its
/// email to Spam and the sender blocked from now on.
#[tauri::command]
async fn invite_spam(app: tauri::State<'_, App>, event: i64, series: Option<bool>) -> Reply<Vec<String>> {
    app.with(|l| l.invite_spam(event, series.unwrap_or(false)))
}

/// A message's invitation (its invite file) and its event on the calendar, if it has one.
#[tauri::command]
async fn invitation(app: tauri::State<'_, App>, id: i64) -> Reply<Option<eudora_core::invites::Invitation>> {
    app.with_free(|l| l.invitation(id))
}

/// Answers an invitation email by email (an invitation not on a calendar Eudora answers
/// through): the answer goes to the organizer from the Out mailbox, at once.
#[tauri::command]
async fn invitation_answer(app: tauri::State<'_, App>, id: i64, answer: String, comment: Option<String>) -> Reply<String> {
    let said = app.with(|l| l.invitation_answer(id, &answer, comment.as_deref()))?;
    send_now();
    Ok(said)
}

/// An invitation email's meeting onto a calendar (one not on a calendar here): nobody is told.
#[tauri::command]
async fn invitation_add(app: tauri::State<'_, App>, id: i64, calendar: Option<i64>) -> Reply<i64> {
    app.with(|l| l.invitation_add(id, calendar))
}

/// Offered reservations: these onto the calendar, those declined.
#[tauri::command]
async fn reservations_answer(app: tauri::State<'_, App>, add: Vec<String>, decline: Vec<String>) -> Reply<u64> {
    app.with(|l| l.answer_reservations(&add, &decline))
}

// ------------------------------------------------------------------ To Do

/// The To Do lists, each shown or hidden, with its open tasks.
#[tauri::command]
async fn todo_lists(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::todo::TodoList>> {
    app.with_free(|l| l.todo_lists())
}

/// A To Do list's color here ("#rrggbb"), or None for its first color again.
#[tauri::command]
async fn todo_list_color(app: tauri::State<'_, App>, id: String, color: Option<String>) -> Reply<()> {
    app.with(|l| l.set_todo_list_color(&id, color.as_deref()))
}

/// Shows or hides a To Do list here (Microsoft To Do keeps it either way).
#[tauri::command]
async fn todo_list_shown(app: tauri::State<'_, App>, id: String, shown: bool) -> Reply<()> {
    app.with(|l| l.set_todo_list_shown(&id, shown))
}

/// Every task (open by due date, then the last month's done ones).
#[tauri::command]
async fn todos_list(handle: tauri::AppHandle) -> Reply<Vec<eudora_core::todo::Todo>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.todos())).await.map_err(text)?
}

#[tauri::command]
async fn todo_add(app: tauri::State<'_, App>, title: String, notes: Option<String>, due: Option<i64>, message: Option<i64>) -> Reply<i64> {
    app.with(|l| l.todo_add(&title, &notes.unwrap_or_default(), due, message))
}

/// Changes a task; `clear_due` takes its date off.
#[tauri::command]
async fn todo_update(app: tauri::State<'_, App>, id: i64, title: Option<String>, notes: Option<String>, due: Option<i64>, clear_due: Option<bool>, done: Option<bool>, important: Option<bool>) -> Reply<()> {
    let due = if clear_due.unwrap_or(false) { Some(None) } else { due.map(Some) };
    app.with(|l| l.todo_update(id, title.as_deref(), notes.as_deref(), due, done, important))
}

#[tauri::command]
async fn todo_delete(app: tauri::State<'_, App>, id: i64) -> Reply<()> {
    app.with(|l| l.todo_delete(id))
}

/// Flags messages as To Do items (or takes the flag off): the tasks made.
#[tauri::command]
async fn todo_flag(app: tauri::State<'_, App>, ids: Vec<i64>, on: bool) -> Reply<Vec<i64>> {
    app.with(|l| l.flag_messages(&ids, on))
}

/// Sends a task to Twos.
#[tauri::command]
async fn todo_twos(handle: tauri::AppHandle, id: i64) -> Reply<()> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.send_todo_to_twos(id))).await.map_err(text)?
}

/// Twos' user id and whether its token is kept (never the token).
#[tauri::command]
async fn twos_get(app: tauri::State<'_, App>) -> Reply<(Option<String>, bool)> {
    app.with(|l| l.twos_settings())
}

/// Keeps Twos' user id and token (the token into this computer's credential store).
#[tauri::command]
async fn twos_set(app: tauri::State<'_, App>, user: String, token: Option<String>) -> Reply<()> {
    app.with(|l| l.set_twos(&user, token.as_deref()))
}

/// Tries Twos: adds a line to its today list.
#[tauri::command]
async fn twos_test(handle: tauri::AppHandle) -> Reply<()> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.twos_add("Eudora can add to Twos (a test from Eudora's Settings)"))).await.map_err(text)?
}

/// Takes a reservation's event off the calendar: what was done.
#[tauri::command]
async fn reservation_off(app: tauri::State<'_, App>, key: String) -> Reply<String> {
    app.with(|l| l.remove_reservation_event(&key))
}

/// Removes a reservation from Trips.
#[tauri::command]
async fn reservation_hide(app: tauri::State<'_, App>, key: String) -> Reply<()> {
    app.with(|l| l.hide_reservation(&key))
}

/// Add to Trips: one email read for reservations (how many it holds).
#[tauri::command]
async fn trips_from_message(app: tauri::State<'_, App>, id: i64) -> Reply<usize> {
    app.with(|l| l.trips_from_message(id))
}

// ------------------------------------------------------------------ rules

#[tauri::command]
async fn rules_get(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::rules::Rule>> {
    app.with(|l| l.rules())
}

#[tauri::command]
async fn rules_set(app: tauri::State<'_, App>, rules: Vec<eudora_core::rules::Rule>) -> Reply<()> {
    app.with(|l| l.set_rules(&rules))
}

/// Reads classic Eudora's Filters.pce; returns the classic rules (not saved: the window adds them).
#[tauri::command]
async fn rules_import_classic(path: String, account: String) -> Reply<Vec<eudora_core::rules::Rule>> {
    let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
    eudora_core::rules::import_classic(&eudora_core::parse::decode_8bit(&bytes), &account).map_err(text)
}

/// Runs the rules (those for running by hand) on a folder, or on the given messages in it.
#[tauri::command]
async fn rules_run(app: tauri::State<'_, App>, folder: i64, ids: Option<Vec<i64>>, dry_run: bool) -> Reply<eudora_core::rules::RunReport> {
    app.with(|l| l.run_rules(folder, ids.as_deref(), dry_run, &Silent))
}

/// Runs the rules on a selection that may span folders (search results): (message, folder).
#[tauri::command]
async fn rules_run_items(app: tauri::State<'_, App>, items: Vec<(i64, i64)>, dry_run: bool) -> Reply<eudora_core::rules::RunReport> {
    // its own connection: a big selection takes a while, and the window keeps working
    let path = app.path.clone().ok_or("no library")?;
    tauri::async_runtime::spawn_blocking(move || Library::open(&path, Access::Read, &Silent).map_err(text).and_then(|own| run_items(&own, &items, dry_run))).await.map_err(text)?
}

/// The rules on (message, folder) items, a folder at a time (each newest first).
fn run_items(l: &Library, items: &[(i64, i64)], dry_run: bool) -> Reply<eudora_core::rules::RunReport> {
    let mut by_folder: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    for (id, folder) in items {
        by_folder.entry(*folder).or_default().push(*id);
    }
    let mut total = eudora_core::rules::RunReport::default();
    for (folder, ids) in by_folder {
        total.add(l.run_rules(folder, Some(&ids), dry_run, &Silent).map_err(text)?);
    }
    Ok(total)
}

// ------------------------------------------------------------------ the spam filter

/// Where the spam filter would put these messages (message, folder) and why; moves nothing.
#[tauri::command]
async fn sort_preview(app: tauri::State<'_, App>, items: Vec<(i64, i64)>) -> Reply<eudora_core::triage_decide::Preview> {
    // its own connection, half the processor's cores (the window stays quick), counted and
    // stoppable
    let path = app.path.clone().ok_or("no library")?;
    let job = std::sync::Arc::new(eudora_core::progress::Job::default());
    job.total.store(items.len() as u64, std::sync::atomic::Ordering::Relaxed);
    if let Some(old) = app.sort_job.lock().map_err(text)?.replace(job.clone()) {
        old.stop(); // (one at a time: a new one stops the one before)
    }
    let running = job.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        eudora_core::progress::on_half_the_cores(|| Library::open(&path, Access::Read, &Silent).and_then(|own| own.triage_messages(&items, &*running)))
    })
    .await
    .map_err(text)?;
    let mut current = app.sort_job.lock().map_err(text)?;
    if current.as_ref().is_some_and(|j| std::sync::Arc::ptr_eq(j, &job)) {
        *current = None;
    }
    match result {
        Err(eudora_core::Error::Canceled) => Err("Stopped: nothing was moved.".into()),
        other => other.map_err(text),
    }
}

/// What a background job did to each message (Activity ▸ Details): its latest run, or the runs
/// before `before` (More).
#[tauri::command]
fn job_details(app: tauri::State<'_, App>, job: String, before: Option<i64>, runs: Option<usize>) -> Reply<eudora_core::job_log::Page> {
    app.with_reader(|l| l.job_log(&job, before, runs.unwrap_or(1).clamp(1, 50)))
}

/// Pauses or resumes one background job (Activity's per-job buttons).
#[tauri::command]
fn job_pause(app: tauri::State<'_, App>, key: String, paused: bool) -> Reply<()> {
    let path = app.path.as_ref().ok_or("no library")?;
    eudora_core::activity::set_job_paused(path, &key, paused).map_err(text)
}

/// The spam filter's deciding now: (done, total), or nothing when none runs.
#[tauri::command]
fn sort_progress(app: tauri::State<'_, App>) -> Reply<Option<(u64, u64)>> {
    use std::sync::atomic::Ordering;
    Ok(app.sort_job.lock().map_err(text)?.as_ref().map(|j| (j.done.load(Ordering::Relaxed), j.total.load(Ordering::Relaxed))))
}

/// Stops the spam filter's deciding (nothing has moved yet: the review comes after).
#[tauri::command]
fn sort_stop(app: tauri::State<'_, App>) -> Reply<()> {
    if let Some(job) = app.sort_job.lock().map_err(text)?.as_ref() {
        job.stop();
    }
    Ok(())
}

/// Carries out the proposals the user kept, in the background on a connection of its own. Returns
/// how many moved.
#[tauri::command]
async fn sort_apply(app: tauri::State<'_, App>, chosen: Vec<eudora_core::triage_decide::Chosen>) -> Reply<u64> {
    let path = app.path.clone().ok_or("no library")?;
    tauri::async_runtime::spawn_blocking(move || Library::open(&path, Access::Read, &Silent).and_then(|own| own.triage_apply(&chosen)).map_err(text)).await.map_err(text)?
}

/// Spam (true) / Not Spam (false) on a selection: (what was done, how many moved).
#[tauri::command]
async fn mark_spam(app: tauri::State<'_, App>, items: Vec<(i64, i64)>, spam: bool) -> Reply<(Vec<String>, u64)> {
    app.with(|l| l.mark_and_move(&items, spam))
}

/// Why the spam filter decides what it does for one message.
#[tauri::command]
async fn sort_why(app: tauri::State<'_, App>, id: i64) -> Reply<eudora_core::triage_decide::Proposal> {
    app.with(|l| l.triage_one(id))
}

#[tauri::command]
async fn spam_rules(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::triage_decide::FilterRule>> {
    app.with(|l| l.spam_filter_rules())
}

#[derive(Serialize)]
struct SpamFilterState {
    /// Arriving mail is sorted by the spam filter.
    on_arrival: bool,
    /// A Spamhaus DQS key is kept on this computer (never the key itself).
    spamhaus: bool,
    /// Each mailbox's learning: (shown as, spam learned, good learned).
    learned: Vec<(String, u64, u64)>,
}

#[tauri::command]
async fn spam_filter_state(app: tauri::State<'_, App>) -> Reply<SpamFilterState> {
    app.with(|l| {
        let mut learned = Vec::new();
        for m in l.spam_filter_mailboxes()?.into_iter().filter(|m| m.on) {
            for a in &m.accounts {
                let (spam, ham) = l.bayes_counts(*a)?;
                learned.push((m.shown.clone(), spam, ham));
            }
        }
        Ok(SpamFilterState { on_arrival: l.spam_filter_on_arrival()?, spamhaus: eudora_core::blocklists::key().is_some(), learned })
    })
}

#[tauri::command]
async fn spam_filter_arrival_set(app: tauri::State<'_, App>, on: bool) -> Reply<()> {
    app.with(|l| l.set_spam_filter_on_arrival(on))
}

/// Keeps the Spamhaus DQS key in this computer's credential store (empty forgets it).
#[tauri::command]
async fn spamhaus_key_set(key: String) -> Reply<()> {
    eudora_core::blocklists::set_key(&key).map_err(text)
}

/// Learns from what each mailbox the spam filter works in holds (a job: minutes).
#[tauri::command]
async fn spam_filter_learn(app: tauri::State<'_, App>) -> Reply<()> {
    let root = app.path.clone().ok_or("no library")?;
    start_job("learn", move || {
        let library = eudora_core::Library::open(&root, Access::Read, &JobProgress).map_err(text)?;
        let mut said = Vec::new();
        for m in library.spam_filter_mailboxes().map_err(text)?.into_iter().filter(|m| m.on) {
            for a in &m.accounts {
                let t = library.bayes_bootstrap(*a, &JobProgress).map_err(text)?;
                said.push(format!("{}: {} spam and {} good messages", m.shown, t.spam, t.ham));
            }
        }
        Ok(format!("Learned: {}.", said.join("; ")))
    })
}

/// The languages the user reads (others go to Spam) and every language Eudora knows.
#[tauri::command]
async fn spam_languages(app: tauri::State<'_, App>) -> Reply<(Vec<String>, Vec<(String, String)>)> {
    let chosen = app.with(|l| l.primary_languages())?;
    Ok((chosen, eudora_core::triage_decide::LANGUAGES.iter().map(|(c, n, _)| (c.to_string(), n.to_string())).collect()))
}

/// Chooses the languages the user reads.
#[tauri::command]
async fn spam_languages_set(app: tauri::State<'_, App>, codes: Vec<String>) -> Reply<()> {
    app.with(|l| l.set_primary_languages(&codes))
}

/// The mailboxes the spam filter may work in, and whether it does.
#[tauri::command]
async fn spam_mailboxes(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::triage_decide::FilterMailbox>> {
    app.with(|l| l.spam_filter_mailboxes())
}

/// Chooses the mailboxes the spam filter works in (their addresses).
#[tauri::command]
async fn spam_mailboxes_set(app: tauri::State<'_, App>, addresses: Vec<String>) -> Reply<()> {
    app.with(|l| l.set_spam_filter_mailboxes(&addresses))
}

#[tauri::command]
async fn spam_rule_set(app: tauri::State<'_, App>, key: String, settings: eudora_core::triage_decide::RuleSettings) -> Reply<()> {
    app.with(|l| l.set_rule_settings(&key, &settings))
}

/// A new blacklist ("block") or whitelist ("allow") entry: an address or "@domain".
#[tauri::command]
async fn spam_entry_add(app: tauri::State<'_, App>, pattern: String, kind: String, action: Option<eudora_core::triage::Action>, note: String, mailbox: Option<String>) -> Reply<i64> {
    app.with(|l| l.set_sender_rule_in(&pattern, &kind, action.as_ref(), &note, mailbox.as_deref().unwrap_or("")))
}

/// Removes a rule from the list: a blacklist / whitelist entry ("block:…", "allow:…") or a
/// "like this" rule ("like:N"). Built-in rules can only be switched off.
#[tauri::command]
async fn spam_rule_remove(app: tauri::State<'_, App>, key: String) -> Reply<()> {
    app.with(|l| {
        if let Some(id) = key.strip_prefix("like:").and_then(|n| n.parse().ok()) {
            return l.remove_like_rule(id);
        }
        let (kind, pattern) = key.split_once(':').ok_or_else(|| eudora_core::Error::Other(format!("{key} can only be switched off")))?;
        // (every mailbox's entry for the pattern: they're one row)
        let entries: Vec<_> = l.sender_rules()?.into_iter().filter(|e| e.kind == kind && e.pattern == pattern).collect();
        match entries.is_empty() {
            false => {
                for e in entries {
                    l.remove_sender_rule(e.id)?;
                }
                l.set_rule_settings(&key, &Default::default())
            }
            true => Err(eudora_core::Error::Other(format!("{key} can only be switched off"))),
        }
    })
}

/// A "messages like this one" rule from a message: what it would look at and how many of the
/// last year's messages it catches (newest 20), before saving.
#[tauri::command]
async fn like_preview(app: tauri::State<'_, App>, id: i64) -> Reply<(eudora_core::triage_decide::Fingerprint, String, u64, Vec<(i64, i64, String, String)>)> {
    app.with(|l| {
        let (keys, count, sample) = l.messages_like(id, 365, 20)?;
        let what = keys.describe();
        Ok((keys, what, count, sample))
    })
}

#[tauri::command]
async fn like_add(app: tauri::State<'_, App>, name: String, keys: eudora_core::triage_decide::Fingerprint, action: eudora_core::triage::Action, mailbox: Option<String>) -> Reply<i64> {
    app.with(|l| l.add_like_rule_in(&name, keys, action, mailbox.as_deref().unwrap_or("")))
}

#[tauri::command]
async fn copy_messages(app: tauri::State<'_, App>, ids: Vec<i64>, to: i64) -> Reply<u64> {
    app.with(|l| l.copy_messages(&ids, to))
}

/// Who hosts an address's mail ("microsoft", "google" or none), so adding an account goes the
/// right way: Microsoft addresses sign in with Microsoft (no passwords over IMAP there).
#[tauri::command]
async fn mail_host(address: String) -> Reply<Option<eudora_core::mail_host::MailHost>> {
    tauri::async_runtime::spawn_blocking(move || eudora_core::mail_host::mail_host(&address)).await.map_err(text)
}

/// Mailbox ▸ New… / Transfer ▸ New…: a new mailbox in an account ("A/B" makes both levels).
#[tauri::command]
async fn folder_create(app: tauri::State<'_, App>, account: i64, path: String) -> Reply<i64> {
    app.with(|l| l.create_folder(account, &path))
}

/// Renames a mailbox (the folders inside go with it), here and on the server: its new path.
#[tauri::command]
async fn folder_rename(app: tauri::State<'_, App>, folder: i64, name: String) -> Reply<String> {
    app.with(|l| l.rename_folder(folder, &name))
}

/// Deletes a mailbox as Outlook does (into Deleted Items, here and on the server): its new
/// path, or None when it was removed (empty, in the Trash already).
#[tauri::command]
async fn folder_delete(app: tauri::State<'_, App>, folder: i64) -> Reply<Option<String>> {
    app.with(|l| l.delete_folder(folder))
}

#[derive(Serialize)]
struct ServerChanges {
    /// The Microsoft 365 account signed in on this computer ("" if none).
    account: String,
    /// It may move mail and make folders on the server (agreed at sign-in).
    may_change: bool,
    waiting: u64,
    given_up: u64,
    last_error: Option<String>,
}

/// Whether Eudora may change mail on the Microsoft 365 server, and the moves waiting for it.
#[tauri::command]
async fn server_changes(app: tauri::State<'_, App>) -> Reply<ServerChanges> {
    let (waiting, given_up, last_error) = app.with(|l| eudora_core::server_changes::waiting(l))?;
    let (account, may_change) = tauri::async_runtime::spawn_blocking(|| {
        eudora_core::graph::GraphClient::resume_best(None, eudora_core::graph::ALL_SCOPES)
            .map(|c| (c.account.clone(), c.may_change_mail()))
            .unwrap_or_default()
    })
    .await
    .map_err(text)?;
    Ok(ServerChanges { account, may_change, waiting, given_up, last_error })
}

/// Signs in to Microsoft 365 again in the browser, agreeing to everything Eudora uses
/// (archive, shared mailboxes, sending, moving mail and making folders); moves given up for
/// lack of permission are tried again. The running download picks the sign-in up at its next pass.
#[tauri::command]
async fn microsoft_sign_in(app: tauri::State<'_, App>, address: Option<String>) -> Reply<String> {
    // (`address`: the account meant; Microsoft's page goes to it, and another one signed in is said)
    let client = tauri::async_runtime::spawn_blocking(move || {
        eudora_core::graph::GraphClient::sign_in_any(&|url| { let _ = eudora_core::graph::open_browser(url); }, address.as_deref())
    })
    .await;
    // (Android: the sign-in is over, so Eudora may be frozen in the background again)
    #[cfg(target_os = "android")]
    android::keep_answering(false);
    let client = client.map_err(text)?.map_err(text)?;
    let _ = app.with(|l| eudora_core::server_changes::retry_given_up(l));
    // its account at once, so it's under Accounts before the download reaches it
    app.with(|l| l.add_microsoft_account(&client.account, &client.display_name))?;
    Ok(client.account)
}

/// Signs in to a Google account through the browser (Gmail, contacts, calendars, all at once);
/// its account at once, and the download asked to bring its mail in.
#[tauri::command]
async fn google_sign_in(app: tauri::State<'_, App>, address: Option<String>) -> Reply<String> {
    let client = tauri::async_runtime::spawn_blocking(move || {
        eudora_core::google::GoogleClient::sign_in(&|url| { let _ = eudora_core::graph::open_browser(url); }, address.as_deref())
    })
    .await;
    // (Android: the sign-in is over, so Eudora may be frozen in the background again)
    #[cfg(target_os = "android")]
    android::keep_answering(false);
    let client = client.map_err(text)?.map_err(text)?;
    app.with(|l| l.add_google_account(&client.account))?;
    if let Some(path) = &app.path {
        let _ = eudora_core::graph_sync::request_check(path);
    }
    Ok(client.account)
}

/// Whether this copy of Eudora has Google's sign-in built in.
#[tauri::command]
async fn google_available() -> Reply<bool> {
    Ok(eudora_core::google::available())
}

/// Settings whose keys start with `prefix` (the window's own: "ui:").
#[tauri::command]
async fn settings_with_prefix(app: tauri::State<'_, App>, prefix: String) -> Reply<Vec<(String, String)>> {
    app.with(|l| l.settings_with_prefix(&prefix))
}

#[derive(Serialize)]
struct SyncState {
    /// The sync key's id on this computer (None: sync isn't set up here).
    key: Option<String>,
    /// The home account (carries the settings).
    home: String,
    /// Other accounts ticked to carry a copy of the settings.
    carriers: Vec<String>,
    /// The accounts that could carry settings: Microsoft 365 and its shared mailboxes, Gmail,
    /// IMAP (address, shown as).
    mailboxes: Vec<(String, String)>,
}

#[tauri::command]
async fn sync_state(app: tauri::State<'_, App>) -> Reply<SyncState> {
    use eudora_core::sync_run::SETTINGS_ACCOUNTS;
    app.with(|l| {
        // (Microsoft 365, its shared mailboxes, Gmail and IMAP accounts can carry it)
        let accounts = l.accounts_counted(false)?;
        let (home, can) = l.sync_carriers()?;
        let mailboxes: Vec<(String, String)> = can
            .iter()
            .map(|(address, _)| {
                let shown = accounts.iter().find(|a| a.address.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(address))).map(|a| a.shown()).unwrap_or_else(|| address.clone());
                (address.clone(), shown)
            })
            .collect();
        let home = home.unwrap_or_default();
        let carriers = l.setting(SETTINGS_ACCOUNTS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        Ok(SyncState { key: eudora_core::sync_envelope::SyncKey::load().map(|k| k.id()), home, carriers, mailboxes })
    })
}

/// Starts sync on this (first) computer: a new sync key, kept in this computer's credential
/// store. Returns its recovery phrase (shown once; it can be shown again on a computer that has it).
#[tauri::command]
async fn sync_setup() -> Reply<String> {
    use eudora_core::sync_envelope::SyncKey;
    if SyncKey::load().is_some() {
        return Err("This computer already syncs: Show recovery phrase gives the phrase for your other computers.".into());
    }
    let key = SyncKey::generate().map_err(text)?;
    key.store().map_err(text)?;
    Ok(key.phrase())
}

/// Joins sync with the recovery phrase from another computer.
#[tauri::command]
async fn sync_join(phrase: String) -> Reply<String> {
    let key = eudora_core::sync_envelope::SyncKey::from_phrase(&phrase).map_err(text)?;
    key.store().map_err(text)?;
    Ok(key.id())
}

#[tauri::command]
async fn sync_phrase() -> Reply<String> {
    eudora_core::sync_envelope::SyncKey::load().map(|k| k.phrase()).ok_or_else(|| "This computer has no sync key.".to_string())
}

/// The home account and the others that carry a copy of the settings (this computer's choice).
#[tauri::command]
async fn sync_set_carriers(app: tauri::State<'_, App>, home: String, carriers: Vec<String>) -> Reply<()> {
    use eudora_core::sync_run::{HOME_SETTING, SETTINGS_ACCOUNTS};
    app.with(|l| {
        l.set_setting(HOME_SETTING, home.trim())?;
        l.set_setting(SETTINGS_ACCOUNTS, &serde_json::to_string(&carriers)?)
    })
}

/// Syncs now (the running download syncs right after a check; this asks for one), or with
/// `repair`, asks the other computers for a full copy first.
#[tauri::command]
async fn sync_now(app: tauri::State<'_, App>, repair: bool) -> Reply<()> {
    let path = app.path.clone().ok_or("no library")?;
    if repair {
        std::fs::create_dir_all(path.join("status")).map_err(text)?;
        std::fs::write(path.join("status").join("sync-repair"), b"").map_err(text)?;
    }
    eudora_core::graph_sync::request_check(&path).map_err(text)
}

/// Help ▸ Statistics: the copy the download counted in the background, at once; `fresh` (or
/// none kept yet) counts now, on a thread of its own (a few seconds on a large library).
#[tauri::command]
async fn mail_stats(handle: tauri::AppHandle, fresh: Option<bool>) -> Reply<eudora_core::stats::MailStats> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        let app = handle.state::<App>();
        let root = app.path.clone().ok_or_else(|| "No library is open.".to_string())?;
        if !fresh.unwrap_or(false) {
            if let Some(s) = eudora_core::stats::read_counted(&root) {
                return Ok(s);
            }
        }
        let s = app.with_slow(|l| l.mail_stats())?;
        eudora_core::stats::write_counted(&root, &s);
        Ok(s)
    })
    .await
    .map_err(text)?
}

/// The Synthetic Views with their counts: in these mailboxes, or (None) the combined ones.
#[tauri::command]
async fn views_list(handle: tauri::AppHandle, accounts: Option<Vec<i64>>) -> Reply<Vec<eudora_core::views::ViewInfo>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        handle.state::<App>().with_slow(|l| {
            let accounts = match &accounts {
                Some(a) => a.clone(),
                None => l.combined_view_accounts()?,
            };
            l.views(&accounts)
        })
    })
    .await
    .map_err(text)?
}

/// A view's groups by sender (Receipts ▸ Amazon, eBay…), the biggest first.
#[tauri::command]
async fn view_groups(handle: tauri::AppHandle, view: String, accounts: Option<Vec<i64>>) -> Reply<Vec<(String, u64)>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        handle.state::<App>().with_slow(|l| {
            let accounts = match &accounts {
                Some(a) => a.clone(),
                None => l.combined_view_accounts()?,
            };
            l.view_groups(&view, &accounts)
        })
    })
    .await
    .map_err(text)?
}

/// Why a message is (or isn't) in each view: every view's points and reasons.
#[tauri::command]
async fn view_why(handle: tauri::AppHandle, id: i64) -> Reply<Vec<eudora_core::views::ViewScore>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.view_scores_of(id))).await.map_err(text)?
}

/// Mailing lists, notifications and marketing (proven senders only), with how often each writes.
#[tauri::command]
async fn subscriptions(handle: tauri::AppHandle) -> Reply<Vec<eudora_core::subscriptions::Subscription>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.subscriptions())).await.map_err(text)?
}

/// Unsubscribes from one: one-click done here, else the page to open or the address to write.
#[tauri::command]
async fn unsubscribe(handle: tauri::AppHandle, key: String, newest: i64) -> Reply<eudora_core::subscriptions::Outcome> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with(|l| l.unsubscribe(&key, newest))).await.map_err(text)?
}

/// The Address Book: every contact (of one account), those matching the words, by name.
#[tauri::command]
async fn cards_list(handle: tauri::AppHandle, query: String, account: Option<i64>) -> Reply<Vec<eudora_core::cards::CardRow>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_free(|l| l.cards(&query, account))).await.map_err(text)?
}

/// How many contacts each account's address books hold (the sidebar's Contacts).
#[tauri::command]
async fn card_counts(handle: tauri::AppHandle) -> Reply<Vec<(i64, String, String, u64)>> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.card_counts())).await.map_err(text)?
}

/// Where a contact came from, and how it stands with its server.
#[tauri::command]
async fn card_origin(app: tauri::State<'_, App>, id: i64) -> Reply<eudora_core::cards::CardOrigin> {
    app.with(|l| l.card_origin(id))
}

/// Changes a contact (on its server through the queue).
#[tauri::command]
async fn card_update(app: tauri::State<'_, App>, id: i64, card: eudora_core::cards::Card) -> Reply<()> {
    app.with(|l| l.update_card(id, &card))
}

/// A new contact in an account's address book; returns its id.
#[tauri::command]
async fn card_create(app: tauri::State<'_, App>, account: i64, book: Option<String>, card: eudora_core::cards::Card) -> Reply<i64> {
    app.with(|l| l.create_card(account, book.as_deref().unwrap_or(""), &card))
}

/// Share: a draft with the contact attached as a vCard; returns its id (the window opens it).
#[tauri::command]
async fn card_share(app: tauri::State<'_, App>, id: i64) -> Reply<i64> {
    app.with(|l| l.share_card_draft(id))
}

/// Deletes a contact (asked first in the window; on its server through the queue).
#[tauri::command]
async fn card_delete(app: tauri::State<'_, App>, id: i64) -> Reply<()> {
    app.with(|l| l.delete_card(id))
}

/// One contact.
#[tauri::command]
async fn card_get(app: tauri::State<'_, App>, id: i64) -> Reply<eudora_core::cards::CardRow> {
    app.with(|l| l.card(id))
}

/// The Address Book written to a file (vCard or CSV, by its name): how many.
#[tauri::command]
async fn cards_export(handle: tauri::AppHandle, path: String, account: Option<i64>) -> Reply<u64> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with(|l| l.export_cards(std::path::Path::new(path.trim()), account))).await.map_err(text)?
}

/// The mailboxes the combined Views may show: (account, address, shown as, on).
#[tauri::command]
async fn views_accounts(app: tauri::State<'_, App>) -> Reply<Vec<(i64, String, String, bool)>> {
    app.with(|l| l.views_accounts())
}

#[tauri::command]
async fn views_accounts_set(app: tauri::State<'_, App>, addresses: Vec<String>) -> Reply<()> {
    app.with(|l| l.set_views_accounts(&addresses))
}

// ------------------------------------------------------------------ the Out mailbox (Send Later)

/// Sends the queued messages that are due (`all`: every one queued, Send Queued Messages). The
/// library is held only to claim and record each; sending runs without it, so the window stays
/// responsive. Returns what happened, for the status bar.
fn send_queued(app: &App, all: bool) -> Vec<String> {
    if app.path.as_deref().is_some_and(eudora_core::graph_sync::is_offline) {
        return if all { vec!["Working offline: the queued messages wait in Out until you go online (File ▸ Work Offline).".into()] } else { Vec::new() };
    }
    let Ok(claimed) = app.with(|l| l.claim_due(all)) else { return Vec::new() };
    let mut notes = Vec::new();
    for (id, draft) in claimed {
        let label = format!("to {}: {}", draft.to, if draft.subject.is_empty() { "(no subject)" } else { &draft.subject });
        let origin = draft.origin.clone();
        let outcome = app.with(|l| l.prepare_send(draft)).and_then(|o| o.send().map_err(text));
        match outcome {
            Ok(_) => {
                let _ = app.with(|l| l.mark_origin(&origin));
                let _ = app.with(|l| l.sent_queued(id));
                notes.push(format!("Sent {label}"));
            }
            Err(e) => {
                let _ = app.with(|l| l.send_failed(id, &e));
                notes.push(format!("Not sent yet ({e}) {label}; tried again later"));
            }
        }
    }
    notes
}

/// Queues the message to be sent at `send_at` (unix seconds; 0 = with Send Queued Messages).
#[tauri::command]
async fn queue_message(app: tauri::State<'_, App>, draft: eudora_core::compose::Draft, send_at: i64) -> Reply<i64> {
    app.with(|l| l.queue_message(&draft, send_at))
}

#[tauri::command]
async fn outbox_list(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::outbox::Queued>> {
    app.with(|l| l.outbox())
}

/// Takes a message out of the Out mailbox, back into Drafts (to change it); returns its draft id.
#[tauri::command]
async fn outbox_unqueue(app: tauri::State<'_, App>, id: i64) -> Reply<i64> {
    app.with(|l| {
        let draft = l.unqueue(id)?;
        l.save_draft(None, &serde_json::to_string(&draft)?)
    })
}

#[tauri::command]
async fn outbox_reschedule(app: tauri::State<'_, App>, id: i64, send_at: i64) -> Reply<()> {
    app.with(|l| l.reschedule(id, send_at))
}

/// File ▸ Send Queued Messages: everything waiting goes now.
#[tauri::command]
async fn outbox_send_now(handle: tauri::AppHandle) -> Reply<Vec<String>> {
    // (sending waits on the network: on a thread of its own)
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || send_queued(&handle.state::<App>(), true)).await.map_err(text)
}

// ------------------------------------------------------------------ back up / restore

/// A long job's progress (a backup, a restore), polled by the window.
#[derive(Clone, Default, Serialize)]
struct JobState {
    kind: String,
    done: u64,
    total: u64,
    line: String,
    running: bool,
    /// When it ended: what happened, or why it failed.
    result: Option<String>,
    failed: bool,
}

static JOB: Mutex<Option<JobState>> = Mutex::new(None);
static JOB_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct JobProgress;

impl eudora_core::Progress for JobProgress {
    fn report(&self, done: u64, total: u64, stage: &str) {
        if let Ok(mut j) = JOB.lock() {
            if let Some(j) = j.as_mut() {
                (j.done, j.total, j.line) = (done, total, stage.to_string());
            }
        }
    }
    fn canceled(&self) -> bool {
        JOB_CANCEL.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Starts a job on a thread of its own (one at a time).
fn start_job(kind: &str, work: impl FnOnce() -> Result<String, String> + Send + 'static) -> Reply<()> {
    let mut job = JOB.lock().map_err(text)?;
    if job.as_ref().is_some_and(|j| j.running) {
        return Err("A backup or restore is running already.".into());
    }
    JOB_CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
    *job = Some(JobState { kind: kind.into(), running: true, line: "Starting…".into(), ..Default::default() });
    drop(job);
    std::thread::spawn(move || {
        let outcome = work();
        if let Ok(mut j) = JOB.lock() {
            if let Some(j) = j.as_mut() {
                j.running = false;
                j.failed = outcome.is_err();
                j.result = Some(outcome.unwrap_or_else(|e| e));
            }
        }
    });
    Ok(())
}

#[tauri::command]
async fn job_state() -> Reply<Option<JobState>> {
    Ok(JOB.lock().map_err(text)?.clone())
}

#[tauri::command]
async fn job_cancel() -> Reply<()> {
    JOB_CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

#[derive(Serialize)]
struct BackupInfo {
    sizes: eudora_core::backup::Sizes,
    /// A name to start from: Documents\Eudora backup <date>.eudora-backup.
    suggested: String,
}

/// What a backup of this library would hold (sizes by kind), and a suggested file.
#[tauri::command]
async fn backup_info(app: tauri::State<'_, App>) -> Reply<BackupInfo> {
    let root = app.path.clone().ok_or("no library")?;
    let sizes = tauri::async_runtime::spawn_blocking(move || eudora_core::backup::sizes(&root)).await.map_err(text)?.map_err(text)?;
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
    let name = format!("Eudora backup {}.{}", chrono::Local::now().format("%Y-%m-%d"), eudora_core::backup::EXTENSION);
    Ok(BackupInfo { sizes, suggested: home.join("Documents").join(name).display().to_string() })
}

/// Backs up the library (the window's download is paused meanwhile: the backup needs the
/// library to itself, and starts again after).
#[tauri::command]
async fn backup_start(
    app: tauri::State<'_, App>,
    bg: tauri::State<'_, background::Background>,
    dest: String,
    password: Option<String>,
    indexes: bool,
    passwords: bool,
    agreed: String,
    part_gb: Option<f64>,
) -> Reply<()> {
    let root = app.path.clone().ok_or("no library")?;
    let options = eudora_core::backup::BackupOptions {
        dest: PathBuf::from(dest.trim()),
        password: password.filter(|p| !p.is_empty()),
        indexes,
        passwords,
        agreed,
        part_size: part_gb.filter(|g| *g > 0.0).map(|g| (g * 1e9) as u64),
    };
    if let Some(dir) = options.dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Can't make {}: {e}", dir.display()))?;
    }
    bg.hold(true);
    let held = bg.inner().clone();
    start_job("backup", move || {
        eudora_core::Progress::report(&JobProgress, 0, 0, "Pausing the download…");
        // the download lets go of the library once its step ends: moments on a computer (its own
        // program is stopped), longer on a phone (a thread that finishes the request it's in)
        let started = std::time::Instant::now();
        let result = loop {
            match eudora_core::backup::back_up(&root, &options, &JobProgress) {
                Err(eudora_core::Error::Other(e)) if e.contains("being written") && started.elapsed() < std::time::Duration::from_secs(300) && !eudora_core::Progress::canceled(&JobProgress) => {
                    eudora_core::Progress::report(&JobProgress, 0, 0, &format!("Pausing the download ({} s)…", started.elapsed().as_secs()));
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                other => break other,
            }
        };
        held.hold(false);
        match result {
            Ok(r) => Ok(format!(
                "Backed up {} files ({:.1} GB){} into {}.",
                r.files,
                r.bytes as f64 / 1e9,
                if r.secrets > 0 { format!(" and {} passwords", r.secrets) } else { String::new() },
                r.parts.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )),
            Err(eudora_core::Error::Canceled) => Err("The backup was stopped; what it had written is incomplete (delete it).".into()),
            Err(e) => Err(format!("The backup didn't finish: {e}")),
        }
    })
}

// ------------------------------------------------------------------ the local API (Programs and plugins)

#[derive(Serialize)]
struct ApiOverview {
    settings: eudora_core::api::ApiSettings,
    state: String,
    clients: Vec<eudora_core::api::Client>,
    requests: Vec<eudora_core::api::Request>,
    sends: Vec<eudora_core::api::PendingSend>,
}

/// Everything Settings ▸ Programs and plugins shows, and what waits for the user.
#[tauri::command]
async fn api_overview(app: tauri::State<'_, App>, host: tauri::State<'_, api_host::ApiHost>) -> Reply<ApiOverview> {
    let state = host.state.lock().map(|s| s.clone()).unwrap_or_default();
    app.with(|l| Ok(ApiOverview { settings: l.api_settings()?, state, clients: l.api_clients()?, requests: l.api_requests()?, sends: l.api_pending_sends()? }))
}

/// Only what waits for the user (asked every few seconds).
#[tauri::command]
async fn api_waiting(app: tauri::State<'_, App>) -> Reply<(Vec<eudora_core::api::Request>, Vec<eudora_core::api::PendingSend>)> {
    app.with(|l| Ok((l.api_requests()?, l.api_pending_sends()?)))
}

/// The user's answer to a program asking for access: what they ticked, or None (refused).
#[tauri::command]
async fn api_answer(app: tauri::State<'_, App>, request: i64, grants: Option<Vec<eudora_core::api::Grant>>) -> Reply<()> {
    app.with(|l| l.api_answer(request, grants.as_deref()))
}

#[tauri::command]
async fn api_answer_send(app: tauri::State<'_, App>, send: i64, ok: bool, trust: bool) -> Reply<()> {
    app.with(|l| l.api_answer_send(send, ok, trust))
}

#[tauri::command]
async fn api_revoke(app: tauri::State<'_, App>, client: i64) -> Reply<()> {
    app.with(|l| l.api_revoke(client))
}

#[tauri::command]
async fn api_set_trust(app: tauri::State<'_, App>, client: i64, log: eudora_core::api::LogLevel, send_without_asking: bool) -> Reply<()> {
    app.with(|l| l.api_set_trust(client, log, send_without_asking))
}

#[tauri::command]
async fn api_log(app: tauri::State<'_, App>, client: Option<i64>, limit: Option<usize>) -> Reply<Vec<eudora_core::api::LogLine>> {
    app.with(|l| l.api_log(client, limit.unwrap_or(500)))
}

/// Where the API is served; turning "while open" on or off starts or stops it at once.
#[tauri::command]
async fn api_settings_set(app: tauri::State<'_, App>, host: tauri::State<'_, api_host::ApiHost>, handle: tauri::AppHandle, settings: eudora_core::api::ApiSettings) -> Reply<()> {
    app.with(|l| l.set_api_settings(&settings))?;
    if settings.with_window { host.start(handle) } else { host.stop(&handle) }
    Ok(())
}

// ------------------------------------------------------------------ plugins (Tools ▸ Plugins)

#[tauri::command]
async fn plugins_list(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::plugins::Installed>> {
    app.with(|l| l.plugins())
}

/// Installs a plugin file; the grant window then asks the user what it may do.
#[tauri::command]
async fn plugin_install(app: tauri::State<'_, App>, path: String) -> Reply<eudora_core::plugins::Manifest> {
    app.with(|l| l.plugin_install(std::path::Path::new(path.trim())))
}

#[tauri::command]
async fn plugin_enable(app: tauri::State<'_, App>, id: String, on: bool) -> Reply<()> {
    app.with(|l| l.plugin_enable(&id, on))
}

#[tauri::command]
async fn plugin_remove(app: tauri::State<'_, App>, id: String) -> Reply<()> {
    app.with(|l| l.plugin_remove(&id))
}

/// Runs a plugin command with the selection (`context`: {ids, folder}).
#[tauri::command]
async fn plugin_run(handle: tauri::AppHandle, id: String, command: String, context: serde_json::Value) -> Reply<eudora_core::plugins::Output> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with(|l| l.plugin_run(&id, &command, &context))).await.map_err(|e| e.to_string())?
}

/// The system's Open dialog: `what` "file", "files" or "folder"; `kind` "mail" filters to
/// mailbox and message files. Nothing chosen = an empty list.
#[tauri::command]
async fn pick_paths(window: tauri::Window, what: String, title: String, kind: Option<String>) -> Reply<Vec<String>> {
    use eudora_core::pick::Pick;
    let what = match what.as_str() {
        "folder" => Pick::Folder,
        "files" => Pick::Files,
        _ => Pick::File,
    };
    let filters: &[(&str, &str)] = match kind.as_deref() {
        Some("mail") => &[("Mail (mailboxes, messages)", "*.mbx;*.mbox;*.eml;*.txt"), ("Every file", "*.*")],
        Some("backup") => &[("Eudora backups", "*.eudora-backup"), ("Every file", "*.*")],
        _ => &[],
    };
    #[cfg(windows)]
    let owner = window.hwnd().ok().map(|h| h.0 as isize);
    #[cfg(not(windows))]
    let owner = None;
    // the Mac shows the panel only from the main thread: run there, wait for it here
    #[cfg(target_os = "macos")]
    let chosen = {
        let (tx, rx) = std::sync::mpsc::channel();
        window
            .run_on_main_thread(move || {
                let _ = tx.send(eudora_core::pick::pick(owner, what, &title, filters));
            })
            .map_err(|e| e.to_string())?;
        tauri::async_runtime::spawn_blocking(move || rx.recv())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|_| "the Open dialog stopped unexpectedly".to_string())?
            .map_err(text)?
    };
    #[cfg(not(target_os = "macos"))]
    let chosen = {
        #[cfg(not(windows))]
        let _ = &window;
        tauri::async_runtime::spawn_blocking(move || eudora_core::pick::pick(owner, what, &title, filters)).await.map_err(|e| e.to_string())?.map_err(text)?
    };
    Ok(chosen.into_iter().map(|p| p.display().to_string()).collect())
}

/// File ▸ Open File / Import…: mailbox files (classic .mbx, mbox), messages (.eml) or folders
/// of them, a classic Eudora folder, into the library (a job; the download is held meanwhile).
/// `account`: whose mail (an address; default: a classic folder's own); `merge`: when that
/// address has an account already, add to it (true) or keep apart (false); None stops and asks.
#[tauri::command]
async fn import_files(
    app: tauri::State<'_, App>,
    bg: tauri::State<'_, background::Background>,
    paths: Vec<String>,
    into: Option<String>,
    account: Option<String>,
    merge: Option<bool>,
) -> Reply<()> {
    let root = app.path.clone().ok_or("no library")?;
    let paths: Vec<PathBuf> = paths.iter().map(|p| PathBuf::from(p.trim())).filter(|p| !p.as_os_str().is_empty()).collect();
    if paths.is_empty() {
        return Err("Choose what to import.".into());
    }
    for p in &paths {
        if !p.exists() {
            return Err(format!("{} isn't there.", p.display()));
        }
    }
    bg.hold(true);
    let held = bg.inner().clone();
    start_job("import", move || {
        let mut tries = 0;
        let opened = loop {
            match eudora_core::Library::open(&root, Access::Write, &JobProgress) {
                Err(eudora_core::Error::Locked(_)) if tries < 60 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                other => break other,
            }
        };
        let options = eudora_core::ImportOptions {
            destination: into.filter(|d| !d.trim().is_empty()),
            account: account.filter(|a| !a.trim().is_empty()),
            merge_account: merge,
            ..Default::default()
        };
        let result = opened.and_then(|mut l| {
            let r = eudora_core::import(&mut l, &paths, &options, &JobProgress);
            l.close(&JobProgress)?;
            r
        });
        held.hold(false);
        let r = result.map_err(text)?;
        let c = &r.counts;
        Ok(format!(
            "Imported into {}: {} messages read, {} new, {} already here, {} merged with copies from elsewhere; {} read with something fixed, {} unreadable (kept as text, searchable).",
            r.destination,
            c.seen,
            c.imported,
            c.duplicates + c.new_locations,
            c.merged,
            c.fixed,
            c.unreadable
        ))
    })
}

/// Tools ▸ Activity (classic Eudora's Task Status): what Eudora is doing now.
#[derive(Serialize)]
struct Activity {
    download: Option<DownloadStatus>,
    moves_waiting: u64,
    changes_waiting: u64,
    given_up: u64,
    last_error: Option<String>,
    out_waiting: u64,
    offline: bool,
    job: Option<JobState>,
    /// The download log's last lines.
    log: Vec<String>,
    /// What the download is doing in the background, job by job.
    background: Option<eudora_core::activity::Activity>,
}

/// Unread messages per folder (the sidebar's bold numbers), counted on the second connection:
/// a few seconds, so the window asks it lazily.
#[tauri::command]
async fn unread_counts(handle: tauri::AppHandle) -> Reply<Vec<(i64, u64)>> {
    let _timed = eudora_core::speed::Timer::new("Unread counts (background)");
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || handle.state::<App>().with_slow(|l| l.unread_by_folder())).await.map_err(text)?
}

/// Mark All as Read on folders (with everything under them): the unread found on the second
/// connection, then marked 2,000 at a time, the library let go between batches so nothing
/// else waits long; the servers are told by the download. Returns how many were marked.
#[tauri::command]
async fn mark_folders_read(handle: tauri::AppHandle, folders: Vec<i64>) -> Reply<u64> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || -> Reply<u64> {
        let app = handle.state::<App>();
        let ids = app.with_slow(|l| l.unread_in(&folders))?;
        let mut marked = 0;
        for chunk in ids.chunks(2_000) {
            marked += app.with(|l| l.set_status(chunk, eudora_core::marks::Status::Read))?;
        }
        Ok(marked)
    })
    .await
    .map_err(text)?
}

/// What the download is doing in the background, job by job (the status bar asks every few
/// seconds: only the file the download writes is read).
#[tauri::command]
fn background_jobs(app: tauri::State<'_, App>) -> Option<eudora_core::activity::Activity> {
    app.path.as_deref().and_then(eudora_core::activity::read)
}

#[tauri::command]
async fn activity(app: tauri::State<'_, App>) -> Reply<Activity> {
    let root = app.path.clone().ok_or("no library")?;
    let (moves_waiting, given_up, last_error, changes_waiting, marks_given_up, out_waiting) = app.with(|l| {
        let (m, g, e) = eudora_core::server_changes::waiting(l)?;
        let (c, cg) = eudora_core::server_changes::waiting_marks(l)?;
        Ok((m, g, e, c, cg, l.outbox()?.len() as u64))
    })?;
    let log = std::fs::read(root.join("logs").join("download.log"))
        .map(|b| {
            let text = String::from_utf8_lossy(&b[b.len().saturating_sub(64 * 1024)..]).into_owned();
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            lines[lines.len().saturating_sub(40)..].to_vec()
        })
        .unwrap_or_default();
    Ok(Activity {
        download: download_status(app.clone()).await?,
        moves_waiting,
        changes_waiting,
        given_up: given_up + marks_given_up,
        last_error,
        out_waiting,
        offline: eudora_core::graph_sync::is_offline(&root),
        job: job_state().await?,
        log,
        background: eudora_core::activity::read(&root),
    })
}

/// Special ▸ Optimize Library: gives back the space of deleted mail, merges the search index,
/// tidies the database (`vacuum`: rewrites it too). The window's download is held meanwhile.
#[tauri::command]
async fn optimize_library(app: tauri::State<'_, App>, bg: tauri::State<'_, background::Background>, vacuum: bool) -> Reply<()> {
    let root = app.path.clone().ok_or("no library")?;
    bg.hold(true);
    let held = bg.inner().clone();
    start_job("optimize", move || {
        let mut tries = 0;
        let opened = loop {
            match eudora_core::Library::open(&root, Access::Write, &JobProgress) {
                Err(eudora_core::Error::Locked(_)) if tries < 60 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                other => break other,
            }
        };
        let result = opened.and_then(|l| {
            let r = l.optimize(vacuum, &JobProgress);
            l.close(&JobProgress)?;
            r
        });
        held.hold(false);
        let r = result.map_err(text)?;
        let mb = |b: u64| b as f64 / 1e6;
        Ok(format!(
            "Optimized: {:.0} MB of deleted mail given back ({} packs rewritten, {} removed, {} messages moved); search index merged from {} parts; database {:.0} MB → {:.0} MB.",
            mb(r.bytes_freed),
            r.packs_rewritten,
            r.packs_removed,
            r.records_moved,
            r.index_segments,
            mb(r.database_before),
            mb(r.database_after)
        ))
    })
}

/// The accounts this computer has no sign-in or password for yet (after a restore), so the
/// window can ask for them.
#[tauri::command]
async fn missing_sign_ins(app: tauri::State<'_, App>) -> Reply<Vec<eudora_core::signins::MissingSignIn>> {
    app.with(|l| l.missing_sign_ins())
}

/// Special ▸ Forget Sign-ins: what would be forgotten, then (`forget`) forgets it.
#[tauri::command]
async fn sign_ins(app: tauri::State<'_, App>, forget: bool) -> Reply<Vec<String>> {
    app.with(|l| if forget { l.forget_sign_ins() } else { Ok(l.sign_ins()?.into_iter().map(|s| s.what).collect()) })
}

/// File ▸ Work Offline: whether it's on; `on` Some changes it (going online sends what waits
/// in Out and checks for mail).
#[tauri::command]
async fn offline(app: tauri::State<'_, App>, handle: tauri::AppHandle, on: Option<bool>) -> Reply<bool> {
    use tauri::Manager;
    let root = app.path.clone().ok_or("no library")?;
    if let Some(on) = on {
        eudora_core::graph_sync::set_offline(&root, on).map_err(text)?;
        if !on {
            let _ = eudora_core::graph_sync::request_check(&root);
            std::thread::spawn(move || send_queued(&handle.state::<App>(), false));
        }
    }
    Ok(eudora_core::graph_sync::is_offline(&root))
}

/// Looks for classic Eudora's attachment files still missing (in the classic folder and the
/// folders given) and stores them in the library. The window's download is held meanwhile:
/// this writes to the library.
#[tauri::command]
async fn find_old_attachments(app: tauri::State<'_, App>, bg: tauri::State<'_, background::Background>, folders: Vec<String>) -> Reply<()> {
    let root = app.path.clone().ok_or("no library")?;
    let folders: Vec<PathBuf> = folders.iter().map(|f| PathBuf::from(f.trim())).filter(|f| !f.as_os_str().is_empty()).collect();
    for f in &folders {
        if !f.is_dir() {
            return Err(format!("{} isn't a folder here.", f.display()));
        }
    }
    bg.hold(true);
    let held = bg.inner().clone();
    start_job("attachments", move || {
        let mut tries = 0;
        let opened = loop {
            match eudora_core::Library::open(&root, Access::Write, &JobProgress) {
                Err(eudora_core::Error::Locked(_)) if tries < 60 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                other => break other,
            }
        };
        let result = opened.and_then(|l| {
            let r = l.find_old_attachments(&folders, &JobProgress);
            l.close(&JobProgress)?;
            r
        });
        held.hold(false);
        match result {
            Ok(r) => Ok(format!(
                "Old attachments: found {} on disk and {} in other copies of their messages ({:.1} MB stored); {} of {} still missing{}.",
                r.found_on_disk,
                r.found_in_copies,
                r.stored_bytes as f64 / 1e6,
                r.missing,
                r.references,
                if r.ambiguous > 0 { format!(" ({} have several different files of that name: none taken)", r.ambiguous) } else { String::new() }
            )),
            Err(eudora_core::Error::Canceled) => Err("Stopped; what was found so far is kept.".into()),
            Err(e) => Err(format!("Looking for old attachments didn't finish: {e}")),
        }
    })
}

#[derive(Serialize)]
struct BackupDescription {
    created: i64,
    encrypted: bool,
    indexes: bool,
    passwords: bool,
    sizes: eudora_core::backup::Sizes,
    parts: usize,
    /// "phone" when a phone made it (Restore warns), else empty.
    device: String,
    /// The phone's download limit then (`download-period` as stored), else empty.
    limit: String,
}

/// What a backup holds (before restoring it).
#[tauri::command]
async fn restore_describe(path: String) -> Reply<BackupDescription> {
    let (h, parts) = eudora_core::backup::describe(std::path::Path::new(path.trim())).map_err(text)?;
    Ok(BackupDescription { created: h.created, encrypted: h.encrypted, indexes: h.indexes, passwords: h.passwords, sizes: h.sizes, parts: parts.len(), device: h.device, limit: h.limit })
}

#[derive(Serialize)]
struct FoundBackup {
    path: String,
    name: String,
    /// All its parts together (bytes).
    size: u64,
    modified: i64,
}

/// A phone's backups: the folder the Files app shows as Eudora's (iOS: On My iPhone ▸ Eudora;
/// a computer's backup is copied there to restore it, and the phone's own backups are made
/// there), and the backups in it (the first part of each when split).
#[tauri::command]
async fn phone_backups(app: tauri::AppHandle) -> Reply<serde_json::Value> {
    use tauri::Manager;
    let folder = app.path().document_dir().map_err(text)?;
    let _ = std::fs::create_dir_all(&folder);
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&folder).map_err(text)?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(&format!(".{}", eudora_core::backup::EXTENSION)) || (name.contains(".part") && !name.contains(".part001.")) {
            continue;
        }
        let parts = eudora_core::backup::backup_parts(&path).unwrap_or_else(|_| vec![path.clone()]);
        let size = parts.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        let modified = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        found.push(FoundBackup { path: path.display().to_string(), name, size, modified });
    }
    found.sort_by(|a, b| b.modified.cmp(&a.modified));
    let name = format!("Eudora backup {}.{}", chrono::Local::now().format("%Y-%m-%d %H.%M"), eudora_core::backup::EXTENSION);
    Ok(serde_json::json!({ "folder": folder.display().to_string(), "suggested": folder.join(name).display().to_string(), "backups": found }))
}

/// Copies one stream to another a megabyte at a time, counting ("Copying the backup: 120 of 900
/// MB"), and stops when the job is stopped. `before`: what earlier files of the same job copied.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn copy_counted(from: &mut impl std::io::Read, to: &mut impl std::io::Write, before: u64, total: u64, what: &str) -> Result<u64, String> {
    use eudora_core::Progress;
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        if JobProgress.canceled() {
            return Err("Stopped.".into());
        }
        let n = from.read(&mut buf).map_err(text)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).map_err(text)?;
        done += n as u64;
        let mb = |b: u64| (b as f64 / 1e6).round() as u64;
        let line = if total > 0 { format!("{what}: {} of {} MB", mb(before + done), mb(total)) } else { format!("{what}: {} MB", mb(before + done)) };
        JobProgress.report(before + done, total, &line);
    }
    to.flush().map_err(text)?;
    Ok(done)
}

/// The folder of the app's cache that holds a backup picked to restore (Android).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn picked_backup_folder(app: &tauri::AppHandle) -> Reply<PathBuf> {
    use tauri::Manager;
    Ok(app.path().app_cache_dir().map_err(text)?.join("picked-backup"))
}

/// Android: a backup picked with Android's file picker (from Downloads, Drive, Files…; every part
/// of one in parts) is copied into the app's cache, as a job (counted, can be stopped) whose
/// result is the copy's path (its first part), for Restore. False when the person canceled.
/// (iOS shows Eudora's folder in the Files app instead; computers pick by path.)
#[tauri::command]
async fn backup_pick(app: tauri::AppHandle) -> Reply<bool> {
    #[cfg(target_os = "android")]
    {
        let picked = tauri::async_runtime::spawn_blocking(android::pick_backups).await.map_err(text)??;
        if picked.is_empty() {
            return Ok(false);
        }
        let folder = picked_backup_folder(&app)?;
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).map_err(text)?;
        start_job("copy", move || {
            let total: u64 = picked.iter().filter_map(|p| p.size).sum();
            let (count, mut before, mut first) = (picked.len(), 0u64, None);
            for (i, mut p) in picked.into_iter().enumerate() {
                // (only the file's own name: never a path out of the folder)
                let name = std::path::Path::new(&p.name).file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).unwrap_or_else(|| format!("backup-{}", i + 1));
                let path = folder.join(&name);
                let what = if count > 1 { format!("Copying the backup (file {} of {count})", i + 1) } else { "Copying the backup".to_string() };
                before += copy_counted(&mut p.file, &mut std::fs::File::create(&path).map_err(text)?, before, total, &what)?;
                if first.is_none() || name.contains(".part001.") {
                    first = Some(path);
                }
            }
            Ok(first.map(|p| p.display().to_string()).unwrap_or_default())
        })?;
        Ok(true)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Picking a backup this way is only on Android.".into())
    }
}

/// Android: the copy `backup_pick` made, deleted once it's restored (or not wanted).
#[tauri::command]
async fn backup_picked_clear(app: tauri::AppHandle) -> Reply<()> {
    let _ = std::fs::remove_dir_all(picked_backup_folder(&app)?);
    Ok(())
}

/// Android: a copy of one of the phone's backups saved where the person picks (Downloads, Drive,
/// Files…), out of the app's own folder, which uninstalling Eudora deletes. A job (counted, can
/// be stopped); false when the person canceled.
#[tauri::command]
async fn backup_save_copy(path: String) -> Reply<bool> {
    #[cfg(target_os = "android")]
    {
        let source = PathBuf::from(path.trim());
        let parts = eudora_core::backup::backup_parts(&source).unwrap_or_else(|_| vec![source.clone()]);
        if parts.len() != 1 {
            return Err("That backup is in parts: copy them over USB instead.".into());
        }
        let name = source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let Some(mut dest) = tauri::async_runtime::spawn_blocking(move || android::save_as(&name)).await.map_err(text)?? else { return Ok(false) };
        let started = start_job("copy", move || {
            let copied = (|| {
                let mut from = std::fs::File::open(&source).map_err(text)?;
                let total = from.metadata().map(|m| m.len()).unwrap_or(0);
                let done = copy_counted(&mut from, &mut dest, 0, total, "Saving a copy")?;
                dest.sync_all().map_err(text)?;
                Ok(format!("Saved a copy ({} MB).", (done as f64 / 1e6).round() as u64))
            })();
            // (closed here first, then Android's own handle: that's when Downloads or Drive sees it whole)
            drop(dest);
            android::saved();
            copied
        });
        if started.is_err() {
            android::saved();
        }
        started?;
        Ok(true)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = path;
        Err("Saving a copy this way is only on Android.".into())
    }
}

/// On a phone, the library a restore replaced is deleted once the restored one opens (a phone
/// has room for one): its folder, written when the restore finished.
const RETIRED_LIBRARY: &str = "retired-library.txt";

/// At start, on a phone: deletes the library a restore replaced, now that another one opens.
#[cfg(mobile)]
fn delete_retired_library(opening: &std::path::Path) {
    let Some(dir) = eudora_core::library::app_data_dir() else { return };
    let marker = dir.join(RETIRED_LIBRARY);
    let Ok(old) = std::fs::read_to_string(&marker) else { return };
    let old = PathBuf::from(old.trim());
    // (an iOS app's folder moves when the app is updated: the same name in the folder it has now)
    let old = if old.exists() { old } else { old.file_name().map(|n| dir.join(n)).unwrap_or(old) };
    if !same_folder(Some(opening), &old) && old.join("eudora.db").exists() && old.starts_with(&dir) {
        let _ = std::fs::remove_dir_all(&old);
    }
    let _ = std::fs::remove_file(marker);
}

/// Switch now (after a restore): Eudora starts again on that library. (A library named on the
/// command line starts its own window, so the new one doesn't hand over to this one.)
#[tauri::command]
async fn library_switch(app: tauri::AppHandle, path: String) -> Reply<()> {
    let path = PathBuf::from(path.trim());
    if !path.join("eudora.db").is_file() {
        return Err(format!("{} isn't an Eudora library.", path.display()));
    }
    let exe = std::env::current_exe().map_err(text)?;
    std::process::Command::new(exe).arg(&path).spawn().map_err(text)?;
    app.exit(0);
    Ok(())
}

/// Where Restore puts the library unless another folder is chosen: the usual place (or beside
/// it, when it holds a library already).
#[tauri::command]
async fn restore_suggested() -> Reply<String> {
    Ok(eudora_core::library::restore_target().map(|p| p.display().to_string()).unwrap_or_default())
}

/// What a restore's destination holds: empty, a Eudora library (Restore asks before replacing
/// it), or other files (never replaced). The library this window has open counts as in use.
#[tauri::command]
async fn restore_destination(state: tauri::State<'_, App>, dest: String) -> Reply<serde_json::Value> {
    let dest = PathBuf::from(dest.trim());
    // the library open here can be replaced too: restored beside it, swapped in at the restart
    let open = same_folder(state.path.as_deref(), &dest);
    let there = tauri::async_runtime::spawn_blocking(move || eudora_core::backup::destination(&dest)).await.map_err(text)?.map_err(text)?;
    let mut value = serde_json::to_value(there).map_err(text)?;
    if open {
        value["kind"] = "open".into();
    }
    Ok(value)
}

fn same_folder(open: Option<&std::path::Path>, other: &std::path::Path) -> bool {
    let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    open.is_some_and(|o| real(o) == real(other))
}

/// Restores a backup into a folder, which becomes the library Eudora opens from then on
/// (`library::restore_as_library`; the window then offers to switch to it). `replace`: the user
/// agreed to replace the Eudora library already there (it's emptied first).
#[tauri::command]
async fn restore_start(state: tauri::State<'_, App>, path: String, dest: String, password: Option<String>, replace: Option<bool>) -> Reply<()> {
    let (backup, dest) = (PathBuf::from(path.trim()), PathBuf::from(dest.trim()));
    if same_folder(state.path.as_deref(), &dest) {
        if replace != Some(true) {
            return Err("That's the library Eudora has open: say to replace it, or restore into another folder.".into());
        }
        if let Some(service) = eudora_core::library::in_sync_folder(&dest) {
            return Err(format!("That folder is inside {service}: a library there gets damaged while {service} copies its files. Choose a folder outside it."));
        }
        return restore_over_open(backup, dest, password);
    }
    if let Some(service) = eudora_core::library::in_sync_folder(&dest) {
        return Err(format!("That folder is inside {service}: a library there gets damaged while {service} copies its files. Choose a folder outside it."));
    }
    let open = state.path.clone();
    start_job("restore", move || {
        match eudora_core::library::restore_as_library(&backup, Some(&dest), password.as_deref().filter(|p| !p.is_empty()), replace == Some(true), &JobProgress) {
            Ok((dest, r)) => {
                // a phone: the library this replaces goes once the restored one opens
                if cfg!(mobile) {
                    if let (Some(dir), Some(old)) = (eudora_core::library::app_data_dir(), &open) {
                        let _ = std::fs::write(dir.join(RETIRED_LIBRARY), old.display().to_string());
                    }
                }
                Ok(format!(
                    "Restored {} files ({:.1} GB){} into {}: Eudora's library from now on. Microsoft 365 accounts sign in again there (Settings ▸ Accounts).",
                    r.files,
                    r.bytes as f64 / 1e9,
                    if r.secrets > 0 { format!(" and {} passwords", r.secrets) } else { String::new() },
                    dest.display()
                ))
            }
            Err(eudora_core::Error::Canceled) => Err("The restore was stopped; the folder holds an incomplete library (restoring there again replaces it).".into()),
            Err(e) => Err(format!("The restore didn't finish: {e}")),
        }
    })
}

/// Restore over the library this window has open: into the folder beside it
/// (`library::staging_for`; one a stopped attempt left is replaced), then noted so the next
/// start swaps it into the library's place (`library::finish_replacement`). The open library is
/// untouched until then.
fn restore_over_open(backup: PathBuf, dest: PathBuf, password: Option<String>) -> Reply<()> {
    let staged = eudora_core::library::staging_for(&dest);
    let leftover = matches!(eudora_core::backup::destination(&staged).map_err(text)?, eudora_core::backup::Destination::Library { .. });
    if !leftover && staged.exists() && std::fs::read_dir(&staged).map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!("{} has other files in it: move them away first (Eudora restores there before replacing the library).", staged.display()));
    }
    start_job("restore", move || {
        let restored = eudora_core::library::restore_as_library(&backup, Some(&staged), password.as_deref().filter(|p| !p.is_empty()), leftover, &JobProgress)
            .and_then(|(staged, r)| eudora_core::library::replace_at_next_start(&staged, &dest).map(|_| r));
        match restored {
            Ok(r) => Ok(format!(
                "Restored {} files ({:.1} GB){}. It takes the place of the library in {} when Eudora starts again (the old one is deleted then). Microsoft 365 accounts sign in again there (Settings ▸ Accounts).",
                r.files,
                r.bytes as f64 / 1e9,
                if r.secrets > 0 { format!(" and {} passwords", r.secrets) } else { String::new() },
                dest.display()
            )),
            Err(eudora_core::Error::Canceled) => Err(format!("The restore was stopped. Your library is as it was; {} holds an incomplete copy (restoring again replaces it).", staged.display())),
            Err(e) => Err(format!("The restore didn't finish: {e}. Your library is as it was.")),
        }
    })
}

#[tauri::command]
async fn pending_changes(app: tauri::State<'_, App>) -> Reply<Vec<(String, u64)>> {
    app.with(|l| l.pending_changes())
}

/// One original copy's bytes as text (an Exchange export is binary: its size and first bytes).
#[tauri::command]
async fn copy_source(app: tauri::State<'_, App>, copy: i64, format: String) -> Reply<String> {
    let raw = app.with(|l| l.copy_bytes(copy))?;
    if format != "mime" {
        let head: Vec<String> = raw.iter().take(256).map(|b| format!("{b:02x}")).collect();
        return Ok(format!(
            "Exchange's own export of this item ({} bytes, binary: the MAPI properties Eudora's copy was made from).\n\nFirst bytes:\n{}",
            raw.len(),
            head.chunks(32).map(|c| c.join(" ")).collect::<Vec<_>>().join("\n")
        ));
    }
    let shown = &raw[..raw.len().min(SOURCE_LIMIT)];
    let mut out = eudora_core::parse::decode_8bit(shown).into_owned();
    if raw.len() > SOURCE_LIMIT {
        out.push_str(&format!("\n\n[… {} more bytes not shown]", raw.len() - SOURCE_LIMIT));
    }
    Ok(out)
}

#[derive(Serialize)]
struct DownloadStatus {
    #[serde(flatten)]
    status: eudora_core::graph_sync::Status,
    /// Seconds since the download last wrote it (a running one writes every 2 s).
    age: i64,
    /// A download holds the library's write lock: it's running, even when a long step keeps it
    /// from writing its status.
    alive: bool,
}

/// How the last mailbox download is doing (None: none ever ran on this library).
#[tauri::command]
async fn download_status(app: tauri::State<'_, App>) -> Reply<Option<DownloadStatus>> {
    let Some(path) = &app.path else { return Ok(None) };
    Ok(eudora_core::graph_sync::read_status(path).map(|status| {
        let age = eudora_core::library::now() - status.updated;
        // (only asked when the status is old: a lock check is a file open)
        let alive = age > 60 && crate::background::writer_elsewhere(path);
        DownloadStatus { status, age, alive }
    }))
}

/// A mailbox opened in the window: the running download fetches its folders' newest mail now.
#[tauri::command]
async fn look_now(app: tauri::State<'_, App>, folders: Vec<i64>) -> Reply<()> {
    let Some(path) = &app.path else { return Ok(()) };
    eudora_core::graph_sync::request_look(path, &folders).map_err(text)
}

/// Check Mail: asks the running download to look for new mail in every mailbox now (and to
/// bring in accounts added since it started). Returns whether a download is running to ask.
#[tauri::command]
async fn check_mail(app: tauri::State<'_, App>) -> Reply<bool> {
    let Some(path) = &app.path else { return Ok(false) };
    eudora_core::graph_sync::request_check(path).map_err(text)?;
    let running = eudora_core::graph_sync::read_status(path)
        .is_some_and(|s| eudora_core::library::now() - s.updated < 90);
    Ok(running)
}

/// Pause: "old" (old mail waits, new mail still comes in), "all" (nothing is downloaded) or
/// "none" (resume). The running download picks it up within a second.
#[tauri::command]
async fn set_pause(app: tauri::State<'_, App>, mode: String) -> Reply<()> {
    use eudora_core::graph_sync::Pause;
    let Some(path) = &app.path else { return Ok(()) };
    let pause = match mode.as_str() {
        "old" => Pause::Old,
        "all" => Pause::All,
        _ => Pause::None,
    };
    eudora_core::graph_sync::set_pause(path, pause).map_err(text)
}

#[tauri::command]
async fn get_pause(app: tauri::State<'_, App>) -> Reply<eudora_core::graph_sync::Pause> {
    Ok(app.path.as_deref().map(eudora_core::graph_sync::read_pause).unwrap_or_default())
}

// ------------------------------------------------------------------ web access (the firewall)

/// Whether phones may reach `eudora-web` through this computer's firewall (firewall.rs).
#[tauri::command]
async fn firewall_status() -> Reply<firewall::FirewallStatus> {
    tauri::async_runtime::spawn_blocking(firewall::status).await.map_err(text)?
}

/// Opens (or closes) eudora-web's port for this local network (Windows asks the user).
#[tauri::command]
async fn firewall_set(port: u16, allow: bool) -> Reply<firewall::FirewallStatus> {
    tauri::async_runtime::spawn_blocking(move || firewall::set(port, allow)).await.map_err(text)?
}

/// Which system this is ("windows", "macos", "linux"): the window words a few things for it
/// (where passwords are kept, the firewall).
#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

/// The message as stored, byte for byte, shown as text.
#[tauri::command]
async fn source(app: tauri::State<'_, App>, id: i64) -> Reply<String> {
    let raw = app.with(|l| l.raw_message(id))?;
    let shown = &raw[..raw.len().min(SOURCE_LIMIT)];
    let mut out = eudora_core::parse::decode_8bit(shown).into_owned();
    if raw.len() > SOURCE_LIMIT {
        out.push_str(&format!("\n\n[… {} more bytes not shown]", raw.len() - SOURCE_LIMIT));
    }
    Ok(out)
}

/// What the background download is doing (Settings ▸ Downloading and sync), and its options.
#[tauri::command]
async fn background_state(app: tauri::State<'_, App>, bg: tauri::State<'_, background::Background>) -> Reply<(background::State, background::Options)> {
    let opts = background::Options::read(app.with(|l| l.setting(background::SETTING)).ok().flatten());
    Ok((bg.state.lock().map_err(text)?.clone(), opts))
}

/// New options: saved, and the running download is started again with them.
#[tauri::command]
async fn background_set(app: tauri::State<'_, App>, bg: tauri::State<'_, background::Background>, options: background::Options) -> Reply<()> {
    let json = serde_json::to_string(&options).map_err(text)?;
    app.with(|l| l.set_setting(background::SETTING, &json))?;
    bg.options_changed();
    Ok(())
}

/// Memory for speed: the choice (normal / more / most), the computer's memory, and what each
/// choice uses.
#[tauri::command]
async fn memory_get(app: tauri::State<'_, App>) -> Reply<serde_json::Value> {
    let choice = app.with_free(|l| l.setting(eudora_core::db::MEMORY))?.and_then(|v| serde_json::from_str::<String>(&v).ok()).unwrap_or_else(|| "normal".into());
    let sizes: serde_json::Map<String, serde_json::Value> = ["normal", "more", "most"]
        .iter()
        .map(|c| {
            let (cache, mapped) = eudora_core::db::memory_sizes(c);
            (c.to_string(), serde_json::json!({ "cache": cache, "mapped": mapped }))
        })
        .collect();
    Ok(serde_json::json!({ "choice": choice, "total": eudora_core::db::total_memory(), "sizes": sizes }))
}

/// Changes the memory choice: the window's connections at once, the download restarted with it.
#[tauri::command]
async fn memory_set(app: tauri::State<'_, App>, bg: tauri::State<'_, background::Background>, choice: String) -> Reply<()> {
    if !["normal", "more", "most"].contains(&choice.as_str()) {
        return Err(format!("unknown choice {choice}"));
    }
    app.with(|l| {
        l.set_setting(eudora_core::db::MEMORY, &serde_json::to_string(&choice)?)?;
        l.apply_memory()
    })?;
    app.with_slow(|l| l.apply_memory())?;
    bg.options_changed();
    Ok(())
}

/// Maintenance (Settings ▸ Speed and maintenance): its schedule, its last run, whether one is asked for.
#[tauri::command]
async fn maintenance_get(app: tauri::State<'_, App>) -> Reply<serde_json::Value> {
    let root = app.path.clone().ok_or_else(|| "No library is open.".to_string())?;
    let (schedule, last) = app.with_free(|l| Ok((l.maintenance_schedule(), l.maintenance_last())))?;
    Ok(serde_json::json!({ "schedule": schedule, "last": last, "waiting": eudora_core::maintenance::waiting(&root) }))
}

#[tauri::command]
async fn maintenance_set(app: tauri::State<'_, App>, schedule: eudora_core::maintenance::Schedule) -> Reply<()> {
    if !["off", "daily", "weekly"].contains(&schedule.when.as_str()) || schedule.hour > 23 || schedule.weekday > 6 {
        return Err("Not a schedule Eudora knows.".into());
    }
    app.with(|l| l.set_maintenance_schedule(&schedule))
}

/// Run Maintenance Now: the download takes it within seconds (and starts if it isn't running).
#[tauri::command]
async fn maintenance_now(app: tauri::State<'_, App>) -> Reply<()> {
    let root = app.path.clone().ok_or_else(|| "No library is open.".to_string())?;
    eudora_core::maintenance::request(&root).map_err(text)
}

/// The user did something in the window (sent at most once a minute): scheduled maintenance waits
/// until Eudora hasn't been used for a while.
#[tauri::command]
async fn user_active(app: tauri::State<'_, App>) -> Reply<()> {
    if let Some(root) = &app.path {
        eudora_core::maintenance::mark_active(root);
    }
    Ok(())
}

/// How often each account is checked for new mail (Settings ▸ Accounts): {address: minutes}
/// for those not at the default, the default, and the choices.
#[tauri::command]
async fn check_every_get(app: tauri::State<'_, App>) -> Reply<serde_json::Value> {
    let chosen = app.with_free(|l| Ok(eudora_core::graph_sync::check_intervals(l)))?;
    Ok(serde_json::json!({ "accounts": chosen, "default": eudora_core::graph_sync::DEFAULT_CHECK_MINUTES, "choices": eudora_core::graph_sync::CHECK_CHOICES }))
}

/// An account's interval (the running download reads it at its next check).
#[tauri::command]
async fn check_every_set(app: tauri::State<'_, App>, address: String, minutes: u32) -> Reply<()> {
    app.with(|l| eudora_core::graph_sync::set_check_minutes(l, &address, minutes))
}

/// How much mail is downloaded from the servers (graph_sync::PERIOD_SETTING): {chosen, days}.
#[tauri::command]
async fn download_period_get(app: tauri::State<'_, App>) -> Reply<serde_json::Value> {
    let (chosen, mb, used) = app.with_free(|l| {
        Ok((eudora_core::graph_sync::download_period(l), eudora_core::graph_sync::download_space_mb(l), eudora_core::graph_sync::library_bytes(&l.root)))
    })?;
    Ok(serde_json::json!({ "chosen": chosen.is_some(), "days": chosen.flatten(), "mb": mb, "used_mb": used / 1_000_000 }))
}

/// Chooses it: the last `days` days, or up to `mb` megabytes, or (both null) everything; the
/// running download starts over with it.
#[tauri::command]
async fn download_period_set(app: tauri::State<'_, App>, days: Option<u32>, mb: Option<u64>) -> Reply<()> {
    app.with(|l| match mb {
        Some(mb) => eudora_core::graph_sync::set_download_space(l, mb),
        None => eudora_core::graph_sync::set_download_period(l, days),
    })?;
    if let Some(path) = &app.path {
        let _ = eudora_core::graph_sync::request_check(path);
    }
    Ok(())
}

/// Settings ▸ Library ▸ Mail kept on this computer: what keeping only the last `days` would
/// remove and keep (trim.rs), changing nothing.
#[tauri::command]
async fn keep_preview(app: tauri::State<'_, App>, days: u32) -> Reply<eudora_core::trim::KeepPreview> {
    app.with_free(|l| l.keep_preview(days.max(1)))
}

/// The library counted by day and by whether each message could leave (trim.rs): read once when
/// the page opens, so the slider's every position is worked out in the window at once.
#[tauri::command]
async fn keep_counts(app: tauri::State<'_, App>) -> Reply<eudora_core::trim::KeepCounts> {
    app.with_free(|l| l.keep_counts())
}

/// This program's version and when it was built (Help ▸ About), from the program itself: it
/// can't fail, whatever the library or the update check is doing.
#[tauri::command]
fn app_version() -> serde_json::Value {
    serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "built": env!("EUDORA_BUILT").parse::<i64>().unwrap_or(0) })
}

/// Updates (eudora-core/src/update.rs; Settings ▸ Updates, Help ▸ Check for Updates…).
#[tauri::command]
async fn update_get() -> Reply<serde_json::Value> {
    let mut status = eudora_core::update::status();
    // (when this program was built: build.rs; Help ▸ About shows it with the version)
    status["built"] = serde_json::json!(env!("EUDORA_BUILT").parse::<i64>().unwrap_or(0));
    Ok(status)
}

#[tauri::command]
async fn update_set(on_start: bool, every_days: u32) -> Reply<eudora_core::update::State> {
    eudora_core::update::set_choices(on_start, every_days).map_err(text)
}

/// Looks for an update (`force`: Check for Updates; else the quiet checks, only when due) and
/// downloads it, its progress in "update-progress" events to the main window.
#[tauri::command]
async fn update_check(app: tauri::AppHandle, force: bool, starting: bool) -> Reply<eudora_core::update::Outcome> {
    use tauri::Emitter;
    tauri::async_runtime::spawn_blocking(move || {
        let mut shown = std::time::Instant::now() - std::time::Duration::from_secs(1);
        eudora_core::update::check_now(force, starting, |done, total| {
            if shown.elapsed().as_millis() >= 250 || done == total {
                shown = std::time::Instant::now();
                let _ = app.emit_to("main", "update-progress", (done, total));
            }
            true
        })
    })
    .await
    .map_err(text)?
    .map_err(text)
}

/// Restart Now (Setup starts, then Eudora closes; Setup starts it again), When I Close Eudora,
/// or Skip This Version.
#[tauri::command]
async fn update_install(app: tauri::AppHandle, when: String) -> Reply<String> {
    let version = eudora_core::update::answer(&when).map_err(text)?;
    if when == "now" {
        app.exit(0);
    }
    Ok(version)
}

/// Help ▸ About's speed table: the window's own times and the download's.
#[tauri::command]
async fn speed_stats(app: tauri::State<'_, App>) -> Reply<serde_json::Value> {
    let download = app.path.as_ref().map(|p| eudora_core::speed::read_summary(p, "download")).unwrap_or_default();
    Ok(serde_json::json!({ "window": eudora_core::speed::summary(), "download": download }))
}

fn open_library(named: Option<PathBuf>) -> App {
    // the folder given (eudora27 <folder>, or EUDORA_LIBRARY), remembered for next time; else
    // the one last given (the installer's shortcut names none), else the usual place (made on
    // the first run)
    let named = named.or_else(|| std::env::var_os("EUDORA_LIBRARY").map(PathBuf::from));
    // a restore over the library that was open: put in its place before anything opens it
    // (the download, started later, would hold the old one)
    let replacement = match eudora_core::library::finish_replacement() {
        Ok(Some(target)) => Some(Ok(format!("The restored library is now Eudora's library, in {}.", target.display()))),
        Ok(None) => None,
        Err(e) => Some(Err(e.to_string())),
    };
    if let Ok(mut r) = REPLACEMENT.lock() {
        *r = replacement;
    }
    if let Some(p) = &named {
        eudora_core::library::remember_library(p);
    }
    let path = named.or_else(eudora_core::library::usual_library);
    #[cfg(mobile)]
    if let Some(p) = &path {
        delete_retired_library(p);
    }
    // opened by `App::open` on a thread of its own, so the window shows at once
    // (the window's speed statistics add up across restarts)
    if let Some(p) = &path {
        eudora_core::speed::set_file(p, "window");
    }
    let problem = path.is_none().then(|| "No library given. Start Eudora with the library folder (eudora27 <folder>) or set EUDORA_LIBRARY.".to_string());
    App {
        opening: Mutex::new(path.is_some()),
        opened: Condvar::new(),
        path,
        library: Mutex::new(None),
        problem: Mutex::new(problem),
        page_ends: Mutex::new(HashMap::new()),
        own: Mutex::new(None),
        slow: Mutex::new(None),
        reader: Mutex::new(None),
        lister: Mutex::new(None),
        searcher: Mutex::new(None),
        sort_job: Mutex::new(None),
        sorted: Mutex::new(None),
        sorting: tauri::async_runtime::Mutex::new(()),
    }
}

/// Classic or Modern (Settings ▸ General; setting "ui:mode", synced), and classic's two
/// shortcut options. Default Modern.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
#[serde(default)]
struct UiMode {
    /// Classic mode: classic Eudora 7.1's menus, names, shortcuts and icons.
    classic: bool,
    /// Classic's "Map Ctrl+R to Reply to All" (Ctrl+Shift+R is then Reply).
    ctrl_r_all: bool,
    /// Classic's "Ctrl+J for Junk" (Filter Messages loses Ctrl+J). Classic mode only.
    ctrl_j_junk: bool,
    /// Whose menus the menu bar has: "outlook", "thunderbird", "gmail", "yahoo"; anything else =
    /// Eudora's own (Classic or Modern). Follows the look.
    menus: String,
}

const UI_MODE_KEY: &str = "ui:mode";

static UI_MODE: Mutex<Option<UiMode>> = Mutex::new(None);

#[cfg(desktop)] // (the menu bar's; phones have none)
fn ui_mode() -> UiMode {
    UI_MODE.lock().ok().and_then(|m| m.clone()).unwrap_or_default()
}

/// The mode as kept in the library, put in effect (the menu bar rebuilt).
fn apply_ui_mode(handle: &tauri::AppHandle, mode: UiMode) {
    if let Ok(mut m) = UI_MODE.lock() {
        *m = Some(mode);
    }
    // (menus are changed on the main thread; a phone has no menu bar)
    #[cfg(desktop)]
    {
        let h = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Ok(menu) = menu_bar(&h) {
                let _ = h.set_menu(menu);
            }
        });
    }
    #[cfg(mobile)]
    let _ = handle;
}

/// Where classic Eudora turned up, if it did: mail imported from a classic Eudora folder, or
/// classic Eudora's program or settings in their usual places.
#[tauri::command]
async fn classic_eudora_found(app: tauri::State<'_, App>) -> Reply<Option<String>> {
    if let Ok(Some(source)) = app.with(|l| Ok(l.accounts_counted(false)?.into_iter().find(|a| a.kind == "eudora").map(|a| a.source.unwrap_or_default()))) {
        return Ok(Some(format!("your mail imported from classic Eudora{}", if source.is_empty() { String::new() } else { format!(" ({})", source.split(" (personalities").next().unwrap_or(&source)) })));
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut places: Vec<PathBuf> = Vec::new();
    for base in [env("APPDATA"), env("LOCALAPPDATA")].into_iter().flatten() {
        places.push(base.join("Qualcomm").join("Eudora"));
    }
    for base in [env("ProgramFiles"), env("ProgramFiles(x86)")].into_iter().flatten() {
        places.push(base.join("Qualcomm").join("Eudora"));
    }
    if let Some(home) = env("USERPROFILE").or_else(|| env("HOME")) {
        places.push(home.join("Documents").join("Eudora"));
        places.push(home.join("Library").join("Mail").join("Eudora Folder")); // classic Eudora on the Mac
        places.push(home.join("Documents").join("Eudora Folder"));
    }
    places.push(PathBuf::from(r"C:\Eudora"));
    for p in places {
        let here = p.join("eudora.ini").exists() || p.join("Eudora.exe").exists() || p.join("In.mbx").exists() || p.join("Eudora Settings").exists();
        if here {
            return Ok(Some(format!("classic Eudora on this computer ({})", p.display())));
        }
    }
    Ok(None)
}

/// The tray icon: new mail to show (`new_mail`), and classic's Minimize to Tray (`minimize`).
#[tauri::command]
async fn tray_set(handle: tauri::AppHandle, new_mail: Option<bool>, minimize: Option<bool>) -> Reply<()> {
    #[cfg(desktop)]
    {
        if let Some(on) = new_mail {
            tray::set_new_mail(&handle, on);
        }
        if let Some(on) = minimize {
            tray::MINIMIZE_TO_TRAY.store(on, std::sync::atomic::Ordering::Relaxed);
        }
    }
    // (a phone has no tray)
    #[cfg(mobile)]
    let _ = (handle, new_mail, minimize);
    Ok(())
}

#[tauri::command]
async fn ui_mode_get(app: tauri::State<'_, App>) -> Reply<UiMode> {
    Ok(app.with(|l| l.setting(UI_MODE_KEY)).ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
}

/// Switches Classic / Modern (and classic's shortcut options): kept, and the menu bar changed at once.
#[tauri::command]
async fn ui_mode_set(app: tauri::State<'_, App>, handle: tauri::AppHandle, mode: UiMode) -> Reply<()> {
    app.with(|l| l.set_setting(UI_MODE_KEY, &serde_json::to_string(&mode)?))?;
    apply_ui_mode(&handle, mode);
    Ok(())
}

/// The menu bar, in classic Eudora's order: File · Edit · Mailbox · Message · Transfer · Special ·
/// Tools · Window · Help (docs/CLASSIC-UI.md). **Classic mode** uses classic Eudora 7.1's names,
/// submenus and shortcuts (Edit ▸ Find ▸ Find Text… Ctrl+Shift+F, Mailbox ▸ In Ctrl+1, Message ▸
/// Change ▸ Queuing… / Status / Priority / Label, Junk, Compact Mailboxes, Forget Passwords, Task
/// Status), with every Eudora 27 command placed where classic would have put it; **Modern mode**
/// Eudora 27's (Ctrl+Shift+F = Sort with the Spam Filter, Ctrl+1 = All Mail, Spam, Optimize
/// Library, Forget Sign-ins, Activity). A command sends its id to the window in front ("menu"
/// event), which does it. Commands not built yet are grayed out.
#[cfg(desktop)]
fn menu_bar(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
    let mode = ui_mode();
    let classic = mode.classic;
    if !classic {
        if let Some(menus) = look_menus(&mode.menus) {
            return mac_menus(app, build_menus(app, &menus)?);
        }
    }
    let item = |id: &str, text: &str, keys: Option<&str>| {
        let b = MenuItemBuilder::with_id(id, text);
        match keys {
            Some(k) => b.accelerator(shortcut_here(k)).build(app),
            None => b.build(app),
        }
    };
    // the mode's own word for a thing (Classic, Modern)
    let say = |c: &'static str, m: &'static str| if classic { c } else { m };
    let later = |text: &str| MenuItemBuilder::new(text).enabled(false).build(app);
    // classic's options: Ctrl+R for Reply to All; Ctrl+J for Junk
    let (reply_key, reply_all_key) = if mode.ctrl_r_all { ("CmdOrCtrl+Shift+R", "CmdOrCtrl+R") } else { ("CmdOrCtrl+R", "CmdOrCtrl+Shift+R") };
    let junk_by_j = classic && mode.ctrl_j_junk;

    let file = SubmenuBuilder::new(app, "&File")
        .item(&item("open-file", "Open File…", Some("CmdOrCtrl+O"))?)
        .item(&item("close", "Close", Some("CmdOrCtrl+W"))?)
        .item(&item("save", say("Save", "Save Draft"), Some("CmdOrCtrl+S"))?)
        .item(&item("drafts", "Drafts…", None)?)
        .separator()
        .item(&item("send-queued", "Send Queued Messages", Some("CmdOrCtrl+T"))?)
        .item(&item("check", "Check Mail", Some("CmdOrCtrl+M"))?)
        .item(&item("work-offline", say("Work Offline", "Work Offline (on / off)"), None)?)
        .separator()
        .item(&item("print", "Print…", Some("CmdOrCtrl+P"))?)
        .item(&item("import", "Import…", None)?)
        .item(&item("backup", "Back Up Eudora…", None)?)
        .item(&item("restore", "Restore Eudora…", None)?)
        .separator();
    // (Modern: Settings and Accounts in the File menu, where people look for them today; Classic
    // keeps them in Tools as Options and Personalities)
    let file = if classic { file } else { file.item(&item("accounts", "Accounts…", None)?).item(&item("options", "Settings…", Some("CmdOrCtrl+,"))?).separator() };
    let file = file.item(&item("exit", "Exit", Some("CmdOrCtrl+Q"))?).build()?;

    let mut edit = SubmenuBuilder::new(app, "&Edit")
        .item(&item("undo", "Undo", Some("CmdOrCtrl+Z"))?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&item("select-all", "Select All", Some("CmdOrCtrl+A"))?)
        .separator();
    edit = if classic {
        // classic's Edit ▸ Find submenu
        edit.item(
            &SubmenuBuilder::new(app, "&Find")
                .item(&item("find-text", "Find Text…", Some("CmdOrCtrl+Shift+F"))?)
                .item(&item("find-again", "Find Text Again", Some("F3"))?)
                .item(&item("find", "Find Messages…", Some("CmdOrCtrl+F"))?)
                .build()?,
        )
    } else {
        edit.item(&item("find", "Find Messages…", Some("CmdOrCtrl+F"))?)
            .item(&item("find-text", "Find Text…", Some("CmdOrCtrl+Alt+F"))?)
            .item(&item("find-again", "Find Again", Some("F3"))?)
    };
    let edit = edit.item(&item("check-spelling", "Check Spelling", Some("CmdOrCtrl+6"))?).build()?;

    let mailbox = if classic {
        // classic's Mailbox menu: In, Out, Junk, Trash, then the rest
        SubmenuBuilder::new(app, "&Mailbox")
            .item(&item("mailbox-in", "In", Some("CmdOrCtrl+1"))?)
            .item(&item("outbox", "Out", Some("CmdOrCtrl+0"))?)
            .item(&item("mailbox-junk", "Junk", None)?)
            .item(&item("mailbox-trash", "Trash", None)?)
            .separator()
            .item(&item("new-mailbox", "New…", None)?)
            .item(&item("all-mail", "All Mail", None)?)
            .item(&item("open-mailbox", "Open Mailbox…", Some("CmdOrCtrl+Shift+M"))?)
            .separator()
            .item(&item("hide-sent", "Hide Mail I Sent in All Mail", None)?)
            .item(&item("columns", "Columns…", None)?)
            .separator()
            .item(&item("subscriptions", "Subscriptions (unsubscribe)…", None)?)
            .build()?
    } else {
        SubmenuBuilder::new(app, "&Mailbox")
            .item(&item("all-mail", "All Mail", Some("CmdOrCtrl+1"))?)
            .item(&item("mailbox-in", "Inbox", Some("CmdOrCtrl+2"))?)
            .item(&item("open-mailbox", "Open Mailbox…", Some("CmdOrCtrl+Shift+M"))?)
            .item(&item("outbox", "Out", Some("CmdOrCtrl+0"))?)
            .item(&item("new-mailbox", "New Mailbox…", None)?)
            .separator()
            .item(&item("hide-sent", "Hide Mail I Sent in All Mail (on / off)", None)?)
            .item(&item("columns", "Columns…", None)?)
            .separator()
            .item(&item("subscriptions", "Subscriptions (unsubscribe)…", None)?)
            .build()?
    };

    let status = SubmenuBuilder::new(app, if classic { "&Status" } else { "Change &Status" })
        .item(&item("status-unread", "Unread", None)?)
        .item(&item("status-read", "Read", None)?)
        .item(&item("status-replied", "Replied", None)?)
        .item(&item("status-forwarded", "Forwarded", None)?)
        .item(&item("status-redirected", "Redirected", None)?)
        .build()?;
    let priority = SubmenuBuilder::new(app, if classic { "&Priority" } else { "Change &Priority" })
        .item(&item("priority-1", "Highest", None)?)
        .item(&item("priority-2", "High", None)?)
        .item(&item("priority-3", "Normal", None)?)
        .item(&item("priority-4", "Low", None)?)
        .item(&item("priority-5", "Lowest", None)?)
        .separator()
        .item(&item("priority-raise", "Raise Priority", None)?)
        .item(&item("priority-lower", "Lower Priority", None)?)
        .item(&item("priority-0", "As the Message Says", None)?)
        .build()?;
    let mut message = SubmenuBuilder::new(app, "M&essage")
        .item(&item("new", "New Message", Some("CmdOrCtrl+N"))?)
        .item(&item("reply", "Reply", Some(reply_key))?)
        .item(&item("reply-all", "Reply to All", Some(reply_all_key))?)
        .item(&item("forward", "Forward", None)?)
        .item(&item("redirect", "Redirect", None)?)
        .item(&item("send-again", "Send Again", None)?)
        .separator()
        .item(&item("attach", "Attach File…", Some("CmdOrCtrl+H"))?)
        .item(&item("send", "Send Immediately", Some("CmdOrCtrl+E"))?);
    message = if classic {
        // classic's Message ▸ Change submenu (Queuing… = Send Later)
        message.separator().item(
            &SubmenuBuilder::new(app, "&Change")
                .item(&item("send-later", "Queuing…", None)?)
                .item(&status)
                .item(&priority)
                .item(&item("change-label", "&Label…", None)?)
                .build()?,
        )
    } else {
        message
            .item(&item("send-later", "Send Later…", None)?)
            .separator()
            .item(&status)
            .item(&priority)
            .item(&item("change-label", "Change &Label…", None)?)
    };
    let message = message
        .item(&item("flag-todo", "Flag as To Do", Some("CmdOrCtrl+Shift+G"))?)
        .item(&item("show-headers", say("Blah Blah Blah (all headers)", "Show All Headers"), Some("F7"))?)
        .separator()
        .item(&item("mark-spam", say("Junk", "Spam"), Some(if junk_by_j { "CmdOrCtrl+J" } else { "CmdOrCtrl+Shift+J" }))?)
        .item(&item("mark-not-spam", say("Not Junk", "Not Spam"), Some("CmdOrCtrl+Alt+J"))?)
        .item(&item("delete", "Delete", Some("CmdOrCtrl+D"))?)
        .build()?;

    let transfer = SubmenuBuilder::new(app, "&Transfer")
        .item(&item("transfer", "Transfer To…", Some("CmdOrCtrl+Shift+T"))?)
        .item(&item("copy-to", "Copy To…", None)?)
        .item(&item("transfer-new", if classic { "New…" } else { "To a New Mailbox…" }, None)?)
        .separator()
        .item(&item("to-trash", "Trash", None)?)
        .build()?;

    let special = SubmenuBuilder::new(app, "&Special")
        .item(&item("filter-messages", "Filter Messages", if junk_by_j { None } else { Some("CmdOrCtrl+J") })?)
        .item(&item("make-filter", "Make Filter…", None)?)
        .separator()
        .item(&item("sort-selected", "Sort with the Spam Filter…", if classic { None } else { Some("CmdOrCtrl+Shift+F") })?)
        .item(&item("sort-why", "Why? (Spam Filter)", None)?)
        .item(&item("views-why", "Why in these Views?", None)?)
        .item(&item("spam-rules", "Mail Filter Rules…", None)?)
        .separator()
        .item(&item("pause-old", "Pause Old Mail (new mail still comes in)", None)?)
        .item(&item("pause-all", "Pause All Downloading", None)?)
        .item(&item("resume", "Resume Downloading", None)?)
        .item(&later("Make Address Book Entry…")?)
        .separator()
        .item(&item("empty-trash", "Empty Trash…", None)?)
        .item(&item("optimize", say("Compact Mailboxes…", "Optimize Library…"), None)?)
        .item(&item("forget-sign-ins", say("Forget Passwords…", "Forget Sign-ins…"), None)?)
        .build()?;

    let mut tools = SubmenuBuilder::new(app, "T&ools")
        .item(&item("filters", "Filters", None)?)
        .item(&item("mailboxes", "Mailboxes", None)?)
        .item(&item("signatures", "Signatures…", None)?)
        .item(&item("personalities", "Personalities…", None)?)
        .item(&item("address-book", "Address Book", Some("CmdOrCtrl+L"))?)
        .item(&item("activity", say("Task Status", "Activity"), None)?);
    if classic {
        tools = tools.item(&item("stats", "Statistics…", None)?);
    }
    let tools = tools
        .separator()
        .item(&item("plugins", "Plugins…", None)?)
        .item(&item("old-attachments", "Find Old Attachments…", None)?)
        .item(&item("add-account", "Add Account…", None)?)
        .separator()
        .item(&item("options", say("Options…", "Settings…"), None)?)
        .build()?;

    let window = SubmenuBuilder::new(app, "&Window")
        .item(&PredefinedMenuItem::minimize(app, None)?)
        .item(&PredefinedMenuItem::maximize(app, None)?)
        .build()?;
    let mut help = SubmenuBuilder::new(app, "&Help").item(&item("manual", "Eudora Help", Some("F1"))?);
    if !classic {
        help = help.item(&item("stats", "Statistics…", None)?);
    }
    let help = help.item(&item("check-updates", "Check for Updates…", None)?).item(&item("about", "About Eudora", None)?).build()?;
    mac_menus(app, MenuBuilder::new(app).items(&[&file, &edit, &mailbox, &message, &transfer, &special, &tools, &window, &help]).build()?)
}

/// The help shortcut: F1, and on the Mac ⌘? (F1 is a Mac's brightness key).
#[cfg(desktop)]
fn shortcut_here(keys: &str) -> &str {
    if cfg!(target_os = "macos") && keys == "F1" { "CmdOrCtrl+Shift+/" } else { keys }
}

/// The Mac's own menu layout: the application menu first (About Eudora, Check for Updates…,
/// Settings…, Services, Hide, Quit: macOS makes the first menu the one named after the program,
/// which was File), the Help menu registered as the Mac's (its search box, Eudora Help ⌘?; About
/// and Check for Updates move to the application menu), the Window menu as the Mac's window list.
/// Elsewhere the menus stay as they are.
#[cfg(desktop)]
fn mac_menus(app: &tauri::AppHandle, menu: tauri::menu::Menu<tauri::Wry>) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    #[cfg(target_os = "macos")]
    {
        use tauri::menu::{MenuItemBuilder, MenuItemKind, PredefinedMenuItem, SubmenuBuilder};
        for kind in menu.items()? {
            let MenuItemKind::Submenu(sub) = kind else { continue };
            match sub.text()?.replace('&', "").as_str() {
                "Help" => {
                    for item in sub.items()? {
                        if matches!(item.id().as_ref(), "about" | "check-updates") {
                            sub.remove(&item)?;
                        }
                    }
                    // (a line left at the end)
                    if let Some(last) = sub.items()?.last() {
                        if matches!(last, MenuItemKind::Predefined(_)) {
                            sub.remove(last)?;
                        }
                    }
                    sub.set_as_help_menu_for_nsapp()?;
                }
                "Window" => sub.set_as_windows_menu_for_nsapp()?,
                _ => {}
            }
        }
        let plain = |id: &str, text: &str| MenuItemBuilder::with_id(id, text).build(app);
        let eudora = SubmenuBuilder::new(app, "Eudora")
            .item(&plain("about", "About Eudora")?)
            .item(&plain("check-updates", "Check for Updates…")?)
            .separator()
            .item(&plain("options", "Settings…")?)
            .separator()
            .item(&PredefinedMenuItem::services(app, None)?)
            .separator()
            .item(&PredefinedMenuItem::hide(app, Some("Hide Eudora"))?)
            .item(&PredefinedMenuItem::hide_others(app, None)?)
            .item(&PredefinedMenuItem::show_all(app, None)?)
            .separator()
            .item(&plain("exit", "Quit Eudora")?)
            .build()?;
        menu.insert(&eudora, 0)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
    Ok(menu)
}

#[cfg(desktop)]
/// One entry of a look's menus: a command (its id, as the window's ACTIONS name it; its text;
/// its shortcut), a separator, a submenu, or the system's own Cut / Copy / Paste.
#[derive(Clone, Debug)]
enum Entry {
    Do(&'static str, &'static str, Option<&'static str>),
    Line,
    Sub(&'static str, Vec<Entry>),
    Cut,
    Copy,
    Paste,
}

#[cfg(desktop)]
/// The Edit menu every look keeps (the system's clipboard commands need it on a Mac); `find`: the
/// look's name and shortcut for finding text in a message.
fn edit_entries(find: (&'static str, &'static str), spelling: (&'static str, &'static str)) -> Vec<Entry> {
    use Entry::*;
    vec![
        Do("undo", "Undo", Some("CmdOrCtrl+Z")),
        Line,
        Cut,
        Copy,
        Paste,
        Do("select-all", "Select All", Some("CmdOrCtrl+A")),
        Line,
        Do("find-text", find.0, Some(find.1)),
        Do("find-again", "Find Again", Some("F3")),
        Do("check-spelling", spelling.0, Some(spelling.1)),
    ]
}

#[cfg(desktop)]
/// The menus of the looks that copy another program. Every command is one of Eudora's (the same
/// ids as its own menus), named and placed, with the shortcuts, as that program has them. None =
/// Eudora's own menus.
fn look_menus(look: &str) -> Option<Vec<(&'static str, Vec<Entry>)>> {
    use Entry::*;
    let help = |name: &'static str| -> Vec<Entry> {
        vec![Do("manual", name, Some("F1")), Do("stats", "Statistics…", None), Line, Do("check-updates", "Check for Updates…", None), Do("about", "About Eudora", None)]
    };
    let menus = match look {
        // Outlook's ribbon tabs as menus (File, Home, Send / Receive, Folder, View), with Outlook's
        // own shortcuts: Ctrl+F forwards, Ctrl+E searches, Ctrl+Q / Ctrl+U read / unread, F9
        // Send/Receive, Ctrl+Shift+V Move to Folder, Ctrl+Y Go to Folder, Ctrl+1…4 the views
        "outlook" => vec![
            (
                "&File",
                vec![
                    Do("open-file", "Open…", Some("CmdOrCtrl+O")),
                    Do("save", "Save", Some("CmdOrCtrl+S")),
                    Do("drafts", "Drafts…", None),
                    Do("close", "Close", None),
                    Line,
                    Sub("Account Settings", vec![Do("add-account", "Add Account…", None), Do("accounts", "Account Settings…", None), Do("forget-sign-ins", "Forget Sign-ins…", None)]),
                    Do("options", "Options", None),
                    Line,
                    Sub("Open && Export", vec![Do("import", "Import…", None), Do("backup", "Back Up (Export)…", None), Do("restore", "Restore…", None), Do("old-attachments", "Find Old Attachments…", None)]),
                    Do("print", "Print…", Some("CmdOrCtrl+P")),
                    Line,
                    Do("exit", "Exit", None),
                ],
            ),
            ("&Edit", edit_entries(("Find in This Message…", "F4"), ("Spelling && Grammar", "F7"))),
            (
                "&Home",
                vec![
                    Do("new", "New Email", Some("CmdOrCtrl+N")),
                    Do("new-appointment", "New Appointment", Some("CmdOrCtrl+Shift+A")),
                    Line,
                    Do("delete", "Delete", Some("CmdOrCtrl+D")),
                    Sub("Junk", vec![Do("mark-spam", "Block Sender (Junk)", Some("CmdOrCtrl+Shift+J")), Do("mark-not-spam", "Not Junk", Some("CmdOrCtrl+Alt+J")), Line, Do("spam-rules", "Junk Email Options…", None)]),
                    Line,
                    Do("reply", "Reply", Some("CmdOrCtrl+R")),
                    Do("reply-all", "Reply All", Some("CmdOrCtrl+Shift+R")),
                    Do("forward", "Forward", Some("CmdOrCtrl+F")),
                    Do("redirect", "Redirect…", None),
                    Do("send-again", "Resend This Message…", None),
                    Line,
                    Sub("Move", vec![Do("transfer", "Move to Folder…", Some("CmdOrCtrl+Shift+V")), Do("copy-to", "Copy to Folder…", None), Do("transfer-new", "Move to a New Folder…", None)]),
                    Sub(
                        "Rules",
                        vec![
                            Do("make-filter", "Create Rule…", None),
                            Do("filters", "Manage Rules && Alerts…", None),
                            Do("filter-messages", "Run Rules Now", None),
                            Line,
                            Do("sort-selected", "Sort with the Spam Filter…", None),
                            Do("sort-why", "Why? (Spam Filter)", None),
                        ],
                    ),
                    Line,
                    Do("status-read", "Mark as Read", Some("CmdOrCtrl+Q")),
                    Do("status-unread", "Mark as Unread", Some("CmdOrCtrl+U")),
                    Do("change-label", "Categorize…", None),
                    Do("flag-todo", "Follow Up (Flag)", Some("CmdOrCtrl+Shift+G")),
                    Sub("Importance", vec![Do("priority-2", "High Importance", None), Do("priority-3", "Normal", None), Do("priority-4", "Low Importance", None)]),
                    Line,
                    Do("address-book", "Address Book", Some("CmdOrCtrl+Shift+B")),
                    Do("find", "Search", Some("CmdOrCtrl+E")),
                    Line,
                    Do("send", "Send (the message being written)", Some("CmdOrCtrl+Enter")),
                    Do("attach", "Attach File…", None),
                    Do("send-later", "Delay Delivery…", None),
                ],
            ),
            (
                "Send / Re&ceive",
                vec![
                    Do("check", "Send/Receive All Folders", Some("F9")),
                    Do("send-queued", "Send All", None),
                    Do("outbox", "Outbox", None),
                    Line,
                    Do("work-offline", "Work Offline", None),
                    Do("pause-old", "Download Headers Only (pause old mail)", None),
                    Do("pause-all", "Cancel All (pause downloading)", None),
                    Do("resume", "Resume Downloading", None),
                    Line,
                    Do("activity", "Show Progress", None),
                ],
            ),
            (
                "F&older",
                vec![
                    Do("new-mailbox", "New Folder…", Some("CmdOrCtrl+Shift+E")),
                    Do("open-mailbox", "Go to Folder…", Some("CmdOrCtrl+Y")),
                    Line,
                    Do("mailbox-in", "Inbox", None),
                    Do("all-mail", "All Mail", None),
                    Do("mailbox-junk", "Junk Email", None),
                    Do("mailbox-trash", "Deleted Items", None),
                    Line,
                    Do("empty-trash", "Empty Deleted Items…", None),
                    Do("subscriptions", "Subscriptions (unsubscribe)…", None),
                    Do("optimize", "Clean Up (Optimize Library)…", None),
                ],
            ),
            (
                "&View",
                vec![
                    Do("go-mail", "Mail", Some("CmdOrCtrl+1")),
                    Do("go-calendar", "Calendar", Some("CmdOrCtrl+2")),
                    Do("go-people", "People", Some("CmdOrCtrl+3")),
                    Do("go-todo", "Tasks", Some("CmdOrCtrl+4")),
                    Line,
                    Do("mailboxes", "Folder Pane", None),
                    Do("columns", "Columns…", None),
                    Do("hide-sent", "Hide Mail I Sent in All Mail", None),
                    Line,
                    Do("show-headers", "Message Headers", None),
                    Do("source", "View Source", None),
                    Do("views-why", "Why in these Views?", None),
                    Line,
                    Do("previous", "Previous Item", Some("CmdOrCtrl+,")),
                    Do("next", "Next Item", Some("CmdOrCtrl+.")),
                ],
            ),
            ("&Help", help("Eudora Help")),
        ],
        // Thunderbird's menu bar (File, Edit, View, Go, Message, Tools, Help) and its shortcuts:
        // Ctrl+F finds in the message, Ctrl+Shift+F searches messages, Ctrl+L forwards, Ctrl+T
        // gets new messages, Ctrl+U the source, Ctrl+Shift+B the address book
        "thunderbird" => vec![
            (
                "&File",
                vec![
                    Sub("&New", vec![Do("new", "Message", Some("CmdOrCtrl+N")), Do("new-mailbox", "Folder…", None), Do("add-account", "Existing Mail Account…", None), Do("new-appointment", "Event", Some("CmdOrCtrl+I"))]),
                    Do("open-file", "Open Saved Message…", Some("CmdOrCtrl+O")),
                    Do("close", "Close", Some("CmdOrCtrl+W")),
                    Do("save", "Save as Draft", Some("CmdOrCtrl+S")),
                    Line,
                    Do("check", "Get New Messages", Some("CmdOrCtrl+T")),
                    Do("send-queued", "Send Unsent Messages", None),
                    Do("work-offline", "Work Offline", None),
                    Line,
                    Do("empty-trash", "Empty Trash…", None),
                    Do("optimize", "Compact Folders…", None),
                    Line,
                    Do("print", "Print…", Some("CmdOrCtrl+P")),
                    Line,
                    Do("exit", "Exit", Some("CmdOrCtrl+Q")),
                ],
            ),
            ("&Edit", edit_entries(("Find in This Message…", "CmdOrCtrl+F"), ("Check Spelling", "CmdOrCtrl+Shift+P"))),
            (
                "&View",
                vec![
                    Do("mailboxes", "Folder Pane", Some("F9")),
                    Do("columns", "Columns…", None),
                    Do("hide-sent", "Hide Mail I Sent in All Mail", None),
                    Line,
                    Do("show-headers", "Headers: All", None),
                    Do("source", "Message Source", Some("CmdOrCtrl+U")),
                ],
            ),
            (
                "&Go",
                vec![
                    Do("next", "Next Message", None),
                    Do("previous", "Previous Message", None),
                    Line,
                    Do("mailbox-in", "Inbox", None),
                    Do("all-mail", "All Mail", None),
                    Do("open-mailbox", "Folder…", Some("CmdOrCtrl+Shift+M")),
                    Line,
                    Do("go-mail", "Mail", Some("CmdOrCtrl+1")),
                    Do("go-calendar", "Calendar", Some("CmdOrCtrl+Shift+C")),
                    Do("go-todo", "Tasks", Some("CmdOrCtrl+Shift+D")),
                ],
            ),
            (
                "&Message",
                vec![
                    Do("reply", "Reply", Some("CmdOrCtrl+R")),
                    Do("reply-all", "Reply to All", Some("CmdOrCtrl+Shift+R")),
                    Do("forward", "Forward", Some("CmdOrCtrl+L")),
                    Do("redirect", "Redirect", Some("CmdOrCtrl+B")),
                    Do("send-again", "Edit as New Message…", Some("CmdOrCtrl+E")),
                    Line,
                    Do("change-label", "Tag…", None),
                    Sub(
                        "Mark",
                        vec![
                            Do("status-read", "As Read", None),
                            Do("status-unread", "As Unread", None),
                            Do("mark-spam", "As Junk", Some("CmdOrCtrl+Shift+J")),
                            Do("mark-not-spam", "As Not Junk", Some("CmdOrCtrl+Alt+J")),
                            Do("flag-todo", "Starred (To Do)", Some("CmdOrCtrl+Shift+G")),
                        ],
                    ),
                    Sub("Priority", vec![Do("priority-1", "Highest", None), Do("priority-2", "High", None), Do("priority-3", "Normal", None), Do("priority-4", "Low", None), Do("priority-5", "Lowest", None)]),
                    Line,
                    Do("transfer", "Move To…", Some("CmdOrCtrl+Shift+V")),
                    Do("copy-to", "Copy To…", None),
                    Do("delete", "Delete Message", Some("CmdOrCtrl+D")),
                    Line,
                    Do("attach", "Attach File(s)…", Some("CmdOrCtrl+Shift+A")),
                    Do("send", "Send Now", Some("CmdOrCtrl+Enter")),
                    Do("send-later", "Send Later…", Some("CmdOrCtrl+Shift+Enter")),
                ],
            ),
            (
                "&Tools",
                vec![
                    Do("address-book", "Address Book", Some("CmdOrCtrl+Shift+B")),
                    Do("activity", "Activity Manager", None),
                    Line,
                    Do("filters", "Message Filters…", None),
                    Do("filter-messages", "Run Filters on Folder", None),
                    Do("make-filter", "Create Filter From Message…", None),
                    Do("sort-selected", "Run Junk Mail Controls…", None),
                    Do("spam-rules", "Junk Mail Settings…", None),
                    Do("subscriptions", "Subscriptions (unsubscribe)…", None),
                    Line,
                    Do("plugins", "Add-ons…", None),
                    Do("import", "Import…", None),
                    Do("backup", "Export (Back Up)…", None),
                    Do("restore", "Restore…", None),
                    Line,
                    Do("accounts", "Account Settings", None),
                    Do("options", "Settings", Some("CmdOrCtrl+,")),
                ],
            ),
            ("&Help", help("Eudora Help")),
        ],
        // Gmail and Yahoo have no menu bar: a short one in their own words (Compose, labels or
        // folders, the settings gear's choices), shortcuts as Eudora's
        "gmail" | "yahoo" => {
            let gmail = look == "gmail";
            vec![
                (
                    "&Mail",
                    vec![
                        Do("new", "Compose", Some("CmdOrCtrl+N")),
                        Do("reply", "Reply", Some("CmdOrCtrl+R")),
                        Do("reply-all", "Reply all", Some("CmdOrCtrl+Shift+R")),
                        Do("forward", "Forward", Some("CmdOrCtrl+Shift+F")),
                        Line,
                        Do("delete", "Delete", Some("CmdOrCtrl+D")),
                        Do("mark-spam", if gmail { "Report spam" } else { "Spam" }, Some("CmdOrCtrl+Shift+J")),
                        Do("mark-not-spam", "Not spam", Some("CmdOrCtrl+Alt+J")),
                        Do("status-read", "Mark as read", Some("CmdOrCtrl+Q")),
                        Do("status-unread", "Mark as unread", Some("CmdOrCtrl+U")),
                        Do("flag-todo", if gmail { "Add to Tasks (star)" } else { "Flag (star)" }, Some("CmdOrCtrl+Shift+G")),
                        Line,
                        Do("check", "Refresh", Some("CmdOrCtrl+M")),
                        Do("print", "Print", Some("CmdOrCtrl+P")),
                        Do("show-headers", "Show original", None),
                        Line,
                        Do("send", "Send", Some("CmdOrCtrl+Enter")),
                        Do("attach", "Attach files", None),
                        Do("send-later", "Schedule send…", None),
                        Do("save", "Save draft", Some("CmdOrCtrl+S")),
                        Do("close", "Close", Some("CmdOrCtrl+W")),
                        Line,
                        Do("exit", "Exit", None),
                    ],
                ),
                ("&Edit", {
                    let mut e = edit_entries(("Find in message…", "CmdOrCtrl+Alt+F"), ("Check spelling", "CmdOrCtrl+6"));
                    e.insert(7, Do("find", "Search mail", Some("CmdOrCtrl+F")));
                    e
                }),
                (
                    if gmail { "&Labels" } else { "F&olders" },
                    vec![
                        Do("mailbox-in", "Inbox", None),
                        Do("all-mail", if gmail { "All Mail" } else { "All mail" }, None),
                        Do("drafts", "Drafts", None),
                        Do("outbox", if gmail { "Scheduled" } else { "Outbox" }, None),
                        Do("mailbox-junk", "Spam", None),
                        Do("mailbox-trash", "Trash", None),
                        Line,
                        Do("change-label", if gmail { "Label as…" } else { "Label…" }, None),
                        Do("transfer", "Move to…", Some("CmdOrCtrl+Shift+V")),
                        Do("copy-to", "Copy to…", None),
                        Do("new-mailbox", if gmail { "Create new label…" } else { "New folder…" }, None),
                        Do("open-mailbox", if gmail { "Go to label…" } else { "Go to folder…" }, Some("CmdOrCtrl+Y")),
                        Line,
                        Do("empty-trash", "Empty Trash…", None),
                        Do("subscriptions", if gmail { "Manage subscriptions…" } else { "Subscriptions…" }, None),
                    ],
                ),
                (
                    if gmail { "&Apps" } else { "&Apps" },
                    vec![
                        Do("go-mail", "Mail", Some("CmdOrCtrl+1")),
                        Do("go-calendar", "Calendar", Some("CmdOrCtrl+2")),
                        Do("go-people", "Contacts", Some("CmdOrCtrl+3")),
                        Do("go-todo", "Tasks", Some("CmdOrCtrl+4")),
                        Line,
                        Do("new-appointment", "New event", None),
                    ],
                ),
                (
                    "&Settings",
                    vec![
                        Do("options", if gmail { "See all settings" } else { "More Settings" }, Some("CmdOrCtrl+,")),
                        Do("accounts", if gmail { "Accounts and Import" } else { "Mailboxes" }, None),
                        Do("signatures", if gmail { "Signature…" } else { "Writing email (signature)…" }, None),
                        Do("filters", "Filters…", None),
                        Do("spam-rules", if gmail { "Spam and blocked addresses…" } else { "Security and privacy (spam)…" }, None),
                        Do("plugins", "Add-ons…", None),
                        Line,
                        Do("work-offline", "Offline", None),
                        Do("activity", "Activity", None),
                        Do("import", "Import mail…", None),
                        Do("backup", "Back up…", None),
                        Do("restore", "Restore…", None),
                    ],
                ),
                ("&Help", help("Help")),
            ]
        }
        _ => return None,
    };
    Some(menus)
}

/// Builds a look's menu bar from its entries.
#[cfg(desktop)]
fn build_menus(app: &tauri::AppHandle, menus: &[(&'static str, Vec<Entry>)]) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{IsMenuItem, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, Submenu, SubmenuBuilder};
    fn sub(app: &tauri::AppHandle, title: &str, entries: &[Entry]) -> tauri::Result<Submenu<tauri::Wry>> {
        let mut b = SubmenuBuilder::new(app, title);
        for e in entries {
            b = match e {
                Entry::Do(id, text, keys) => {
                    let item = MenuItemBuilder::with_id(*id, *text);
                    let item = match keys {
                        Some(k) => item.accelerator(shortcut_here(k)).build(app)?,
                        None => item.build(app)?,
                    };
                    b.item(&item)
                }
                Entry::Line => b.separator(),
                Entry::Sub(t, inner) => b.item(&sub(app, t, inner)?),
                Entry::Cut => b.item(&PredefinedMenuItem::cut(app, None)?),
                Entry::Copy => b.item(&PredefinedMenuItem::copy(app, None)?),
                Entry::Paste => b.item(&PredefinedMenuItem::paste(app, None)?),
            };
        }
        b.build()
    }
    let subs = menus.iter().map(|(title, entries)| sub(app, title, entries)).collect::<tauri::Result<Vec<_>>>()?;
    let items: Vec<&dyn IsMenuItem<tauri::Wry>> = subs.iter().map(|s| s as &dyn IsMenuItem<tauri::Wry>).collect();
    MenuBuilder::new(app).items(&items).build()
}

/// Eudora: the desktop program's `main` and the phone apps' entry point.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::{Emitter, Manager};
    // files given (an .eml or .ics opened in Explorer): to the Eudora window already open, if
    // there is one (this start then ends), else opened by this one
    let (named, files) = opening::sort_args();
    // (iOS: the background check is registered before the app finishes launching)
    #[cfg(target_os = "ios")]
    ios::register();
    // (a phone runs one copy of an app by itself)
    #[cfg(desktop)]
    if opening::hand_over(&files, named.is_some()) {
        return;
    }
    let builder = tauri::Builder::default();
    // a phone opens links (and Microsoft's and Google's sign-in pages) through the system's own
    // call, which this plugin makes
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_opener::init());
    let builder = builder
        .setup(move |app| {
            // Android: servers' certificates are checked with Android's trust store (before any
            // connection)
            #[cfg(target_os = "android")]
            if let Err(e) = android::init_certificates() {
                eprintln!("{e}");
            }
            // a phone app keeps its library in its own storage, which the system names
            #[cfg(mobile)]
            if let Ok(dir) = app.path().app_data_dir() {
                // (Android: the background job finds the library through this)
                #[cfg(target_os = "android")]
                if let Ok(base) = app.path().data_dir() {
                    android::remember_data_dir(&base, &dir);
                }
                eudora_core::library::set_app_data_dir(dir);
            }
            #[cfg(mobile)]
            {
                let handle = app.handle().clone();
                eudora_core::graph::set_browser_opener(Box::new(move |url: &str| {
                    use tauri_plugin_opener::OpenerExt;
                    // a sign-in page: Eudora stays awake until the sign-in ends (android.rs)
                    #[cfg(target_os = "android")]
                    if url.starts_with("https://login.microsoftonline.com/") || url.starts_with("https://accounts.google.com/") {
                        android::keep_answering(true);
                    }
                    handle.opener().open_url(url, None::<&str>).map_err(|e| std::io::Error::other(e.to_string()))
                }));
            }
            // (opened here, not before: the place above must be known first; the window is
            // made after setup, so it finds the library's state ready)
            app.manage(open_library(named.clone()));
            // (iOS: may Eudora notify of new mail and number its icon? asked once)
            #[cfg(target_os = "ios")]
            {
                ios::ask_permission();
                ios::keep_watch();
            }
            opening::watch(app.handle().clone(), files.clone());
            // the download, run by the window while it's open (background.rs)
            let state = app.state::<App>();
            let opener = app.handle().clone();
            std::thread::spawn(move || {
                opener.state::<App>().open();
                // Classic or Modern, as kept in the library
                let mode = opener.state::<App>().with(|l| l.setting(UI_MODE_KEY)).ok().flatten().and_then(|j| serde_json::from_str::<UiMode>(&j).ok()).unwrap_or_default();
                apply_ui_mode(&opener, mode);
            });
            let bg = match &state.path {
                Some(path) => {
                    let handle = app.handle().clone();
                    background::Background::start(path.clone(), move || {
                        let app = handle.state::<App>();
                        let mut options = background::Options::read(app.with(|l| l.setting(background::SETTING)).ok().flatten());
                        // (a library that didn't open: nothing to download into)
                        if app.library.lock().map(|l| l.is_none()).unwrap_or(true) {
                            options.on = false;
                        }
                        options
                    })
                }
                None => background::Background::idle("No library is open."),
            };
            app.manage(bg);
            // the tray icon (classic's blue in-tray)
            #[cfg(desktop)]
            if let Err(e) = tray::install(app.handle()) {
                eprintln!("no tray icon: {e}");
            }
            // the local API, while the window is open (when the user allows it)
            app.manage(api_host::ApiHost::new());
            let api = app.handle().clone();
            std::thread::spawn(move || api.state::<api_host::ApiHost>().start(api.clone()));
            // the Out mailbox: what's due goes out (every 30 s while the window is open)
            let sender = app.handle().clone();
            std::thread::spawn(move || loop {
                {
                    // every 30 s, or at once when a message was just sent (queued)
                    let woken = SEND_NOW.0.lock().unwrap();
                    let (mut woken, _) = SEND_NOW.1.wait_timeout_while(woken, std::time::Duration::from_secs(30), |w| !*w).unwrap();
                    *woken = false;
                }
                let notes = send_queued(&sender.state::<App>(), false);
                if !notes.is_empty() {
                    let _ = sender.emit_to("main", "outbox", notes);
                }
            });
            // the main window, as configured (`create: false`), made here so links in its
            // messages open (`links_go_to_eudora`)
            if let Some(config) = app.config().app.windows.iter().find(|w| w.label == "main").cloned() {
                links_go_to_eudora(tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?, app.handle(), "main").build()?;
            }
            Ok(())
        });
    // the menu bar and the tray: desktops only
    #[cfg(desktop)]
    let builder = builder
        .menu(menu_bar)
        .on_window_event(|window, event| {
            use tauri::Manager;
            match event {
                // classic's Minimize to Tray: a minimized window leaves the taskbar for the tray
                tauri::WindowEvent::Resized(_) if window.label() == "main" => {
                    if tray::MINIMIZE_TO_TRAY.load(std::sync::atomic::Ordering::Relaxed) && tray::SHOWN.load(std::sync::atomic::Ordering::Relaxed) && window.is_minimized().unwrap_or(false) {
                        let _ = window.hide();
                    }
                }
                // looking at the window again: the new-mail tray picture goes
                tauri::WindowEvent::Focused(true) if window.label() == "main" => tray::set_new_mail(window.app_handle(), false),
                _ => {}
            }
        })
        .on_menu_event(|app, event| {
            let id = event.id().as_ref().to_string();
            if id == "exit" {
                app.exit(0);
                return;
            }
            // to the window in front (a message being written, or the main window)
            let front = app.webview_windows().into_values().find(|w| w.is_focused().unwrap_or(false));
            // the manual from any window but the main one (which picks the topic for what it
            // shows): opened here, at its start
            if id == "manual" && front.as_ref().is_some_and(|w| w.label() != "main") {
                let _ = show_help(app, None);
                return;
            }
            match front {
                Some(w) => {
                    let _ = w.emit_to(w.label(), "menu", id);
                }
                None => {
                    let _ = app.emit_to("main", "menu", id);
                }
            }
        });
    builder
        .invoke_handler(tauri::generate_handler![
            info,
            folders,
            page,
            count,
            header_values,
            list_ids,
            sorted_page,
            accounts,
            set_account_label,
            search,
            message,
            source,
            copy_source,
            download_status,
            check_mail,
            firewall_status,
            firewall_set,
            platform,
            set_pause,
            get_pause,
            find_mailboxes,
            add_shared_mailboxes,
            imap_test,
            imap_save,
            imap_accounts,
            web_pictures,
            setting,
            set_setting,
            identities,
            compose_seed,
            send_message,
            save_draft,
            drafts,
            delete_draft,
            open_compose,
            open_reader,
            open_mailto,
            open_help,
            search_scope_get,
            search_scope_set,
            search_warm,
            open_link,
            move_messages,
            delete_messages,
            copy_messages,
            folder_create,
            folder_rename,
            folder_delete,
            mail_host,
            pending_changes,
            job_state,
            job_cancel,
            mail_stats,
            views_list,
            view_groups,
            view_why,
            look_now,
            attachment_open,
            attachment_drag,
            messages_write,
            files_drag,
            files_place,
            file_icon,
            attachment_save,
            attachments_save_all,
            cards_list,
            card_counts,
            google_sign_in,
            google_available,
            card_origin,
            card_update,
            card_create,
            card_delete,
            card_share,
            card_get,
            cards_export,
            subscriptions,
            unsubscribe,
            views_accounts,
            views_accounts_set,
            queue_message,
            outbox_list,
            outbox_unqueue,
            outbox_reschedule,
            outbox_send_now,
            backup_info,
            backup_start,
            find_old_attachments,
            restore_describe,
            restore_start,
            restore_destination,
            settings_with_prefix,
            sync_state,
            sync_setup,
            sync_join,
            sync_phrase,
            sync_set_carriers,
            sync_now,
            server_changes,
            microsoft_sign_in,
            rules_get,
            rules_set,
            rules_import_classic,
            rules_run,
            rules_run_items,
            sort_preview,
            sort_progress,
            job_pause,
            job_details,
            sort_stop,
            sort_apply,
            mark_spam,
            sort_why,
            spam_rules,
            spam_rule_set,
            spam_mailboxes,
            spam_mailboxes_set,
            spam_languages,
            spam_languages_set,
            calendars_list,
            calendar_shown,
            calendar_color,
            calendar_events,
            calendar_event,
            calendar_search,
            event_respond,
            event_update,
            event_delete,
            reservations_list,
            invites_list,
            memory_get,
            memory_set,
            maintenance_get,
            maintenance_set,
            maintenance_now,
            user_active,
            speed_stats,
            unread_counts,
            mark_folders_read,
            background_jobs,
            appointment_get,
            appointment_edit,
            appointment_ics,
            zoom_get,
            zoom_set,
            zoom_sign_in,
            zoom_sign_out,
            complete_directory,
            places_suggest,
            maps_key_present,
            maps_key_set,
            conference_options,
            appointment_new,
            theme_set,
            invite_spam,
            invitation,
            invitation_add,
            invitation_answer,
            reservations_answer,
            reservation_off,
            reservation_hide,
            trips_from_message,
            todos_list,
            app_version,
            todo_lists,
            todo_list_shown,
            todo_list_color,
            todo_add,
            todo_update,
            todo_delete,
            todo_flag,
            todo_twos,
            twos_get,
            twos_set,
            twos_test,
            spam_filter_state,
            spam_filter_arrival_set,
            spamhaus_key_set,
            spam_filter_learn,
            set_status,
            set_priority,
            set_label,
            label_list,
            label_list_set,
            trash_summary,
            empty_trash,
            notices,
            signatures,
            signatures_set,
            spell_check,
            opening::ics_file,
            opening::ics_add,
            dictate,
            spell_add,
            optimize_library,
            sign_ins,
            missing_sign_ins,
            offline,
            pick_paths,
            import_files,
            activity,
            api_overview,
            api_waiting,
            api_answer,
            api_answer_send,
            api_revoke,
            api_set_trust,
            api_log,
            api_settings_set,
            plugins_list,
            plugin_install,
            plugin_enable,
            plugin_remove,
            plugin_run,
            ui_mode_get,
            tray_set,
            classic_eudora_found,
            ui_mode_set,
            spam_entry_add,
            spam_rule_remove,
            like_preview,
            like_add,
            complete_address,
            rebuild_contacts,
            background_state,
            background_set,
            update_get,
            update_set,
            update_check,
            update_install,
            restore_suggested,
            library_switch,
            check_every_get,
            check_every_set,
            download_period_get,
            keep_preview,
            keep_counts,
            download_period_set,
            phone_backups,
            backup_pick,
            backup_picked_clear,
            backup_save_copy,
            attachment_share
        ])
        .build(tauri::generate_context!())
        .expect("error while starting Eudora")
        .run(|app, event| {
            // closing Eudora stops the download it started (it resumes next time)
            if let tauri::RunEvent::Exit = event {
                app.state::<background::Background>().shut_down();
                app.state::<api_host::ApiHost>().stop(app);
                // When I Close Eudora: the update goes in now
                eudora_core::update::install_on_close();
            }
            // the Mac hands files opened in the Finder to the running program
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = &event {
                for url in urls {
                    if url.scheme().eq_ignore_ascii_case("mailto") {
                        opening::open_or_say(app, std::path::Path::new(url.as_str()));
                    } else if let Ok(path) = url.to_file_path() {
                        opening::open_or_say(app, &path);
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Every window command and the API method that does the same.
    const IN_THE_API: &[(&str, &str)] = &[
        ("info", "v1.library.info"),
        ("folders", "v1.folders.list"),
        ("page", "v1.mail.page"),
        ("count", "v1.mail.count"),
        ("list_ids", "v1.mail.ids"),
        ("sorted_page", "v1.mail.sorted"),
        ("header_values", "v1.mail.header_values"),
        ("accounts", "v1.accounts.list"),
        ("set_account_label", "v1.accounts.set_name"),
        ("search", "v1.mail.search"),
        ("search_scope_get", "v1.search.scope"),
        ("search_scope_set", "v1.search.scope_set"),
        ("message", "v1.mail.view"),
        ("source", "v1.mail.source"),
        ("copy_source", "v1.mail.copy_source"),
        ("web_pictures", "v1.mail.pictures"),
        ("download_status", "v1.library.activity"),
        ("activity", "v1.library.activity"),
        ("background_jobs", "v1.library.jobs"),
        ("job_pause", "v1.library.job_pause"),
        ("job_details", "v1.library.job_log"),
        ("memory_get", "v1.settings.get"),
        ("memory_set", "v1.settings.set"),
        ("maintenance_get", "v1.library.maintenance"),
        ("maintenance_set", "v1.library.maintenance_set"),
        ("maintenance_now", "v1.library.maintain"),
        ("speed_stats", "v1.library.speed"),
        ("unread_counts", "v1.folders.unread"),
        ("mark_folders_read", "v1.folders.mark_read"),
        ("check_mail", "v1.library.check_mail"),
        ("firewall_status", "v1.firewall.status"),
        ("firewall_set", "v1.firewall.set"),
        ("set_pause", "v1.library.pause"),
        ("get_pause", "v1.library.pause_state"),
        ("find_mailboxes", "v1.accounts.find_shared"),
        ("add_shared_mailboxes", "v1.accounts.add_shared"),
        ("imap_test", "v1.accounts.imap_test"),
        ("imap_save", "v1.accounts.imap_save"),
        ("imap_accounts", "v1.accounts.imap_list"),
        ("setting", "v1.settings.get"),
        ("set_setting", "v1.settings.set"),
        ("settings_with_prefix", "v1.settings.prefix"),
        ("identities", "v1.accounts.identities"),
        ("compose_seed", "v1.compose.start"),
        ("send_message", "v1.mail.send"),
        ("queue_message", "v1.mail.send"),
        ("save_draft", "v1.drafts.save"),
        ("drafts", "v1.drafts.list"),
        ("delete_draft", "v1.drafts.delete"),
        ("move_messages", "v1.mail.move"),
        ("delete_messages", "v1.mail.delete"),
        ("copy_messages", "v1.mail.copy"),
        ("folder_create", "v1.folders.create"),
        ("folder_rename", "v1.folders.rename"),
        ("folder_delete", "v1.folders.delete"),
        ("mail_host", "v1.accounts.mail_host"),
        ("pending_changes", "v1.library.pending"),
        ("mail_stats", "v1.library.stats"),
        ("views_list", "v1.views.list"),
        ("view_groups", "v1.views.groups"),
        ("view_why", "v1.views.why"),
        ("look_now", "v1.library.look"),
        ("attachment_save", "v1.mail.attachment_save"),
        ("attachments_save_all", "v1.mail.attachments_save_all"),
        ("cards_list", "v1.contacts.list"),
        ("card_counts", "v1.contacts.counts"),
        ("google_sign_in", "v1.accounts.google_sign_in"),
        ("google_available", "v1.accounts.google_available"),
        ("card_origin", "v1.contacts.origin"),
        ("card_update", "v1.contacts.update"),
        ("card_create", "v1.contacts.create"),
        ("card_delete", "v1.contacts.delete"),
        ("card_share", "v1.contacts.share"),
        ("card_get", "v1.contacts.get"),
        ("cards_export", "v1.contacts.export"),
        ("subscriptions", "v1.subscriptions.list"),
        ("unsubscribe", "v1.subscriptions.unsubscribe"),
        ("views_accounts", "v1.views.accounts"),
        ("views_accounts_set", "v1.views.accounts_set"),
        ("outbox_list", "v1.outbox.list"),
        ("outbox_unqueue", "v1.outbox.unqueue"),
        ("outbox_reschedule", "v1.outbox.reschedule"),
        ("outbox_send_now", "v1.outbox.send_now"),
        ("backup_info", "v1.library.backup_info"),
        ("backup_start", "v1.library.backup"),
        ("find_old_attachments", "v1.library.old_attachments"),
        ("restore_describe", "v1.library.restore_describe"),
        ("restore_start", "v1.library.restore"),
        ("restore_destination", "v1.library.restore_destination"),
        ("sync_state", "v1.sync.state"),
        ("sync_setup", "v1.sync.setup"),
        ("sync_join", "v1.sync.join"),
        ("sync_set_carriers", "v1.sync.carriers"),
        ("sync_now", "v1.sync.now"),
        ("server_changes", "v1.library.server_changes"),
        ("microsoft_sign_in", "v1.accounts.microsoft_sign_in"),
        ("rules_get", "v1.rules.list"),
        ("rules_set", "v1.rules.set"),
        ("rules_import_classic", "v1.rules.import_classic"),
        ("rules_run", "v1.rules.run"),
        ("rules_run_items", "v1.rules.run_items"),
        ("sort_preview", "v1.spam.preview"),
        ("sort_apply", "v1.spam.apply"),
        ("mark_spam", "v1.spam.mark"),
        ("sort_why", "v1.spam.why"),
        ("spam_rules", "v1.spam.rules"),
        ("spam_rule_set", "v1.spam.rule_set"),
        ("spam_mailboxes", "v1.spam.mailboxes"),
        ("spam_mailboxes_set", "v1.spam.mailboxes_set"),
        ("spam_languages", "v1.spam.languages"),
        ("spam_languages_set", "v1.spam.languages_set"),
        ("calendars_list", "v1.calendar.calendars"),
        ("calendar_shown", "v1.calendar.shown"),
        ("calendar_color", "v1.calendar.color"),
        ("calendar_events", "v1.calendar.events"),
        ("calendar_event", "v1.calendar.get"),
        ("calendar_search", "v1.calendar.search"),
        ("event_respond", "v1.calendar.respond"),
        ("event_update", "v1.calendar.update"),
        ("event_delete", "v1.calendar.delete"),
        ("reservations_list", "v1.calendar.reservations"),
        ("invites_list", "v1.calendar.invites"),
        ("places_suggest", "v1.calendar.places"),
        ("conference_options", "v1.calendar.conference_options"),
        ("appointment_new", "v1.calendar.create"),
        ("appointment_get", "v1.calendar.appointment"),
        ("appointment_edit", "v1.calendar.edit"),
        ("appointment_ics", "v1.calendar.ics"),
        ("invite_spam", "v1.calendar.invite_spam"),
        ("invitation", "v1.calendar.invitation"),
        ("invitation_add", "v1.calendar.invitation_add"),
        ("reservations_answer", "v1.calendar.reservations_answer"),
        ("reservation_off", "v1.calendar.reservation_off"),
        ("reservation_hide", "v1.calendar.reservation_hide"),
        ("trips_from_message", "v1.calendar.trips_from_message"),
        ("todos_list", "v1.todo.list"),
        ("todo_lists", "v1.todo.lists"),
        ("todo_list_shown", "v1.todo.list_shown"),
        ("todo_list_color", "v1.todo.list_color"),
        ("todo_add", "v1.todo.add"),
        ("todo_update", "v1.todo.update"),
        ("todo_delete", "v1.todo.delete"),
        ("todo_flag", "v1.todo.flag"),
        ("todo_twos", "v1.todo.twos"),
        ("spam_filter_state", "v1.spam.state"),
        ("spam_filter_arrival_set", "v1.spam.arrival"),
        ("spamhaus_key_set", "v1.spam.spamhaus_key"),
        ("spam_filter_learn", "v1.spam.learn"),
        ("spam_entry_add", "v1.spam.entry_add"),
        ("spam_rule_remove", "v1.spam.rule_remove"),
        ("like_preview", "v1.spam.like_preview"),
        ("like_add", "v1.spam.like_add"),
        ("set_status", "v1.mail.mark"),
        ("set_priority", "v1.mail.mark"),
        ("set_label", "v1.mail.mark"),
        ("label_list", "v1.labels.list"),
        ("label_list_set", "v1.labels.set_list"),
        ("trash_summary", "v1.trash.summary"),
        ("empty_trash", "v1.trash.empty"),
        ("signatures", "v1.signatures.get"),
        ("signatures_set", "v1.signatures.set"),
        ("spell_check", "v1.compose.spell"),
        ("spell_add", "v1.compose.spell_add"),
        ("optimize_library", "v1.library.optimize"),
        ("sign_ins", "v1.accounts.sign_ins"),
        ("missing_sign_ins", "v1.accounts.missing_sign_ins"),
        ("offline", "v1.library.offline"),
        ("import_files", "v1.library.import"),
        ("plugins_list", "v1.plugins.list"),
        ("plugin_install", "v1.plugins.install"),
        ("plugin_enable", "v1.plugins.enable"),
        ("plugin_remove", "v1.plugins.remove"),
        ("plugin_run", "v1.plugins.run"),
        ("complete_address", "v1.compose.complete"),
        ("complete_directory", "v1.compose.directory"),
        ("rebuild_contacts", "v1.compose.rebuild_contacts"),
        ("background_state", "v1.library.background"),
        ("background_set", "v1.library.background"),
        ("ui_mode_get", "v1.settings.get"),
        ("ui_mode_set", "v1.settings.set"),
        ("update_get", "v1.updates.status"),
        ("update_set", "v1.updates.settings_set"),
        ("update_check", "v1.updates.check"),
        ("update_install", "v1.updates.install"),
        ("check_every_get", "v1.accounts.check_every"),
        ("check_every_set", "v1.accounts.check_every_set"),
        ("download_period_get", "v1.accounts.download_period"),
        ("download_period_set", "v1.accounts.download_period_set"),
        ("keep_preview", "v1.accounts.keep_preview"),
        ("keep_counts", "v1.accounts.keep_preview"),
    ];

    /// The only window commands with no API method, and why.
    const WINDOW_ONLY: &[(&str, &str)] = &[
        ("pick_paths", "the window's Open dialog (programs pass paths themselves)"),
        ("open_compose", "opens a window"),
        ("attachment_drag", "the system's drag of a file out of the window"),
        ("messages_write", "files for the window's own drag of messages out (programs: v1.mail.save_as)"),
        ("files_drag", "the system's drag of files out of the window"),
        ("files_place", "moves the window's dragged-out files into the folder the user chose"),
        ("sort_progress", "the count of the window's own spam-filter job"),
        ("sort_stop", "stops the window's own spam-filter job"),
        ("open_reader", "opens a window"),
        ("open_mailto", "opens a message window from a mailto: link (v1.mail.send sends one)"),
        ("open_help", "opens a window"),
        ("app_version", "the program's own version (v1.updates.status has it too)"),
        ("search_warm", "gets the window's own search connection ready (v1.mail.search does the same when it runs)"),
        ("library_switch", "starts the window again on another library"),
        ("attachment_share", "the phone's share sheet (the API has attachment_save)"),
        ("phone_backups", "lists the phone's Files folder for Restore (a picker's job on a computer)"),
        ("backup_pick", "Android's file picker (programs pass a backup's path to v1.library.restore)"),
        ("backup_picked_clear", "deletes the copy Android's file picker made"),
        ("backup_save_copy", "Android's picker for where to save (programs pass a path to v1.library.backup)"),
        ("restore_suggested", "the window's suggested folder (v1.library.restore picks the same when dest is left out)"),
        ("theme_set", "the windows' own light or dark look"),
        ("dictate", "presses the system's voice-typing keys in the window in front"),
        ("invitation_answer", "sends an email: programs send through v1.mail.send, which the user confirms"),
        ("opening::ics_file", "a calendar file opened in the window (kept in its memory)"),
        ("opening::ics_add", "adds an event from a calendar file opened in the window (v1.calendar.create adds one from anywhere)"),
        ("user_active", "the window telling the download the user is here (maintenance waits)"),
        ("maps_key_present", "a secret: whether the Google Maps key is set"),
        ("zoom_get", "a secret: the Zoom app's sign-in"),
        ("zoom_set", "a secret: the Zoom app's Client ID and Client Secret, typed by the user"),
        ("zoom_sign_in", "a sign-in in the browser, the user's own"),
        ("zoom_sign_out", "a sign-in: the user's own"),
        ("maps_key_set", "a secret: the Google Maps key, typed by the user"),
        ("open_link", "opens the browser on a link the user clicked"),
        ("attachment_open", "starts the system's program for a file the user clicked (programs save attachments themselves)"),
        ("file_icon", "a picture the window draws beside an attachment"),
        ("twos_get", "Twos' sign-in is set up in the window only (its token is a secret)"),
        ("twos_set", "Twos' sign-in is set up in the window only (its token is a secret)"),
        ("twos_test", "the window's test of the Twos sign-in typed there"),
        ("platform", "how the window words things for this system"),
        ("tray_set", "the tray icon is the window's own"),
        ("classic_eudora_found", "the window's offer of Classic mode"),
        ("notices", "the window's own status bar"),
        ("job_state", "the window's progress bar (API calls wait for their answer instead)"),
        ("job_cancel", "the window's Stop button"),
        ("sync_phrase", "a secret: the sync recovery phrase is shown only in the window"),
        ("api_overview", "managing the API is the user's alone"),
        ("api_waiting", "managing the API is the user's alone"),
        ("api_answer", "a program can never grant itself permissions"),
        ("api_answer_send", "a program can never approve its own sends"),
        ("api_revoke", "managing the API is the user's alone"),
        ("api_set_trust", "a program can never raise its own trust"),
        ("api_log", "managing the API is the user's alone"),
        ("api_settings_set", "managing the API is the user's alone"),
    ];

    /// The names each page's scripts declare at their top level. The scripts of a page share one
    /// scope, and a name declared twice there makes the browser refuse the whole script: on
    /// 2026-10-18 a second `const KIND_NAMES` left the main window with no folders or mail.
    #[test]
    fn no_page_declares_a_name_twice() {
        let ask = include_str!("../ui/ask.js");
        let find = include_str!("../ui/find.js");
        let pages = [("index.html", vec![ask, find, include_str!("../ui/app.js"), include_str!("../ui/calendar.js"), include_str!("../ui/todo.js")]), ("compose.html", vec![ask, find, include_str!("../ui/compose.js")])];
        for (page, scripts) in pages {
            let mut seen = std::collections::HashSet::new();
            for script in scripts {
                for line in script.lines() {
                    let rest = ["async function ", "function ", "const ", "let ", "var ", "class "].iter().find_map(|k| line.strip_prefix(k));
                    let Some(rest) = rest else { continue };
                    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
                    if !name.is_empty() {
                        assert!(seen.insert(name.clone()), "{page}: `{name}` is declared twice at the top level of its scripts");
                    }
                }
            }
        }
    }

    /// What's New has this version: raising the version without saying what's new fails here, as
    /// does a link in it to a page the manual doesn't have.
    #[test]
    fn whats_new_has_this_version_and_its_links_lead_somewhere() {
        let help = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/help");
        let notes = std::fs::read_to_string(help.join("whats-new.html")).unwrap();
        let version = env!("CARGO_PKG_VERSION");
        assert!(notes.contains(&format!("<h2 id=\"v{}\">Version {version} ", version.replace('.', "-"))), "help/whats-new.html has no section for version {version}: say what's new in it");
        let mut ids = String::new();
        for f in std::fs::read_dir(&help).unwrap().flatten() {
            if f.path().extension().is_some_and(|e| e == "html") {
                ids.push_str(&std::fs::read_to_string(f.path()).unwrap());
            }
        }
        for link in notes.split("href=\"#").skip(1) {
            let id = &link[..link.find('"').unwrap()];
            assert!(ids.contains(&format!("<section id=\"{id}\"")), "What's New links to #{id}, which no manual page has");
        }
    }

    /// Every command in the looks' menus is one the window does (its ACTIONS, or a message being
    /// written's own), every shortcut is written as the menu library reads it, and no menu bar
    /// gives one shortcut to two commands.
    #[cfg(desktop)]
    #[test]
    fn the_looks_menus_name_real_commands_and_shortcuts_once() {
        use crate::{look_menus, Entry};
        let app = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.js")).unwrap();
        let actions = &app[app.find("const ACTIONS = {").unwrap()..];
        let actions = &actions[..actions.find("\n};").unwrap()];
        // (done by a message being written, or by the program itself)
        let elsewhere = ["exit", "close", "save", "attach", "send"];
        let keys = ["F1", "F3", "F4", "F7", "F9", "Enter", ",", ".", "Insert"];
        for look in ["outlook", "thunderbird", "gmail", "yahoo"] {
            let menus = look_menus(look).unwrap();
            let mut seen_ids = std::collections::HashSet::new();
            let mut seen_keys = std::collections::HashMap::new();
            fn walk<'a>(entries: &'a [Entry], out: &mut Vec<(&'a str, Option<&'a str>)>) {
                for e in entries {
                    match e {
                        Entry::Do(id, _, k) => out.push((id, *k)),
                        Entry::Sub(_, inner) => walk(inner, out),
                        _ => {}
                    }
                }
            }
            let mut all = Vec::new();
            for (_, entries) in &menus {
                walk(entries, &mut all);
            }
            for (id, key) in all {
                let known = elsewhere.contains(&id) || actions.contains(&format!("\"{id}\":")) || actions.contains(&format!("\n  {id}:")) || actions.contains(&format!("\n  {id},"));
                assert!(known, "{look}: the menu command {id} isn't one the window does");
                assert!(seen_ids.insert(id), "{look}: {id} is in the menus twice");
                if let Some(k) = key {
                    let parts: Vec<&str> = k.split('+').collect();
                    let (last, mods) = parts.split_last().unwrap();
                    assert!(mods.iter().all(|m| ["CmdOrCtrl", "Shift", "Alt"].contains(m)), "{look}: {k}");
                    assert!(keys.contains(last) || (last.len() == 1 && last.chars().all(|c| c.is_ascii_alphanumeric())), "{look}: {k}");
                    assert!(!mods.is_empty() || last.starts_with('F') || *last == "Insert", "{look}: {k} alone would be taken from typing");
                    if let Some(other) = seen_keys.insert(k, id) {
                        panic!("{look}: {k} is the shortcut of both {other} and {id}");
                    }
                }
            }
        }
        assert!(look_menus("eudora").is_none());
    }

    #[test]
    fn every_window_command_is_in_the_api_or_has_a_reason_not_to_be() {
        let source = include_str!("lib.rs");
        let start = source.find("generate_handler![").expect("the command list");
        let end = start + source[start..].find("])").unwrap();
        let commands: Vec<&str> = source[start + "generate_handler![".len()..end].split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        assert!(commands.len() > 100, "{commands:?}");
        let methods: Vec<&str> = eudora_core::api::all_methods().map(|m| m.name).collect();
        for c in &commands {
            match IN_THE_API.iter().find(|(w, _)| w == c) {
                Some((_, method)) => assert!(methods.contains(method), "{c} maps to {method}, which isn't an API method"),
                None => assert!(WINDOW_ONLY.iter().any(|(w, _)| w == c), "the window command {c} is neither in the API nor on the window-only list"),
            }
        }
        for (w, _) in IN_THE_API.iter().chain(WINDOW_ONLY) {
            assert!(commands.contains(w), "{w} is listed but isn't a window command any more");
        }
    }

    #[test]
    fn only_web_links_open() {
        assert_eq!(super::web_link(" https://example.com/a?b=1&c=2 "), Some("https://example.com/a?b=1&c=2"));
        assert_eq!(super::web_link("HTTP://example.com"), Some("HTTP://example.com"));
        for bad in ["javascript:alert(1)", "file:///C:/Windows/System32/calc.exe", "data:text/html,x", "ms-settings:", "\\\\server\\share", "https://x.com/\"&calc", "http:no-slashes"] {
            assert_eq!(super::web_link(bad), None, "{bad}");
        }
        assert_eq!(super::query_escape("a b&c=d@é"), "a%20b%26c%3Dd%40%C3%A9");
    }

    /// Every icon the UI names has its Font Awesome Free version committed, so a build without
    /// the Pro package still shows every icon.
    #[test]
    fn every_icon_has_a_free_fallback() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let table = std::fs::read_to_string(here.join("ui/icons.txt")).unwrap();
        let mut missing = Vec::new();
        for name in table.lines().map(|l| l.split('#').next().unwrap().trim()).filter(|l| !l.is_empty()) {
            if !here.join(format!("ui/icons/free/{name}.svg")).is_file() {
                missing.push(name.to_string());
            }
        }
        assert!(missing.is_empty(), "no Free icon for: {missing:?}");
    }

    /// The UI only names icons from the table.
    #[test]
    fn the_ui_names_only_icons_in_the_table() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let table = std::fs::read_to_string(here.join("ui/icons.txt")).unwrap();
        let known: Vec<&str> = table.lines().map(|l| l.split('#').next().unwrap().trim()).filter(|l| !l.is_empty()).collect();
        for (file, marker) in
            [("ui/app.js", "icon(\""), ("ui/index.html", "data-icon=\""), ("ui/compose.js", "icon(\""), ("ui/compose.html", "data-icon=\"")]
        {
            let source = std::fs::read_to_string(here.join(file)).unwrap();
            for piece in source.split(marker).skip(1) {
                let name = piece.split('"').next().unwrap();
                assert!(known.contains(&name), "{file} uses icon \"{name}\", which isn't in ui/icons.txt");
            }
        }
    }
}
