//! Changes made in Eudora carried out in Gmail.
//!
//! Gmail has labels, not folders: each folder of a Gmail account in Eudora is a label (its
//! `server_id` the label's id: INBOX, SENT, SPAM, TRASH, a user label's id), "All Mail" is no
//! label (`ALL`), and a message's id is the same whatever labels it has. So a move from A to B is
//! "take label A off, put label B on"; to Trash is Gmail's trash, out of it Gmail's untrash; to
//! All Mail just takes A off (Gmail's Archive); a folder made in Eudora becomes a label ("/" makes
//! it nested, as Gmail shows it). Read / unread is the UNREAD label, high priority the star.
//!
//! The same queue and steps as the IMAP side (`pending_ops` "move" of Gmail accounts and
//! "gmail-mark"; planned with the IMAP batch, carried out on the same thread, recorded by
//! `server_changes::record_batch`). Empty Trash leaves Gmail's Trash alone: Gmail deletes it for
//! good after 30 days, and a mistake can still be undone there until then.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::error::{Error, Result};
use crate::google::GoogleClient;
use crate::library::Library;
use crate::server_changes::{BatchResult, GIVE_UP, MoveResult, Outcome};

/// "All Mail": the folder of mail with no label.
const ALL: &str = "ALL";

/// A move inside a Gmail account.
#[derive(Clone, Debug)]
pub struct GmailMove {
    pub op: i64,
    pub message: i64,
    /// The message's Gmail id.
    pub item: String,
    /// The label it leaves (ALL: none) and the one it goes to (None: a label still to make).
    pub from: String,
    pub to: i64,
    pub to_label: Option<String>,
    /// The folder's path, as the label's name when it's made.
    pub to_path: String,
}

