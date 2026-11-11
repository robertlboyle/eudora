//! Keeping a library up to date: the download that runs while Eudora is open (`eudora-cli graph
//! download --watch`, started by the desktop window as a program of its own), moved here from the
//! command line so a phone app, which can't start another program, runs the same loop in a thread
//! of its own.
//!
//! One pass over every account (`download_all`), then a check for new mail every few minutes, the
//! Online Archive every 6 hours; between checks the header index and the Views are prepared, and
//! after each check the Out mailbox's due messages go and settings / filing sync runs. Check Mail
//! and opening a folder wake it; Pause ▸ All holds it. It ends when `progress.canceled()` says so.

use std::time::{Duration, Instant};

use crate::graph::GraphClient;
use crate::graph_sync::{self, Pause, SyncOptions, SyncReport};
use crate::progress::{Progress, thousands};
use crate::sync_run::SyncAll;
use crate::{Error, Library, Result};

/// What to download and how often.
#[derive(Clone, Debug)]
pub struct Watch {
    pub options: SyncOptions,
    /// Only the IMAP / POP3 (and no Microsoft 365) accounts.
    pub imap_only: bool,
    /// The Microsoft permissions to fall back on when the full sign-in can't be renewed.
    pub scopes: &'static str,
    /// Minutes between checks for new mail; None = one pass only.
    pub every: Option<u64>,
}

impl Watch {
    /// The window's background download: every account's mail, the Online Archive (`archive`),
    /// the shared mailboxes marked active, checked every 5 minutes.
    pub fn background(archive: bool) -> Watch {
        Watch {
            options: SyncOptions { primary: true, archive, only: None, workers: 4, hidden: false, originals: false, shared: true, imap: true, google: true },
            imap_only: false,
            scopes: crate::graph::ALL_SCOPES,
            every: Some(5),
        }
    }

    fn google(&self) -> bool {
        !crate::google::signed_in_accounts().is_empty() && !self.imap_only
    }
}

/// Every Microsoft account signed in on this computer (a work account and an Outlook.com one,
/// say), the default first; none for only the mail servers.
fn resume_all(w: &Watch) -> Vec<GraphClient> {
    if w.imap_only {
        return Vec::new();
    }
    GraphClient::signed_in_accounts().iter().filter_map(|a| GraphClient::resume_best(Some(a), w.scopes).ok()).collect()
}

