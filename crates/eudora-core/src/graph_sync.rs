//! Downloads a Microsoft 365 mailbox into the library: the primary mailbox (each message's MIME,
//! as Exchange gives it) and, when asked, the Online Archive (Exchange's exports, turned into
//! MIME by `fts`). Read-only on the server: nothing there is changed or deleted.
//!
//! Resumable: every server item stored is noted in `server_items`, so a run that stops (closed,
//! canceled, network gone) continues where it was. Copies of messages already in the library
//! (the same mail imported from old Eudora mailboxes, or in both mailboxes) are merged by
//! Message-ID by the writer: only where they are is recorded.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use rayon::prelude::*;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::db;
use crate::error::{Error, IoContext, Result};
use crate::fts;
use crate::graph::{GraphClient, Link};
use crate::import::{Checkpoint, Counts, Prepared, WriterState, commit_index, original, prepare};
use crate::library::{Library, now};
use crate::pack::Compression;
use crate::parse::DateHints;
use crate::progress::{Progress, thousands};

/// Messages fetched, then stored, at a time.
const CHUNK: usize = 200;
/// Items per export request (Graph's limit).
const EXPORT_BATCH: usize = 20;
/// Make new messages searchable at least this often.
const INDEX_COMMIT: Duration = Duration::from_secs(120);
/// Failures listed in the report (all are counted).
const FAILURES_LISTED: usize = 200;
/// Item classes in archive folders that aren't mail (calendar, contacts, tasks…).

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub primary: bool,
    /// Also the Online Archive (the account setting "Include the Online Archive").
    pub archive: bool,
    /// Only folders whose path starts with this ("Inbox", "Online Archive/Old Sent Items").
    pub only: Option<String>,
    /// Requests at a time (Exchange allows 4 per mailbox).
    pub workers: usize,
    /// Hidden folders too (Exchange's own: conversation settings, sync issues…).
    pub hidden: bool,
    /// Instead of new mail: fetch again the originals of items already here whose own bytes
    /// weren't kept (copies merged before every copy was kept; archive items before their
    /// Exchange export was kept).
    pub originals: bool,
    /// Also the shared mailboxes added to the library (`db::shared_mailboxes`).
    pub shared: bool,
    /// Also the IMAP accounts added to the library (`Library::imap_accounts`).
    pub imap: bool,
    /// Also the Google accounts signed in on this computer (the Gmail API).
    pub google: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions { primary: true, archive: false, only: None, workers: 4, hidden: false, originals: false, shared: false, imap: false, google: false }
    }
}

/// What the fetching threads hand the writer.
enum Chunk {
    /// New items.
    Store(Vec<Prepared>),
    /// Originals of items already here: (message, the copy as fetched).
    Copies(Vec<(i64, Prepared)>),
    /// A folder listed in full with nothing missing, at the server's count then.
    Listed { folder: i64, total: u64 },
    /// A message already here (by its server id in this account) is also in this folder (a
    /// Gmail message's other labels), and unread or not.
    Locate(Vec<(i64, String, bool)>),
    /// A setting to keep (the Gmail side's History position).
    Setting { key: String, value: String },
    /// A folder listed in full (since `since`, when a download period limits it): the server ids
    /// it holds now. What this library had there and the server no longer has was moved or
    /// deleted elsewhere (the computer's rules or spam filter, Outlook): it leaves this folder.
    Present { folder: i64, ids: Vec<String>, since: Option<i64> },
}

/// A fetched item: the message (MIME) and, for an archive item, Exchange's export it was made from.
type Fetched = Result<(Vec<u8>, Option<Vec<u8>>)>;

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncReport {
    pub account: String,
    pub folders: usize,
    /// Items the server listed.
    pub listed: u64,
    /// Already in the library from an earlier run.
    pub already: u64,
    pub downloaded: u64,
    /// Calendar, contact and task items in archive folders (left on the server).
    pub not_mail: u64,
    /// Couldn't be downloaded this time (tried again next run).
    pub failed: u64,
    pub failures: Vec<String>,
    /// POP3 messages left on the server by the account's size limit.
    pub left_big: u64,
    /// POP3 messages deleted from the server (the account's "delete after N days").
    pub pop_deleted: u64,
    /// Archive items Exchange exported but Eudora couldn't convert: stored with the export attached.
    pub unconverted: u64,
    /// Such items from earlier runs that this version could convert (no new download needed).
    pub reconverted: u64,
    pub counts: Counts,
    pub elapsed_secs: f64,
    pub canceled: bool,
    /// Stopped early to start again at once: an account was added while it ran (Check Mail).
    pub restart: bool,
}

enum Place {
    Primary { id: String },
    Archive { owner: String, id: String },
    /// An IMAP account's folder: the account's address and the server's name for the folder.
    Imap { address: String, name: String },
    /// A Gmail label (the Gmail API): the account's address and the label's id ("ALL" = the
    /// messages under no label, Gmail's archived mail: the folder "All Mail").
    Gmail { address: String, label: String },
}

struct Job {
    path: String,
    folder: i64,
    total: u64,
    place: Place,
    /// A shared mailbox's address (None: the user's own mailbox or its archive).
    mailbox: Option<String>,
}

/// A folder to download, before the library has its account and folder row.
struct Planned {
    /// A shared mailbox (address, name); None: the user's own.
    shared: Option<(String, String)>,
    path: String,
    total: u64,
    place: Place,
}

/// One item to fetch: its server id and the server's date for it (a hint when the message has none).
struct Wanted {
    id: String,
    date: Option<String>,
    /// The message it is already here as (fetching originals).
    message: Option<i64>,
    /// Unread on the server (Microsoft 365): stored unread.
    unread: bool,
}

/// How often each account is checked for new mail: setting `check-every` = {address: minutes};
/// an account not in it, every [`DEFAULT_CHECK_MINUTES`].
pub const CHECK_EVERY: &str = "check-every";
pub const DEFAULT_CHECK_MINUTES: u32 = 5;
/// The choices offered (minutes).
pub const CHECK_CHOICES: &[u32] = &[1, 5, 10, 15, 30, 60];

/// Each account's own interval, as chosen (accounts at the default aren't listed).
pub fn check_intervals(library: &Library) -> HashMap<String, u32> {
    library.setting(CHECK_EVERY).ok().flatten().and_then(|j| serde_json::from_str::<HashMap<String, u32>>(&j).ok()).unwrap_or_default().into_iter().map(|(a, m)| (a.to_lowercase(), m)).collect()
}

/// How many minutes between an account's checks.
pub fn check_minutes(library: &Library, address: &str) -> u32 {
    check_intervals(library).get(&address.to_lowercase()).copied().unwrap_or(DEFAULT_CHECK_MINUTES)
}

/// Sets an account's interval (one of [`CHECK_CHOICES`]; the default takes it off the list).
pub fn set_check_minutes(library: &Library, address: &str, minutes: u32) -> Result<()> {
    if !CHECK_CHOICES.contains(&minutes) {
        return Err(Error::Other(format!("Every {} minutes: one of {CHECK_CHOICES:?}.", minutes)));
    }
    let mut all = check_intervals(library);
    if minutes == DEFAULT_CHECK_MINUTES {
        all.remove(&address.to_lowercase());
    } else {
        all.insert(address.to_lowercase(), minutes);
    }
    library.set_setting(CHECK_EVERY, &serde_json::to_string(&all)?)
}

/// The shortest interval of any account: how often the watching download wakes.
pub fn shortest_check(library: &Library) -> u32 {
    check_intervals(library).values().copied().chain([DEFAULT_CHECK_MINUTES]).min().unwrap_or(DEFAULT_CHECK_MINUTES)
}

/// How much mail this library downloads from the servers: setting `download-period` = {"days":
/// N} (any number; weeks, months and years are said in days) or {"days": null} (everything).
/// Not set = everything and never asked (desktops); the phone asks before its first account.
/// With a period, only mail received within it is listed and fetched (newest first, as always)
/// and the Online Archive is left out (it holds older mail).
pub const PERIOD_SETTING: &str = "download-period";
/// The period (`PERIOD_SETTING`'s value) the folders in `listed_folders` were listed under.
const LISTED_UNDER: &str = "listed-folders-period";

///
/// Or by space: {"mb": N}: newest first until the library takes up that much (megabytes),
/// then older mail waits on the server (new mail still comes in).
pub fn download_period(library: &Library) -> Option<Option<u32>> {
    let value = period_setting(library)?;
    Some(value["days"].as_u64().map(|d| d.clamp(1, 100 * 365) as u32))
}

/// The space chosen (megabytes), when the limit is by space.
pub fn download_space_mb(library: &Library) -> Option<u64> {
    period_setting(library)?["mb"].as_u64().filter(|mb| *mb > 0)
}

fn period_setting(library: &Library) -> Option<serde_json::Value> {
    serde_json::from_str(&library.setting(PERIOD_SETTING).ok().flatten()?).ok()
}

/// Chooses the period (`None` = everything).
pub fn set_download_period(library: &Library, days: Option<u32>) -> Result<()> {
    if days == Some(0) {
        return Err(Error::Other("A period of at least one day, or everything.".into()));
    }
    library.set_setting(PERIOD_SETTING, &serde_json::json!({ "days": days }).to_string())
}

/// Chooses a space limit instead (megabytes, at least 100).
pub fn set_download_space(library: &Library, mb: u64) -> Result<()> {
    if mb < 100 {
        return Err(Error::Other("A space of at least 100 MB.".into()));
    }
    library.set_setting(PERIOD_SETTING, &serde_json::json!({ "days": null, "mb": mb }).to_string())
}

/// What a library takes up on disk (every file in its folder).
pub fn library_bytes(root: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => walk(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    walk(root)
}

/// The oldest moment downloaded now (unix seconds), when a period is chosen.
pub fn download_cutoff(library: &Library) -> Option<i64> {
    download_period(library).flatten().map(|days| now() - i64::from(days) * 86_400)
}

/// When each account was last checked in this run of the download.
static LAST_CHECKED: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
/// Check Mail asked: this pass checks every account, whatever its interval.
pub static CHECK_EVERY_ACCOUNT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether an account is due for its check now (then it's marked checked): its first check in
/// this run, after Check Mail, or once its interval has passed (with 30 seconds' grace, so a
/// check that wakes a moment early isn't put off a whole interval). Whether the Inbox of a
/// Microsoft account has mail Eudora doesn't have yet: its newest 10 asked of Microsoft (one
/// small request), each compared with what's here. `seen` keeps the ones already reported, so one
/// that can't be downloaded doesn't start a check every minute. Checked every minute between
/// checks; new mail starts that account's check at once (`make_due`).
pub(crate) fn inbox_has_new(library: &Library, client: &mut GraphClient, seen: &mut HashSet<String>) -> Result<bool> {
    let folder: Option<i64> = library
        .conn
        .query_row(
            "SELECT f.id FROM folders f JOIN accounts a ON a.id = f.account WHERE a.kind = 'graph' AND a.address = ?1 COLLATE NOCASE AND f.path = 'Inbox'",
            params![client.account],
            |r| r.get(0),
        )
        .optional()?;
    let Some(folder) = folder else { return Ok(false) };
    let newest = client.newest_ids("inbox", 10)?;
    let mut here = library.conn.prepare_cached("SELECT 1 FROM server_items WHERE folder = ?1 AND server_id = ?2")?;
    let mut fresh = false;
    for (id, _) in newest {
        if !seen.contains(&id) && !here.exists(params![folder, id])? {
            seen.insert(id);
            fresh = true;
        }
    }
    Ok(fresh)
}

/// Accounts to check at the next pass whatever their interval (new mail seen in the Inbox).
static DUE_NOW: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Checks this account at the next pass (lowercase address).
pub(crate) fn make_due(address: &str) {
    let key = address.to_lowercase();
    let mut now = DUE_NOW.lock().unwrap_or_else(|e| e.into_inner());
    if !now.contains(&key) {
        now.push(key);
    }
}

fn account_due(library: &Library, address: &str) -> bool {
    let key = address.to_lowercase();
    let mut minutes = check_minutes(library, &key);
    // a Gmail account while nobody uses Eudora here: every few hours
    if let Some(r) = gmail_resting(library).filter(|_| crate::google::signed_in_accounts().iter().any(|a| a.eq_ignore_ascii_case(&key))) {
        minutes = minutes.max(r);
    }
    let every = Duration::from_secs(u64::from(minutes) * 60).saturating_sub(Duration::from_secs(30));
    let mut last = LAST_CHECKED.lock().unwrap_or_else(|e| e.into_inner());
    let last = last.get_or_insert_with(HashMap::new);
    let asked_now = {
        let mut now = DUE_NOW.lock().unwrap_or_else(|e| e.into_inner());
        let before = now.len();
        now.retain(|a| a != &key);
        now.len() != before
    };
    let due = asked_now || CHECK_EVERY_ACCOUNT.load(Ordering::Relaxed) || last.get(&key).is_none_or(|t| t.elapsed() >= every);
    if due {
        last.insert(key, Instant::now());
    }
    due
}

/// Minutes between a Gmail account's checks while nobody uses Eudora on this computer: setting
/// `gmail-idle-minutes` (this computer only), 120 when not set, 0 = as usual. Other accounts
/// keep their own intervals.
pub const GMAIL_IDLE_SETTING: &str = "gmail-idle-minutes";
pub const GMAIL_IDLE_DEFAULT: u32 = 120;
/// Not being used = the window untouched this long (or closed).
const IDLE_AFTER: i64 = 15 * 60;

pub fn gmail_idle_minutes(library: &Library) -> u32 {
    library.setting(GMAIL_IDLE_SETTING).ok().flatten().and_then(|v| v.trim().parse().ok()).unwrap_or(GMAIL_IDLE_DEFAULT)
}

/// The longer wait for Gmail accounts when nobody is using Eudora here (None: in use, the choice
/// is off, or no Gmail account). Phones check on their own schedule (the system's background time).
pub fn gmail_resting(library: &Library) -> Option<u32> {
    if cfg!(any(target_os = "android", target_os = "ios")) || crate::google::signed_in_accounts().is_empty() {
        return None;
    }
    let minutes = gmail_idle_minutes(library);
    let idle = crate::maintenance::idle_for(&library.root).is_none_or(|s| s >= IDLE_AFTER);
    (minutes > 0 && idle).then_some(minutes)
}

/// Whether archive items keep Exchange's export beside their MIME (`Library::keeps_exchange_exports`),
/// read at the start of each download.
static KEEP_EXPORTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Reads back what changed on a Microsoft account's server (read in Outlook, labels, calendars,
/// address books, To Do) on a thread of its own that outlives a pass, with a client and a
/// connection state of its own (a pass ending doesn't cancel it). A pass hands it a round when
/// it's free and records its results when they come, in that pass or a later one.
pub(crate) struct Readback {
    pub(crate) jobs: Sender<crate::server_changes::Batch>,
    pub(crate) results: crossbeam_channel::Receiver<crate::server_changes::BatchResult>,
    /// A round is out (its results not recorded yet).
    pub(crate) busy: AtomicBool,
}

static READBACKS: Mutex<Vec<(String, std::sync::Arc<Readback>)>> = Mutex::new(Vec::new());

/// The account's read-back worker, started the first time.
fn readback_for(account: &str, client: &GraphClient) -> std::sync::Arc<Readback> {
    let mut all = READBACKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, r)) = all.iter().find(|(a, _)| a.eq_ignore_ascii_case(account)) {
        return r.clone();
    }
    let (jobs, jobs_rx) = crossbeam_channel::unbounded::<crate::server_changes::Batch>();
    let (results_tx, results) = crossbeam_channel::unbounded::<crate::server_changes::BatchResult>();
    let mut worker = client.worker();
    worker.link = std::sync::Arc::new(Link::default());
    worker.set_mailbox(None);
    std::thread::spawn(move || {
        for batch in jobs_rx {
            if results_tx.send(crate::server_changes::carry_out_batch(&mut worker, &batch)).is_err() {
                break;
            }
        }
    });
    let r = std::sync::Arc::new(Readback { jobs, results, busy: AtomicBool::new(false) });
    all.push((account.to_lowercase(), r.clone()));
    r
}