/// A message's read state and star.
#[derive(Clone, Debug)]
pub struct GmailMark {
    pub op: i64,
    pub item: String,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

/// One Gmail account's changes.
#[derive(Clone, Debug)]
pub struct GmailWork {
    pub account: i64,
    pub address: String,
    pub moves: Vec<GmailMove>,
    pub marks: Vec<GmailMark>,
    /// The account's Google contacts read again (hourly).
    pub contacts: bool,
    /// The account's Google calendars read again for this range (Unix seconds): the near weeks
    /// every 15 minutes, everything once a day, as Microsoft 365's.
    pub calendars: Option<(i64, i64)>,
    /// Folders made, renamed or deleted in Eudora (labels in Gmail; done first).
    pub folders: Vec<crate::folder_changes::FolderJob>,
    /// What changed in Gmail itself since this history id ("" = start counting now), read back.
    pub history: Option<String>,
    /// Google contacts made, changed or deleted in Eudora.
    pub cards: Vec<GoogleCard>,
    /// Google calendar events made, changed, answered or deleted in Eudora.
    pub events: Vec<GoogleEvent>,
}

/// An event change for Google Calendar (as `server_changes::EventJob` is Microsoft's).
#[derive(Clone, Debug)]
pub struct GoogleEvent {
    pub op: i64,
    pub event: Option<i64>,
    /// The calendar's id in Google ("primary" when not known).
    pub calendar: String,
    pub server_id: String,
    /// create, update, respond, cancel, delete
    pub action: String,
    pub answer: Option<String>,
    pub patch: Option<serde_json::Value>,
}

/// A contact change for Google (People API).
#[derive(Clone, Debug)]
pub struct GoogleCard {
    pub op: i64,
    pub card: Option<i64>,
    pub change: GoogleCardChange,
}

#[derive(Clone, Debug)]
pub enum GoogleCardChange {
    Create { person: serde_json::Value },
    Update { resource: String, etag: String, person: serde_json::Value },
    Delete { resource: String },
}

impl GmailWork {
    fn new(account: i64, address: String) -> GmailWork {
        GmailWork { account, address, moves: Vec::new(), marks: Vec::new(), contacts: false, calendars: None, folders: Vec::new(), history: None, cards: Vec::new(), events: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.marks.is_empty() && !self.contacts && self.calendars.is_none() && self.folders.is_empty() && self.history.is_none() && self.cards.is_empty() && self.events.is_empty()
    }
}

/// Where reading an account's changes back starts (setting: Gmail's history id).
fn history_key(address: &str) -> String {
    format!("gmail-history:{address}")
}

/// Whether `every` seconds have passed since `key` was last done (kept on disk: a restart
/// doesn't start every Gmail job over), noting it as done now when they have.
pub(crate) fn due_and_noted(key: &str, every: i64) -> bool {
    let now = crate::library::now();
    if crate::google::schedule_get(key).is_some_and(|last| now - last < every && last <= now) {
        return false;
    }
    crate::google::schedule_set(key, now);
    true
}

/// When each Google account's changes were last read back: every minute, or at the account's
/// own interval when one was chosen (`graph_sync::check_minutes`: a Gmail account checked every
/// 10 minutes because Gmail was throttling it isn't asked every minute either).
fn history_due(library: &Library, address: &str) -> bool {
    let mut minutes = crate::graph_sync::check_intervals(library).get(&address.to_lowercase()).copied().unwrap_or(1);
    // nobody using Eudora here: every few hours too (`graph_sync::gmail_resting`)
    if let Some(r) = crate::graph_sync::gmail_resting(library) {
        minutes = minutes.max(r);
    }
    due_and_noted(&format!("history:{}", address.to_lowercase()), i64::from(minutes) * 60 - 5)
}

/// What reading an account's changes back brought: the changes, and where to start next time.
#[derive(Debug)]
pub struct HistoryFetched {
    pub account: i64,
    pub address: String,
    pub next: Option<String>,
    pub changes: Vec<crate::google::GmailChange>,
}

/// The range of an account's calendars due to be read again, if any: everything daily, the near
/// weeks every 15 minutes (every few hours while nobody uses Eudora here); remembered on disk, so
/// a restart doesn't read every calendar since 2000 again.
pub(crate) fn calendars_due(address: &str, near_minutes: u32) -> Option<(i64, i64)> {
    let now = crate::library::now();
    let a = address.to_lowercase();
    if due_and_noted(&format!("calendars-all:{a}"), 86_400) {
        crate::google::schedule_set(&format!("calendars-near:{a}"), now);
        return Some((EVERYTHING_FROM, now + 3 * 365 * 86400));
    }
    due_and_noted(&format!("calendars-near:{a}"), i64::from(near_minutes.max(15)) * 60).then_some((now - 30 * 86400, now + 365 * 86400))
}

/// The start of "everything" in a calendar (2000-01-01).
const EVERYTHING_FROM: i64 = 946_684_800;

/// Whether an account's contacts are due (hourly; noted as read when they are, on disk).
fn contacts_due(address: &str) -> bool {
    due_and_noted(&format!("contacts:{}", address.to_lowercase()), 3600)
}

/// The next batch: up to `limit` moves and marks of each Gmail account.
pub fn plan(library: &Library, limit: usize) -> Result<Vec<GmailWork>> {
    let conn = &library.conn;
    let mut work: HashMap<i64, GmailWork> = HashMap::new();
    // Only accounts signed in to Google on this computer: another's changes wait, untried, until
    // it is (2026-11-06: an account restored from a backup without its sign-in was tried every
    // 5 seconds, said so in the log each time, and its changes were given up after 10 tries)
    let signed_in: Vec<String> = crate::google::signed_in_accounts().into_iter().map(|a| a.to_lowercase()).collect();
    let address_of = |account: i64| -> Result<Option<String>> {
        let address: Option<String> = conn.query_row("SELECT lower(address) FROM accounts WHERE id = ?1 AND kind = 'gmail'", params![account], |r| r.get(0)).optional()?.flatten();
        Ok(address.filter(|a| signed_in.contains(a)))
    };
    // changes given up only because the account wasn't signed in here: tried again (once it is)
    conn.execute(
        "UPDATE pending_ops SET attempts = 0, last_error = NULL WHERE attempts > 0 AND last_error LIKE '%isn''t signed in to Google on this computer%'",
        [],
    )?;

    // moves
    let ops: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.payload FROM pending_ops p JOIN folders f ON f.id = json_extract(p.payload, '$.from') JOIN accounts a ON a.id = f.account
             WHERE p.kind = 'move' AND p.attempts < ?1 AND a.kind = 'gmail' ORDER BY p.id LIMIT ?2",
        )?;
        stmt.query_map(params![GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut moving = Vec::new();
    for (op, payload) in ops {
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (Some(message), Some(item), Some(from), Some(to)) = (v["message"].as_i64(), v["server_id"].as_str(), v["from"].as_i64(), v["to"].as_i64()) else {
            conn.execute("UPDATE pending_ops SET attempts = ?2, last_error = 'unreadable entry' WHERE id = ?1", params![op, GIVE_UP])?;
            continue;
        };
        let folder = |id: i64| -> Result<Option<(i64, String, Option<String>)>> {
            Ok(conn.query_row("SELECT account, path, server_id FROM folders WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)
        };
        let (Some((account, _, Some(from_label))), Some((_, to_path, to_label))) = (folder(from)?, folder(to)?) else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // a folder that's gone, or not in Gmail
            continue;
        };
        let Some(address) = address_of(account)? else { continue };
        let w = work.entry(account).or_insert_with(|| GmailWork::new(account, address));
        if w.moves.len() < limit {
            moving.push(message);
            w.moves.push(GmailMove { op, message, item: item.to_string(), from: from_label, to, to_label, to_path });
        }
    }

    // read / unread, the star
    let ops: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare("SELECT id, json_extract(payload, '$.message') FROM pending_ops WHERE kind = 'gmail-mark' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (limit * 2) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, message) in ops {
        if moving.contains(&message) {
            continue; // (after its move)
        }
        let Some(state) = conn.query_row("SELECT flags FROM messages WHERE id = ?1", params![message], |r| r.get::<_, i64>(0)).optional()? else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let place: Option<(i64, String)> = conn
            .query_row(
                "SELECT f.account, l.server_id FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
                 WHERE l.message = ?1 AND a.kind = 'gmail' AND l.server_id IS NOT NULL LIMIT 1",
                params![message],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((account, item)) = place else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // no longer in Gmail
            continue;
        };
        let (add, remove) = labels_for(state);
        let Some(address) = address_of(account)? else { continue };
        let w = work.entry(account).or_insert_with(|| GmailWork::new(account, address));
        if w.marks.len() < limit {
            w.marks.push(GmailMark { op, item, add, remove });
        }
    }
    // each signed-in Google account's contacts, hourly (into the Address Book, "Google Contacts")
    let accounts: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, lower(address) FROM accounts WHERE kind = 'gmail' AND address IS NOT NULL")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (account, address) in accounts {
        if !signed_in.contains(&address) {
            continue;
        }
        let contacts = contacts_due(&address);
        let calendars = calendars_due(&address, crate::graph_sync::gmail_resting(library).unwrap_or(15));
        // what changed in Gmail itself (a phone, the web site): read back every minute (or the
        // account's own interval)
        let history = history_due(library, &address).then(|| library.setting(&history_key(&address)).ok().flatten().unwrap_or_default());
        if contacts || calendars.is_some() || history.is_some() {
            let w = work.entry(account).or_insert_with(|| GmailWork::new(account, address));
            w.contacts = contacts;
            w.calendars = calendars;
            w.history = history;
        }
    }
    // Google contacts made, changed or deleted in Eudora
    let ops: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, kind, payload FROM pending_ops WHERE kind IN ('card-create', 'card-update', 'card-delete') AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, kind, payload) in ops {
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let card = v["card"].as_i64();
        let row: Option<(i64, Option<String>, Option<String>, String)> = match card {
            Some(c) => conn.query_row("SELECT account, server_id, version, card FROM cards WHERE id = ?1", params![c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?,
            None => None,
        };
        let account = row.as_ref().map(|r| r.0).or_else(|| v["account"].as_i64());
        let Some(account) = account else { continue };
        let Some(address) = address_of(account)? else { continue }; // (not a Google account's: Microsoft's side)
        let change = match (kind.as_str(), row) {
            ("card-delete", _) => match v["server_id"].as_str() {
                Some(resource) => GoogleCardChange::Delete { resource: resource.to_string() },
                None => continue,
            },
            ("card-create", Some((_, _, _, json))) => GoogleCardChange::Create { person: crate::google::person_from_card(&serde_json::from_str(&json)?) },
            ("card-update", Some((_, Some(resource), Some(etag), json))) => {
                GoogleCardChange::Update { resource, etag, person: crate::google::person_from_card(&serde_json::from_str(&json)?) }
            }
            _ => {
                conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // (goes with its creation)
                continue;
            }
        };
        let w = work.entry(account).or_insert_with(|| GmailWork::new(account, address));
        if w.cards.len() < limit {
            w.cards.push(GoogleCard { op, card, change });
        }
    }
    // Google calendar events changed in Eudora (Microsoft's go through `server_changes`)
    let ops: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'event-change' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, payload) in ops {
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (Some(account), Some(server_id)) = (v["account"].as_i64(), v["server_id"].as_str()) else { continue };
        let Some(address) = address_of(account)? else { continue };
        let action = v["action"].as_str().unwrap_or("").to_string();
        // (travel tickets are attached on Microsoft calendars only: Google's API takes files
        // only from Google Drive)
        if action == "attach" {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        }
        if action != "create" && server_id.starts_with("local:") {
            continue; // (goes once the event is made there)
        }
        let w = work.entry(account).or_insert_with(|| GmailWork::new(account, address));
        if w.events.len() < limit {
            w.events.push(GoogleEvent {
                op,
                event: v["event"].as_i64(),
                calendar: v["calendar"].as_str().filter(|c| !c.is_empty()).unwrap_or("primary").to_string(),
                server_id: server_id.to_string(),
                action,
                answer: v["answer"].as_str().map(str::to_string),
                patch: v.get("patch").cloned(),
            });
        }
    }
    // folders made, renamed or deleted in Eudora: labels in Gmail
    for (account, job) in library.folder_jobs(&["gmail"], limit, &|_, _| Some(None))? {
        if let Some(address) = address_of(account)? {
            work.entry(account).or_insert_with(|| GmailWork::new(account, address)).folders.push(job);
        }
    }
    let mut out: Vec<GmailWork> = work.into_values().collect();
    out.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(out)
}

/// Gmail accounts in this library with no Google sign-in on this computer, and how many of their
/// changes wait for one (said once by the download).
pub fn waiting_for_sign_in(library: &Library) -> Result<Vec<(String, u64)>> {
    let signed_in: Vec<String> = crate::google::signed_in_accounts().into_iter().map(|a| a.to_lowercase()).collect();
    let mut stmt = library.conn.prepare("SELECT id, address FROM accounts WHERE kind = 'gmail' AND address IS NOT NULL")?;
    let accounts: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (account, address) in accounts {
        if signed_in.contains(&address.to_lowercase()) {
            continue;
        }
        // (its messages' marks and moves, and its folders' changes)
        let waiting: i64 = library.conn.query_row(
            "SELECT COUNT(*) FROM pending_ops p WHERE p.kind IN ('gmail-mark', 'move', 'folder-change') AND (
               EXISTS (SELECT 1 FROM locations l JOIN folders f ON f.id = l.folder WHERE f.account = ?1 AND l.message = json_extract(p.payload, '$.message'))
               OR EXISTS (SELECT 1 FROM folders f WHERE f.account = ?1 AND f.id IN (json_extract(p.payload, '$.from'), json_extract(p.payload, '$.folder'))))",
            params![account],
            |r| r.get(0),
        )?;
        out.push((address, waiting.max(0) as u64));
    }
    Ok(out)
}

/// Gmail's labels for a message's state: unread = UNREAD; priority set by hand: high = starred,
/// normal or low = not (none: left alone). (add, remove)
fn labels_for(state: i64) -> (Vec<String>, Vec<String>) {
    use crate::marks::{UNREAD, priority_of};
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    if state & UNREAD != 0 { add.push("UNREAD".to_string()) } else { remove.push("UNREAD".to_string()) }
    if let Some(p) = priority_of(state) {
        if p <= 2 { add.push("STARRED".to_string()) } else { remove.push("STARRED".to_string()) }
    }
    (add, remove)
}

/// What a move asks of Gmail: untrash first?, then the labels to add and take off, or to trash.
#[derive(Debug, PartialEq)]
enum Step {
    Trash,
    Labels { untrash: bool, add: Vec<String>, remove: Vec<String> },
}

fn step_for(from: &str, to: &str) -> Step {
    if to == "TRASH" {
        return Step::Trash;
    }
    let untrash = from == "TRASH";
    let mut add = Vec::new();
    let mut remove = Vec::new();
    if to != ALL {
        add.push(to.to_string());
    }
    if from != ALL && from != "TRASH" {
        remove.push(from.to_string());
    }
    // (to Spam it leaves the Inbox too, as Gmail's "Report spam" does)
    if to == "SPAM" && !remove.iter().any(|l| l == "INBOX") {
        remove.push("INBOX".to_string());
    }
    Step::Labels { untrash, add, remove }
}

/// Makes the changes in Gmail (the change thread: no database here). Each account once a batch.
pub fn carry_out(work: &[GmailWork]) -> BatchResult {
    let mut result = BatchResult::default();
    for w in work {
        let mut client = match GoogleClient::resume(&w.address) {
            Ok(c) => c,
            Err(e) => {
                // (a network error is no try: the change waits for the connection)
                let why = if matches!(e, Error::Network(_)) { "canceled".to_string() } else { e.to_string() };
                for m in &w.moves {
                    result.moves.push(MoveResult { op: m.op, message: m.message, to: m.to, folders: Vec::new(), outcome: Outcome::Failed(why.clone()) });
                }
                result.marks.extend(w.marks.iter().map(|m| (m.op, Outcome::Failed(why.clone()))));
                result.folders_done.extend(w.folders.iter().map(|f| crate::folder_changes::FolderDone { op: f.op, ids: Vec::new(), outcome: Err(why.clone()) }));
                result.events_done.extend(w.events.iter().map(|e| (e.op, e.event, Err(why.clone()))));
                continue;
            }
        };
        // folders made, renamed or deleted in Eudora first (moves may go into one just made)
        for job in &w.folders {
            let mut ids = Vec::new();
            let outcome = folder_change(&mut client, job, &mut ids).map_err(|e| e.to_string());
            result.folders_done.push(crate::folder_changes::FolderDone { op: job.op, ids, outcome });
        }
        // the labels there by name (a folder made in Eudora: made in Gmail, or found if it's there)
        let mut by_name: Option<HashMap<String, String>> = None;
        let mut made: HashMap<i64, String> = HashMap::new();
        for m in &w.moves {
            let mut folders = Vec::new();
            let outcome = (|| -> Result<Outcome> {
                let to = match m.to_label.clone().or_else(|| made.get(&m.to).cloned()) {
                    Some(l) => l,
                    None => {
                        if by_name.is_none() {
                            by_name = Some(client.labels()?.into_iter().map(|(id, name, _)| (name.to_lowercase(), id)).collect());
                        }
                        let names = by_name.as_mut().expect("read just now");
                        let id = match names.get(&m.to_path.to_lowercase()) {
                            Some(id) => id.clone(),
                            None => {
                                let id = client.make_label(&m.to_path)?;
                                names.insert(m.to_path.to_lowercase(), id.clone());
                                id
                            }
                        };
                        made.insert(m.to, id.clone());
                        folders.push((m.to, id.clone()));
                        id
                    }
                };
                match step_for(&m.from, &to) {
                    Step::Trash => client.trash(&m.item)?,
                    Step::Labels { untrash, add, remove } => {
                        if untrash {
                            client.untrash(&m.item)?;
                        }
                        let add: Vec<&str> = add.iter().map(String::as_str).collect();
                        let remove: Vec<&str> = remove.iter().map(String::as_str).collect();
                        if !add.is_empty() || !remove.is_empty() {
                            client.modify(&m.item, &add, &remove)?;
                        }
                    }
                }
                // (a Gmail message keeps its id whatever its labels)
                Ok(Outcome::Moved(m.item.clone()))
            })();
            let outcome = match outcome {
                Ok(o) => o,
                Err(e) if gone(&e) => Outcome::Gone,
                Err(e) => Outcome::Failed(e.to_string()),
            };
            result.moves.push(MoveResult { op: m.op, message: m.message, to: m.to, folders, outcome });
        }
        if w.contacts {
            let books = client.contacts().map(|people| vec![(None, "Google Contacts".to_string(), people)]).map_err(|e| e.to_string());
            result.contacts.push(crate::cards::ContactsFetched { account: w.account, books, source: crate::cards::Source::Google });
        }
        if let Some((from, to)) = w.calendars {
            let calendars = client
                .calendars()
                .and_then(|list| list.into_iter().map(|cal| Ok((cal["id"].as_str().unwrap_or("").to_string(), cal))).collect::<Result<Vec<_>>>())
                .and_then(|list| list.into_iter().map(|(id, cal)| Ok((cal, client.events(&id, from, to)?))).collect::<Result<Vec<_>>>())
                .map_err(|e| e.to_string());
            result.calendars.push(crate::calendar::CalendarFetched { account: w.account, from, to, calendars });
        }
        // Google contacts changed here
        for c in &w.cards {
            let done = match &c.change {
                GoogleCardChange::Create { person } => client.create_contact(person).map(Some),
                GoogleCardChange::Update { resource, etag, person } => client.update_contact(resource, etag, person).map(Some),
                GoogleCardChange::Delete { resource } => match client.delete_contact(resource) {
                    Err(e) if gone(&e) => Ok(None), // (already gone there)
                    other => other.map(|_| None),
                },
            };
            let done = done.map_err(|e| if matches!(e, Error::Network(_)) { "canceled".to_string() } else { e.to_string() });
            result.cards_done.push(crate::cards::CardDone { op: c.op, card: c.card, result: done });
        }
        // what changed in Gmail itself since the last look (the first time: counting starts now;
        // Gmail keeps about a week of history: further back, counting starts again from now)
        if let Some(start) = &w.history {
            let fetched = if start.is_empty() {
                client.history_now().map(|id| (id, Vec::new()))
            } else {
                match client.history_since(start) {
                    Err(e) if e.to_string() == "too old" => client.history_now().map(|id| (id, Vec::new())),
                    other => other,
                }
            };
            if let Ok((next, changes)) = fetched {
                result.gmail_history.push(HistoryFetched { account: w.account, address: w.address.clone(), next: Some(next), changes });
            }
        }
        // calendar changes made here (Google sends the people the invitations and updates)
        for e in &w.events {
            let done = event_change(&mut client, e);
            let done = match done {
                Err(err) if gone(&err) && e.action != "create" && e.action != "update" => Ok(None), // (already gone there)
                Err(Error::Network(_)) => Err("canceled".to_string()),
                other => other.map_err(|err| err.to_string()),
            };
            result.events_done.push((e.op, e.event, done));
        }
        for k in &w.marks {
            let add: Vec<&str> = k.add.iter().map(String::as_str).collect();
            let remove: Vec<&str> = k.remove.iter().map(String::as_str).collect();
            let outcome = match client.modify(&k.item, &add, &remove) {
                Ok(()) => Outcome::Moved(String::new()),
                Err(e) if gone(&e) => Outcome::Gone,
                Err(e) => Outcome::Failed(e.to_string()),
            };
            result.marks.push((k.op, outcome));
        }
    }
    result
}

/// An event change made in Google Calendar; a made or changed event comes back as Eudora reads
/// Microsoft's (`server_changes::record_batch` keeps its id and version).
fn event_change(client: &mut GoogleClient, e: &GoogleEvent) -> Result<Option<serde_json::Value>> {
    let empty = serde_json::Value::Null;
    let patch = crate::google::event_from_graph_patch(e.patch.as_ref().unwrap_or(&empty));
    let comment = e.patch.as_ref().and_then(|p| p["comment"].as_str());
    Ok(match e.action.as_str() {
        "create" => Some(crate::google::event_as_graph(&client.event_insert(&e.calendar, &patch)?)),
        "update" => Some(crate::google::event_as_graph(&client.event_patch(&e.calendar, &e.server_id, &patch, true)?)),
        "respond" => {
            let answer = match e.answer.as_deref() {
                Some("accept") => "accept",
                Some("tentativelyAccept" | "tentative") => "tentative",
                _ => "decline",
            };
            client.event_respond(&e.calendar, &e.server_id, answer, comment)?;
            None
        }
        "cancel" => client.event_delete(&e.calendar, &e.server_id, true).map(|_| None)?,
        "delete" => client.event_delete(&e.calendar, &e.server_id, false).map(|_| None)?,
        other => return Err(Error::Other(format!("unknown event change {other}"))),
    })
}

/// A folder made, renamed or deleted in Eudora, done in Gmail: a label made (or found by its
/// name), renamed with the labels under it ("A/B" goes with "A", as Gmail's own rename does), or
/// deleted with its mail moved to Gmail's Trash (Eudora moves a deleted folder's mail to the
/// Trash: Gmail deletes it for good after 30 days). `ids`: (folder, label id) made or renamed.
fn folder_change(client: &mut GoogleClient, job: &crate::folder_changes::FolderJob, ids: &mut Vec<(i64, String)>) -> Result<()> {
    use crate::folder_changes::Action;
    let name: String = job.path.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join("/");
    let labels = client.labels()?;
    let by_id = |id: &str| labels.iter().find(|(l, _, _)| l == id).map(|(_, n, _)| n.clone());
    match (job.action, &job.server_id) {
        (Action::Create, _) | (Action::Rename, None) => {
            let id = match labels.iter().find(|(_, n, _)| n.eq_ignore_ascii_case(&name)) {
                Some((id, _, _)) => id.clone(),
                None => client.make_label(&name)?,
            };
            ids.push((job.folder, id));
        }
        (Action::Rename, Some(id)) => {
            let Some(old) = by_id(id) else {
                // (gone in Gmail: made again under its new name)
                let made = client.make_label(&name)?;
                ids.push((job.folder, made));
                return Ok(());
            };
            client.rename_label(id, &name)?;
            let under = format!("{old}/");
            for (child, child_name, kind) in &labels {
                if kind == "user" && child_name.starts_with(&under) {
                    client.rename_label(child, &format!("{name}/{}", &child_name[under.len()..]))?;
                }
            }
            ids.push((job.folder, id.clone()));
        }
        (Action::Delete, Some(id)) => {
            let Some(old) = by_id(id) else { return Ok(()) }; // (already gone there)
            let under = format!("{old}/");
            let mut doomed: Vec<String> = labels.iter().filter(|(_, n, k)| k == "user" && n.starts_with(&under)).map(|(l, _, _)| l.clone()).collect();
            doomed.push(id.clone());
            for label in &doomed {
                let messages = client.message_ids(label, &|_| {})?;
                client.batch_modify(&messages, &["TRASH"], &[label.as_str()])?;
                client.delete_label(label)?;
            }
        }
        (Action::Delete, None) => {} // (never made there)
    }
    Ok(())
}

/// Brings in what changed in Gmail itself (`HistoryFetched`): read / unread, stars, labels added
/// and taken off (a message's folders here), messages deleted there for good (to the account's
/// Trash here: nothing goes silently). A message with a change of the user's still on its way to
/// Gmail keeps it. Returns how many changes were made here.
pub fn record_history(library: &Library, h: &HistoryFetched) -> Result<u64> {
    use crate::marks::{UNREAD, priority_of, with_priority};
    let conn = &library.conn;
    if let Some(next) = &h.next {
        library.set_setting(&history_key(&h.address), next)?;
    }
    // mail kept local (`local_only`): what changed in Gmail stays there
    if h.changes.is_empty() || library.local_only_account(h.account, crate::local_only::Service::Mail)? {
        return Ok(0);
    }
    // the account's folders by label id, and its All Mail and Trash
    let folders: HashMap<String, i64> = {
        let mut stmt = conn.prepare("SELECT server_id, id FROM folders WHERE account = ?1 AND server_id IS NOT NULL")?;
        stmt.query_map(params![h.account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let all_mail = folders.get(ALL).copied();
    let trash = folders.get("TRASH").copied();
    let ids_here: Vec<i64> = folders.values().copied().collect();
    if ids_here.is_empty() {
        return Ok(0); // (its folders not downloaded yet)
    }
    // messages with a change of the user's waiting: theirs wins
    let waiting: std::collections::HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT json_extract(payload, '$.message') FROM pending_ops WHERE kind IN ('gmail-mark', 'move')")?;
        stmt.query_map([], |r| r.get::<_, Option<i64>>(0))?.filter_map(|r| r.ok().flatten()).collect()
    };
    let mut changed = 0u64;
    library.in_transaction(|| {
        for c in &h.changes {
            // the message here: by its Gmail id in one of the account's folders
            let message: Option<i64> = {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT message FROM server_items WHERE server_id = ?1 AND folder IN ({}) LIMIT 1",
                    ids_here.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
                ))?;
                stmt.query_row(params![c.id], |r| r.get(0)).optional()?
            };
            let Some(message) = message else { continue }; // (not downloaded yet: the download brings it)
            if waiting.contains(&message) {
                continue;
            }
            let date: i64 = conn.query_row("SELECT date FROM messages WHERE id = ?1", params![message], |r| r.get(0))?;
            let place = |folder: i64| -> Result<()> {
                conn.execute("INSERT OR IGNORE INTO locations (message, folder, date, server_id) VALUES (?1, ?2, ?3, ?4)", params![message, folder, date, c.id])?;
                conn.execute("INSERT OR IGNORE INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)", params![folder, c.id, message])?;
                Ok(())
            };
            let unplace = |folder: i64| -> Result<usize> {
                conn.execute("DELETE FROM server_items WHERE folder = ?1 AND server_id = ?2", params![folder, c.id])?;
                Ok(conn.execute("DELETE FROM locations WHERE message = ?1 AND folder = ?2", params![message, folder])?)
            };
            let in_account = || -> Result<i64> {
                Ok(conn.query_row(
                    &format!("SELECT COUNT(*) FROM locations WHERE message = ?1 AND folder IN ({})", ids_here.iter().map(i64::to_string).collect::<Vec<_>>().join(",")),
                    params![message],
                    |r| r.get(0),
                )?)
            };
            if c.deleted {
                // deleted there for good: to the Trash here
                for f in &ids_here {
                    if Some(*f) != trash {
                        unplace(*f)?;
                    }
                }
                if let Some(t) = trash {
                    place(t)?;
                }
                changed += 1;
                continue;
            }
            let flags: i64 = conn.query_row("SELECT flags FROM messages WHERE id = ?1", params![message], |r| r.get(0))?;
            let mut new_flags = flags;
            for l in &c.added {
                match l.as_str() {
                    "UNREAD" => new_flags |= UNREAD,
                    "STARRED" if priority_of(new_flags).is_none_or(|p| p > 2) => new_flags = with_priority(new_flags, Some(2)),
                    _ => {
                        if let Some(&f) = folders.get(l) {
                            place(f)?;
                            // (a label now: no longer only in All Mail)
                            if let Some(a) = all_mail.filter(|a| *a != f) {
                                unplace(a)?;
                            }
                            changed += 1;
                        }
                    }
                }
            }
            for l in &c.removed {
                match l.as_str() {
                    "UNREAD" => new_flags &= !UNREAD,
                    "STARRED" if priority_of(new_flags).is_some_and(|p| p <= 2) => new_flags = with_priority(new_flags, None),
                    _ => {
                        if let Some(&f) = folders.get(l) {
                            if unplace(f)? > 0 {
                                changed += 1;
                            }
                            // no label left: archived (All Mail)
                            if in_account()? == 0 {
                                if let Some(a) = all_mail {
                                    place(a)?;
                                }
                            }
                        }
                    }
                }
            }
            if new_flags != flags {
                conn.execute("UPDATE messages SET flags = ?2 WHERE id = ?1", params![message, new_flags])?;
                changed += 1;
            }
        }
        Ok(())
    })?;
    Ok(changed)
}

/// Gmail no longer has the message (deleted there): nothing to change.
fn gone(e: &Error) -> bool {
    let s = e.to_string();
    s.contains("Google 404")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_in_eudora_is_the_right_change_of_labels_in_gmail() {
        let labels = |untrash: bool, add: &[&str], remove: &[&str]| Step::Labels { untrash, add: add.iter().map(|s| s.to_string()).collect(), remove: remove.iter().map(|s| s.to_string()).collect() };
        // Inbox → a label of one's own: the label on, Inbox off
        assert_eq!(step_for("INBOX", "Label_7"), labels(false, &["Label_7"], &["INBOX"]));
        // to Trash: Gmail's trash
        assert_eq!(step_for("INBOX", "TRASH"), Step::Trash);
        // back out of the Trash: untrashed, then into its folder
        assert_eq!(step_for("TRASH", "INBOX"), labels(true, &["INBOX"], &[]));
        // to All Mail: only the old label off (Gmail's Archive)
        assert_eq!(step_for("INBOX", ALL), labels(false, &[], &["INBOX"]));
        // from All Mail: only the new label on
        assert_eq!(step_for(ALL, "Label_3"), labels(false, &["Label_3"], &[]));
        // to Spam from another label: out of the Inbox too, as Gmail's Report spam
        assert_eq!(step_for("Label_3", "SPAM"), labels(false, &["SPAM"], &["Label_3", "INBOX"]));
    }

    #[test]
    fn changes_made_in_gmail_itself_come_in() {
        use crate::library::Access;
        use crate::progress::Silent;
        use crate::google::GmailChange;
        let dir = tempfile::tempdir().unwrap();
        let mut l = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("m.eml");
        std::fs::write(&src, "From: a@b.example\r\nSubject: s\r\nMessage-ID: <1@x>\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\nhi\r\n").unwrap();
        crate::import(&mut l, &[src], &crate::ImportOptions::default(), &Silent).unwrap();
        let m: i64 = l.conn.query_row("SELECT id FROM messages", [], |r| r.get(0)).unwrap();
        l.conn.execute("DELETE FROM locations", []).unwrap();
        let account = crate::db::graph_account(&l.conn, "gmail", "me@gmail.com", "", 0).unwrap();
        let folder = |path: &str, label: &str| {
            let f = crate::db::ensure_folder(&l.conn, account, path).unwrap();
            l.conn.execute("UPDATE folders SET server_id = ?2 WHERE id = ?1", params![f, label]).unwrap();
            f
        };
        let (inbox, work, all, trash) = (folder("Inbox", "INBOX"), folder("Work", "Label_1"), folder("All Mail", ALL), folder("Trash", "TRASH"));
        l.conn.execute("INSERT INTO locations (message, folder, date, server_id) VALUES (?1, ?2, 1, 'g1')", params![m, inbox]).unwrap();
        l.conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, 'g1', ?2)", params![inbox, m]).unwrap();
        l.conn.execute("UPDATE messages SET flags = 1 WHERE id = ?1", [m]).unwrap();
        let folders = |l: &Library| -> Vec<i64> { l.conn.prepare("SELECT folder FROM locations WHERE message = ?1 ORDER BY folder").unwrap().query_map([m], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect() };
        let apply = |l: &Library, c: GmailChange| record_history(l, &HistoryFetched { account, address: "me@gmail.com".into(), next: Some("9".into()), changes: vec![c] }).unwrap();
        let change = |added: &[&str], removed: &[&str], deleted: bool| GmailChange { id: "g1".into(), added: added.iter().map(|s| s.to_string()).collect(), removed: removed.iter().map(|s| s.to_string()).collect(), deleted };
        // read on the phone, starred, moved Inbox → Work in Gmail
        apply(&l, change(&["Label_1", "STARRED"], &["INBOX", "UNREAD"], false));
        assert_eq!(folders(&l), vec![work]);
        let flags: i64 = l.conn.query_row("SELECT flags FROM messages WHERE id = ?1", [m], |r| r.get(0)).unwrap();
        assert_eq!(flags & crate::marks::UNREAD, 0);
        assert_eq!(crate::marks::priority_of(flags), Some(2));
        // its last label taken off: archived (All Mail)
        apply(&l, change(&[], &["Label_1"], false));
        assert_eq!(folders(&l), vec![all]);
        // deleted there for good: to the Trash here, never dropped silently
        apply(&l, change(&[], &[], true));
        assert_eq!(folders(&l), vec![trash]);
        assert_eq!(l.setting(&history_key("me@gmail.com")).unwrap().as_deref(), Some("9"));
        // a change of the user's waiting for Gmail: theirs wins
        l.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (0, 'gmail-mark', ?1)", [format!("{{\"message\":{m}}}")]).unwrap();
        assert_eq!(apply(&l, change(&["INBOX"], &[], false)), 0);
        assert_eq!(folders(&l), vec![trash]);
    }

    #[test]
    fn read_state_and_priority_are_gmails_labels() {
        use crate::marks::UNREAD;
        assert_eq!(labels_for(UNREAD), (vec!["UNREAD".to_string()], vec![]));
        assert_eq!(labels_for(0), (vec![], vec!["UNREAD".to_string()]));
    }

    /// Gmail's work is remembered on disk: what was just done isn't done again (a restart of the
    /// program used to read every calendar since 2000 and every contact again at once).
    #[test]
    fn gmail_work_once_done_waits_its_turn() {
        let a = format!("test-{}@gmail.com", std::process::id());
        assert_eq!(calendars_due(&a, 15).map(|r| r.0), Some(EVERYTHING_FROM), "everything the first time");
        assert_eq!(calendars_due(&a, 15), None, "then not again within 15 minutes");
        assert!(contacts_due(&a));
        assert!(!contacts_due(&a));
        // (the memory of it is the file: what another program, or this one restarted, reads)
        assert!(crate::google::schedule_get(&format!("contacts:{a}")).is_some());
    }
}