/// The Microsoft sign-ins to download with. Watching a library with no account yet (the window's
/// first run) waits for one: a Microsoft sign-in, a Google account, or a mail server added in
/// Settings ▸ Accounts. Without a Microsoft sign-in the mail servers' accounts still download.
pub fn sign_ins(library: &Library, w: &Watch, progress: &dyn Progress) -> Result<Vec<GraphClient>> {
    let o = &w.options;
    // (o.imap: the mail servers are in this run, i.e. neither --no-imap nor --archive-only)
    let mail_servers = |library: &Library| -> Result<bool> { Ok((!library.imap_accounts()?.is_empty() || w.google()) && o.imap) };
    let mut servers = mail_servers(library)?;
    let has_microsoft = library.accounts_counted(false)?.iter().any(|a| a.kind.starts_with("graph"));
    if w.every.is_some() && !servers && !w.imap_only {
        let mut said = Instant::now() - Duration::from_secs(60);
        while GraphClient::resume_best(None, w.scopes).is_err() {
            if progress.canceled() {
                return Err(Error::Canceled);
            }
            if mail_servers(library)? {
                servers = true;
                break;
            }
            if said.elapsed() >= Duration::from_secs(20) {
                graph_sync::write_watching(library, "", "No accounts yet: add one in Settings ▸ Accounts (Microsoft 365, Google, IMAP or POP3).", None);
                said = Instant::now();
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    let clients = resume_all(w);
    if clients.is_empty() && !w.imap_only {
        if servers {
            if has_microsoft {
                progress.event("Microsoft 365 isn't signed in on this computer: only the mail server accounts are downloaded. Sign in from Settings ▸ Accounts.");
            }
        } else if !has_microsoft {
            return Err(Error::Other(
                "no accounts to download: sign in to Microsoft 365 (`eudora-cli graph login`) or add an IMAP / POP3 account (Settings ▸ Accounts ▸ Add account)".into(),
            ));
        } else {
            // (says why: the default sign-in can't be renewed)
            GraphClient::resume_best(None, w.scopes)?;
        }
    }
    Ok(clients)
}

/// One pass over every account: each Microsoft account signed in, one after another (the mail
/// servers and the shared mailboxes with the first, the default one), or only the mail servers
/// when none is. A Microsoft account whose pass fails doesn't stop the others (said, and tried
/// again next pass); it's an error only when every pass failed. One report per pass.
pub fn download_all(library: &mut Library, clients: &mut [GraphClient], options: &SyncOptions, progress: &dyn Progress) -> Result<Vec<SyncReport>> {
    if clients.is_empty() {
        return Ok(vec![graph_sync::download(library, None, options, progress)?]);
    }
    let (mut reports, mut last_error) = (Vec::new(), None);
    for (i, client) in clients.iter_mut().enumerate() {
        let mut each = options.clone();
        if i > 0 {
            each.imap = false;
            each.google = false;
            each.shared = false;
        }
        match graph_sync::download(library, Some(client), &each, progress) {
            Ok(r) => {
                let restart = r.restart;
                reports.push(r);
                if restart {
                    break; // an account was added: the next pass starts at once
                }
            }
            Err(Error::Canceled) => return Err(Error::Canceled),
            Err(e) => {
                progress.event(&format!("{}: this check didn't finish ({e}); trying again at the next one.", client.account));
                // broken stored text: repaired now, so the next check isn't stopped by it again
                if e.to_string().contains("invalid utf-8") {
                    match library.repair_text(progress) {
                        Ok(n) => progress.event(&format!("Checked all stored text: {n} broken values repaired.")),
                        Err(r) => progress.event(&format!("Couldn't repair the stored text ({r}).")),
                    }
                }
                last_error = Some(e);
            }
        }
    }
    match (reports.is_empty(), last_error) {
        (true, Some(e)) => Err(e),
        _ => Ok(reports),
    }
}

/// Keeps checking for new mail after the first pass, until `progress.canceled()`: the mailboxes
/// and shared mailboxes every `w.every` minutes, the archive (old mail) every 6 hours. `at_once`:
/// the first pass found an account added meanwhile (the next pass starts now). `on_sync` hears
/// what settings and filing sync did.
pub fn keep_up(library: &mut Library, clients: &mut Vec<GraphClient>, w: &Watch, mut at_once: bool, progress: &dyn Progress, on_sync: &dyn Fn(&SyncAll)) -> Result<()> {
    let shown_account = |clients: &[GraphClient], library: &Library| -> String {
        let mut all: Vec<String> = clients.iter().map(|c| c.account.clone()).collect();
        all.extend(library.imap_accounts().unwrap_or_default().iter().map(|c| c.address.clone()));
        all.join(", ")
    };
    let mut last_archive = Instant::now();
    let mut last_trim = None;
    // each Microsoft account's Inbox, looked at every minute between checks: a client of its own
    // (a pass ending stops the pass's connection state), and the new mail already reported
    let mut inboxes: Vec<(GraphClient, std::collections::HashSet<String>)> = Vec::new();
    let mut inbox_looked = Instant::now();
    trim_when_due(library, clients, &mut last_trim, progress);
    while !progress.canceled() {
        // woken as often as the most frequently checked account wants (each account is checked
        // only when its own interval has passed: `graph_sync::account_due`)
        let minutes = w.every.unwrap_or(5).min(u64::from(graph_sync::shortest_check(library))).max(1);
        // nobody using Eudora here: Gmail accounts are checked only every few hours
        // (`graph_sync::account_due`); someone back: every account is checked at once
        let resting = graph_sync::gmail_resting(library);
        let every = Duration::from_secs(minutes * 60);
        let next = chrono::Local::now() + chrono::Duration::from_std(every).unwrap_or_default();
        let wake = Instant::now() + every;
        let mut said = Instant::now() - Duration::from_secs(60);
        // wait for the next check, or Check Mail in the window
        let all_paused = |library: &Library| graph_sync::read_pause(&library.root) == Pause::All;
        let asked = |library: &Library| graph_sync::check_requested(&library.root) || library.root.join(graph_sync::LOOK_FILE).exists();
        let mut back = false;
        let mut new_in: Vec<String> = Vec::new();
        while !at_once && (Instant::now() < wake || all_paused(library)) && !(asked(library) && !all_paused(library)) {
            if resting.is_some() && in_use_now(&library.root) && !all_paused(library) {
                back = true;
                break;
            }
            if progress.canceled() {
                let _ = library.commit_header_index();
                return Ok(());
            }
            // new mail in an Inbox: that account is checked now (only it: Gmail keeps resting)
            if inbox_looked.elapsed() >= Duration::from_secs(60) && !all_paused(library) {
                inbox_looked = Instant::now();
                for c in clients.iter() {
                    if !inboxes.iter().any(|(w, _)| w.account.eq_ignore_ascii_case(&c.account)) {
                        let mut w = c.worker();
                        w.account = c.account.clone();
                        w.link = std::sync::Arc::new(crate::graph::Link::default());
                        w.set_mailbox(None);
                        inboxes.push((w, Default::default()));
                    }
                }
                for (w, seen) in &mut inboxes {
                    if graph_sync::inbox_has_new(library, w, seen).unwrap_or(false) {
                        graph_sync::make_due(&w.account);
                        new_in.push(w.account.clone());
                    }
                }
                if !new_in.is_empty() {
                    break;
                }
            }
            if said.elapsed() >= Duration::from_secs(20) {
                let paused = all_paused(library);
                let line = if paused {
                    "Paused: nothing is downloaded until you resume.".to_string()
                } else if let Some(r) = resting {
                    format!("Up to date. Next check at {}; nobody is using Eudora here, so Gmail is checked every {} (at once when you're back)", next.format("%-I:%M %p"), crate::progress::minutes_text(r))
                } else {
                    format!("Up to date. Next check at {}", next.format("%-I:%M %p"))
                };
                graph_sync::write_watching(library, &shown_account(clients, library), &line, (!paused).then(|| next.timestamp()));
                said = Instant::now();
            }
            // idle between checks: prepare header search and the views (a batch a second; each
            // message's text is read, 2,000 at a time)
            let views = library.tag_views(2_000).unwrap_or(0);
            if library.update_header_index(1000).unwrap_or(0) == 0 && views == 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        let _ = library.commit_header_index();
        if back {
            progress.event("Eudora is being used again here: checking every account now.");
        }
        if !new_in.is_empty() {
            progress.event(&format!("New mail in the Inbox of {}: checking it now.", new_in.join(", ")));
        }
        // Check Mail (or a new account) checks every account now, whatever its interval (new
        // mail seen in an Inbox checks only its account: `make_due`)
        let every_account = at_once || (Instant::now() < wake && new_in.is_empty());
        graph_sync::CHECK_EVERY_ACCOUNT.store(every_account, std::sync::atomic::Ordering::Relaxed);
        at_once = false;
        // signed in meanwhile (another Microsoft account, or again with permission to change
        // mail): from this pass on
        let now_signed_in = resume_all(w);
        for c in &now_signed_in {
            match clients.iter().find(|o| o.account.eq_ignore_ascii_case(&c.account)) {
                None => progress.event(&format!("Signed in to Microsoft as {}: its mail is downloaded from now on.", c.account)),
                Some(old) if !old.may_change_mail() && c.may_change_mail() => {
                    progress.event(&format!("{}: signed in with permission to change mail: moves made in Eudora are now made on the server too.", c.account))
                }
                _ => {}
            }
        }
        if !now_signed_in.is_empty() || clients.is_empty() {
            *clients = now_signed_in;
        }
        let mut pass = w.options.clone();
        pass.archive = w.options.archive && last_archive.elapsed() >= Duration::from_secs(6 * 3600);
        if pass.archive {
            last_archive = Instant::now();
        }
        let passed = download_all(library, clients, &pass, progress);
        graph_sync::CHECK_EVERY_ACCOUNT.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(line) = crate::graph::throttling_said() {
            progress.event(&line);
        }
        match passed {
            Ok(reports) => {
                at_once = reports.iter().any(|r| r.restart);
                let imported: u64 = reports.iter().map(|r| r.counts.imported).sum();
                if imported > 0 {
                    progress.event(&format!("{} new messages", thousands(imported)));
                }
                // within the period or space chosen (phones)
                trim_when_due(library, clients, &mut last_trim, progress);
                // the Out mailbox: what's due goes (the window sends too: each is claimed once)
                match crate::outbox::send_due(library, false) {
                    Ok(s) => s.notes.iter().for_each(|n| progress.event(n)),
                    Err(e) => progress.event(&format!("Queued messages weren't sent ({e}); trying again at the next check.")),
                }
                // (Settings ▸ Downloading and sync ▸ Repair leaves a request)
                let repair = std::fs::remove_file(library.root.join("status").join("sync-repair")).is_ok();
                // settings and filing, with this person's other computers: through each account's
                // own server (Microsoft 365, Gmail, IMAP), else the home account's
                match crate::sync_run::sync_all(library, clients.first_mut(), repair) {
                    Ok(done) => on_sync(&done),
                    Err(e) => progress.event(&format!("Sync didn't run ({e}); trying again at the next check.")),
                }
            }
            Err(Error::Canceled) => return Ok(()),
            Err(e) => progress.event(&format!("This check didn't finish ({e}); trying again at the next one.")),
        }
    }
    Ok(())
}

/// Someone used the window in the last minute and a half (it says so at most once a minute).
fn in_use_now(root: &std::path::Path) -> bool {
    crate::maintenance::idle_for(root).is_some_and(|s| s < 90)
}

/// Trims the library to the period or space chosen (`trim.rs`): hourly, and at once when the
/// choice changed. `last` = when it last ran and with which choice.
pub fn trim_when_due(library: &Library, clients: &mut [GraphClient], last: &mut Option<(Instant, Option<String>)>, progress: &dyn Progress) {
    let choice = library.setting(graph_sync::PERIOD_SETTING).ok().flatten();
    if choice.is_none() {
        return;
    }
    if last.as_ref().is_some_and(|(at, was)| *was == choice && at.elapsed() < Duration::from_secs(3600)) {
        return;
    }
    *last = Some((Instant::now(), choice));
    // (each message's server is asked first: only what it still has leaves this device)
    let mut confirm = |l: &Library, ids: &[i64]| crate::keep_check::confirm_on_servers(l, clients, ids, progress);
    match library.trim_to_limit(&mut confirm, progress) {
        Ok(r) => {
            for note in &r.notes {
                progress.event(note);
            }
            if r.messages > 0 {
                progress.event(&format!(
                    "Removed {} older {} from this device to keep only the mail chosen ({} MB → {} MB). Their servers confirmed they still have them.",
                    thousands(r.messages),
                    if r.messages == 1 { "message" } else { "messages" },
                    r.before / 1_000_000,
                    r.after / 1_000_000
                ));
            }
            if r.unconfirmed > 0 {
                progress.event(&format!(
                    "Kept {} older {} here: {} couldn't confirm {} still {} {}.",
                    thousands(r.unconfirmed),
                    if r.unconfirmed == 1 { "message" } else { "messages" },
                    if r.unconfirmed == 1 { "its server" } else { "their servers" },
                    if r.unconfirmed == 1 { "it" } else { "they" },
                    if r.unconfirmed == 1 { "has" } else { "have" },
                    if r.unconfirmed == 1 { "it" } else { "them" },
                ));
            }
        }
        Err(e) => progress.event(&format!("Older mail wasn't trimmed ({e}); trying again in an hour.")),
    }
}

/// The whole background download (a phone app's thread): sign-ins, the first pass, then
/// [`keep_up`], until `progress.canceled()`. Each pass's result goes to `progress` as events.
pub fn run(library: &mut Library, w: &Watch, progress: &dyn Progress) -> Result<()> {
    let mut clients = sign_ins(library, w, progress)?;
    let first = match download_all(library, &mut clients, &w.options, progress) {
        Err(Error::Canceled) => return Ok(()),
        other => other?,
    };
    for r in &first {
        progress.event(&pass_summary(r));
    }
    let at_once = first.iter().any(|r| r.restart);
    keep_up(library, &mut clients, w, at_once, progress, &|_| {})
}

/// One line on a pass: whether everything on the server is here.
pub fn pass_summary(r: &SyncReport) -> String {
    if r.failed > 0 {
        format!("{}: {} new; {} items on the server aren't here yet (tried again next pass).", r.account, thousands(r.downloaded), thousands(r.failed))
    } else {
        format!("{}: {} new; every item on the server is here.", r.account, thousands(r.downloaded))
    }
}