pub fn download(library: &mut Library, mut client: Option<&mut GraphClient>, options: &SyncOptions, progress: &dyn Progress) -> Result<SyncReport> {
    let started = Instant::now();
    KEEP_EXPORTS.store(library.keeps_exchange_exports(), Ordering::Relaxed);
    // without a Microsoft 365 sign-in (an IMAP / POP3-only setup): only those accounts
    let mut without_microsoft = options.clone();
    if client.is_none() {
        without_microsoft.primary = false;
        without_microsoft.archive = false;
        without_microsoft.shared = false;
    }
    let options = &without_microsoft;
    // who the status file and the report name: the Microsoft 365 account, else the mail servers'
    let account = match &client {
        Some(c) if options.primary || options.archive || options.shared => c.account.clone(),
        _ => imap_addresses(library, options).join(", "),
    };
    let mut report = SyncReport { account: account.clone(), ..Default::default() };
    // a message put back in a folder it was moved out of (the server not caught up yet): gone
    match crate::server_changes::drop_ghost_locations(library) {
        Ok(n) if n > 0 => progress.event(&format!("Took {n} messages out of folders they had been moved out of (they had come back before the server caught up).")),
        Ok(_) => {}
        Err(e) => progress.event(&format!("Couldn't tidy moved messages yet ({e}).")),
    }
    let alive = Alive { inner: progress, root: library.root.clone(), account: account.clone(), last: Mutex::new(Instant::now() - STATUS_EVERY) };
    let progress: &dyn Progress = &alive;
    // a watching download's next pass: the last one ended with link.stop(), and the folders
    // are read below through the same link (resumed only after them, every check after the
    // first was "canceled" there: found 2026-10-16)
    if let Some(c) = client.as_deref() {
        c.link.resume();
    }
    let wanted =|path: &str| options.only.as_deref().is_none_or(|only| path.to_lowercase().starts_with(&only.to_lowercase()));

    // 1. The folders: the user's own mailbox, its archive, and the shared mailboxes added.
    let mut plan: Vec<Planned> = Vec::new();
    let mut mailboxes: Vec<Option<(String, String)>> = Vec::new();
    // (each account only when its own interval has passed: `account_due`)
    if options.primary && client.as_deref().is_some_and(|c| account_due(library, &c.account)) {
        mailboxes.push(None);
    }
    if options.shared {
        // only the shared mailboxes marked active; every one when no choice was ever made (the
        // command line's `--shared` as before)
        let active = shared_active(library);
        mailboxes.extend(
            db::shared_mailboxes(&library.conn)?
                .into_iter()
                .filter(|(address, _)| active.as_ref().is_none_or(|list| list.iter().any(|a| a.eq_ignore_ascii_case(address))))
                .filter(|(address, _)| account_due(library, address))
                .map(Some),
        );
    }
    for (n, shared) in mailboxes.into_iter().enumerate() {
        let Some(client) = client.as_deref_mut() else { break };
        let mut reader = client.worker();
        reader.set_mailbox(shared.as_ref().map(|(address, _)| address.clone()));
        let name = shared.as_ref().map_or_else(|| "the mailbox".to_string(), |(address, _)| address.clone());
        progress.report(0, 0, &format!("Reading the folders of {name} (mailbox {})…", n + 1));
        let listed = reader.folders();
        if let Some(line) = crate::graph::throttling_said() {
            progress.event(&format!("Reading the folders of {name}: {line}"));
        }
        let folders = match listed {
            Ok(folders) => folders,
            Err(Error::Canceled) => return Err(Error::Canceled),
            Err(e) if shared.is_some() => {
                // a shared mailbox the user can't open (any more): skipped, said why
                report.failures.push(format!("{name}: couldn't open it ({e})"));
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut path: Vec<String> = Vec::new();
        for f in folders {
            path.truncate(f.depth);
            path.push(f.display_name.replace('/', "-"));
            let p = path.join("/");
            if (f.is_hidden && !options.hidden) || f.display_name == crate::graph::SYNC_FOLDER || !wanted(&p) {
                continue;
            }
            plan.push(Planned { shared: shared.clone(), path: p, total: f.total_item_count, place: Place::Primary { id: f.id } });
        }
    }
    // (a personal account can't have an Online Archive, and a work account may have none:
    // then there's simply nothing to read there)
    // (with a download period, only the archive's mail within it; with a space limit the
    // archive is left out: it holds older mail)
    // (by space: not what was trimmed to stay within it: older than the newest trimmed)
    let cutoff = download_cutoff(library).or_else(|| crate::trim::trimmed_before(library));
    let space = download_space_mb(library).map(|mb| (library.root.clone(), mb * 1_000_000));
    // Folders remembered as listed in full were listed under the period chosen then: a longer
    // one (or everything) must list them again, or the older mail would never come back.
    if !options.originals {
        let period_now = library.setting(PERIOD_SETTING)?.unwrap_or_default();
        if library.setting(LISTED_UNDER)?.unwrap_or_default() != period_now {
            library.conn.execute("DELETE FROM listed_folders", [])?;
            library.set_setting(LISTED_UNDER, &period_now)?;
        }
    }
    let archive_id = match (options.archive && space.is_none(), client.as_deref_mut().filter(|c| c.may_read_archive())) {
        (true, Some(client)) => {
            let (_, archive) = client.mailbox_ids()?;
            if archive.is_none() {
                report.failures.push(format!("{} has no Online Archive: nothing to download from one", client.account));
            }
            archive
        }
        _ => None,
    };
    if let (Some(archive), Some(client)) = (archive_id, client.as_deref_mut()) {
        // the archive's folders not answering never stops the pass: new mail comes first
        let folders = match client.mailbox_folders(&archive, &|n| progress.report(0, 0, &format!("Reading the Online Archive's folders: {}…", thousands(n as u64)))) {
            Ok(folders) => folders,
            Err(e) => {
                report.failures.push(format!("The Online Archive's folders couldn't be read this time ({e}): it's left for the next pass"));
                Vec::new()
            }
        };
        let mut path: Vec<String> = vec!["Online Archive".into()];
        for f in &folders {
            path.truncate(f.depth + 1);
            path.push(f.display_name.replace('/', "-"));
            let p = path.join("/");
            // (calendar, contacts, tasks and notes too: the archive is brought in whole)
            if (f.is_hidden && !options.hidden) || !wanted(&p) {
                continue;
            }
            plan.push(Planned { shared: None, path: p, total: f.total_item_count, place: Place::Archive { owner: GraphClient::folder_owner(&archive, f), id: f.id.clone() } });
        }
    }

    let mut imap_configs: HashMap<String, crate::imap_sync::ImapConfig> = HashMap::new();
    let imap_at_start = imap_addresses(library, options);
    let microsoft_at_start = GraphClient::signed_in_accounts();
    if options.imap && !options.originals {
        for config in library.imap_accounts()? {
            if !account_due(library, &config.address) {
                continue;
            }
            progress.report(0, 0, &format!("Reading the folders of {}…", config.address));
            let listed = crate::imap_sync::Connection::open(&config, None).and_then(|mut connection| {
                let folders = connection.folders();
                connection.logout();
                folders
            });
            match listed {
                Ok(folders) => {
                    for f in folders {
                        if !wanted(&f.path) {
                            continue;
                        }
                        plan.push(Planned {
                            shared: None,
                            path: f.path,
                            total: f.messages as u64,
                            place: Place::Imap { address: config.address.clone(), name: f.server_name },
                        });
                    }
                    imap_configs.insert(config.address.to_lowercase(), config);
                }
                // (tried again on the next run)
                Err(e) => report.failures.push(format!("{}: couldn't read its folders ({e})", config.address)),
            }
        }
    }

    // Google accounts (the Gmail API): each label a folder, and "All Mail" for archived mail.
    // Listed in full once a day; between, a check asks Gmail's History only for what's new
    // (`gmail_from`: the history id each account's check starts from)
    let mut gmail_from: HashMap<String, (String, i64)> = HashMap::new();
    if options.google && !options.originals {
        // a Gmail account here without its sign-in (restored from a backup, say): said once
        // while this download runs, not at every pass
        static SAID: Mutex<Option<HashSet<String>>> = Mutex::new(None);
        for (address, waiting) in crate::gmail_changes::waiting_for_sign_in(library).unwrap_or_default() {
            let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
            if said.get_or_insert_with(HashSet::new).insert(address.to_lowercase()) {
                let changes = match waiting {
                    0 => String::new(),
                    1 => " One change made here waits for it.".into(),
                    n => format!(" {} changes made here wait for it.", thousands(n)),
                };
                progress.event(&format!(
                    "{address} isn't signed in to Google on this computer, so its mail isn't downloaded here.{changes} To sign in: Settings ▸ Accounts ▸ Add a Google account."
                ));
            }
        }
        for address in crate::google::signed_in_accounts() {
            if !account_due(library, &address) {
                continue;
            }
            let listed: Option<serde_json::Value> = library.setting(&gmail_listed_key(&address))?.and_then(|v| serde_json::from_str(&v).ok());
            let incremental = listed.as_ref().and_then(|v| Some((v["from"].as_str()?.to_string(), v["at"].as_i64()?))).filter(|(_, at)| now() - at < GMAIL_FULL_EVERY);
            if let Some(from) = incremental.clone() {
                gmail_from.insert(address.clone(), from);
            }
            progress.report(0, 0, &format!("Reading the labels of {address}…"));
            let planned = crate::google::GoogleClient::resume(&address).and_then(|mut g| {
                let mut out = Vec::new();
                for (id, name, kind) in g.labels()? {
                    let path = match (kind.as_str(), id.as_str()) {
                        ("system", "INBOX") => "Inbox".to_string(),
                        ("system", "SENT") => "Sent".to_string(),
                        ("system", "SPAM") => "Spam".to_string(),
                        ("system", "TRASH") => "Trash".to_string(),
                        ("system", _) => continue, // (UNREAD, STARRED, IMPORTANT, categories, drafts: not folders)
                        // (sync's own label: its items are settings, never mail)
                        _ if name.eq_ignore_ascii_case(crate::graph::SYNC_FOLDER) => continue,
                        _ => name.clone(),
                    };
                    if wanted(&path) {
                        // (the counts only for a full listing: a check for new mail asks nothing per label)
                        out.push((path, if incremental.is_some() { 0 } else { g.label_total(&id).unwrap_or(0) }, id));
                    }
                }
                if wanted("All Mail") {
                    out.push(("All Mail".to_string(), 0, "ALL".to_string()));
                }
                Ok(out)
            });
            match planned {
                Ok(labels) => {
                    for (path, total, label) in labels {
                        plan.push(Planned { shared: None, path, total, place: Place::Gmail { address: address.clone(), label } });
                    }
                }
                Err(e) => report.failures.push(format!("{address}: couldn't read its labels ({e})")),
            }
        }
    }

    // 2. The account, its folders, and a row for this run (its counts and problems).
    let (import_id, jobs) = {
        let tx = library.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // (the Microsoft 365 account row only when its mail is downloaded: an IMAP-only run adds none)
        let mut own: Option<i64> = None;
        let mut jobs = Vec::with_capacity(plan.len());
        for p in plan {
            let account = match (&p.shared, &p.place) {
                (_, Place::Gmail { address, .. }) => db::graph_account(&tx, "gmail", address, "", now())?,
                (_, Place::Imap { address, .. }) => tx.query_row(
                    "SELECT id FROM accounts WHERE kind IN ('imap', 'pop3') AND lower(address) = lower(?1)",
                    [address],
                    |r| r.get::<_, i64>(0),
                )?,
                (Some((address, name)), _) => db::graph_account(&tx, "graph-shared", address, name, now())?,
                (None, _) => match own {
                    Some(id) => id,
                    None => {
                        // (Microsoft 365 folders are only planned with a sign-in)
                        let c = client.as_deref().ok_or_else(|| Error::Other("no Microsoft 365 sign-in".into()))?;
                        *own.insert(db::graph_account(&tx, "graph", &c.account, &c.display_name, now())?)
                    }
                },
            };
            let server_id = match &p.place {
                Place::Primary { id } | Place::Archive { id, .. } | Place::Imap { name: id, .. } | Place::Gmail { label: id, .. } => id,
            };
            // the folder by its server id first, then by its path: a folder renamed or deleted
            // (moved into Deleted Items) in Eudora keeps its mail, and one renamed in Outlook takes
            // its new name here, unless Eudora's own change to it still waits
            let by_id: Option<(i64, String)> =
                tx.query_row("SELECT id, path FROM folders WHERE account = ?1 AND server_id = ?2", params![account, server_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let folder = match by_id {
                Some((id, path)) => {
                    if path != p.path {
                        let waiting: bool = tx.query_row(
                            "SELECT EXISTS (SELECT 1 FROM pending_ops WHERE kind = ?1 AND json_extract(payload, '$.folder') = ?2)",
                            params![crate::folder_changes::KIND, id],
                            |r| r.get(0),
                        )?;
                        let taken: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM folders WHERE account = ?1 AND path = ?2)", params![account, p.path], |r| r.get(0))?;
                        if !waiting && !taken {
                            let (parent_path, name) = p.path.rsplit_once('/').map(|(a, b)| (Some(a), b)).unwrap_or((None, p.path.as_str()));
                            let parent = match parent_path {
                                Some(pp) => Some(db::ensure_folder(&tx, account, pp)?),
                                None => None,
                            };
                            tx.execute("UPDATE folders SET path = ?2, name = ?3, parent = ?4 WHERE id = ?1", params![id, p.path, name, parent])?;
                        }
                    }
                    id
                }
                None => db::ensure_folder(&tx, account, &p.path)?,
            };
            tx.execute("UPDATE folders SET server_id = ?2 WHERE id = ?1", params![folder, server_id])?;
            let label = match (&p.shared, &p.place) {
                (_, Place::Imap { address, .. }) | (_, Place::Gmail { address, .. }) | (Some((address, _)), _) => format!("{address}: {}", p.path),
                (None, _) => p.path,
            };
            jobs.push(Job { path: label, folder, total: p.total, place: p.place, mailbox: p.shared.map(|(address, _)| address) });
        }
        tx.execute(
            "INSERT INTO imports (source_key, sources, started, status) VALUES (?1, ?2, ?3, 'running')",
            params![format!("graph:{account}"), serde_json::to_string(&jobs.iter().map(|j| &j.path).collect::<Vec<_>>())?, now()],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        (id, jobs)
    };
    report.folders = jobs.len();
    let mut writer = WriterState::new(import_id, Checkpoint::default());
    if options.archive {
        reconvert(library, &mut writer, &mut report, progress)?;
    }
    // Fetching originals: per folder, the items here without their own bytes (an archive item
    // without its Exchange export; any other without a copy from that item).
    let mut missing: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
    if options.originals {
        for (n, job) in jobs.iter().enumerate() {
            progress.report(n as u64, jobs.len() as u64, &format!("Finding items without their originals: folder {} of {}", n + 1, jobs.len()));
            let sql = match job.place {
                Place::Archive { .. } => {
                    "SELECT s.server_id, s.message FROM server_items s WHERE s.folder = ?1 AND NOT EXISTS
                     (SELECT 1 FROM copies c WHERE c.folder = s.folder AND c.server_id = s.server_id AND c.format = 'exchange-export')"
                }
                Place::Primary { .. } | Place::Imap { .. } | Place::Gmail { .. } => {
                    "SELECT s.server_id, s.message FROM server_items s WHERE s.folder = ?1 AND NOT EXISTS
                     (SELECT 1 FROM copies c WHERE c.folder = s.folder AND c.server_id = s.server_id)"
                }
            };
            let mut stmt = library.conn.prepare(sql)?;
            let items: Vec<(String, i64)> = stmt.query_map(params![job.folder], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            if !items.is_empty() {
                missing.insert(job.folder, items);
            }
        }
    }
    let total: u64 = if options.originals { missing.values().map(|v| v.len() as u64).sum() } else { jobs.iter().map(|j| j.total).sum() };
    // What's here already, per folder (read once: the fetching threads can't use the database).
    let mut known: HashMap<i64, HashSet<String>> = HashMap::new();
    {
        // (read as bytes: one broken id, a bit flipped on the disk, stopped every check on
        // 2026-10-25; it's taken as it is, broken bytes repaired, and never stops anything)
        let mut stmt = library.conn.prepare("SELECT folder, CAST(server_id AS BLOB) FROM server_items")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))? {
            let (folder, id) = row?;
            known.entry(folder).or_default().insert(crate::text_repair::from_bytes(&id));
        }
        // emptied from the Trash here, still to be deleted on the server, or moved away from a
        // folder here, still to be moved there: not fetched again
        let mut stmt = library.conn.prepare(
            "SELECT json_extract(payload, '$.folder'), json_extract(payload, '$.server_id') FROM pending_ops
             WHERE kind = 'delete' AND json_extract(payload, '$.folder') IS NOT NULL
             UNION ALL
             SELECT json_extract(payload, '$.from'), json_extract(payload, '$.server_id') FROM pending_ops
             WHERE kind = 'move' AND json_extract(payload, '$.from') IS NOT NULL AND json_extract(payload, '$.server_id') IS NOT NULL",
        )?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (folder, id) = row?;
            known.entry(folder).or_default().insert(id);
        }
    }
    // POP3 accounts set to delete from the server some days after Eudora has a message: the
    // messages stored here long enough (only those: nothing is deleted that isn't kept)
    let mut pop_old: HashMap<i64, HashSet<String>> = HashMap::new();
    for job in &jobs {
        let Place::Imap { address, .. } = &job.place else { continue };
        let Some(days) = imap_configs.get(&address.to_lowercase()).filter(|c| c.is_pop3()).and_then(|c| c.pop_delete_after_days) else { continue };
        let mut stmt = library.conn.prepare_cached("SELECT s.server_id FROM server_items s JOIN messages m ON m.id = s.message WHERE s.folder = ?1 AND m.added < ?2")?;
        let ids = stmt.query_map(params![job.folder, now() - days as i64 * 86_400], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        if !ids.is_empty() {
            pop_old.insert(job.folder, ids);
        }
    }
    let (gmail_jobs, jobs): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|j| matches!(j.place, Place::Gmail { .. }));
    let (mut imap_jobs, rest): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|j| matches!(j.place, Place::Imap { .. }));
    imap_jobs.sort_by_key(|j| folder_rank(&j.path));
    let (archive_jobs, rest): (Vec<Job>, Vec<Job>) = rest.into_iter().partition(|j| matches!(j.place, Place::Archive { .. }));
    let (mut shared_jobs, mut primary_jobs): (Vec<Job>, Vec<Job>) = rest.into_iter().partition(|j| j.mailbox.is_some());
    // new mail first: Inboxes, then Sent Items, then the rest (each folder newest first)
    primary_jobs.sort_by_key(|j| folder_rank(&j.path));
    shared_jobs.sort_by_key(|j| folder_rank(&j.path));
    let since = (chrono::Utc::now() - chrono::Duration::minutes(10)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let compression = library.compression();
    let sides_with_jobs = [!primary_jobs.is_empty(), !archive_jobs.is_empty(), !shared_jobs.is_empty(), !imap_jobs.is_empty(), !gmail_jobs.is_empty()].iter().filter(|x| **x).count();
    let shared = Shared {
        report: Mutex::new(report),
        done: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        at: Mutex::new(vec![String::new(); 5]),
        running: AtomicUsize::new(sides_with_jobs),
        check: AtomicU64::new(0),
        pause: AtomicU8::new(read_pause(&library.root) as u8),
        seen: Mutex::new(HashSet::new()),
        listed: {
            let mut stmt = library.conn.prepare("SELECT folder, total FROM listed_folders")?;
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?
        },
        held: {
            let now = now();
            failed_items(library)?.into_iter().filter(|(_, f)| f.next > now).map(|(id, _)| id).collect()
        },
        failed: Mutex::new(Vec::new()),
        cutoff,
        space,
        full: Mutex::new(None),
        gmail_from,
    };
    let mut restart = false;
    let n = options.workers.clamp(1, 4);
    let link = client.as_deref().map_or_else(Default::default, |c| c.link.clone());
    // Microsoft 365's clients (none without a sign-in: then only the mail servers run)
    let mut graph = client.as_deref().map(|c| GraphSides {
        primary: c.worker(),
        archive: c.worker(),
        shared: c.worker(),
        primary_workers: (0..n).map(|_| c.worker()).collect(),
        archive_workers: (0..n.min(3)).map(|_| c.worker()).collect(),
        shared_workers: (0..n).map(|_| c.worker()).collect(),
        changes: c.worker(),
    });
    let may_change = client.as_deref().is_some_and(|c| c.may_change_mail()) && !options.originals;
    let signed_in = client.as_deref().map(|c| c.account.clone()).unwrap_or_default();

    // 3. The user's mailbox, its archive and the shared mailboxes (one after another) at the
    // same time (Exchange limits each mailbox on its own): each side lists its folders, skips
    // what's here and fetches the rest a chunk at a time; this thread, the library's only
    // writer, stores the chunks as they come.
    let outcome: Result<()> = std::thread::scope(|scope| {
        let (tx, rx) = bounded::<Chunk>(4);
        let (graph_sides, changes_client) = match graph.as_mut() {
            Some(g) => (
                vec![
                    (0usize, "main mailbox", &primary_jobs, &mut g.primary, &mut g.primary_workers),
                    (1usize, "Online Archive", &archive_jobs, &mut g.archive, &mut g.archive_workers),
                    (2usize, "shared mailboxes", &shared_jobs, &mut g.shared, &mut g.shared_workers),
                ],
                Some(&mut g.changes),
            ),
            None => (Vec::new(), None),
        };
        let mut handles = Vec::new();
        for (which, label, jobs, client, workers) in graph_sides {
            if jobs.is_empty() {
                continue;
            }
            let tx = tx.clone();
            let (shared, known, compression) = (&shared, &known, &compression);
            let missing = options.originals.then_some(&missing);
            let since = since.as_str();
            handles.push(scope.spawn(move || {
                let result = produce(which, label, jobs, client, workers, known, missing, since, compression, shared, &tx);
                shared.running.fetch_sub(1, Ordering::SeqCst);
                if result.is_err() {
                    shared.stop.store(true, Ordering::SeqCst);
                    return result;
                }
                shared.at.lock().unwrap()[which] = format!("{label}: done");
                // done: keep fetching new mail while the others still download
                if which != 1 && missing.is_none() {
                    let watched = watch_new_mail(which, label, jobs, workers, known, since, compression, shared, &tx);
                    if watched.is_err() {
                        shared.stop.store(true, Ordering::SeqCst);
                    }
                    shared.at.lock().unwrap()[which] = format!("{label}: done");
                    return watched;
                }
                result
            }));
        }
        if !imap_jobs.is_empty() {
            let tx = tx.clone();
            let (shared, known, compression, imap_jobs, imap_configs, pop_old) = (&shared, &known, &compression, &imap_jobs, &imap_configs, &pop_old);
            handles.push(scope.spawn(move || {
                let result = produce_imap(imap_jobs, imap_configs, known, pop_old, compression, shared, &tx);
                shared.running.fetch_sub(1, Ordering::SeqCst);
                if result.is_err() {
                    shared.stop.store(true, Ordering::SeqCst);
                }
                shared.at.lock().unwrap()[3] = "IMAP: done".into();
                result
            }));
        }
        if !gmail_jobs.is_empty() {
            let tx = tx.clone();
            let (shared, known, compression, gmail_jobs) = (&shared, &known, &compression, &gmail_jobs);
            handles.push(scope.spawn(move || {
                let result = produce_gmail(gmail_jobs, known, compression, shared, &tx);
                shared.running.fetch_sub(1, Ordering::SeqCst);
                if result.is_err() {
                    shared.stop.store(true, Ordering::SeqCst);
                }
                shared.at.lock().unwrap()[4] = "Gmail: done".into();
                result
            }));
        }
        drop(tx);
        // Moves made in Eudora, carried out on the server by a thread of its own (no database
        // there): this thread hands it a batch and records what happened.
        let (jobs_tx, jobs_rx) = bounded::<crate::server_changes::Batch>(1);
        let (results_tx, results_rx) = bounded::<crate::server_changes::BatchResult>(1);
        // What changed on the server (read in Outlook, labels, calendars, address books, To Do)
        // is read back by a thread of its own, on a client of its own: a whole mailbox's read-back
        // can take many minutes, and the user's own changes must never wait behind it
        // (2026-10-26: a Microsoft cancellation waited 30 minutes behind a 9-minute read-back)
        // (a worker that outlives the pass: a check never waits for its round to end; what it
        // finds is recorded by whichever pass is running when it's done)
        let readback = changes_client.as_deref().filter(|_| may_change).map(|client| readback_for(&signed_in, client));
        let changes = changes_client.filter(|_| may_change).map(|client| {
            scope.spawn(move || {
                for batch in jobs_rx {
                    if results_tx.send(crate::server_changes::carry_out_batch(client, &batch)).is_err() {
                        break;
                    }
                }
            })
        });
        // IMAP accounts' changes: a thread of their own (each batch signs in to each server)
        let (imap_jobs_tx, imap_jobs_rx) = bounded::<crate::imap_changes::ImapBatch>(1);
        let (imap_results_tx, imap_results_rx) = bounded::<crate::server_changes::BatchResult>(1);
        let imap_changes = (options.imap && !options.originals).then(|| {
            scope.spawn(move || {
                for batch in imap_jobs_rx {
                    if imap_results_tx.send(crate::imap_changes::carry_out(&batch)).is_err() {
                        break;
                    }
                }
            })
        });
        let mut imap_busy = false;
        let mut changes_busy = false;
        let mut told_no_permission = false;
        let mut categories_done = false;
        let mut told_contacts = false;
        let mut told_calendar = false;
        let mut told_todo = false;

        // Store chunks as they come; once a second (also while nothing comes) watch the
        // connection, say what's happening and update the status file.
        let mut result = Ok(());
        let mut last_commit = Instant::now();
        let mut last_status = Instant::now() - STATUS_EVERY;
        let mut last_spool = Instant::now() - Duration::from_secs(60);
        let mut last_headers = Instant::now();
        let mut last_maintain = Instant::now() - Duration::from_secs(60);
        let mut last_throttle = Instant::now();
        // (long jobs write their steps as they go: the window shows them at once)
        crate::activity::set_root(&library.root);
        crate::speed::set_file(&library.root, "download");
        let mut outage: Option<Instant> = None;
        loop {
            let chunk = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(chunk) => Some(chunk),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if progress.canceled() {
                result = Err(Error::Canceled);
                break;
            }
            // header search: a batch whenever nothing came in, else every 10 s (new mail first,
            // then older mail from the newest down) (Stop, "pause everything", halts this
            // background work too)
            if (chunk.is_none() || last_headers.elapsed() >= Duration::from_secs(10)) && shared.pause() != Pause::All {
                last_headers = Instant::now();
                crate::job_log::start_run(); // (this round's details go together in Activity)
                // (each job says what it did, for the window's status bar and Activity)
                let n = |count: u64, one: &str, many: &str| format!("{} {}", thousands(count), if count == 1 { one } else { many });
                // (a job the user paused in Activity is passed over: it does nothing this round)
                let held = crate::activity::paused_jobs(&library.root);
                match if held.contains("headers") { Ok(0) } else { library.update_header_index(500) } {
                    Ok(done) => crate::activity::did("headers", "Indexing headers for header search", done as u64, &n(done as u64, "message indexed", "messages indexed")),
                    Err(e) => progress.event(&format!("Preparing header search paused ({e}); trying again.")),
                }
                // Synthetic Views: new mail first, then older mail a batch at a time
                // mail stored before its HTML-only text was read: preview and search entry made
                match if held.contains("html-text") { Ok(0) } else { library.refill_empty_text(500) } {
                    Ok(done) => crate::activity::did("html-text", "Reading HTML-only messages' text", done, &n(done, "message read", "messages read")),
                    Err(e) => progress.event(&format!("Reading HTML-only messages again paused ({e}); trying again.")),
                }
                // reservations ahead in Travel mail: flights onto the calendar, hotel stays offered
                match if held.contains("travel") { Ok(Default::default()) } else { library.scan_travel(50) } {
                    Ok(t) => {
                        crate::activity::did("travel", "Reading travel mail for Trips", t.messages, &format!("{}; {} added to the calendar, {} offered", n(t.messages, "email read", "emails read"), t.flights_added, t.stays_suggested));
                        if t.flights_added > 0 {
                            progress.event(&format!("Calendar: {} {} from your travel mail added (flights, trains, buses, ferries, rides).", t.flights_added, if t.flights_added == 1 { "trip" } else { "trips" }));
                        }
                    }
                    Err(e) => progress.event(&format!("Reading travel mail for the calendar paused ({e}); trying again.")),
                }
                // Online Archive mail filed elsewhere: moved inside the archive on the server (once
                // the user agreed), or put back in its year folder
                match if held.contains("archive") { Ok((0, 0)) } else { library.queue_archive_refiling(2000) } {
                    Ok((q, back)) if q + back > 0 => crate::activity::did("archive", "Reorganizing the Online Archive", q + back, &format!("{} queued to move inside the archive, {} put back in their year folders", thousands(q), thousands(back))),
                    Ok(_) => {}
                    Err(e) => progress.event(&format!("Reorganizing the Online Archive paused ({e}); trying again.")),
                }
                // the tickets of each trip on the calendar go onto its event (once)
                match if held.contains("travel") { Ok(0) } else { library.queue_tickets() } {
                    Ok(n) if n > 0 => progress.event(&format!("Calendar: tickets and the email put on {n} travel {} (they go to the server in the background).", if n == 1 { "event" } else { "events" })),
                    Ok(_) => {}
                    Err(e) => progress.event(&format!("Putting tickets on travel events paused ({e}); trying again.")),
                }
                // a picture the message shows (a logo) is no longer counted as an attachment
                match if held.contains("attachments") { Ok(0) } else { library.recount_attachments(500) } {
                    Ok(done) => crate::activity::did("attachments", "Recounting attachments", done, &n(done, "message recounted", "messages recounted")),
                    Err(e) => progress.event(&format!("Recounting attachments paused ({e}); trying again.")),
                }
                match if held.contains("views") { Ok(0) } else { library.tag_views(2000) } {
                    Ok(done) => crate::activity::did("views", "Sorting mail into Views", done, &n(done, "message sorted", "messages sorted")),
                    Err(e) => progress.event(&format!("Sorting mail into views paused ({e}); trying again.")),
                }
                // payments and their receipts: read for amounts, linked when they match
                match if held.contains("alerts") { Ok(0) } else { library.confirm_alerts(200) } {
                    Ok(done) => crate::activity::did("alerts", "Checking Red Alerts", done, &n(done, "message checked", "messages checked")),
                    Err(e) => progress.event(&format!("Checking Red Alerts paused ({e}); trying again.")),
                }
                match if held.contains("payments") { Ok(0) } else { library.link_payments(200) } {
                    Ok(done) => crate::activity::did("payments", "Linking payments to receipts", done, &n(done, "message read for amounts", "messages read for amounts")),
                    Err(e) => progress.event(&format!("Linking payments to receipts paused ({e}); trying again.")),
                }
                // duplicates: shown from their most original copy (the user's own files first)
                if let Err(e) = if held.contains("originals") { Ok(()) } else { library.prefer_originals(&mut writer, 2000).map(|_| ()) } {
                    progress.event(&format!("Choosing the most original copies paused ({e}); trying again."));
                }
                crate::activity::write(&library.root);
            }
            // sent copies the viewer left for IMAP / POP3 accounts (it can't write the library)
            if last_spool.elapsed() >= Duration::from_secs(5) {
                last_spool = Instant::now();
                crate::job_log::start_run();
                if let Err(e) = file_sent_copies(library, &mut writer, &compression) {
                    progress.event(&format!("Couldn't file a sent copy yet ({e}); trying again."));
                }
                // the rules for arriving mail, on what reached an Inbox since the last look
                let held = crate::activity::paused_jobs(&library.root);
                let rules = if held.contains("rules") {
                    Ok(Default::default())
                } else {
                    let _timed = crate::speed::Timer::new("Rules and spam filter on new mail");
                    library.run_incoming_rules()
                };
                if let Ok(r) = &rules {
                    let sorted = r.sorted.iter().map(|(c, n)| format!("{n} {c}")).collect::<Vec<_>>().join(", ");
                    crate::activity::did(
                        "rules",
                        "Sorting new mail (your rules and the spam filter)",
                        r.checked,
                        &format!("{} checked: {} moved, {} copied, {} deleted{}", thousands(r.checked), r.moved, r.copied, r.deleted, if sorted.is_empty() { String::new() } else { format!(" (spam filter: {sorted})") }),
                    );
                }
                // maintenance: on its schedule (looked at once a minute), or now when the window asks
                if (last_maintain.elapsed() >= Duration::from_secs(60) || crate::maintenance::waiting(&library.root)) && shared.pause() != Pause::All && !held.contains("maintenance") {
                    last_maintain = Instant::now();
                    crate::activity::next("maintenance", "Maintenance (tidying the library for speed)", library.maintenance_next());
                    let asked = crate::maintenance::requested(&library.root);
                    if asked || library.maintenance_due() {
                        progress.event(if asked { "Maintenance started (asked for)." } else { "Maintenance started (scheduled)." });
                        let _timed = crate::speed::Timer::new("Maintenance (a whole run)");
                        let run = commit_index(library).and_then(|_| library.maintain(asked));
                        match run {
                            Ok(r) => progress.event(&format!("Maintenance done in {}: {}.", span(Duration::from_secs(r.took.max(0) as u64)), r.said)),
                            Err(e) => progress.event(&format!("Maintenance stopped: {e}")),
                        }
                    }
                }
                // Help ▸ Statistics, counted hourly beside (a connection of its own)
                crate::stats::refresh_in_background(&library.root);
                if let Ok(waiting) = crate::server_changes::waiting(library) {
                    crate::activity::left("server", "Making your changes on the servers (moves, read, labels, deletes)", Some(waiting.0));
                }
                crate::activity::write(&library.root);
                match rules {
                    Ok(r) if r.moved + r.copied + r.deleted > 0 => {
                        let sorted = r.sorted.iter().map(|(c, n)| format!("{n} {c}")).collect::<Vec<_>>().join(", ");
                        progress.event(&format!(
                            "Rules on new mail: {} moved, {} copied, {} deleted{}.",
                            r.moved, r.copied, r.deleted, if sorted.is_empty() { String::new() } else { format!(" (spam filter: {sorted})") }
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => progress.event(&format!("The rules for new mail didn't run ({e}); trying again.")),
                }
                // changes made in Eudora (moves, status, labels, deletes): the next batch for the
                // server; and now and then what changed there (read in Outlook…) read back
                if changes.is_some() && !changes_busy && shared.pause() != Pause::All && !crate::activity::paused_jobs(&library.root).contains("server") {
                    // (the timers of what's read back move on only when it can go now)
                    let readback_free = readback.as_ref().is_some_and(|r| !r.busy.load(Ordering::SeqCst));
                    let reconcile = if readback_free { reconcile_due() } else { None };
                    let categories = reconcile == Some(false) && !categories_done;
                    match crate::server_changes::plan_batch(library, &signed_in, crate::server_changes::BATCH, reconcile, categories) {
                        Ok(mut batch) => {
                            // the account's address books: at the start, then hourly
                            if readback_free && contacts_due(&signed_in) {
                                batch.contacts = library.contacts_jobs(&signed_in).unwrap_or_default();
                            }
                            // To Do (the primary Microsoft account's): changes made here at once, read back every 5 minutes
                            if GraphClient::signed_in_accounts().first().is_some_and(|p| p.eq_ignore_ascii_case(&signed_in)) {
                                batch.todos = library.todo_jobs(20).unwrap_or_default();
                                if readback_free && todo_due() {
                                    batch.todo_fetch = library.calendar_jobs(&signed_in, 0, 0).ok().and_then(|j| j.first().map(|j| j.account));
                                }
                            }
                            // its calendars: the near weeks every 15 minutes, everything daily
                            if let Some((from, to)) = calendar_due(&signed_in).filter(|_| readback_free) {
                                batch.calendars = library.calendar_jobs(&signed_in, from, to).unwrap_or_default();
                            }
                            // folders just opened in the window: their newest mail first
                            let asked = take_looks(&library.root);
                            if !asked.is_empty() {
                                match crate::server_changes::look_jobs(library, &asked, &signed_in) {
                                    Ok((jobs, rest)) => {
                                        batch.looks = jobs;
                                        if !rest.is_empty() {
                                            let _ = request_look(&library.root, &rest); // (another account's: its own pass)
                                        }
                                    }
                                    Err(e) => progress.event(&format!("Couldn't look at the folders opened ({e}).")),
                                }
                            }
                            categories_done |= categories;
                            let back = crate::server_changes::Batch {
                                reconcile: std::mem::take(&mut batch.reconcile),
                                categories: std::mem::take(&mut batch.categories),
                                contacts: std::mem::take(&mut batch.contacts),
                                calendars: std::mem::take(&mut batch.calendars),
                                todo_fetch: batch.todo_fetch.take(),
                                ..Default::default()
                            };
                            if !back.is_empty() {
                                if let Some(r) = &readback {
                                    r.busy.store(r.jobs.send(back).is_ok(), Ordering::SeqCst);
                                }
                            }
                            if !batch.is_empty() {
                                changes_busy = jobs_tx.send(batch).is_ok();
                            }
                        }
                        Err(e) => progress.event(&format!("Couldn't read the moves waiting for the server ({e}); trying again.")),
                    }
                }
                if imap_changes.is_some() && !imap_busy && shared.pause() != Pause::All {
                    match crate::imap_changes::plan(library, crate::server_changes::BATCH) {
                        Ok(batch) if !batch.is_empty() => imap_busy = imap_jobs_tx.send(batch).is_ok(),
                        Ok(_) => {}
                        Err(e) => progress.event(&format!("Couldn't read the changes waiting for IMAP servers ({e}); trying again.")),
                    }
                }
                if changes.is_none() && !told_no_permission && !options.originals && !signed_in.is_empty() {
                    told_no_permission = true;
                    if let Ok((waiting, _, _)) = crate::server_changes::waiting(library) {
                        if waiting > 0 {
                            progress.event(&format!(
                                "{} moves wait to be made on the server: sign in again to let Eudora change mail there (Settings ▸ Accounts ▸ Sign in again).",
                                thousands(waiting)
                            ));
                        }
                    }
                }
            }
            let came = match results_rx.try_recv() {
                Ok(r) => Some((r, false)),
                Err(_) => readback.as_ref().and_then(|r| r.results.try_recv().ok()).map(|r| (r, true)),
            };
            if let Some((results, read_back)) = came {
                if read_back {
                    if let Some(r) = &readback {
                        r.busy.store(false, Ordering::SeqCst);
                    }
                } else {
                    changes_busy = false;
                }
                // folders just opened: what was new there, stored now
                for looked in &results.looked {
                    match &looked.items {
                        Ok(items) if !items.is_empty() => {
                            let prepared: Result<Vec<Prepared>> = items
                                .iter()
                                .map(|(id, raw, date)| {
                                    let hints = DateHints { envelope: date.as_deref().and_then(iso_seconds), file: None };
                                    prepare(raw.clone(), hints, format!("{} (server item {})", looked.path, short(id)), &looked.path, looked.folder, Some(id.clone()), &compression, false)
                                })
                                .collect();
                            let unread: Vec<(i64, String, bool)> = looked.unread.iter().map(|id| (looked.folder, id.clone(), true)).collect();
                            match prepared.and_then(|p| writer.store(library, &p)).and_then(|()| locate(library, &unread)) {
                                Ok(()) => progress.event(&format!("{}: {} new (the folder was opened).", looked.path, items.len())),
                                Err(e) => progress.event(&format!("{}: couldn't store its new mail ({e}).", looked.path)),
                            }
                        }
                        Ok(_) => {}
                        Err(e) => progress.event(&format!("{}: couldn't look for new mail ({e}).", looked.path)),
                    }
                }
                match crate::server_changes::record_batch(library, &results) {
                    Ok(b) => {
                        let r = b.moves;
                        let made = r.moved + b.marked + b.deleted + b.events_sent + b.cards_sent;
                        crate::activity::did(
                            "server",
                            "Making your changes on the servers (moves, read, labels, deletes)",
                            made,
                            &format!("{} moved, {} marked, {} deleted, {} calendar and {} contact changes{}", thousands(r.moved), thousands(b.marked), thousands(b.deleted), b.events_sent, b.cards_sent, if b.failed > 0 { format!("; {} waiting ({})", b.failed, b.error.clone().unwrap_or_default()) } else { String::new() }),
                        );
                        if b.turned_unread + b.turned_read + b.labels_changed + b.events.added + b.events.changed + b.events.removed + b.cards.added + b.cards.changed + b.cards.removed > 0 {
                            crate::activity::did(
                                "from-server",
                                "Bringing in changes made elsewhere (Outlook, phones)",
                                b.turned_unread + b.turned_read + b.labels_changed + b.events.added + b.events.changed + b.events.removed + b.cards.added + b.cards.changed + b.cards.removed,
                                &format!("{} read / unread, {} labels, {} calendar events, {} contacts", thousands(b.turned_unread + b.turned_read), thousands(b.labels_changed), thousands(b.events.added + b.events.changed + b.events.removed), thousands(b.cards.added + b.cards.changed + b.cards.removed)),
                            );
                        }
                        let mut said = Vec::new();
                        if b.marked > 0 {
                            said.push(format!("{} marked (read, labels, priority)", thousands(b.marked)));
                        }
                        if b.deleted > 0 {
                            said.push(format!("{} deleted (Empty Trash)", thousands(b.deleted)));
                        }
                        if b.failed > 0 {
                            said.push(format!("{} not changed yet ({})", thousands(b.failed), b.error.clone().unwrap_or_default()));
                        }
                        if results.categories_made > 0 {
                            said.push(format!("{} labels added to Outlook's categories", results.categories_made));
                        }
                        if b.turned_unread + b.turned_read + b.labels_changed > 0 {
                            progress.event(&format!(
                                "From the server: {} now unread, {} now read, {} labels changed (in Outlook or on another device).",
                                thousands(b.turned_unread), thousands(b.turned_read), thousands(b.labels_changed)
                            ));
                        }
                        if b.cards.added + b.cards.changed + b.cards.removed > 0 {
                            progress.event(&format!(
                                "Address book from the server: {} added, {} changed, {} removed.",
                                thousands(b.cards.added), thousands(b.cards.changed), thousands(b.cards.removed)
                            ));
                        }
                        if b.cards_sent > 0 {
                            progress.event(&format!("Address book on the server: {} contact changes made (edits, new contacts, deletions).", thousands(b.cards_sent)));
                        }
                        if b.cards_failed > 0 {
                            progress.event(&format!("{} contact changes not made on the server yet ({}); tried again.", thousands(b.cards_failed), b.cards_send_error.clone().unwrap_or_default()));
                        }
                        if b.events.added + b.events.changed + b.events.removed > 0 {
                            progress.event(&format!(
                                "Calendar from the server: {} events added, {} changed, {} removed.",
                                thousands(b.events.added), thousands(b.events.changed), thousands(b.events.removed)
                            ));
                        }
                        if b.todo.added + b.todo.changed + b.todo.removed > 0 {
                            progress.event(&format!("To Do from Microsoft: {} added, {} changed, {} removed.", thousands(b.todo.added), thousands(b.todo.changed), thousands(b.todo.removed)));
                        }
                        if b.todos_sent > 0 {
                            progress.event(&format!("To Do on Microsoft: {} task changes made.", thousands(b.todos_sent)));
                        }
                        if b.todos_failed > 0 {
                            progress.event(&format!("{} To Do changes not made on Microsoft yet ({}); tried again.", thousands(b.todos_failed), b.todos_send_error.clone().unwrap_or_default()));
                        }
                        if let Some(e) = &b.todo_error {
                            if e.contains("403") || e.contains("Forbidden") || e.contains("Access") || e.contains("scope") {
                                if !told_todo {
                                    told_todo = true;
                                    progress.event(&format!("To Do waits for your next Microsoft sign-in (Settings ▸ Accounts ▸ Sign in again): Microsoft asks you to let Eudora read and change your tasks. (Microsoft said: {e})"));
                                }
                            } else {
                                progress.event(&format!("Couldn't read Microsoft To Do yet ({e}); trying again."));
                            }
                        }
                        if b.events_sent > 0 {
                            progress.event(&format!("Calendar on the server: {} changes made (answers, changes, cancellations, deletions).", thousands(b.events_sent)));
                        }
                        if b.events_failed > 0 {
                            progress.event(&format!("{} calendar changes not made on the server yet ({}); tried again.", thousands(b.events_failed), b.events_send_error.clone().unwrap_or_default()));
                        }
                        if let Some(e) = &b.events_error {
                            calendar_retry_soon(&signed_in);
                            if e.contains("403") || e.contains("Forbidden") || e.contains("Access") || e.contains("scope") {
                                if !told_calendar {
                                    told_calendar = true;
                                    progress.event(&format!("The calendar waits for your next Microsoft sign-in (Settings ▸ Accounts ▸ Sign in again): Microsoft asks you to let Eudora read and change your calendars. (Microsoft said: {e})"));
                                }
                            } else {
                                progress.event(&format!("Couldn't read the calendar yet ({e}); trying again soon."));
                            }
                        }
                        if let Some(e) = &b.cards_error {
                            contacts_retry_soon(&signed_in);
                            if e.contains("403") || e.contains("Forbidden") || e.contains("Access") || e.contains("scope") {
                                if !told_contacts {
                                    told_contacts = true;
                                    progress.event(&format!("The address book waits for your next Microsoft sign-in (Settings ▸ Accounts ▸ Sign in again): Microsoft asks you to let Eudora read and change your contacts. (Microsoft said: {e})"));
                                }
                            } else {
                                progress.event(&format!("Couldn't read the address book yet ({e}); trying again in an hour."));
                            }
                        }
                        if r.moved > 0 {
                            said.push(format!("{} moved", thousands(r.moved)));
                        }
                        if r.gone > 0 {
                            said.push(format!("{} no longer where they were on the server (left as they are)", thousands(r.gone)));
                        }
                        if r.kept_here > 0 {
                            said.push(format!("{} from the Online Archive filed in Eudora only", thousands(r.kept_here)));
                        }
                        if b.archive_moved > 0 {
                            said.push(format!("{} moved inside the Online Archive", thousands(b.archive_moved)));
                        }
                        if b.archive_failed > 0 {
                            said.push(format!("{} not moved inside the Online Archive yet ({})", thousands(b.archive_failed), b.archive_error.clone().unwrap_or_default()));
                        }
                        if b.folders_changed > 0 {
                            said.push(format!("{} mailbox{} made, renamed or deleted", thousands(b.folders_changed), if b.folders_changed == 1 { "" } else { "es" }));
                        }
                        if b.folders_failed > 0 {
                            said.push(format!("{} mailbox change{} not made yet ({})", b.folders_failed, if b.folders_failed == 1 { "" } else { "s" }, b.folders_error.clone().unwrap_or_default()));
                        }
                        if r.failed > 0 {
                            said.push(format!("{} not moved yet ({})", thousands(r.failed), r.error.unwrap_or_default()));
                        }
                        if !r.folders_made.is_empty() {
                            said.push(format!("folders made there: {}", r.folders_made.join(", ")));
                        }
                        if !said.is_empty() {
                            progress.event(&format!("Changes on the server: {}.", said.join("; ")));
                        }
                    }
                    Err(e) => progress.event(&format!("Couldn't record the moves made on the server ({e}); they're checked again.")),
                }
            }
            if let Ok(results) = imap_results_rx.try_recv() {
                imap_busy = false;
                match crate::server_changes::record_batch(library, &results) {
                    Ok(b) => {
                        if let Some(line) = imap_changes_said(b) {
                            progress.event(&line);
                        }
                    }
                    Err(e) => progress.event(&format!("Couldn't record the changes made on IMAP servers ({e}); they're checked again.")),
                }
            }
            let pause = read_pause(&library.root);
            if pause as u8 != shared.pause.load(Ordering::Relaxed) {
                shared.pause.store(pause as u8, Ordering::Relaxed);
                progress.event(match pause {
                    Pause::Old => "Paused old mail: new mail still comes in (Inbox and Sent, every 5 minutes).",
                    Pause::All => "Paused: nothing is downloaded until you resume.",
                    Pause::None => "Resumed: downloading continues where it was.",
                });
            }
            // Check Mail in the viewer: every side looks for new mail now; an account added
            // since this pass started means a new pass (it skips what's here)
            if check_requested(&library.root) {
                // (a Microsoft account signed in meanwhile too: Add a Microsoft account asks for
                // a check, and its mail shouldn't wait for this pass to end)
                if imap_addresses(library, options) != imap_at_start || GraphClient::signed_in_accounts() != microsoft_at_start {
                    progress.event("An account was added: starting again to bring it in.");
                    restart = true;
                    break;
                }
                shared.check.fetch_add(1, Ordering::SeqCst);
                progress.event("Checking for new mail now (Check Mail).");
            }
            if let Some(chunk) = chunk {
                let _timed = crate::speed::Timer::new("Storing downloaded mail (a batch)");
                let stored = match chunk {
                    Chunk::Store(items) => writer.store(library, &items),
                    Chunk::Copies(items) => writer.add_copies(library, &items).map(|_| ()),
                    Chunk::Locate(places) => locate(library, &places),
                    Chunk::Present { folder, ids, since } => forget_gone(library, folder, &ids, since).map(|_| ()),
                    Chunk::Setting { key, value } => library.set_setting(&key, &value),
                    Chunk::Listed { folder, total } => library
                        .conn
                        .execute("INSERT OR REPLACE INTO listed_folders (folder, total, at) VALUES (?1, ?2, ?3)", rusqlite::params![folder, total as i64, now()])
                        .map(|_| ())
                        .map_err(Into::into),
                };
                if let Err(e) = stored {
                    result = Err(e);
                    break;
                }
                if last_commit.elapsed() >= INDEX_COMMIT {
                    if let Err(e) = commit_index(library) {
                        result = Err(e);
                        break;
                    }
                    last_commit = Instant::now();
                }
            }
            // Microsoft's "wait" answers, said once a minute (they used to pass unseen)
            if last_throttle.elapsed() >= Duration::from_secs(60) {
                last_throttle = Instant::now();
                if let Some(line) = crate::graph::throttling_said() {
                    progress.event(&line);
                }
            }
            let lost = link.lost();
            match (&lost, outage) {
                (Some((for_, why)), None) => {
                    outage = Some(Instant::now() - *for_);
                    progress.event(&format!(
                        "Connection lost at {} ({why}). The download is waiting and will continue by itself when the network is back.",
                        clock(*for_)
                    ));
                }
                (None, Some(since)) => {
                    outage = None;
                    progress.event(&format!("Connection back at {}, after {}; continuing.", clock(Duration::ZERO), span(since.elapsed())));
                }
                _ => {}
            }
            let done = shared.done.load(Ordering::Relaxed);
            let c = writer.counts();
            let at = shared.at.lock().unwrap().iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · ");
            let paused = match shared.pause() {
                Pause::Old => "Old mail paused (new mail still comes in) · ",
                Pause::All => "Paused · ",
                Pause::None => "",
            };
            let line = match &lost {
                Some((for_, _)) => format!("Waiting for the network (no connection for {}); {} of {} done", span(*for_), thousands(done), thousands(total)),
                None => format!(
                    "Checked {} of {} items: {} new, {} already here as other copies · {at}",
                    thousands(done),
                    thousands(total),
                    thousands(c.imported),
                    thousands(c.merged + c.new_locations + c.duplicates)
                ),
            };
            let line = format!("{paused}{line}");
            progress.report(done, total, &line);
            if last_status.elapsed() >= STATUS_EVERY {
                let state = if lost.is_some() {
                    "waiting"
                } else if shared.pause() != Pause::None {
                    "paused"
                } else {
                    "running"
                };
                write_status(&library.root, &status_of(state, &account, done, total, c, &lost, &link, &line));
                last_status = Instant::now();
            }
        }
        // a pass that found nothing new can end before the first look at the changes waiting:
        // one batch of each goes now (recorded below), the rest next time
        if result.is_ok() && shared.pause() != Pause::All {
            if changes.is_some() && !changes_busy {
                if let Ok(batch) = crate::server_changes::plan_batch(library, &signed_in, crate::server_changes::BATCH, None, false) {
                    if !batch.is_empty() {
                        let _ = jobs_tx.send(batch);
                    }
                }
            }
            if imap_changes.is_some() && !imap_busy {
                if let Ok(batch) = crate::imap_changes::plan(library, crate::server_changes::BATCH) {
                    if !batch.is_empty() {
                        let _ = imap_jobs_tx.send(batch);
                    }
                }
            }
        }
        drop(jobs_tx);
        drop(imap_jobs_tx);
        // what changed on the server, if its round ended meanwhile; a round still going goes on
        // and is recorded by a later pass (never waited for: an hour-long read-back once held
        // every new-mail check back)
        if let Some(r) = &readback {
            while let Ok(results) = r.results.try_recv() {
                r.busy.store(false, Ordering::SeqCst);
                let _ = crate::server_changes::record_batch(library, &results);
            }
        }
        if let Some(changes) = changes {
            // a batch it finished meanwhile is recorded; one cut short is simply tried again
            while let Ok(results) = results_rx.recv() {
                let _ = crate::server_changes::record_batch(library, &results);
            }
            let _ = changes.join();
        }
        if let Some(imap_changes) = imap_changes {
            while let Ok(results) = imap_results_rx.recv() {
                match crate::server_changes::record_batch(library, &results) {
                    Ok(b) => {
                        if let Some(line) = imap_changes_said(b) {
                            progress.event(&line);
                        }
                    }
                    Err(e) => progress.event(&format!("Couldn't record the changes made on IMAP servers ({e}); they're checked again.")),
                }
            }
            let _ = imap_changes.join();
        }
        shared.stop.store(true, Ordering::SeqCst);
        link.stop(); // clients still waiting for the network give up
        drop(rx);
        for handle in handles {
            let produced = handle.join().unwrap_or_else(|_| Err(Error::Other("a download thread stopped unexpectedly".into())));
            if result.is_ok() {
                result = produced;
            }
        }
        result
    });

    if let Err(e) = record_failures(library, shared.failed.into_inner().unwrap(), progress) {
        progress.event(&format!("Couldn't note which items failed ({e}); they're tried again next pass."));
    }
    let mut report = shared.report.into_inner().unwrap();
    report.canceled = matches!(outcome, Err(Error::Canceled));
    report.restart = restart;
    progress.report(0, 0, "Making the new messages searchable…");
    commit_index(library)?;
    library.commit_header_index()?;
    report.counts = writer.counts().clone();
    report.elapsed_secs = started.elapsed().as_secs_f64();
    let status = match &outcome {
        Ok(()) => "done",
        Err(Error::Canceled) => "canceled",
        Err(_) => "failed",
    };
    library.conn.execute(
        "UPDATE imports SET status = ?2, finished = ?3, counts = ?4 WHERE id = ?1",
        params![import_id, status, now(), serde_json::to_string(&report.counts)?],
    )?;
    let file = library.root.join("imports").join(format!("download-{import_id}.json"));
    std::fs::write(&file, serde_json::to_vec_pretty(&report)?).context(|| format!("writing {}", file.display()))?;
    let (state, line) = match &outcome {
        Ok(()) if report.failed > 0 => ("done", format!("Finished; {} items couldn't be downloaded (run it again to retry them)", thousands(report.failed))),
        Ok(()) => ("done", "Finished: everything is here".to_string()),
        Err(Error::Canceled) => ("stopped", "Stopped; run it again to continue where it was".to_string()),
        Err(e) => ("failed", format!("Stopped by an error: {e}. Run it again to continue where it was")),
    };
    let done = report.already + report.downloaded + report.failed + report.not_mail;
    write_status(&library.root, &status_of(state, &account, done, total, &report.counts, &None, &link, &line));
    match outcome {
        Err(e) if !report.canceled => Err(e),
        _ => Ok(report),
    }
}

/// Where a download says how it's going, inside the library (the viewer shows it).
pub const STATUS_FILE: &str = "status/download.json";
const STATUS_EVERY: Duration = Duration::from_secs(2);

/// Whether it's time to read back what changed on the server (read in Outlook, labels given on
/// the phone): Some(true) = each mailbox's Inbox (every 5 minutes), Some(false) = whole mailboxes
/// (a minute after starting, then hourly). Kept across the passes of `--watch`.
fn reconcile_due() -> Option<bool> {
    static LAST: std::sync::Mutex<(Option<Instant>, Option<Instant>)> = std::sync::Mutex::new((None, None));
    static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let started = *STARTED.get_or_init(Instant::now);
    let mut last = LAST.lock().unwrap();
    let due = |at: Option<Instant>, every: Duration| at.is_none_or(|t| t.elapsed() >= every);
    if started.elapsed() >= Duration::from_secs(60) && due(last.1, Duration::from_secs(3600)) {
        *last = (Some(Instant::now()), Some(Instant::now()));
        return Some(false);
    }
    if due(last.0, Duration::from_secs(300)) {
        last.0 = Some(Instant::now());
        return Some(true);
    }
    None
}

/// Whether to read this Microsoft account's address books now: at the start, then hourly (kept
/// per account across the passes of `--watch`).
static CONTACTS_LAST: std::sync::Mutex<Option<HashMap<String, Instant>>> = std::sync::Mutex::new(None);

fn contacts_due(account: &str) -> bool {
    let mut last = CONTACTS_LAST.lock().unwrap();
    let map = last.get_or_insert_with(HashMap::new);
    let key = account.to_lowercase();
    if map.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
        return false;
    }
    map.insert(key, Instant::now());
    true
}

/// An address book Microsoft refused (no consent yet): asked again in 5 minutes, not an hour,
/// so a sign-in made meanwhile is picked up soon (2026-10-18: the user had signed in, the
/// download waited the rest of the hour).
fn contacts_retry_soon(account: &str) {
    let mut last = CONTACTS_LAST.lock().unwrap();
    if let Some(t) = Instant::now().checked_sub(Duration::from_secs(3600 - 300)) {
        last.get_or_insert_with(HashMap::new).insert(account.to_lowercase(), t);
    }
}

/// Whether to read Microsoft To Do now: at the start, then every 5 minutes.
static TODO_LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn todo_due() -> bool {
    let mut last = TODO_LAST.lock().unwrap();
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

/// Whether to read this Microsoft account's calendars now, and which time range: everything
/// (1990 to three years ahead) at the start and then daily, else the near weeks (the last month
/// and the next year) every 15 minutes (kept per account across the passes of `--watch`).
static CALENDAR_LAST: std::sync::Mutex<Option<HashMap<String, (Instant, Instant)>>> = std::sync::Mutex::new(None);

fn calendar_due(account: &str) -> Option<(i64, i64)> {
    use crate::calendar::{AHEAD, EVERYTHING_FROM, NEAR_AHEAD, NEAR_BACK};
    let mut last = CALENDAR_LAST.lock().unwrap();
    let map = last.get_or_insert_with(HashMap::new);
    let key = account.to_lowercase();
    let now = crate::library::now();
    match map.get(&key).copied() {
        Some((_, full)) if full.elapsed() >= Duration::from_secs(24 * 3600) => {
            map.insert(key, (Instant::now(), Instant::now()));
            Some((EVERYTHING_FROM, now + AHEAD))
        }
        Some((near, _)) if near.elapsed() < Duration::from_secs(15 * 60) => None,
        Some((_, full)) => {
            map.insert(key, (Instant::now(), full));
            Some((now - NEAR_BACK, now + NEAR_AHEAD))
        }
        None => {
            map.insert(key, (Instant::now(), Instant::now()));
            Some((EVERYTHING_FROM, now + AHEAD))
        }
    }
}

/// A calendar Microsoft refused (no consent yet) or couldn't send: everything asked for again
/// in 5 minutes.
fn calendar_retry_soon(account: &str) {
    // the whole range again once 5 minutes have passed
    if let Some(full) = Instant::now().checked_sub(Duration::from_secs(24 * 3600 - 300)) {
        CALENDAR_LAST.lock().unwrap().get_or_insert_with(HashMap::new).insert(account.to_lowercase(), (Instant::now(), full));
    }
}

/// The status file: what a running (or finished) download is doing.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct Status {
    /// running, waiting (no network), done, stopped, failed
    pub state: String,
    pub account: String,
    pub done: u64,
    pub total: u64,
    pub new: u64,
    pub already: u64,
    /// The line shown for it ("Downloading item 305,859 of 774,665: …").
    pub line: String,
    /// Seconds without a connection, while waiting for it.
    pub offline_secs: Option<u64>,
    pub offline_reason: Option<String>,
    /// Times the connection was lost and came back during this run.
    pub outages: u64,
    /// Unix seconds when written: a running download writes it every 2 s, so an old one
    /// means the download isn't running any more.
    pub updated: i64,
}

#[allow(clippy::too_many_arguments)]
fn status_of(state: &str, account: &str, done: u64, total: u64, c: &Counts, lost: &Option<(Duration, String)>, link: &Link, line: &str) -> Status {
    Status {
        state: state.into(),
        account: account.into(),
        done,
        total,
        new: c.imported,
        already: c.merged + c.new_locations + c.duplicates,
        line: line.into(),
        offline_secs: lost.as_ref().map(|(d, _)| d.as_secs()),
        offline_reason: lost.as_ref().map(|(_, why)| why.clone()),
        outages: link.outages.load(Ordering::Relaxed),
        updated: now(),
    }
}

/// Writes the status file (whole, then renamed into place: a reader never sees half of it). A
/// failure here never stops the download. The download's row in Activity's list of jobs.
const DOWNLOAD_JOB: &str = "Downloading mail (every account)";

fn write_status(root: &std::path::Path, status: &Status) {
    match status.state.as_str() {
        "running" | "waiting" | "paused" => crate::activity::step("download", DOWNLOAD_JOB, &status.line, status.done, status.total),
        _ => crate::activity::idle("download", DOWNLOAD_JOB, &status.line, None),
    }
    let path = root.join(STATUS_FILE);
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if serde_json::to_vec_pretty(status).ok().and_then(|b| std::fs::write(&tmp, b).ok()).is_some() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// A download between checks for new mail (`--watch`): up to date, says when it looks next.
/// Written often enough that the viewer knows it's still running.
pub fn write_watching(library: &Library, account: &str, line: &str, next: Option<i64>) {
    let mut status = read_status(&library.root).unwrap_or_default();
    status.state = "watching".into();
    status.account = account.into();
    status.line = line.into();
    status.offline_secs = None;
    status.offline_reason = None;
    status.updated = now();
    write_status(&library.root, &status);
    crate::activity::idle("download", DOWNLOAD_JOB, line, next);
}

/// The last download's status, if a download ever ran on this library.
pub fn read_status(root: &std::path::Path) -> Option<Status> {
    serde_json::from_slice(&std::fs::read(root.join(STATUS_FILE)).ok()?).ok()
}

/// Passes progress on, and keeps the status file current during the steps before the download
/// proper (reading folders, converting notes again, finding missing originals): the viewer
/// takes a status file older than a minute for a download that stopped.
struct Alive<'a> {
    inner: &'a dyn Progress,
    root: std::path::PathBuf,
    account: String,
    last: Mutex<Instant>,
}

impl Progress for Alive<'_> {
    fn report(&self, done: u64, total: u64, stage: &str) {
        self.inner.report(done, total, stage);
        let mut last = self.last.lock().unwrap();
        if last.elapsed() >= STATUS_EVERY {
            let line = if total > 0 { format!("{stage}: {} of {}", thousands(done), thousands(total)) } else { stage.to_string() };
            write_status(&self.root, &Status { state: "running".into(), account: self.account.clone(), line, updated: now(), ..Default::default() });
            *last = Instant::now();
        }
    }

    fn canceled(&self) -> bool {
        self.inner.canceled()
    }

    fn event(&self, text: &str) {
        self.inner.event(text);
    }
}

/// The local time `ago` ago, like "10:58 AM".
fn clock(ago: Duration) -> String {
    let at = chrono::Local::now() - chrono::Duration::from_std(ago).unwrap_or_default();
    at.format("%-I:%M %p").to_string()
}

/// "42 s", "3 min 12 s", "2 h 5 min".
fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

/// Microsoft 365's clients for one pass: each side's lister and workers, and the one that
/// makes the changes waiting for the server.
struct GraphSides {
    primary: GraphClient,
    archive: GraphClient,
    shared: GraphClient,
    primary_workers: Vec<GraphClient>,
    archive_workers: Vec<GraphClient>,
    shared_workers: Vec<GraphClient>,
    changes: GraphClient,
}

/// What the two fetching threads and the writer share.
struct Shared {
    report: Mutex<SyncReport>,
    /// Items done (stored, already here, or failed), for progress.
    done: AtomicU64,
    /// The writer stopped (error, canceled): the fetching threads stop too.
    stop: AtomicBool,
    /// Where each side is ("main mailbox folder 20 of 258: Inbox").
    at: Mutex<Vec<String>>,
    /// Sides still downloading (sides that are done keep checking for new mail meanwhile).
    running: AtomicUsize,
    /// Counts Check Mail requests: a side waiting to look for new mail looks at once.
    check: AtomicU64,
    /// The viewer's Pause (`Pause` as a number), read by the writer once a second.
    pause: AtomicU8,
    /// New mail already fetched by a new-mail check (any side), so it isn't fetched twice.
    seen: Mutex<HashSet<String>>,
    /// Folders listed in full with nothing missing: the server's count then (`listed_folders`).
    listed: HashMap<i64, u64>,
    /// Items that failed before and wait for their next try (or were given up): not asked for.
    held: HashSet<String>,
    /// This pass's failures: (folder, its path, server id, why), recorded at the end.
    failed: Mutex<Vec<(i64, String, String, String)>>,
    /// Only mail received since then (unix seconds): the download period, when one is chosen.
    cutoff: Option<i64>,
    /// The space limit (the library's folder, bytes), when the limit is by space.
    space: Option<(std::path::PathBuf, u64)>,
    /// Whether the library had reached it, and when that was measured.
    full: Mutex<Option<(Instant, bool)>>,
    /// Gmail accounts listed in full within the last day: the history id their check for new
    /// mail starts from, and when they were listed in full.
    gmail_from: HashMap<String, (String, i64)>,
}

/// Each Gmail account's last full listing: {"at": unix seconds, "from": the history id taken
/// just before it, or after the last check for new mail}.
fn gmail_listed_key(address: &str) -> String {
    format!("gmail-listed:{}", address.to_lowercase())
}

/// Gmail accounts are listed in full (every label's message ids) once a day per computer, to
/// catch anything History missed; every other check asks History for what's new.
const GMAIL_FULL_EVERY: i64 = 86_400;

/// Items of Microsoft 365 mailboxes that couldn't be downloaded, by server id (setting, JSON):
/// tried again once a day for a week, then given up, so no message stays in the queue for
/// ever. Given-up items stay listed (`eudora-cli graph failed`), never dropped silently, and
/// can be asked for again (`--retry`).
pub const FAILED_SETTING: &str = "download-failed";
const RETRY_WAIT: i64 = 86400;
/// The first try and seven daily ones.
pub const GIVE_UP_AFTER: u32 = 8;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FailedItem {
    pub folder: i64,
    pub path: String,
    pub tries: u32,
    /// When it's tried again (i64::MAX: given up).
    pub next: i64,
    pub error: String,
    pub first: i64,
}

impl FailedItem {
    pub fn given_up(&self) -> bool {
        self.next == i64::MAX
    }
}

/// The items that couldn't be downloaded, by server id.
pub fn failed_items(library: &Library) -> Result<HashMap<String, FailedItem>> {
    Ok(library.setting(FAILED_SETTING)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

/// Asks for every given-up (or waiting) item again at the next pass.
pub fn retry_failed(library: &Library) -> Result<usize> {
    let mut items = failed_items(library)?;
    for item in items.values_mut() {
        item.tries = 0;
        item.next = 0;
        item.first = now();
    }
    library.set_setting(FAILED_SETTING, &serde_json::to_string(&items)?)?;
    Ok(items.len())
}

/// Records a pass's failures (and forgets items that came in after all); says what happened.
fn record_failures(library: &Library, failures: Vec<(i64, String, String, String)>, progress: &dyn Progress) -> Result<()> {
    let mut items = failed_items(library)?;
    if items.is_empty() && failures.is_empty() {
        return Ok(());
    }
    let before = items.len();
    {
        let mut here = library.conn.prepare_cached("SELECT 1 FROM server_items WHERE folder = ?1 AND server_id = ?2")?;
        items.retain(|id, item| !here.exists(params![item.folder, id]).unwrap_or(false));
    }
    let came = before - items.len();
    let now = now();
    let (mut waiting, mut given_up) = (0u64, 0u64);
    for (folder, path, id, error) in failures {
        let item = items.entry(id).or_insert_with(|| FailedItem { folder, path: path.clone(), tries: 0, next: 0, error: String::new(), first: now });
        item.tries += 1;
        item.error = error;
        item.path = path;
        if item.tries >= GIVE_UP_AFTER {
            item.next = i64::MAX;
            given_up += 1;
        } else {
            item.next = now + RETRY_WAIT;
            waiting += 1;
        }
    }
    library.set_setting(FAILED_SETTING, &serde_json::to_string(&items)?)?;
    if came > 0 {
        progress.event(&format!("{} items that had failed came in after all.", thousands(came as u64)));
    }
    if waiting > 0 {
        progress.event(&format!("{} items couldn't be downloaded; each is tried again once a day for a week.", thousands(waiting)));
    }
    if given_up > 0 {
        progress.event(&format!(
            "Gave up on {} items after a week of daily tries: listed by \"eudora-cli graph failed\", which can ask for them again.",
            thousands(given_up)
        ));
    }
    Ok(())
}

impl Shared {
    /// Whether the library takes up the space chosen (measured at most every 5 seconds): then
    /// older mail stops coming down (new mail still does).
    fn full(&self) -> bool {
        let Some((root, limit)) = &self.space else { return false };
        let mut full = self.full.lock().unwrap();
        if let Some((at, answer)) = *full {
            if at.elapsed() < Duration::from_secs(5) {
                return answer;
            }
        }
        let answer = library_bytes(root) >= *limit;
        *full = Some((Instant::now(), answer));
        answer
    }

    fn pause(&self) -> Pause {
        match self.pause.load(Ordering::Relaxed) {
            1 => Pause::Old,
            2 => Pause::All,
            _ => Pause::None,
        }
    }
}

/// The viewer's Pause button: old mail (the long download of history) waits while new mail
/// still comes in, or everything waits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pause {
    #[default]
    None = 0,
    /// Old mail waits; every account's Inbox and Sent are still checked for new mail.
    Old = 1,
    /// Nothing is downloaded.
    All = 2,
}

/// Where the viewer leaves the pause for the running download.
pub const PAUSE_FILE: &str = "status/pause.json";

pub fn set_pause(root: &std::path::Path, pause: Pause) -> Result<()> {
    let file = root.join(PAUSE_FILE);
    if pause == Pause::None {
        let _ = std::fs::remove_file(&file);
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&file, serde_json::to_vec(&serde_json::json!({ "pause": pause, "since": now() }))?).context(|| format!("writing {}", file.display()))
}

pub fn read_pause(root: &std::path::Path) -> Pause {
    std::fs::read(root.join(PAUSE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| serde_json::from_value(v["pause"].clone()).ok())
        .unwrap_or_default()
}

/// File ▸ Work Offline (classic Eudora's): nothing is downloaded, no change goes to a server and
/// nothing is sent (Send Immediately waits in the Out mailbox), until the user goes online
/// again. Kept as a file (`status/offline`, holding the pause there was before), so it holds
/// across restarts and the running download sees it.
pub const OFFLINE_FILE: &str = "status/offline";

pub fn is_offline(root: &std::path::Path) -> bool {
    root.join(OFFLINE_FILE).exists()
}

pub fn set_offline(root: &std::path::Path, offline: bool) -> Result<()> {
    let file = root.join(OFFLINE_FILE);
    if offline {
        if is_offline(root) {
            return Ok(());
        }
        let before = read_pause(root);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&file, serde_json::to_vec(&serde_json::json!({ "pause_before": before, "since": now() }))?).context(|| format!("writing {}", file.display()))?;
        set_pause(root, Pause::All)
    } else {
        let before: Pause = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| serde_json::from_value(v["pause_before"].clone()).ok())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&file);
        set_pause(root, before)
    }
}

/// Where the viewer's Check Mail leaves its request for the running download.
pub const CHECK_FILE: &str = "status/check-now";

/// Asks the running download to look for new mail now (and bring in accounts added since it started).
pub fn request_check(root: &std::path::Path) -> Result<()> {
    let file = root.join(CHECK_FILE);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&file, now().to_string()).context(|| format!("writing {}", file.display()))
}

/// The shared mailboxes downloaded in the background (addresses); the rest are inactive: looked at
/// only when opened and Check Mail is clicked there. None: no choice made (all of them).
pub const SHARED_ACTIVE: &str = "shared-active";

pub fn shared_active(library: &Library) -> Option<Vec<String>> {
    library.setting(SHARED_ACTIVE).ok().flatten().and_then(|j| serde_json::from_str(&j).ok())
}

/// Where the viewer leaves the folders just opened, for the running download to bring up to date.
pub const LOOK_FILE: &str = "status/look-now.json";

/// Asks the running download to fetch these folders' newest messages now (a folder just opened).
pub fn request_look(root: &std::path::Path, folders: &[i64]) -> Result<()> {
    let file = root.join(LOOK_FILE);
    let mut all: Vec<i64> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for f in folders {
        if !all.contains(f) {
            all.push(*f);
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&file, serde_json::to_vec(&all)?).context(|| format!("writing {}", file.display()))
}

/// The folders asked for since the last look (the request is used up).
pub fn take_looks(root: &std::path::Path) -> Vec<i64> {
    let file = root.join(LOOK_FILE);
    let all: Vec<i64> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let _ = std::fs::remove_file(&file);
    all
}

/// Whether Check Mail was asked for since the last look (the request is used up).
pub fn check_requested(root: &std::path::Path) -> bool {
    std::fs::remove_file(root.join(CHECK_FILE)).is_ok()
}

fn imap_addresses(library: &Library, options: &SyncOptions) -> Vec<String> {
    if !options.imap || options.originals {
        return Vec::new();
    }
    let mut all: Vec<String> = library.imap_accounts().unwrap_or_default().into_iter().map(|c| c.address.to_lowercase()).collect();
    all.sort();
    all
}

/// One side (main mailbox or archive): folder by folder, list, skip what's here, fetch the
/// rest a chunk at a time and hand each chunk, parsed and compressed, to the writer.
#[allow(clippy::too_many_arguments)]
/// Folders whose newest messages were found here, with their count then and when (in memory:
/// a restart looks at every one again once).
static NEWEST_SEEN: Mutex<Option<HashMap<i64, (u64, Instant)>>> = Mutex::new(None);
/// How long an unchanged folder is taken as it is before its newest are looked at again.
const NEWEST_AGAIN: Duration = Duration::from_secs(2 * 3600);

fn produce(
    which: usize,
    label: &str,
    jobs: &[Job],
    client: &mut GraphClient,
    workers: &mut [GraphClient],
    known: &HashMap<i64, HashSet<String>>,
    missing: Option<&HashMap<i64, Vec<(String, i64)>>>,
    since: &str,
    compression: &Compression,
    shared: &Shared,
    out: &Sender<Chunk>,
) -> Result<()> {
    let nothing = HashSet::new();
    // (Pause: before each folder and each chunk; meanwhile new mail still comes in)
    let mut hold = |workers: &mut [GraphClient]| hold_while_paused(which, label, jobs, workers, known, since, compression, shared, out);
    for (n, job) in jobs.iter().enumerate() {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if job.total == 0 {
            continue;
        }
        hold(workers)?;
        // a shared mailbox's folder: every client of this side opens that mailbox
        if client.mailbox != job.mailbox {
            client.set_mailbox(job.mailbox.clone());
            for w in workers.iter_mut() {
                w.set_mailbox(job.mailbox.clone());
            }
        }
        let where_ = format!("{label} folder {} of {}: {}", n + 1, jobs.len(), job.path);
        if let Some(missing) = missing {
            // fetching originals: only the items here without their own bytes
            let Some(items) = missing.get(&job.folder) else { continue };
            let items: Vec<Wanted> = items.iter().map(|(id, message)| Wanted { id: id.clone(), date: None, message: Some(*message), unread: false }).collect();
            shared.at.lock().unwrap()[which] = format!("{where_} (originals)");
            fetch_chunks(job, &items, workers, compression, shared, out, &mut hold, true)?;
            continue;
        }
        // Moves and deletes made elsewhere come back: when the folder's count changed since its
        // last look, its recent mail (the download period, else the last 30 days) is listed,
        // and what left it here leaves too.
        if let Place::Primary { id } = &job.place {
            let mut looked = READ_BACK.lock().unwrap_or_else(|e| e.into_inner());
            let seen = looked.get_or_insert_with(HashMap::new);
            if seen.get(&job.folder) != Some(&job.total) {
                let since = shared.cutoff.unwrap_or_else(|| now() - READ_BACK_DAYS * 86_400);
                if let Ok(ids) = client.message_ids_since(id, Some(since), &|_| {}) {
                    let _ = out.send(Chunk::Present { folder: job.folder, ids: ids.into_iter().map(|(i, _)| i).collect(), since: Some(since) });
                    seen.insert(job.folder, job.total);
                }
            }
        }
        let known = known.get(&job.folder).unwrap_or(&nothing);
        // As many known as the server holds isn't proof enough: messages moved away (by Eudora,
        // Outlook or a server rule) can leave their old record behind, and then new mail was
        // never listed (2026-10-17: the user's Inbox stopped getting mail). The newest few must
        // be known too (one small request), else the folder is listed.
        // (or listed in full at this very count before: what's left over are items Eudora
        // leaves on the server, calendar items and the like)
        // A folder whose count hasn't changed since its newest were last found here is taken as
        // it is for 2 hours (the Inbox and Sent Items are looked at every time): asking every one
        // of a big mailbox's 930 folders every few minutes, on every computer, made Microsoft
        // throttle the account for an hour at a time
        let busy_folder = matches!(job.path.as_str(), "Inbox" | "Sent Items");
        let recently = !busy_folder
            && NEWEST_SEEN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&job.folder)).is_some_and(|(total, at)| *total == job.total && at.elapsed() < NEWEST_AGAIN);
        let complete = (known.len() as u64 >= job.total || shared.listed.get(&job.folder) == Some(&job.total))
            && (recently
                || match &job.place {
                    Place::Primary { id } => workers[0].newest_ids(id, 25).map(|newest| newest.iter().all(|(i, _)| known.contains(i))).unwrap_or(false),
                    _ => true,
                });
        if complete && !recently && matches!(job.place, Place::Primary { .. }) {
            NEWEST_SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(job.folder, (job.total, Instant::now()));
        }
        if complete {
            shared.report.lock().unwrap().already += job.total;
            shared.done.fetch_add(job.total, Ordering::Relaxed);
            continue;
        }
        let listing = |count: usize| {
            shared.at.lock().unwrap()[which] = format!("{where_} (listing {} of {})", thousands(count as u64), thousands(job.total));
        };
        listing(0);
        // (network trouble is waited out inside; an error the server gives for one folder skips
        // just that folder: it's tried again on the next run)
        let listed: Result<Vec<Wanted>> = match &job.place {
            Place::Imap { .. } | Place::Gmail { .. } => continue, // (`produce_imap`, `produce_gmail`)
            Place::Primary { id } => client.listed_since(id, shared.cutoff, &listing).map(|ids| ids.into_iter().map(|(id, date, unread)| Wanted { id, date, message: None, unread }).collect()),
            Place::Archive { owner, id } => client.all_folder_items(owner, id, &listing).map(|all| {
                let before = all.len();
                let mail: Vec<Wanted> = all
                    .into_iter()
                    // (every kind of item: an appointment or task comes in as a readable entry with
                    // Exchange's export kept)
                    .map(|i| Wanted { id: i.id, date: i.created_date_time, message: None, unread: false })
                    // (a download period: only the archive's items within it)
                    .filter(|w| match (shared.cutoff, w.date.as_deref().and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())) {
                        (Some(cutoff), Some(date)) => date.timestamp() >= cutoff,
                        _ => true,
                    })
                    .collect();
                shared.report.lock().unwrap().not_mail += (before - mail.len()) as u64;
                mail
            }),
        };
        let mut items = match listed {
            Ok(items) => items,
            Err(Error::Canceled) => return Err(Error::Canceled),
            Err(e) => {
                let mut report = shared.report.lock().unwrap();
                report.failed += job.total;
                if report.failures.len() < FAILURES_LISTED {
                    report.failures.push(format!("{}: couldn't list the folder ({e}); it's tried again next time", job.path));
                }
                shared.done.fetch_add(job.total, Ordering::Relaxed);
                continue;
            }
        };
        let listed = items.len() as u64;
        // what the server counts but isn't mail (calendar items, tasks, meeting notices) is left
        // there: done too, so a complete pass reaches 100%
        shared.done.fetch_add(job.total.saturating_sub(listed), Ordering::Relaxed);
        items.retain(|i| !known.contains(&i.id));
        // (items that failed wait for their next try, given-up ones aren't asked for again)
        let held = items.len();
        items.retain(|i| !shared.held.contains(&i.id));
        let held = (held - items.len()) as u64;
        if held > 0 {
            shared.report.lock().unwrap().failed += held;
            shared.done.fetch_add(held, Ordering::Relaxed);
        }
        if items.is_empty() {
            // everything here: passed over next time while the server's count stays this one
            let _ = out.send(Chunk::Listed { folder: job.folder, total: job.total });
        }
        // newest first (ISO dates sort as text)
        items.sort_by(|a, b| b.date.cmp(&a.date));
        {
            let mut report = shared.report.lock().unwrap();
            report.listed += listed;
            report.already += listed - items.len() as u64;
        }
        shared.done.fetch_add(listed - items.len() as u64, Ordering::Relaxed);
        shared.at.lock().unwrap()[which] = where_.clone();
        fetch_chunks(job, &items, workers, compression, shared, out, &mut hold, true)?;
    }
    Ok(())
}

/// While old mail is paused: waits, looking at the Inbox and Sent folders' newest messages
/// every few minutes (or at once on Check Mail); while everything is paused: just waits.
#[allow(clippy::too_many_arguments)]
fn hold_while_paused(
    which: usize,
    label: &str,
    jobs: &[Job],
    workers: &mut [GraphClient],
    known: &HashMap<i64, HashSet<String>>,
    since: &str,
    compression: &Compression,
    shared: &Shared,
    out: &Sender<Chunk>,
) -> Result<()> {
    let mut next: Option<Instant> = None;
    let mut asked = shared.check.load(Ordering::SeqCst);
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match shared.pause() {
            Pause::None => return Ok(()),
            Pause::All => shared.at.lock().unwrap()[which] = format!("{label}: paused"),
            Pause::Old => {
                let check = shared.check.load(Ordering::SeqCst);
                if next.is_none_or(|at| Instant::now() >= at) || check != asked {
                    asked = check;
                    let new = new_mail_once(which, label, jobs, true, workers, known, since, compression, shared, out)?;
                    next = Some(Instant::now() + NEW_MAIL_CHECK);
                    shared.at.lock().unwrap()[which] = format!(
                        "{label}: old mail paused{} · new mail checked again at {}",
                        if new > 0 { format!(" ({new} new)") } else { String::new() },
                        (chrono::Local::now() + chrono::Duration::from_std(NEW_MAIL_CHECK).unwrap()).format("%-I:%M %p")
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Waits while everything is paused (new-mail checks themselves aren't held by an old-mail pause).
fn hold_if_all_paused(shared: &Shared) -> Result<()> {
    while shared.pause() == Pause::All && !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// One look at the newest messages of this side's folders (`recent_only`: just the Inbox and
/// Sent), fetching those that arrived since the download started. Returns how many.
#[allow(clippy::too_many_arguments)]
fn new_mail_once(
    which: usize,
    label: &str,
    jobs: &[Job],
    recent_only: bool,
    workers: &mut [GraphClient],
    known: &HashMap<i64, HashSet<String>>,
    since: &str,
    compression: &Compression,
    shared: &Shared,
    out: &Sender<Chunk>,
) -> Result<usize> {
    let nothing = HashSet::new();
    let mut new = 0;
    for job in jobs {
        let Place::Primary { id } = &job.place else { continue };
        if recent_only && folder_rank(&job.path) > 1 {
            continue;
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        for w in workers.iter_mut() {
            if w.mailbox != job.mailbox {
                w.set_mailbox(job.mailbox.clone());
            }
        }
        shared.at.lock().unwrap()[which] = format!("{label}: checking for new mail ({})", job.path);
        let Ok(newest) = workers[0].newest_listed(id, 50) else { continue };
        let known = known.get(&job.folder).unwrap_or(&nothing);
        let items: Vec<Wanted> = {
            let mut seen = shared.seen.lock().unwrap();
            newest
                .into_iter()
                .filter(|(i, date, _)| date.as_deref().is_some_and(|d| d >= since) && !known.contains(i) && !shared.held.contains(i) && seen.insert(i.clone()))
                .map(|(id, date, unread)| Wanted { id, date, message: None, unread })
                .collect()
        };
        if !items.is_empty() {
            new += items.len();
            fetch_chunks(job, &items, workers, compression, shared, out, &mut |_| hold_if_all_paused(shared), false)?;
        }
    }
    Ok(new)
}

/// How often a side that's done looks for new mail while the others still download.
const NEW_MAIL_CHECK: Duration = Duration::from_secs(300);

/// New mail first: while the other sides still download (the archive, big shared mailboxes),
/// a side that's done looks at each of its folders' newest messages every few minutes and
/// fetches what arrived since the download started.
#[allow(clippy::too_many_arguments)]
fn watch_new_mail(
    which: usize,
    label: &str,
    jobs: &[Job],
    workers: &mut [GraphClient],
    known: &HashMap<i64, HashSet<String>>,
    since: &str,
    compression: &Compression,
    shared: &Shared,
    out: &Sender<Chunk>,
) -> Result<()> {
    loop {
        // wait, unless everything else is done (or stopping), or Check Mail was asked for
        let until = Instant::now() + NEW_MAIL_CHECK;
        let asked = shared.check.load(Ordering::SeqCst);
        while (Instant::now() < until && shared.check.load(Ordering::SeqCst) == asked) || shared.pause() == Pause::All {
            if shared.stop.load(Ordering::Relaxed) || shared.running.load(Ordering::SeqCst) == 0 {
                return Ok(());
            }
            if shared.pause() == Pause::All {
                shared.at.lock().unwrap()[which] = format!("{label}: paused");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        // (an old-mail pause: just the Inbox and Sent)
        let new = new_mail_once(which, label, jobs, shared.pause() == Pause::Old, workers, known, since, compression, shared, out)?;
        shared.at.lock().unwrap()[which] = format!(
            "{label}: up to date{} · next check at {}",
            if new > 0 { format!(" ({new} new)") } else { String::new() },
            (chrono::Local::now() + chrono::Duration::from_std(NEW_MAIL_CHECK).unwrap()).format("%-I:%M %p")
        );
    }
}

/// Files the sent copies the viewer left in `imap_sync::SENT_SPOOL` ("<account>-….eml") in
/// each account's Sent folder, then removes them.
fn file_sent_copies(library: &mut Library, writer: &mut WriterState, compression: &Compression) -> Result<()> {
    let dir = library.root.join(crate::imap_sync::SENT_SPOOL);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(()) };
    let mut files: Vec<std::path::PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "eml")).collect();
    files.sort();
    for file in files {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let Some(account) = name.split('-').next().and_then(|a| a.parse::<i64>().ok()) else { continue };
        let raw = std::fs::read(&file).context(|| format!("reading {}", file.display()))?;
        let folder = db::ensure_folder(&library.conn, account, "Sent")?;
        let item = prepare(raw, DateHints { envelope: None, file: None }, format!("sent from Eudora ({name})"), "Sent", folder, None, compression, false)?;
        writer.store(library, &[item])?;
        std::fs::remove_file(&file).context(|| format!("removing {}", file.display()))?;
    }
    Ok(())
}

/// Each Microsoft 365 folder's count when its moves were last read back (this run).
static READ_BACK: Mutex<Option<HashMap<i64, u64>>> = Mutex::new(None);
/// Without a download period, moves are read back over the last this many days.
const READ_BACK_DAYS: i64 = 30;

/// The writer's side of `Chunk::Present`: messages this library had in `folder` (dated since
/// `since`, a day's grace inside the period) that the server's folder no longer holds leave it,
/// and the library when that was their last place (they come back in the folder they went to,
/// listed in the same pass). Messages with a change of their own still waiting are left alone.
/// Returns how many left the folder.
fn forget_gone(library: &mut Library, folder: i64, ids: &[String], since: Option<i64>) -> Result<u64> {
    let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let since = since.map(|t| t + 86_400);
    let had: Vec<(String, i64)> = {
        let mut stmt = library.conn.prepare(
            "SELECT s.server_id, s.message FROM server_items s JOIN messages m ON m.id = s.message WHERE s.folder = ?1 AND (?2 IS NULL OR m.date >= ?2)",
        )?;
        stmt.query_map(params![folder, since], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let all = had.len();
    let gone: Vec<(String, i64)> = had.into_iter().filter(|(id, _)| !present.contains(id.as_str())).collect();
    if gone.is_empty() {
        return Ok(0);
    }
    // every one gone at once while the folder isn't empty: the server numbered its messages
    // afresh (an IMAP UIDVALIDITY change), not moves; the download brings them in again
    if gone.len() == all && all > 20 && !ids.is_empty() {
        return Ok(0);
    }
    let waiting = library.messages_with_changes_waiting()?;
    let mut left = 0;
    library.in_transaction(|| {
        for (server_id, message) in &gone {
            if waiting.contains(message) {
                continue;
            }
            library.conn.execute("DELETE FROM server_items WHERE folder = ?1 AND server_id = ?2", params![folder, server_id])?;
            let still: bool = library.conn.query_row("SELECT EXISTS (SELECT 1 FROM server_items WHERE folder = ?1 AND message = ?2)", params![folder, message], |r| r.get(0))?;
            if still {
                continue; // (the server held two copies: one is still there)
            }
            library.conn.execute("DELETE FROM locations WHERE folder = ?1 AND message = ?2", params![folder, message])?;
            left += 1;
            let elsewhere: bool = library.conn.query_row("SELECT EXISTS (SELECT 1 FROM locations WHERE message = ?1)", params![message], |r| r.get(0))?;
            if elsewhere {
                library.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
            } else {
                library.purge_message(*message)?;
            }
        }
        Ok(())
    })?;
    Ok(left)
}

/// The writer's side of `Chunk::Locate`: a message already here (found by its server id among
/// its account's folders) also in another folder (a Gmail label), and its unread mark.
fn locate(library: &mut Library, places: &[(i64, String, bool)]) -> Result<()> {
    let tx = library.conn.transaction()?;
    for (folder, server_id, unread) in places {
        let message: Option<i64> = tx
            .query_row(
                "SELECT s.message FROM server_items s JOIN folders f ON f.id = s.folder
                 WHERE s.server_id = ?1 AND f.account = (SELECT account FROM folders WHERE id = ?2) LIMIT 1",
                params![server_id, folder],
                |r| r.get(0),
            )
            .optional()?;
        let Some(message) = message else { continue };
        let added = tx.execute("INSERT OR IGNORE INTO locations (message, folder, date, server_id) SELECT id, ?2, date, ?3 FROM messages WHERE id = ?1", params![message, folder, server_id])?;
        tx.execute("INSERT OR IGNORE INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)", params![folder, server_id, message])?;
        if added > 0 {
            tx.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
        }
        if *unread {
            tx.execute("UPDATE messages SET flags = flags | 1 WHERE id = ?1", params![message])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Gmail messages fetched before they're handed to the writer.
const GMAIL_CHUNK: usize = 25;

/// The Gmail side (the Gmail API): label by label (Inbox and Sent first), the messages not here
/// yet, newest first; each message fetched once with the labels it has, its other labels recorded
/// as more places of the same message; then \"All Mail\": what's under no label (archived).
fn produce_gmail(jobs: &[Job], known: &HashMap<i64, HashSet<String>>, compression: &Compression, shared: &Shared, out: &Sender<Chunk>) -> Result<()> {
    let nothing = HashSet::new();
    // each account's labels → folders, and every message id already here in it
    let mut folder_of: HashMap<(String, String), i64> = HashMap::new();
    let mut here: HashMap<String, HashSet<String>> = HashMap::new();
    for job in jobs {
        let Place::Gmail { address, label } = &job.place else { continue };
        folder_of.insert((address.clone(), label.clone()), job.folder);
        here.entry(address.clone()).or_default().extend(known.get(&job.folder).into_iter().flatten().cloned());
    }
    let mut clients: HashMap<String, crate::google::GoogleClient> = HashMap::new();
    let path_of: HashMap<i64, &str> = jobs.iter().map(|j| (j.folder, j.path.as_str())).collect();
    let mut addresses: Vec<String> = Vec::new();
    for job in jobs {
        if let Place::Gmail { address, .. } = &job.place {
            if !addresses.contains(address) {
                addresses.push(address.clone());
            }
        }
    }
    // Accounts listed in full within the last day: only what History says is new (one request
    // when nothing is). Gmail no longer keeping history that far back: listed in full below.
    let mut done_by_history: HashSet<String> = HashSet::new();
    for address in &addresses {
        let Some((from, at)) = shared.gmail_from.get(address) else { continue };
        if shared.stop.load(Ordering::Relaxed) || shared.pause() != Pause::None {
            return Ok(());
        }
        shared.at.lock().unwrap()[4] = format!("Gmail: {address}: checking for new mail");
        let client = match gmail_client(&mut clients, address) {
            Ok(c) => c,
            Err(e) => {
                shared.report.lock().unwrap().failures.push(format!("{address}: {e}"));
                done_by_history.insert(address.clone());
                continue;
            }
        };
        let (latest, added) = match client.history_added(from) {
            Ok(found) => found,
            Err(e) if e.to_string() == "too old" => continue,
            Err(e) => {
                shared.report.lock().unwrap().failures.push(format!("{address}: couldn't ask Gmail what's new ({e}); asked again next time"));
                done_by_history.insert(address.clone());
                continue;
            }
        };
        done_by_history.insert(address.clone());
        let account_here = here.entry(address.clone()).or_default();
        let new: Vec<String> = added.into_iter().filter(|id| !account_here.contains(id)).collect();
        shared.report.lock().unwrap().listed += new.len() as u64;
        let mut places: Vec<(i64, String, bool)> = Vec::new();
        for (n, chunk) in new.chunks(GMAIL_CHUNK).enumerate() {
            if shared.stop.load(Ordering::Relaxed) || shared.pause() != Pause::None || shared.full() {
                return Ok(());
            }
            shared.at.lock().unwrap()[4] = format!("Gmail: {address}: new message {} of {}", thousands((n * GMAIL_CHUNK + 1) as u64), thousands(new.len() as u64));
            let mut prepared = Vec::new();
            for id in chunk {
                match client.raw_message(id) {
                    Ok((raw, labels)) => {
                        let mapped: Vec<i64> = labels.iter().filter_map(|l| folder_of.get(&(address.clone(), l.clone())).copied()).collect();
                        // its folder: the Inbox first, else its first label that's a folder, else
                        // All Mail; a draft, or one under a label made since (sync's own, say),
                        // waits for the day's full listing
                        let unknown_label = labels.iter().any(|l| l.starts_with("Label_") && !folder_of.contains_key(&(address.clone(), l.clone())));
                        let home = folder_of
                            .get(&(address.clone(), "INBOX".to_string()))
                            .copied()
                            .filter(|_| labels.iter().any(|l| l == "INBOX"))
                            .or_else(|| mapped.first().copied())
                            .or_else(|| (!unknown_label && !labels.iter().any(|l| l == "DRAFT")).then(|| folder_of.get(&(address.clone(), "ALL".to_string())).copied()).flatten());
                        let Some(home) = home else { continue };
                        let unread = labels.iter().any(|l| l == "UNREAD");
                        let path = path_of.get(&home).copied().unwrap_or("Inbox");
                        prepared.push(prepare(raw, DateHints::default(), format!("{path} (server item {})", short(id)), path, home, Some(id.clone()), compression, false)?);
                        for f in mapped {
                            if f != home || unread {
                                places.push((f, id.clone(), unread));
                            }
                        }
                        if unread {
                            places.push((home, id.clone(), true));
                        }
                        account_here.insert(id.clone());
                    }
                    // (gone again since: deleted, or a draft sent)
                    Err(e) if e.to_string().contains("404") => {}
                    Err(e) => {
                        let mut report = shared.report.lock().unwrap();
                        report.failed += 1;
                        if report.failures.len() < FAILURES_LISTED {
                            report.failures.push(format!("{address}: new message {}: {e}", short(id)));
                        }
                    }
                }
            }
            shared.report.lock().unwrap().downloaded += prepared.len() as u64;
            if out.send(Chunk::Store(prepared)).is_err() || (!places.is_empty() && out.send(Chunk::Locate(std::mem::take(&mut places))).is_err()) {
                return Ok(());
            }
        }
        let value = serde_json::json!({ "at": at, "from": latest }).to_string();
        if out.send(Chunk::Setting { key: gmail_listed_key(address), value }).is_err() {
            return Ok(());
        }
    }
    // the others: every label listed, from History's position just before (where the next
    // check for new mail starts)
    let mut cursor: HashMap<String, String> = HashMap::new();
    let mut not_whole: HashSet<String> = HashSet::new();
    let mut ordered: Vec<&Job> = jobs.iter().filter(|j| !matches!(&j.place, Place::Gmail { address, .. } if done_by_history.contains(address))).collect();
    ordered.sort_by_key(|j| match &j.place {
        Place::Gmail { label, .. } if label == "INBOX" => 0,
        Place::Gmail { label, .. } if label == "SENT" => 1,
        Place::Gmail { label, .. } if label == "ALL" => 3, // (last: what no label holds)
        _ => 2,
    });
    for (n, job) in ordered.iter().enumerate() {
        let Place::Gmail { address, label } = &job.place else { continue };
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if shared.pause() != Pause::None {
            shared.at.lock().unwrap()[4] = "Gmail: paused".into();
            return Ok(());
        }
        let where_ = format!("Gmail folder {} of {}: {}", n + 1, ordered.len(), job.path);
        let client = match gmail_client(&mut clients, address) {
            Ok(c) => c,
            Err(e) => {
                shared.report.lock().unwrap().failures.push(format!("{address}: {e}"));
                not_whole.insert(address.clone());
                continue;
            }
        };
        if !cursor.contains_key(address) {
            match client.history_now() {
                Ok(h) => {
                    cursor.insert(address.clone(), h);
                }
                Err(_) => {
                    not_whole.insert(address.clone());
                }
            }
        }
        shared.at.lock().unwrap()[4] = format!("{where_} (listing)");
        let listing = |count: usize| shared.at.lock().unwrap()[4] = format!("{where_} (listing {})", thousands(count as u64));
        let ids = match client.message_ids_since(if label == "ALL" { "" } else { label }, shared.cutoff, &listing) {
            // (All Mail holds sync's own items too, under its hidden label: left out)
            Ok(ids) if label == "ALL" => {
                let sync: HashSet<String> = match client.labels() {
                    Ok(labels) => match labels.into_iter().find(|(_, n, _)| n.eq_ignore_ascii_case(crate::graph::SYNC_FOLDER)) {
                        Some((sync_label, ..)) => client.message_ids(&sync_label, &|_| {}).unwrap_or_default().into_iter().collect(),
                        None => HashSet::new(),
                    },
                    Err(_) => {
                        not_whole.insert(address.clone());
                        continue; // (not knowing which are sync's, All Mail waits for next time)
                    }
                };
                ids.into_iter().filter(|id| !sync.contains(id)).collect()
            }
            Ok(ids) => ids,
            Err(e) => {
                let mut report = shared.report.lock().unwrap();
                if report.failures.len() < FAILURES_LISTED {
                    report.failures.push(format!("{}: couldn't list the label ({e}); it's tried again next time", job.path));
                }
                not_whole.insert(address.clone());
                continue;
            }
        };
        let in_folder = known.get(&job.folder).unwrap_or(&nothing);
        let account_here = here.entry(address.clone()).or_default();
        let mut wanted: Vec<String> = Vec::new();
        let mut places: Vec<(i64, String, bool)> = Vec::new();
        for id in &ids {
            if in_folder.contains(id) {
                continue;
            }
            if account_here.contains(id) {
                // here already under another label: only this place added (All Mail: nothing)
                if label != "ALL" {
                    places.push((job.folder, id.clone(), false));
                }
            } else {
                wanted.push(id.clone());
            }
        }
        {
            let mut report = shared.report.lock().unwrap();
            report.listed += ids.len() as u64;
            report.already += (ids.len() - wanted.len()) as u64;
        }
        shared.done.fetch_add((ids.len() - wanted.len()) as u64, Ordering::Relaxed);
        if !places.is_empty() && out.send(Chunk::Locate(std::mem::take(&mut places))).is_err() {
            return Ok(());
        }
        shared.at.lock().unwrap()[4] = where_.clone();
        for chunk in wanted.chunks(GMAIL_CHUNK) {
            if shared.stop.load(Ordering::Relaxed) || shared.pause() != Pause::None || shared.full() {
                return Ok(());
            }
            let mut prepared = Vec::new();
            for id in chunk {
                match client.raw_message(id) {
                    Ok((raw, labels)) => {
                        let unread = labels.iter().any(|l| l == "UNREAD");
                        let item = prepare(raw, DateHints::default(), format!("{} (server item {})", job.path, short(id)), &job.path, job.folder, Some(id.clone()), compression, false)?;
                        prepared.push(item);
                        // its other labels: the same message there too; and its unread mark
                        for l in &labels {
                            if let Some(&f) = folder_of.get(&(address.clone(), l.clone())) {
                                if f != job.folder || unread {
                                    places.push((f, id.clone(), unread));
                                }
                            }
                        }
                        if unread {
                            places.push((job.folder, id.clone(), true));
                        }
                        account_here.insert(id.clone());
                    }
                    Err(e) => {
                        let mut report = shared.report.lock().unwrap();
                        report.failed += 1;
                        if report.failures.len() < FAILURES_LISTED {
                            report.failures.push(format!("{}: message {}: {e}", job.path, short(id)));
                        }
                    }
                }
            }
            {
                let mut report = shared.report.lock().unwrap();
                report.downloaded += prepared.len() as u64;
            }
            shared.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            if out.send(Chunk::Store(prepared)).is_err() || (!places.is_empty() && out.send(Chunk::Locate(std::mem::take(&mut places))).is_err()) {
                return Ok(());
            }
        }
    }
    // every label of an account listed: its checks ask History for what's new for a day
    for (address, from) in cursor {
        if !not_whole.contains(&address) {
            let value = serde_json::json!({ "at": now(), "from": from }).to_string();
            if out.send(Chunk::Setting { key: gmail_listed_key(&address), value }).is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The account's Gmail client, signed in once per pass.
fn gmail_client<'a>(clients: &'a mut HashMap<String, crate::google::GoogleClient>, address: &str) -> Result<&'a mut crate::google::GoogleClient> {
    if !clients.contains_key(address) {
        clients.insert(address.to_string(), crate::google::GoogleClient::resume(address)?);
    }
    Ok(clients.get_mut(address).expect("just put there"))
}

/// IMAP messages fetched at a time (whole messages: kept smaller than Graph's chunks).
const IMAP_CHUNK: usize = 50;

/// What a batch of IMAP changes did, for the download's log (None: nothing to say).
fn imap_changes_said(b: crate::server_changes::BatchRecorded) -> Option<String> {
    let mut said = Vec::new();
    for (n, what) in [(b.moves.moved, "moved"), (b.marked, "marked (read, flags, labels)"), (b.deleted, "deleted (Empty Trash)")] {
        if n > 0 {
            said.push(format!("{} {what}", thousands(n)));
        }
    }
    if b.moves.gone > 0 {
        said.push(format!("{} no longer where they were on the server (left as they are)", thousands(b.moves.gone)));
    }
    if b.moves.kept_here > 0 {
        said.push(format!("{} from the Online Archive filed in Eudora only (the archive isn't changed)", thousands(b.moves.kept_here)));
    }
    if b.gmail_changed > 0 {
        said.push(format!("{} brought in from Gmail (read, stars, labels, deleted there)", thousands(b.gmail_changed)));
    }
    if b.folders_changed > 0 {
        said.push(format!("{} folders made, renamed or deleted there", thousands(b.folders_changed)));
    }
    let failed = b.failed + b.moves.failed;
    if failed > 0 {
        said.push(format!("{} not changed yet ({})", thousands(failed), b.error.or(b.moves.error).unwrap_or_default()));
    }
    if !b.moves.folders_made.is_empty() {
        said.push(format!("folders made there: {}", b.moves.folders_made.join(", ")));
    }
    (!said.is_empty()).then(|| format!("Changes on IMAP servers: {}.", said.join("; ")))
}

/// The IMAP side: account by account, folder by folder (Inbox and Sent first), the messages not
/// here yet, newest first; then, while the other sides still download, new mail every few
/// minutes. Network trouble: signs in again and carries on where it was (waiting as long as it
/// takes); a folder the server refuses is skipped until the next run.
fn produce_imap(
    jobs: &[Job],
    configs: &HashMap<String, crate::imap_sync::ImapConfig>,
    known: &HashMap<i64, HashSet<String>>,
    pop_old: &HashMap<i64, HashSet<String>>,
    compression: &Compression,
    shared: &Shared,
    out: &Sender<Chunk>,
) -> Result<()> {
    use crate::imap_sync;
    let nothing = HashSet::new();
    let mut sessions: HashMap<String, imap_sync::Connection> = HashMap::new();
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    let stopping = || shared.stop.load(Ordering::Relaxed);
    // Runs `f` with the account's session, signing in again after network trouble.
    let mut with_session = |address: &str, what: &str, f: &mut dyn FnMut(&mut imap_sync::Connection) -> Result<()>| -> Result<()> {
        let key = address.to_lowercase();
        let Some(config) = configs.get(&key) else { return Ok(()) };
        let mut wait = Duration::from_secs(2);
        loop {
            if stopping() {
                return Ok(());
            }
            if !sessions.contains_key(&key) {
                match imap_sync::Connection::open(config, None) {
                    Ok(s) => {
                        sessions.insert(key.clone(), s);
                    }
                    Err(Error::Network(e)) => {
                        shared.at.lock().unwrap()[3] = format!("IMAP {address}: waiting for the network ({e})");
                        std::thread::sleep(wait);
                        wait = (wait * 2).min(Duration::from_secs(30));
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let session = sessions.get_mut(&key).expect("signed in above");
            match f(session) {
                Err(Error::Network(e)) => {
                    sessions.remove(&key);
                    shared.at.lock().unwrap()[3] = format!("IMAP {address}: connection lost while {what} ({e}); signing in again");
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(30));
                }
                other => return other,
            }
        }
    };
    // folders whose counts are in the report already (a paused folder is read again later)
    let mut counted: HashSet<i64> = HashSet::new();
    // returns (messages fetched, stopped by a pause)
    let mut fetch_folder = |n: usize, job: &Job, only_new: bool| -> Result<(usize, bool)> {
        let Place::Imap { address, name } = &job.place else { return Ok((0, false)) };
        let mut interrupted = false;
        let where_ = format!("IMAP folder {} of {}: {}", n + 1, jobs.len(), job.path);
        let known = known.get(&job.folder).unwrap_or(&nothing);
        let mut fetched = 0;
        let mut step = |session: &mut imap_sync::Connection| -> Result<()> {
            shared.at.lock().unwrap()[3] = if only_new { format!("IMAP: checking for new mail ({})", job.path) } else { format!("{where_} (listing)") };
            let listed = session.list_since(name, shared.cutoff)?;
            // (moves and deletes made elsewhere: see `produce`; the listing is the whole folder or
            // the download period)
            if !configs.get(&address.to_lowercase()).is_some_and(|c| c.is_pop3()) {
                let _ = out.send(Chunk::Present { folder: job.folder, ids: listed.iter().map(|i| i.id.clone()).collect(), since: shared.cutoff });
            }
            let mut wanted: Vec<&imap_sync::Item> = listed.iter().filter(|i| !known.contains(&i.id) && !seen.contains(&(job.folder, i.id.clone()))).collect();
            wanted.reverse(); // newest first
            // POP3 messages over the account's size limit stay on the server, and say so
            let mut too_big = 0u64;
            if let Some(mb) = configs.get(&address.to_lowercase()).filter(|c| c.is_pop3()).and_then(|c| c.pop_skip_over_mb) {
                let limit = mb as u64 * 1_000_000;
                let (big, rest): (Vec<&imap_sync::Item>, Vec<&imap_sync::Item>) = wanted.into_iter().partition(|i| i.size.is_some_and(|s| s > limit));
                wanted = rest;
                too_big = big.len() as u64;
                if !big.is_empty() {
                    for i in &big {
                        seen.insert((job.folder, i.id.clone()));
                    }
                    let mut report = shared.report.lock().unwrap();
                    report.left_big += too_big;
                    if report.failures.len() < FAILURES_LISTED {
                        report.failures.push(format!("{}: {} messages over {mb} MB left on the server (the account's size limit)", job.path, big.len()));
                    }
                }
            }
            if !only_new && counted.insert(job.folder) {
                let listed = wanted.len() as u64 + too_big;
                let mut report = shared.report.lock().unwrap();
                report.listed += job.total.max(listed);
                report.already += job.total.saturating_sub(listed);
                shared.done.fetch_add(job.total.saturating_sub(listed) + too_big, Ordering::Relaxed);
            }
            for chunk in wanted.chunks(IMAP_CHUNK) {
                // (older mail: not once the library takes up the space chosen)
                if stopping() || (!only_new && shared.full()) {
                    return Ok(());
                }
                // paused: old mail stops here (read again on resume); new mail only when everything is
                let pause = shared.pause();
                if pause == Pause::All || (pause == Pause::Old && !only_new) {
                    interrupted = true;
                    return Ok(());
                }
                if !only_new {
                    shared.at.lock().unwrap()[3] = where_.clone();
                }
                let messages = session.fetch(chunk)?;
                let prepared: Vec<Prepared> = messages
                    .into_par_iter()
                    .map(|(id, raw, date)| {
                        let hints = DateHints { envelope: date, file: None };
                        prepare(raw, hints, format!("{} (server item {id})", job.path), &job.path, job.folder, Some(id), compression, false)
                    })
                    .collect::<Result<_>>()?;
                for p in &prepared {
                    if let Some(id) = &p.server_id {
                        seen.insert((job.folder, id.clone()));
                    }
                }
                let missing = chunk.len() - prepared.len();
                {
                    let mut report = shared.report.lock().unwrap();
                    report.downloaded += prepared.len() as u64;
                    if missing > 0 {
                        report.failed += missing as u64; // gone from the server meanwhile
                    }
                }
                fetched += prepared.len();
                if out.send(Chunk::Store(prepared)).is_err() {
                    return Ok(()); // the writer stopped
                }
                if !only_new {
                    shared.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
            }
            Ok(())
        };
        let result = with_session(address, &format!("reading {}", job.path), &mut step);
        match result {
            Ok(()) => Ok((fetched, interrupted)),
            Err(Error::Canceled) => Err(Error::Canceled),
            Err(e) => {
                let mut report = shared.report.lock().unwrap();
                if report.failures.len() < FAILURES_LISTED {
                    report.failures.push(format!("{}: {e}; it's tried again next time", job.path));
                }
                Ok((fetched, false))
            }
        }
    };
    let mut n = 0;
    let mut next_new: Option<Instant> = None;
    let mut asked = shared.check.load(Ordering::SeqCst);
    while n < jobs.len() {
        if stopping() {
            return Ok(());
        }
        match shared.pause() {
            Pause::None => {}
            Pause::All => {
                shared.at.lock().unwrap()[3] = "IMAP: paused".into();
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Pause::Old => {
                // old mail waits; the Inbox and Sent folders are still checked for new mail
                let check = shared.check.load(Ordering::SeqCst);
                if next_new.is_none_or(|at| Instant::now() >= at) || check != asked {
                    asked = check;
                    for (i, job) in jobs.iter().enumerate().filter(|(_, j)| folder_rank(&j.path) <= 1) {
                        fetch_folder(i, job, true)?;
                    }
                    next_new = Some(Instant::now() + NEW_MAIL_CHECK);
                    shared.at.lock().unwrap()[3] = format!(
                        "IMAP: old mail paused · new mail checked again at {}",
                        (chrono::Local::now() + chrono::Duration::from_std(NEW_MAIL_CHECK).unwrap()).format("%-I:%M %p")
                    );
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        let (_, interrupted) = fetch_folder(n, &jobs[n], false)?;
        if !interrupted {
            n += 1;
        }
    }
    // POP3 accounts that delete after some days: what Eudora has had long enough goes from the
    // server (a connection of its own: POP3 deletes at QUIT)
    for job in jobs {
        let (Place::Imap { address, name }, Some(old)) = (&job.place, pop_old.get(&job.folder)) else { continue };
        let Some(config) = configs.get(&address.to_lowercase()) else { continue };
        if stopping() || shared.pause() == Pause::All {
            break;
        }
        let deleted = imap_sync::Connection::open(config, None).and_then(|mut connection| {
            let listed = connection.list(name)?;
            let going: Vec<&imap_sync::Item> = listed.iter().filter(|i| old.contains(&i.id)).collect();
            connection.delete_and_quit(&going)
        });
        let mut report = shared.report.lock().unwrap();
        match deleted {
            Ok(0) => {}
            Ok(n) => {
                report.pop_deleted += n;
                shared.at.lock().unwrap()[3] = format!("POP3 {address}: {n} messages deleted from the server (kept in Eudora)");
            }
            Err(e) if report.failures.len() < FAILURES_LISTED => report.failures.push(format!("{}: old messages not deleted from the server yet ({e})", job.path)),
            Err(_) => {}
        }
    }
    // new mail while the others still download
    loop {
        shared.at.lock().unwrap()[3] = format!(
            "IMAP: up to date · next check at {}",
            (chrono::Local::now() + chrono::Duration::from_std(NEW_MAIL_CHECK).unwrap()).format("%-I:%M %p")
        );
        let until = Instant::now() + NEW_MAIL_CHECK;
        let asked = shared.check.load(Ordering::SeqCst);
        while (Instant::now() < until && shared.check.load(Ordering::SeqCst) == asked) || shared.pause() == Pause::All {
            // (this side counts itself among the running ones)
            if stopping() || shared.running.load(Ordering::SeqCst) <= 1 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        // (an old-mail pause: just the Inbox and Sent)
        let recent_only = shared.pause() == Pause::Old;
        for (n, job) in jobs.iter().enumerate().filter(|(_, j)| !recent_only || folder_rank(&j.path) <= 1) {
            fetch_folder(n, job, true)?;
        }
    }
}

/// Which folders come first: the Inbox, then Sent Items, then the rest.
fn folder_rank(path: &str) -> u8 {
    let name = path.rsplit(": ").next().unwrap_or(path).to_lowercase();
    match name.as_str() {
        "inbox" => 0,
        "sent items" | "sent" => 1,
        _ => 2,
    }
}

/// Fetches `items` a chunk at a time, prepares each (the message, plus Exchange's export it
/// was made from for archive items) and hands the chunk to the writer: as new items, or as
/// originals of items already here (`Wanted::message`).
fn fetch_chunks(
    job: &Job,
    items: &[Wanted],
    workers: &mut [GraphClient],
    compression: &Compression,
    shared: &Shared,
    out: &Sender<Chunk>,
    hold: &mut dyn FnMut(&mut [GraphClient]) -> Result<()>,
    older: bool,
) -> Result<()> {
    let path = job.path.as_str();
    for chunk in items.chunks(CHUNK) {
        hold(workers)?;
        // (older mail: not once the library takes up the space chosen)
        if shared.stop.load(Ordering::Relaxed) || (older && shared.full()) {
            return Ok(());
        }
        // (a new-mail check while paused may have opened another mailbox)
        for w in workers.iter_mut() {
            if w.mailbox != job.mailbox {
                w.set_mailbox(job.mailbox.clone());
            }
        }
        let fetched = match &job.place {
            Place::Primary { .. } => fetch_primary(workers, chunk),
            Place::Archive { owner, .. } => fetch_archive(workers, owner, chunk, path, shared),
            Place::Imap { .. } | Place::Gmail { .. } => return Ok(()), // (`produce_imap`, `produce_gmail`)
        };
        let mut good = Vec::with_capacity(fetched.len());
        for (wanted, result) in chunk.iter().zip(fetched) {
            match result {
                Ok(bytes) => good.push((wanted, bytes)),
                Err(e) => {
                    // (remembered for a later try, after a wait; a canceled one is simply next time's)
                    if !matches!(e, Error::Canceled) {
                        shared.failed.lock().unwrap().push((job.folder, path.to_string(), wanted.id.clone(), e.to_string()));
                    }
                    let mut report = shared.report.lock().unwrap();
                    report.failed += 1;
                    if report.failures.len() < FAILURES_LISTED {
                        report.failures.push(format!("{path}: {e}"));
                    }
                }
            }
        }
        let prepared: Vec<(Option<i64>, Prepared)> = good
            .into_par_iter()
            .map(|(wanted, (raw, export))| {
                let hints = DateHints { envelope: wanted.date.as_deref().and_then(iso_seconds), file: None };
                let item = prepare(raw, hints, format!("{path} (server item {})", short(&wanted.id)), path, job.folder, Some(wanted.id.clone()), compression, false)?;
                let item = match export.filter(|_| KEEP_EXPORTS.load(Ordering::Relaxed)) {
                    Some(export) => item.made_from(original(&export, "exchange-export", path, compression)?),
                    None => item,
                };
                Ok((wanted.message, item))
            })
            .collect::<Result<_>>()?;
        shared.report.lock().unwrap().downloaded += prepared.len() as u64;
        let copies = prepared.iter().any(|(m, _)| m.is_some());
        let chunk_out = if copies {
            Chunk::Copies(prepared.into_iter().filter_map(|(m, p)| m.map(|m| (m, p))).collect())
        } else {
            Chunk::Store(prepared.into_iter().map(|(_, p)| p).collect())
        };
        if out.send(chunk_out).is_err() {
            return Ok(()); // the writer stopped
        }
        // the new ones unread on the server: unread here too, at once (not at the next read-back)
        let unread: Vec<(i64, String, bool)> = if copies { Vec::new() } else { chunk.iter().filter(|w| w.unread).map(|w| (job.folder, w.id.clone(), true)).collect() };
        if !unread.is_empty() && out.send(Chunk::Locate(unread)).is_err() {
            return Ok(());
        }
        shared.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    Ok(())
}

fn short(id: &str) -> &str {
    &id[id.len().saturating_sub(16)..]
}

fn iso_seconds(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|d| d.timestamp())
}

/// Runs `f` over `tasks` with one thread per worker client; results in the tasks' order.
fn in_parallel<T: Sync, R: Send>(workers: &mut [GraphClient], tasks: &[T], f: &(dyn Fn(&mut GraphClient, &T) -> R + Sync)) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..tasks.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for worker in workers.iter_mut() {
            let (next, results) = (&next, &results);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= tasks.len() {
                        break;
                    }
                    let r = f(worker, &tasks[i]);
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    results.into_inner().unwrap().into_iter().map(|r| r.expect("every task ran")).collect()
}

/// A mailbox the user can open, found in the directory.
#[derive(Clone, Debug, Serialize)]
pub struct OpenMailbox {
    pub name: String,
    pub address: String,
    /// Messages in its Inbox.
    pub inbox: u64,
}

/// The mailboxes in the organization the user can open (shared mailboxes, and others' they
/// were given access to): Microsoft offers apps no list of those, so every directory entry is
/// tried, 8 at a time (Exchange answers only for the ones the user may open).
pub fn find_mailboxes(client: &mut GraphClient, progress: &dyn Progress) -> Result<Vec<OpenMailbox>> {
    let people = client.directory(&|n| progress.report(0, 0, &format!("Reading the directory: {} entries…", thousands(n as u64))))?;
    let mut workers: Vec<GraphClient> = (0..8).map(|_| client.worker()).collect();
    let done = AtomicUsize::new(0);
    let total = people.len() as u64;
    let results = in_parallel(&mut workers, &people, &|c, (name, address)| {
        c.set_mailbox(Some(address.clone()));
        let open = c.mailbox_access().ok().map(|inbox| OpenMailbox { name: name.clone(), address: address.clone(), inbox });
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        progress.report(n as u64, total, &format!("Checking which mailboxes you can open: {} of {}", thousands(n as u64), thousands(total)));
        open
    });
    let mut open: Vec<OpenMailbox> = results.into_iter().flatten().collect();
    open.sort_by(|a, b| a.address.to_lowercase().cmp(&b.address.to_lowercase()));
    Ok(open)
}

/// Primary mailbox: each message's MIME (a body cut off by the network is fetched again).
fn fetch_primary(workers: &mut [GraphClient], chunk: &[Wanted]) -> Vec<Fetched> {
    in_parallel(workers, chunk, &|client, wanted| {
        let mut last = None;
        // (no network is waited out inside raw_message; these tries are for server errors)
        for attempt in 0..3 {
            match client.raw_message(&wanted.id) {
                Ok(raw) if !raw.is_empty() => return Ok((raw, None)),
                Ok(_) => last = Some(Error::Other("the server sent an empty message".into())),
                Err(Error::Canceled) => return Err(Error::Canceled),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_secs(1 << attempt));
        }
        Err(last.unwrap())
    })
}

/// Online Archive: exported 20 at a time, then converted to MIME. An export Eudora can't
/// convert is stored anyway, attached to a note (nothing is dropped).
fn fetch_archive(workers: &mut [GraphClient], owner: &str, chunk: &[Wanted], path: &str, shared: &Shared) -> Vec<Fetched> {
    let groups: Vec<&[Wanted]> = chunk.chunks(EXPORT_BATCH).collect();
    let n = workers.len().min(3);
    let exported = in_parallel(&mut workers[..n], &groups, &|client, group| {
        let ids: Vec<String> = group.iter().map(|w| w.id.clone()).collect();
        match client.export_items(owner, &ids) {
            // a group that fails as a whole (an answer too big, one item Exchange chokes on) is
            // asked for item by item, so one message never keeps the other 19 from coming down
            Err(e) if ids.len() > 1 && !matches!(e, Error::Canceled) => {
                let mut items = Vec::with_capacity(ids.len());
                for id in &ids {
                    match client.export_items(owner, std::slice::from_ref(id)) {
                        Ok(mut one) => items.append(&mut one),
                        Err(Error::Canceled) => return Err(Error::Canceled),
                        Err(e) => items.push(crate::graph::ExportedItem { item_id: id.clone(), data: None, error: Some(e.to_string()) }),
                    }
                }
                Ok(items)
            }
            // the group came back, but an item in it didn't (Exchange's answer for that one item,
            // e.g. too big to go with the others): that one is asked for again on its own
            Ok(mut items) if ids.len() > 1 => {
                for id in &ids {
                    let got = items.iter().position(|i| &i.item_id == id);
                    if got.is_some_and(|n| items[n].data.is_some()) {
                        continue;
                    }
                    match client.export_items(owner, std::slice::from_ref(id)) {
                        Ok(one) => {
                            if let Some(again) = one.into_iter().find(|i| i.data.is_some()) {
                                match got {
                                    Some(n) => items[n] = again,
                                    None => items.push(again),
                                }
                            }
                        }
                        Err(Error::Canceled) => return Err(Error::Canceled),
                        Err(_) => {} // (its first answer stays, with why)
                    }
                }
                Ok(items)
            }
            other => other,
        }
    });
    let mut out = Vec::with_capacity(chunk.len());
    for (group, result) in groups.iter().zip(exported) {
        match result {
            Err(e) => out.extend(group.iter().map(|_| Err(Error::Other(format!("export: {e}"))))),
            Ok(items) => {
                for wanted in group.iter() {
                    let item = items.iter().find(|i| i.item_id == wanted.id);
                    out.push(match item.and_then(|i| i.data.as_deref()) {
                        Some(data) => Ok((
                            match fts::parse(data) {
                                Ok(parsed) => fts::to_mime(&parsed),
                                Err(e) => {
                                    shared.report.lock().unwrap().unconverted += 1;
                                    unconverted(&wanted.id, data, &e.to_string(), path)
                                }
                            },
                            Some(data.to_vec()),
                        )),
                        None => Err(Error::Other(format!(
                            "export: {}",
                            item.and_then(|i| i.error.clone()).unwrap_or_else(|| "Exchange returned nothing for this item".into())
                        ))),
                    });
                }
            }
        }
    }
    out
}

/// Converts again the archive items earlier runs stored as "couldn't convert" notes (the note
/// holds Exchange's export, so nothing is downloaded). Ones this version reads take the note's
/// place; the rest stay notes.
fn reconvert(library: &mut Library, writer: &mut WriterState, report: &mut SyncReport, progress: &dyn Progress) -> Result<()> {
    let notes: Vec<(i64, i64, i64, String)> = {
        let mut stmt = library.conn.prepare(
            "SELECT m.id, m.date, l.folder, f.path FROM messages m JOIN locations l ON l.message = m.id JOIN folders f ON f.id = l.folder
             WHERE m.message_id LIKE '%@unconverted.eudora' GROUP BY m.id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let compression = library.compression();
    for (n, (id, date, folder, path)) in notes.iter().enumerate() {
        progress.report(n as u64, notes.len() as u64, &format!("Converting again archive item {} of {} that an earlier version couldn't read", n + 1, notes.len()));
        let raw = library.raw_message(*id)?;
        let Some(export) = attached_export(&raw) else { continue };
        let Ok(item) = fts::parse(&export) else { continue };
        let hints = DateHints { envelope: Some(*date), file: None };
        let mut prepared = prepare(fts::to_mime(&item), hints, format!("{path} (converted again)"), path, *folder, None, &compression, false)?;
        if library.keeps_exchange_exports() {
            prepared = prepared.made_from(original(&export, "exchange-export", path, &compression)?);
        }
        writer.replace_message(library, *id, &prepared)?;
        report.reconverted += 1;
    }
    Ok(())
}

/// The export inside a "couldn't convert" note.
fn attached_export(raw: &[u8]) -> Option<Vec<u8>> {
    use mail_parser::MimeHeaders;
    let message = mail_parser::MessageParser::default().parse(raw)?;
    message.attachments().find(|a| a.attachment_name() == Some("item.fts")).map(|a| a.contents().to_vec())
}

/// A note holding an export Eudora couldn't convert, with the export attached.
fn unconverted(item_id: &str, export: &[u8], why: &str, folder: &str) -> Vec<u8> {
    let id = blake3::hash(item_id.as_bytes()).to_hex();
    let b64 = base64::engine::general_purpose::STANDARD.encode(export);
    let lines: Vec<&str> = b64.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap()).collect();
    format!(
        "From: Eudora <eudora@unconverted.invalid>\r\nSubject: Exchange item Eudora couldn't convert\r\nMessage-ID: <{}@unconverted.eudora>\r\n\
         MIME-Version: 1.0\r\nX-Eudora-Converted: failed\r\nContent-Type: multipart/mixed; boundary=\"=_unconverted\"\r\n\r\n\
         --=_unconverted\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
         This item from {folder} couldn't be turned into an ordinary message ({why}).\r\n\
         Exchange's export of it is attached, unchanged (item.fts).\r\n\r\n\
         --=_unconverted\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"item.fts\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{}\r\n--=_unconverted--\r\n",
        &id[..32],
        lines.join("\r\n")
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_moved_on_the_server_by_another_device_leaves_its_folder_here() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        let mut files = Vec::new();
        for n in 0..4 {
            let file = dir.path().join(format!("{n}.eml"));
            let when = chrono::DateTime::from_timestamp(now() - 3 * 86_400, 0).unwrap().to_rfc2822();
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Hi {n}\r\nDate: {when}\r\nMessage-ID: <{n}@a>\r\n\r\nHello {n}\r\n")).unwrap();
            files.push(file);
        }
        crate::import(&mut library, &files, &crate::ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &crate::progress::Silent).unwrap();
        let conn = &library.conn;
        let inbox: i64 = conn.query_row("SELECT folder FROM locations LIMIT 1", [], |r| r.get(0)).unwrap();
        let account: i64 = conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        let spam = db::ensure_folder(conn, account, "Spam").unwrap();
        let ids: Vec<i64> = conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        for (n, id) in ids.iter().enumerate() {
            conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)", params![inbox, format!("s{n}"), id]).unwrap();
        }
        // 1 is in Spam here too already (the same pass listed Spam first); 2 has a change waiting
        conn.execute("INSERT INTO locations (message, folder, date) SELECT id, ?1, date FROM messages WHERE id = ?2", params![spam, ids[1]]).unwrap();
        conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (0, 'mark', ?1)", [serde_json::json!({ "message": ids[2] }).to_string()]).unwrap();
        // the server's Inbox now holds only s3: 0, 1 and 2 were moved or deleted elsewhere
        let left = forget_gone(&mut library, inbox, &["s3".to_string()], Some(now() - 30 * 86_400)).unwrap();
        assert_eq!(left, 2);
        let conn = &library.conn;
        let in_inbox: Vec<i64> = conn.prepare("SELECT message FROM locations WHERE folder = ?1 ORDER BY message").unwrap().query_map([inbox], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(in_inbox, vec![ids[2], ids[3]], "the waiting change keeps 2 where it is");
        let exists = |id: i64| conn.query_row("SELECT COUNT(*) FROM messages WHERE id = ?1", [id], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(exists(ids[0]), 0, "0 had no other place: gone (it comes back where it went)");
        assert_eq!(exists(ids[1]), 1, "1 stays, in Spam");
        // mail older than the period isn't judged
        assert_eq!(forget_gone(&mut library, inbox, &[], Some(now())).unwrap(), 0);
    }

    #[test]
    fn each_account_keeps_its_own_check_interval() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        assert_eq!(check_minutes(&library, "ann@gmail.com"), DEFAULT_CHECK_MINUTES);
        set_check_minutes(&library, "Ann@Gmail.com", 10).unwrap();
        assert_eq!(check_minutes(&library, "ann@gmail.com"), 10, "addresses match whatever their case");
        assert_eq!(shortest_check(&library), DEFAULT_CHECK_MINUTES, "the others stay at the default");
        set_check_minutes(&library, "bob@example.com", 1).unwrap();
        assert_eq!(shortest_check(&library), 1);
        assert!(set_check_minutes(&library, "bob@example.com", 7).is_err(), "only the choices offered");
        set_check_minutes(&library, "bob@example.com", DEFAULT_CHECK_MINUTES).unwrap();
        assert!(!check_intervals(&library).contains_key("bob@example.com"), "the default isn't stored");
        // the first check in a run is always due; the next only once the interval passed
        assert!(account_due(&library, "ann@gmail.com"));
        assert!(!account_due(&library, "ann@gmail.com"));
        CHECK_EVERY_ACCOUNT.store(true, Ordering::Relaxed);
        assert!(account_due(&library, "ann@gmail.com"), "Check Mail checks every account");
        CHECK_EVERY_ACCOUNT.store(false, Ordering::Relaxed);
    }

    #[test]
    fn a_failed_item_waits_longer_each_time_and_is_given_up_at_last() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        let fail = |id: &str, why: &str| record_failures(&library, vec![(1, "Online Archive/Inbox".into(), id.into(), why.into())], &crate::progress::Silent).unwrap();
        fail("a", "export: the response body is larger than request limit");
        let items = failed_items(&library).unwrap();
        let a = &items["a"];
        assert_eq!(a.tries, 1);
        assert!(a.next > now() + 86000 && a.next <= now() + 86400, "tried again in a day");
        for _ in 1..GIVE_UP_AFTER {
            fail("a", "export: still too big");
        }
        assert!(failed_items(&library).unwrap()["a"].given_up(), "given up after {GIVE_UP_AFTER} tries");
        // "not found" may be Microsoft's passing trouble: tried again like the rest
        fail("b", "Microsoft Graph 404 ErrorItemNotFound: The specified object was not found");
        assert!(!failed_items(&library).unwrap()["b"].given_up());
        // asked for again: both wait for nothing
        assert_eq!(retry_failed(&library).unwrap(), 2);
        assert!(failed_items(&library).unwrap().values().all(|f| f.next == 0 && f.tries == 0));
    }

    #[test]
    fn pause_is_kept_beside_the_library() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pause(dir.path()), Pause::None);
        set_pause(dir.path(), Pause::Old).unwrap();
        assert_eq!(read_pause(dir.path()), Pause::Old);
        set_pause(dir.path(), Pause::All).unwrap();
        assert_eq!(read_pause(dir.path()), Pause::All);
        set_pause(dir.path(), Pause::None).unwrap();
        assert_eq!(read_pause(dir.path()), Pause::None);
        assert!(!dir.path().join(PAUSE_FILE).exists());
    }

    #[test]
    fn new_mail_while_paused_is_the_inbox_and_sent() {
        assert!(folder_rank("Inbox") <= 1 && folder_rank("sam@example.com: Sent Items") <= 1 && folder_rank("INBOX") <= 1);
        assert!(folder_rank("Online Archive/Old Sent Items/2010") > 1);
    }
}
