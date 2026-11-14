//! Changes made in Eudora carried out on the server: messages moved (Transfer, Delete, the
//! rules, the spam filter) in a Microsoft 365 mailbox, and the folders they go to made there
//! when the server doesn't have them yet ("Bulk/Marketing/Travelzoo").
//!
//! `Library::move_messages` queues each move (`pending_ops` kind "move"). The download, the
//! library's only writer, takes a batch now and then (`plan`), a thread of its own does the
//! requests (`carry_out`: no database there), and the writer records what happened (`record`):
//! the message's new server id in its new folder (so the download doesn't fetch it again), the
//! folders' server ids, or why a move failed (tried again later; given up after `GIVE_UP` tries).

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::error::{Error, Result};
use crate::graph::GraphClient;
use crate::library::{Library, in_online_archive};

/// Changes taken per round: moves (and contacts, events, deletes) this many, marks `MARKS_PER`
/// times as many (sent 20 to a request: Graph's JSON batching).
pub const BATCH: usize = 500;
pub const MARKS_PER: usize = 2;
/// Archive moves a round (each an export uploaded, then a delete: about 3 s each per worker,
/// 4 workers): kept to about a minute, as a folder opened waits for the round to end.
pub const ARCHIVE_BATCH: usize = 80;
/// Tries before a move is given up (kept in the list with its error, never retried by itself).
pub const GIVE_UP: i64 = 10;

/// One folder on the way to the target, top first: its row and, when known, its server id.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderStep {
    pub folder: i64,
    pub name: String,
    pub server_id: Option<String>,
}

/// A queued move, ready for the server.
#[derive(Clone, Debug)]
pub struct MoveJob {
    pub op: i64,
    pub message: i64,
    /// A shared mailbox's address; None = the signed-in user's own mailbox.
    pub mailbox: Option<String>,
    /// The message's id on the server, in the folder it's in there now.
    pub item: String,
    pub to: i64,
    /// The target folder's path, top first (only the last step may be the target itself).
    pub path: Vec<FolderStep>,
}

#[derive(Debug)]
pub enum Outcome {
    /// Moved: its id in the new folder.
    Moved(String),
    /// The server no longer has it where it was (moved or deleted there, e.g. in Outlook).
    Gone,
    /// Its id belongs to another mailbox (the Online Archive's): it stays where it is on the
    /// server, filed in Eudora only (the archive is never changed).
    Elsewhere,
    Failed(String),
}

#[derive(Debug)]
pub struct MoveResult {
    pub op: i64,
    pub message: i64,
    pub to: i64,
    /// Folders made (or found) on the server on the way: (folder, server id).
    pub folders: Vec<(i64, String)>,
    pub outcome: Outcome,
}

/// What a batch did, for the download's log.
#[derive(Debug, Default, PartialEq)]
pub struct Recorded {
    pub moved: u64,
    pub gone: u64,
    /// Archive mail filed in Eudora only (its id is the Online Archive's).
    pub kept_here: u64,
    pub failed: u64,
    /// Folders made on the server (their paths).
    pub folders_made: Vec<String>,
    /// The last error, if any.
    pub error: Option<String>,
}

/// Up to `limit` queued moves of Microsoft 365 mailboxes that `account` (the signed-in user's
/// address) may change: their own mailbox and the shared ones. Moves given up, and moves the
/// writer is still waiting on (`busy`), are left out. Moves Eudora must not make (into or out
/// of the Online Archive, queued by an older version) are dropped.
pub fn plan(library: &Library, account: &str, limit: usize, busy: &[i64]) -> Result<Vec<MoveJob>> {
    let conn = &library.conn;
    let mut stmt = conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'move' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
    let ops: Vec<(i64, String)> = stmt.query_map(params![GIVE_UP, (limit + busy.len()) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut jobs = Vec::new();
    for (op, payload) in ops {
        if busy.contains(&op) || jobs.len() >= limit {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (Some(message), Some(item), Some(from), Some(to)) = (v["message"].as_i64(), v["server_id"].as_str(), v["from"].as_i64(), v["to"].as_i64()) else {
            conn.execute("UPDATE pending_ops SET attempts = ?2, last_error = 'unreadable entry' WHERE id = ?1", params![op, GIVE_UP])?;
            continue;
        };
        let folder = |id: i64| -> Result<Option<(String, String, Option<String>)>> {
            Ok(conn
                .query_row(
                    "SELECT f.path, a.kind, a.address FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?)
        };
        let (Some((_, kind, address)), Some((to_path, _, _))) = (folder(from)?, folder(to)?) else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // a folder that's gone
            continue;
        };
        // (from an archive-only folder is fine: a mailbox message only filed there)
        if in_online_archive(&to_path) {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        }
        let mailbox = match kind.as_str() {
            "graph" if address.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(account)) => None,
            "graph-shared" => address,
            _ => continue, // another Microsoft account: waits for its own sign-in
        };
        let account_id: i64 = conn.query_row("SELECT account FROM folders WHERE id = ?1", params![to], |r| r.get(0))?;
        let mut path = Vec::new();
        let mut so_far = String::new();
        for name in to_path.split('/') {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(name);
            let (folder, server_id): (i64, Option<String>) = conn.query_row(
                "SELECT id, server_id FROM folders WHERE account = ?1 AND path = ?2",
                params![account_id, so_far],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            path.push(FolderStep { folder, name: name.to_string(), server_id });
        }
        jobs.push(MoveJob { op, message, mailbox, item: item.to_string(), to, path });
    }
    Ok(jobs)
}

/// Does the moves on the server (no database here: the download's own thread). Folders made
/// on the way are remembered across the batch, so a hundred messages into a new folder make it
/// once.
pub fn carry_out(client: &mut GraphClient, jobs: &[MoveJob]) -> Vec<MoveResult> {
    let mut made: HashMap<i64, String> = HashMap::new();
    // first each one's target folder (made on the server when missing, one at a time), then the
    // moves themselves 20 to a request, per mailbox
    let mut targets: Vec<(Vec<(i64, String)>, Result<String>)> = Vec::with_capacity(jobs.len());
    for job in jobs {
        client.set_mailbox(job.mailbox.clone());
        let mut folders = Vec::new();
        let target = (|| -> Result<String> {
            let mut parent: Option<String> = None;
            for step in &job.path {
                let id = match step.server_id.clone().or_else(|| made.get(&step.folder).cloned()) {
                    Some(id) => id,
                    None => {
                        let id = client.ensure_folder(parent.as_deref(), &step.name)?;
                        made.insert(step.folder, id.clone());
                        folders.push((step.folder, id.clone()));
                        id
                    }
                };
                parent = Some(id);
            }
            parent.ok_or_else(|| Error::Other("no folder to move to".into()))
        })();
        let canceled = matches!(&target, Err(e) if e.to_string() == "canceled");
        targets.push((folders, target));
        if canceled {
            break;
        }
    }
    let mut outcomes: Vec<Option<Outcome>> = targets.iter().map(|(_, t)| t.as_ref().err().map(|e| Outcome::Failed(e.to_string()))).collect();
    let mut mailboxes: Vec<Option<String>> = Vec::new();
    for (i, job) in jobs.iter().enumerate().take(targets.len()) {
        if outcomes[i].is_none() && !mailboxes.contains(&job.mailbox) {
            mailboxes.push(job.mailbox.clone());
        }
    }
    for mailbox in mailboxes {
        client.set_mailbox(mailbox.clone());
        let mine: Vec<usize> = (0..targets.len()).filter(|&i| outcomes[i].is_none() && jobs[i].mailbox == mailbox).collect();
        let items: Vec<_> = mine.iter().map(|&i| client.move_item(&jobs[i].item, targets[i].1.as_ref().expect("checked"))).collect();
        match client.batch(&items) {
            Err(e) => {
                for &i in &mine {
                    outcomes[i] = Some(Outcome::Failed(e.to_string()));
                }
            }
            Ok(answers) => {
                for (&i, (status, body)) in mine.iter().zip(answers) {
                    let error = crate::graph::batch_error(status, &body);
                    outcomes[i] = Some(match status {
                        200..=299 => match body["id"].as_str().filter(|s| !s.is_empty()) {
                            Some(id) => Outcome::Moved(id.to_string()),
                            None => Outcome::Failed("Microsoft 365 moved it but gave no new id".into()),
                        },
                        // an id of another mailbox (the Online Archive's): filed here only
                        404 if error.contains("doesn't belong to the targeted mailbox") => Outcome::Elsewhere,
                        // "not found" may be the message or the folder: gone only if the message is
                        404 => match client.message_exists(&jobs[i].item) {
                            Ok(false) => Outcome::Gone,
                            _ => Outcome::Failed(error),
                        },
                        _ => Outcome::Failed(error),
                    });
                }
            }
        }
    }
    client.set_mailbox(None);
    targets
        .into_iter()
        .zip(outcomes)
        .zip(jobs)
        .map(|(((folders, _), outcome), job)| MoveResult { op: job.op, message: job.message, to: job.to, folders, outcome: outcome.unwrap_or(Outcome::Failed("canceled".into())) })
        .collect()
}

/// Records a batch's results (the writer's side).
pub fn record(library: &Library, results: &[MoveResult]) -> Result<Recorded> {
    let conn = &library.conn;
    let mut done = Recorded::default();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<()> {
        for r in results {
            for (folder, server_id) in &r.folders {
                let changed = conn.execute("UPDATE folders SET server_id = ?2 WHERE id = ?1 AND server_id IS NULL", params![folder, server_id])?;
                if changed > 0 {
                    done.folders_made.push(conn.query_row("SELECT path FROM folders WHERE id = ?1", params![folder], |row| row.get(0))?);
                }
            }
            // where it was on the server: that record goes once it has left (else the folder
            // looks complete with the record of a message no longer there, and new mail in it
            // was never listed: 2026-10-17)
            if matches!(r.outcome, Outcome::Moved(_) | Outcome::Gone) {
                let was: Option<String> = conn.query_row("SELECT payload FROM pending_ops WHERE id = ?1", params![r.op], |row| row.get(0)).optional()?;
                if let Some(v) = was.and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok()) {
                    if let (Some(from), Some(item)) = (v["from"].as_i64(), v["server_id"].as_str()) {
                        conn.execute("DELETE FROM server_items WHERE folder = ?1 AND server_id = ?2 AND message = ?3", params![from, item, r.message])?;
                    }
                }
            }
            match &r.outcome {
                Outcome::Moved(id) => {
                    conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![r.op])?;
                    // (an IMAP server may not say its new id: the download finds it there and
                    // sees it's the same message)
                    if !id.is_empty() {
                        conn.execute("UPDATE locations SET server_id = ?3 WHERE message = ?1 AND folder = ?2", params![r.message, r.to, id])?;
                        // the download finds it in its new folder: already here
                        conn.execute("INSERT OR IGNORE INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)", params![r.to, id, r.message])?;
                    }
                    done.moved += 1;
                    library.log_job("server", Some(r.message), &format!("Moved on the server to {}", library.folder_path_of(r.to)))?;
                }
                Outcome::Gone => {
                    conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![r.op])?;
                    done.gone += 1;
                    library.log_job("server", Some(r.message), "Not moved: no longer in that folder on the server (moved or deleted elsewhere)")?;
                }
                Outcome::Elsewhere => {
                    conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![r.op])?;
                    done.kept_here += 1;
                    library.log_job("server", Some(r.message), &format!("Filed in {} here only (it stays where it is on the server)", library.folder_path_of(r.to)))?;
                }
                Outcome::Failed(e) if e == "canceled" => {} // the download stopped meanwhile: not a try
                Outcome::Failed(e) => {
                    library.log_job("server", Some(r.message), &format!("Move to {} failed, tried again later: {e}", library.folder_path_of(r.to)))?;
                    conn.execute("UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![r.op, e])?;
                    done.failed += 1;
                    done.error = Some(e.clone());
                }
            }
        }
        Ok(())
    })();
    conn.execute_batch(if outcome.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    outcome.map(|_| done)
}

/// Takes a message out of a folder it was moved out of in Eudora when the download put it back
/// there because the server hadn't made the move yet (its move still waits; it's also in the
/// folder it was moved to). Returns how many.
pub fn drop_ghost_locations(library: &Library) -> Result<u64> {
    let conn = &library.conn;
    let ghosts: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT l.id, l.message FROM locations l JOIN pending_ops p ON p.kind = 'move' AND json_extract(p.payload, '$.message') = l.message
               AND json_extract(p.payload, '$.from') = l.folder
             WHERE EXISTS (SELECT 1 FROM locations l2 WHERE l2.message = l.message AND l2.folder = json_extract(p.payload, '$.to'))",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (location, message) in &ghosts {
        conn.execute("DELETE FROM locations WHERE id = ?1", params![location])?;
        conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
    }
    Ok(ghosts.len() as u64)
}

/// Every change waiting for a server, one line each: its kind, the account, what it does, how
/// many tries and the last error (for finding out why something hasn't reached the server).
pub fn waiting_list(library: &Library) -> Result<Vec<String>> {
    let conn = &library.conn;
    let mut stmt = conn.prepare("SELECT id, kind, payload, attempts, COALESCE(last_error, '') FROM pending_ops WHERE kind IN ('move', 'mark', 'imap-mark', 'delete') ORDER BY id")?;
    let rows: Vec<(i64, String, String, i64, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
    let folder = |id: Option<i64>| -> String {
        id.and_then(|id| {
            conn.query_row("SELECT a.kind || ' ' || COALESCE(a.address, '?') || ' › ' || f.path FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1", params![id], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        })
        .unwrap_or_else(|| "(gone)".into())
    };
    Ok(rows
        .into_iter()
        .map(|(op, kind, payload, attempts, error)| {
            let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            let what = match kind.as_str() {
                "move" => format!("message {} from {} to {} (server id {})", v["message"], folder(v["from"].as_i64()), folder(v["to"].as_i64()), if v["server_id"].as_str().is_some_and(|s| !s.is_empty()) { "known" } else { "missing" }),
                _ => format!("message {} {}", v["message"], payload.chars().take(120).collect::<String>()),
            };
            format!("#{op} {kind}: {what}; tries {attempts}{}", if error.is_empty() { String::new() } else { format!("; last error: {error}") })
        })
        .collect())
}

/// The queued moves: (waiting, given up), and the last error of one given up.
pub fn waiting(library: &Library) -> Result<(u64, u64, Option<String>)> {
    let (waiting, given_up): (i64, i64) = library.conn.query_row(
        "SELECT COALESCE(SUM(attempts < ?1), 0), COALESCE(SUM(attempts >= ?1), 0) FROM pending_ops WHERE kind = 'move'",
        params![GIVE_UP],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let last = library
        .conn
        .query_row("SELECT last_error FROM pending_ops WHERE kind = 'move' AND last_error IS NOT NULL ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok((waiting as u64, given_up as u64, last))
}

/// Tries the moves given up once more (after the user fixed what stopped them).
pub fn retry_given_up(library: &Library) -> Result<u64> {
    Ok(library.conn.execute("UPDATE pending_ops SET attempts = 0 WHERE kind IN ('move', 'mark', 'delete', 'imap-mark') AND attempts >= ?1", params![GIVE_UP])? as u64)
}

/// Drops the read / label / priority changes waiting for one Microsoft 365 mailbox (`address`):
/// they stay in Eudora and the server is left as it is. A message also in another Microsoft 365
/// mailbox keeps its change. Returns how many were dropped.
pub fn drop_marks(library: &Library, address: &str) -> Result<u64> {
    Ok(library.conn.execute(
        "DELETE FROM pending_ops WHERE kind = 'mark'
           AND CAST(json_extract(payload, '$.message') AS INTEGER) IN (
             SELECT l.message FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account WHERE lower(a.address) = lower(?1))
           AND NOT EXISTS (
             SELECT 1 FROM locations l2 JOIN folders f2 ON f2.id = l2.folder JOIN accounts a2 ON a2.id = f2.account
             WHERE l2.message = CAST(json_extract(pending_ops.payload, '$.message') AS INTEGER) AND a2.kind LIKE 'graph%' AND lower(a2.address) <> lower(?1))",
        params![address],
    )? as u64)
}

// ── Status, labels and priority (`marks`), deletes (Empty Trash), and what changed there ──

/// A message's read state, labels and priority, for each place a Microsoft 365 mailbox has it.
#[derive(Clone, Debug)]
pub struct MarkJob {
    pub op: i64,
    pub message: i64,
    /// (shared mailbox or None = the user's own, its id there)
    pub targets: Vec<(Option<String>, String)>,
    /// What Exchange is told: isRead, categories, importance.
    pub patch: serde_json::Value,
}

/// A message deleted on the server (Empty Trash).
#[derive(Clone, Debug)]
pub struct DeleteJob {
    pub op: i64,
    pub mailbox: Option<String>,
    pub item: String,
}

/// Reading back what changed on the server: which messages are unread there (the Inbox's,
/// or the whole mailbox's) and, for the whole mailbox, which carry each label.
#[derive(Clone, Debug)]
pub struct ReconcileJob {
    pub account: i64,
    pub mailbox: Option<String>,
    pub inbox_only: bool,
    pub labels: Vec<String>,
}

#[derive(Debug)]
pub struct ReconcileResult {
    pub job: ReconcileJob,
    /// When the server was asked: changes made there since are newer than this answer.
    pub asked: std::time::Instant,
    /// The unread messages' ids (an error: nothing is changed).
    pub unread: std::result::Result<Vec<String>, String>,
    /// For each label asked about, the ids carrying it (labels whose lookup failed are left out).
    pub labels: Vec<(String, Vec<String>)>,
    /// The newest messages answered in Outlook, on a phone, anywhere: (id, Exchange's last verb:
    /// 102 replied, 103 replied to all, 104 forwarded). Empty when it couldn't be asked.
    pub answered: Vec<(String, i64)>,
}

/// Everything the download's change thread does in one go.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub moves: Vec<MoveJob>,
    pub marks: Vec<MarkJob>,
    pub deletes: Vec<DeleteJob>,
    pub reconcile: Vec<ReconcileJob>,
    /// Labels to put in Outlook's category list (name, color) for each mailbox: once a run.
    pub categories: Vec<(Option<String>, Vec<(String, String)>)>,
    /// Address books to read (about once an hour per account: `graph_sync`).
    pub contacts: Vec<crate::cards::ContactsJob>,
    /// Folders just opened in the window: their newest messages fetched now.
    pub looks: Vec<LookJob>,
    /// Contact edits, new contacts and deletions going to the server.
    pub cards: Vec<crate::cards::CardJob>,
    /// Calendars to read (the near weeks every 15 minutes, everything daily: `graph_sync`).
    pub calendars: Vec<crate::calendar::CalendarJob>,
    /// Event answers, changes and deletions going to the server.
    pub events: Vec<EventJob>,
    /// Microsoft To Do read back (the primary account's), and task changes going there.
    pub todo_fetch: Option<i64>,
    pub todos: Vec<crate::todo::TodoJob>,
    /// Folders made, renamed or deleted in Eudora (done first: moves may go into them).
    pub folders: Vec<crate::folder_changes::FolderJob>,
    /// Online Archive mail filed elsewhere, moved inside the archive on the server.
    pub archive_moves: Vec<crate::archive_moves::ArchiveMoveJob>,
}

/// A folder opened in the window, brought up to date first: its newest messages not here.
#[derive(Clone, Debug)]
pub struct LookJob {
    pub folder: i64,
    pub path: String,
    pub mailbox: Option<String>,
    pub server_folder: String,
    pub known: std::collections::HashSet<String>,
}

/// The messages a look fetched: (server id, MIME, received date), or why not.
#[derive(Debug)]
pub struct Looked {
    pub folder: i64,
    pub path: String,
    pub items: std::result::Result<Vec<(String, Vec<u8>, Option<String>)>, String>,
    /// The server ids among them that are unread there.
    pub unread: Vec<String>,
}

/// The contact changes waiting for this sign-in's mailboxes (edits, new contacts, deletions),
/// oldest first. Those of another Microsoft account wait for its pass.
pub fn card_jobs(library: &Library, signed_in: &str, limit: usize) -> Result<Vec<crate::cards::CardJob>> {
    use crate::cards::{CardChange, CardJob};
    let conn = &library.conn;
    let ops: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, kind, payload FROM pending_ops WHERE kind IN ('card-create', 'card-update', 'card-delete') AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut jobs = Vec::new();
    for (op, kind, payload) in ops {
        if jobs.len() >= limit {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (account, card_row): (Option<i64>, Option<(i64, String, Option<String>, String)>) = match v["card"].as_i64() {
            Some(card) => {
                let row: Option<(i64, String, Option<String>, String)> = conn
                    .query_row("SELECT account, book, server_id, card FROM cards WHERE id = ?1", params![card], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .optional()?;
                (row.as_ref().map(|r| r.0), row)
            }
            None => (v["account"].as_i64(), None),
        };
        let Some(account) = account else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // its card is gone
            continue;
        };
        let (akind, address): (String, Option<String>) = conn.query_row("SELECT kind, address FROM accounts WHERE id = ?1", params![account], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let Ok(mailbox) = mailbox_of(&akind, address, signed_in) else { continue };
        let card = v["card"].as_i64();
        let what = match (kind.as_str(), card_row) {
            ("card-delete", _) => match v["server_id"].as_str() {
                Some(id) => CardChange::Delete { server_id: id.to_string() },
                None => continue,
            },
            ("card-create", Some((_, book, _, json))) => {
                let c: crate::cards::Card = serde_json::from_str(&json)?;
                CardChange::Create { book: (!book.is_empty()).then_some(book), contact: crate::cards::to_graph(&c) }
            }
            ("card-update", Some((_, _, Some(server_id), json))) => {
                let c: crate::cards::Card = serde_json::from_str(&json)?;
                CardChange::Update { server_id, contact: crate::cards::to_graph(&c) }
            }
            // (a change to a card not on the server yet: it goes with its creation)
            _ => {
                conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                continue;
            }
        };
        jobs.push(CardJob { op, card, mailbox, what });
    }
    Ok(jobs)
}

/// An event change going to Microsoft (an answer, a change, a cancellation or a deletion).
#[derive(Clone, Debug)]
pub struct EventJob {
    pub op: i64,
    pub event: Option<i64>,
    pub mailbox: Option<String>,
    pub server_id: String,
    /// respond, update, cancel, delete
    pub action: String,
    pub answer: Option<String>,
    pub patch: Option<serde_json::Value>,
    /// The calendar a new event goes in (its server id).
    pub calendar: Option<String>,
}

/// A travel event's tickets and text: each file attached unless the event has one by that name
/// already (a retry), pictures on the web fetched now; then the event's text, when it has one to
/// set.
fn attach_tickets(client: &mut GraphClient, job: &EventJob) -> Result<()> {
    use base64::Engine;
    let empty = serde_json::Value::Null;
    let p = job.patch.as_ref().unwrap_or(&empty);
    let have: Vec<String> = client.event_attachment_names(&job.server_id).unwrap_or_default();
    for f in p["files"].as_array().into_iter().flatten() {
        let name = f["name"].as_str().unwrap_or("ticket");
        if have.iter().any(|h| h == name) {
            continue;
        }
        let (content_type, bytes) = match (f["contentBytes"].as_str(), f["url"].as_str()) {
            (Some(b), _) => (f["contentType"].as_str().unwrap_or("application/octet-stream").to_string(), b.to_string()),
            (None, Some(url)) => {
                // (a data: URL back: "data:image/png;base64,…")
                let Some(data) = crate::remote_images::fetch(&[url.to_string()], None).into_iter().next().and_then(|p| p.data) else { continue };
                let Some((head, b64)) = data.split_once(";base64,") else { continue };
                let len = base64::engine::general_purpose::STANDARD.decode(b64).map(|b| b.len()).unwrap_or(usize::MAX);
                if !(crate::travel::TICKET_MIN..=TICKET_MAX).contains(&len) {
                    continue;
                }
                (head.trim_start_matches("data:").to_string(), b64.to_string())
            }
            _ => continue,
        };
        client.add_event_attachment(&job.server_id, name, &content_type, &bytes)?;
    }
    if let Some(body) = p.get("body").filter(|b| !b.is_null()) {
        client.update_event(&job.server_id, &serde_json::json!({ "body": body }))?;
    }
    Ok(())
}

/// The largest ticket attached to an event (Graph takes up to 3 MB in one request).
pub const TICKET_MAX: usize = 2_900_000;

/// The event changes waiting for this sign-in's mailbox, oldest first.
pub fn event_jobs(library: &Library, signed_in: &str, limit: usize) -> Result<Vec<EventJob>> {
    let conn = &library.conn;
    let ops: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'event-change' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut jobs = Vec::new();
    for (op, payload) in ops {
        if jobs.len() >= limit {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (Some(account), Some(server_id)) = (v["account"].as_i64(), v["server_id"].as_str()) else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let found: Option<(String, Option<String>)> = conn.query_row("SELECT kind, address FROM accounts WHERE id = ?1", params![account], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((kind, address)) = found else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let Ok(mailbox) = mailbox_of(&kind, address, signed_in) else { continue };
        // a travel event's tickets: once the event is on the server (its id now, not the one
        // it had when queued), with the files read from the email here
        let mut server_id = server_id.to_string();
        let mut v = v;
        if v["action"].as_str() == Some("attach") {
            let now_id: Option<String> = match v["event"].as_i64() {
                Some(e) => conn.query_row("SELECT server_id FROM events WHERE id = ?1", params![e], |r| r.get(0)).optional()?,
                None => None,
            };
            match now_id {
                Some(id) if !id.starts_with("local:") => server_id = id,
                Some(_) => continue, // not on the server yet
                None => {
                    conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // the event is gone
                    continue;
                }
            }
            v["patch"]["files"] = serde_json::Value::Array(library.ticket_bytes(&v["patch"]["files"])?);
        }
        let server_id = server_id.as_str();
        jobs.push(EventJob {
            op,
            event: v["event"].as_i64(),
            mailbox,
            server_id: server_id.to_string(),
            action: v["action"].as_str().unwrap_or("").to_string(),
            answer: v["answer"].as_str().map(str::to_string),
            patch: v.get("patch").cloned(),
            calendar: v["calendar"].as_str().map(str::to_string),
        });
    }
    Ok(jobs)
}

/// The newest messages a look reads (more come with the next pass, if there are more).
pub const LOOK_NEWEST: usize = 50;

/// Folders opened in the window to look at now: those this sign-in can read (its own mailbox's,
/// the shared mailboxes'; not the Online Archive's), and the rest, left for another.
pub fn look_jobs(library: &Library, folders: &[i64], signed_in: &str) -> Result<(Vec<LookJob>, Vec<i64>)> {
    let conn = &library.conn;
    let (mut jobs, mut rest) = (Vec::new(), Vec::new());
    for &folder in folders {
        let found: Option<(String, Option<String>, String, Option<String>)> = conn
            .query_row(
                "SELECT f.path, f.server_id, a.kind, a.address FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
                params![folder],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((path, Some(server_folder), kind, address)) = found else { continue };
        if in_online_archive(&path) || !kind.starts_with("graph") {
            continue;
        }
        let Ok(mailbox) = mailbox_of(&kind, address, signed_in) else {
            rest.push(folder);
            continue;
        };
        let mut known: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare_cached("SELECT server_id FROM server_items WHERE folder = ?1")?;
            stmt.query_map(params![folder], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut stmt = conn.prepare_cached("SELECT json_extract(payload, '$.server_id') FROM pending_ops WHERE kind = 'move' AND json_extract(payload, '$.from') = ?1")?;
        for id in stmt.query_map(params![folder], |r| r.get::<_, Option<String>>(0))? {
            if let Some(id) = id? {
                known.insert(id);
            }
        }
        jobs.push(LookJob { folder, path, mailbox, server_folder, known });
    }
    Ok((jobs, rest))
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.folders.is_empty() && self.archive_moves.is_empty() && self.marks.is_empty() && self.deletes.is_empty() && self.reconcile.is_empty() && self.categories.is_empty() && self.contacts.is_empty() && self.looks.is_empty() && self.cards.is_empty() && self.calendars.is_empty() && self.events.is_empty() && self.todo_fetch.is_none() && self.todos.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct BatchResult {
    pub moves: Vec<MoveResult>,
    pub marks: Vec<(i64, Outcome)>,
    pub deletes: Vec<(i64, Outcome)>,
    pub reconcile: Vec<ReconcileResult>,
    /// Categories made in Outlook, and why they couldn't be (no permission).
    pub categories_made: u64,
    pub categories_error: Option<String>,
    pub contacts: Vec<crate::cards::ContactsFetched>,
    pub looked: Vec<Looked>,
    pub cards_done: Vec<crate::cards::CardDone>,
    pub calendars: Vec<crate::calendar::CalendarFetched>,
    /// (op, event, the event as Graph keeps it after a change, or why not)
    pub events_done: Vec<(i64, Option<i64>, std::result::Result<Option<serde_json::Value>, String>)>,
    pub todo_fetched: Option<crate::todo::TodoFetched>,
    /// (op, task, create / update / delete, the task as Graph keeps it, or why not)
    pub todos_done: Vec<(i64, i64, String, std::result::Result<Option<serde_json::Value>, String>)>,
    pub folders_done: Vec<crate::folder_changes::FolderDone>,
    /// What changed in Gmail accounts since Eudora last looked (`gmail_changes::record_history`).
    pub gmail_history: Vec<crate::gmail_changes::HistoryFetched>,
    pub archive_done: Vec<crate::archive_moves::ArchiveMoveDone>,
}

/// What a batch did, for the download's log.
#[derive(Debug, Default, PartialEq)]
pub struct BatchRecorded {
    pub moves: Recorded,
    pub marked: u64,
    pub deleted: u64,
    pub failed: u64,
    pub error: Option<String>,
    /// From the server: messages now unread / read, labels given / taken off.
    pub turned_unread: u64,
    pub turned_read: u64,
    /// Replied / forwarded marks brought in from the server.
    pub turned_answered: u64,
    pub labels_changed: u64,
    /// Address books read: cards added, changed, removed; why one couldn't be read.
    pub cards: crate::cards::CardsRecorded,
    pub cards_error: Option<String>,
    /// Contact changes made on the server / not yet (and why).
    pub cards_sent: u64,
    pub cards_failed: u64,
    pub cards_send_error: Option<String>,
    /// Calendars read: events added, changed, removed; why one couldn't be read.
    pub events: crate::calendar::EventsRecorded,
    pub events_error: Option<String>,
    /// Event changes made on the server / not yet (and why).
    pub events_sent: u64,
    pub events_failed: u64,
    pub events_send_error: Option<String>,
    /// To Do read back; task changes made there / not yet.
    pub todo: crate::todo::TodoRecorded,
    pub todo_error: Option<String>,
    pub todos_sent: u64,
    pub todos_failed: u64,
    pub todos_send_error: Option<String>,
    /// Folders made, renamed or deleted on the server; not yet (and why).
    pub folders_changed: u64,
    pub folders_failed: u64,
    pub folders_error: Option<String>,
    /// Online Archive mail moved inside the archive on the server; not yet (and why).
    pub archive_moved: u64,
    pub archive_failed: u64,
    pub archive_error: Option<String>,
    /// Changes made in Gmail itself brought in (read / unread, stars, labels, deleted).
    pub gmail_changed: u64,
}

/// The mailbox a Microsoft 365 account's changes go to: None = the signed-in user's own,
/// Some(address) = a shared one; Err(()) = not one this sign-in may change.
fn mailbox_of(kind: &str, address: Option<String>, signed_in: &str) -> std::result::Result<Option<String>, ()> {
    match kind {
        "graph" if address.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(signed_in)) => Ok(None),
        "graph-shared" => Ok(address),
        _ => Err(()),
    }
}

/// The next batch: moves, marks and deletes waiting (up to `limit` of each), and the read-back
/// asked for (`reconcile`: Some(true) = each mailbox's Inbox, Some(false) = whole mailboxes).
pub fn plan_batch(library: &Library, account: &str, limit: usize, reconcile: Option<bool>, categories: bool) -> Result<Batch> {
    let conn = &library.conn;
    let mut batch = Batch { moves: plan(library, account, limit, &[])?, ..Default::default() };
    batch.archive_moves = library.archive_move_jobs(ARCHIVE_BATCH, account)?;
    batch.folders = library
        .folder_jobs(&["graph", "graph-shared"], limit, &|kind, address| mailbox_of(kind, address, account).ok())?
        .into_iter()
        .map(|(_, job)| job)
        .collect();
    batch.cards = card_jobs(library, account, limit)?;
    batch.events = event_jobs(library, account, limit)?;
    let moving: std::collections::HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT json_extract(payload, '$.message') FROM pending_ops WHERE kind = 'move' AND attempts < ?1")?;
        stmt.query_map(params![GIVE_UP], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };

    let mark_limit = limit * MARKS_PER;
    let ops: Vec<(i64, i64, bool)> = {
        let mut stmt = conn.prepare("SELECT id, json_extract(payload, '$.message'), COALESCE(json_extract(payload, '$.flag'), 0) FROM pending_ops WHERE kind = 'mark' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (mark_limit * 2) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut places = conn.prepare(
        "SELECT a.kind, a.address, l.server_id FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
         WHERE l.message = ?1 AND l.server_id IS NOT NULL AND a.kind LIKE 'graph%' AND f.path NOT LIKE 'Online Archive/%'",
    )?;
    for (op, message, flag) in ops {
        if batch.marks.len() >= mark_limit {
            break;
        }
        if moving.contains(&message) {
            continue; // its id changes with the move: after it
        }
        let flags: Option<i64> = conn.query_row("SELECT flags FROM messages WHERE id = ?1", params![message], |r| r.get(0)).optional()?;
        let Some(flags) = flags else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let mut targets = Vec::new();
        let mut elsewhere = false;
        for place in places.query_map(params![message], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?)))? {
            let (kind, address, item) = place?;
            match mailbox_of(&kind, address, account) {
                Ok(mailbox) => targets.push((mailbox, item)),
                Err(()) => elsewhere = true,
            }
        }
        if targets.is_empty() {
            if !elsewhere {
                conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // no longer on a server
            }
            continue;
        }
        let mut patch = serde_json::json!({
            "isRead": flags & crate::marks::UNREAD == 0,
            "categories": library.labels_on(message)?,
        });
        if let Some(p) = crate::marks::priority_of(flags) {
            patch["importance"] = crate::marks::importance(Some(p)).into();
        }
        // replied / forwarded in Eudora: Outlook and phones show it too (Exchange's last action,
        // PidTagLastVerbExecuted, and the icon Outlook draws from it, PidTagIconIndex)
        let verb = if flags & crate::marks::FORWARDED != 0 { Some((104, 262)) } else if flags & crate::marks::REPLIED != 0 { Some((102, 261)) } else { None };
        if let Some((verb, icon)) = verb {
            patch["singleValueExtendedProperties"] = serde_json::json!([
                { "id": "Integer 0x1081", "value": verb.to_string() },
                { "id": "Integer 0x1080", "value": icon.to_string() },
            ]);
        }
        // Outlook's flag (a To Do): only when it was flagged or unflagged in Eudora
        if flag {
            patch["flag"] = serde_json::json!({ "flagStatus": if flags & crate::marks::FLAGGED != 0 { "flagged" } else { "notFlagged" } });
        }
        batch.marks.push(MarkJob { op, message, targets, patch });
    }
    drop(places);

    let deletes: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'delete' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, payload) in deletes {
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (Some(account_id), Some(item)) = (v["account"].as_i64(), v["server_id"].as_str()) else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let found: Option<(String, Option<String>)> =
            conn.query_row("SELECT kind, address FROM accounts WHERE id = ?1", params![account_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((kind, address)) = found else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        if let Ok(mailbox) = mailbox_of(&kind, address, account) {
            batch.deletes.push(DeleteJob { op, mailbox, item: item.to_string() });
        }
    }

    if reconcile.is_some() || categories {
        let labels = library.label_list()?;
        let mut stmt = conn.prepare("SELECT id, kind, address FROM accounts WHERE kind LIKE 'graph%' ORDER BY id")?;
        let accounts: Vec<(i64, String, Option<String>)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        for (id, kind, address) in accounts {
            let Ok(mailbox) = mailbox_of(&kind, address, account) else { continue };
            if let Some(inbox_only) = reconcile {
                let names = if inbox_only { Vec::new() } else { labels.iter().map(|l| l.name.clone()).collect() };
                batch.reconcile.push(ReconcileJob { account: id, mailbox: mailbox.clone(), inbox_only, labels: names });
            }
            if categories {
                batch.categories.push((mailbox, labels.iter().map(|l| (l.name.clone(), l.color.clone())).collect()));
            }
        }
    }
    Ok(batch)
}

/// Does a batch on the server (the change thread: no database here).
// (`part` times each part of the round: replacing it records the one before)
#[allow(unused_assignments, unused_variables)]
pub fn carry_out_batch(client: &mut GraphClient, batch: &Batch) -> BatchResult {
    // folders first: moves may go into one just made
    let folders_done = crate::folder_changes::carry_out_graph(client, &batch.folders);
    let moves = {
        let _timed = crate::speed::Timer::new("Round: moves");
        carry_out(client, &batch.moves)
    };
    let mut result = BatchResult { moves, folders_done, ..Default::default() };
    // The archive moves (minutes a round) run beside everything else, on their own clients:
    // they go to the archive's mailboxes, which Exchange limits apart from the mailbox
    let mut archive_client = client.worker();
    let archive_jobs = &batch.archive_moves;
    std::thread::scope(|s| {
    let archive = s.spawn(move || {
        let _timed = crate::speed::Timer::new("Round: archive moves");
        crate::archive_moves::carry_out(&mut archive_client, archive_jobs)
    });
    let canceled = |e: &Error| e.to_string() == "canceled";
    let mut part = crate::speed::Timer::new("Round: Outlook categories");
    for (mailbox, labels) in &batch.categories {
        client.set_mailbox(mailbox.clone());
        match client.ensure_categories(labels) {
            Ok(n) => result.categories_made += n,
            Err(e) => result.categories_error = Some(e.to_string()),
        }
    }
    part = crate::speed::Timer::new("Round: marks");
    // marks: 20 to a request, per mailbox (a message gone there, or whose id is another
    // mailbox's, has nothing to mark)
    {
        let mut groups: Vec<(Option<String>, Vec<(usize, &str)>)> = Vec::new();
        for (j, job) in batch.marks.iter().enumerate() {
            for (mailbox, item) in &job.targets {
                match groups.iter_mut().find(|g| &g.0 == mailbox) {
                    Some(g) => g.1.push((j, item)),
                    None => groups.push((mailbox.clone(), vec![(j, item)])),
                }
            }
        }
        let mut failed: HashMap<usize, String> = HashMap::new();
        let mut stopped = false;
        for (mailbox, list) in &groups {
            if stopped {
                for (j, _) in list {
                    failed.entry(*j).or_insert_with(|| "canceled".into());
                }
                continue;
            }
            client.set_mailbox(mailbox.clone());
            let items: Vec<_> = list.iter().map(|(j, item)| client.patch_item(item, &batch.marks[*j].patch)).collect();
            match client.batch(&items) {
                Ok(answers) => {
                    for ((j, _), (status, body)) in list.iter().zip(answers) {
                        if !(200..300).contains(&status) && status != 404 {
                            failed.entry(*j).or_insert_with(|| crate::graph::batch_error(status, &body));
                        }
                    }
                }
                Err(e) => {
                    stopped = canceled(&e);
                    for (j, _) in list {
                        failed.entry(*j).or_insert_with(|| e.to_string());
                    }
                }
            }
        }
        for (j, job) in batch.marks.iter().enumerate() {
            result.marks.push((job.op, failed.remove(&j).map(Outcome::Failed).unwrap_or(Outcome::Moved(String::new()))));
        }
    }
    part = crate::speed::Timer::new("Round: deletes");
    for job in &batch.deletes {
        client.set_mailbox(job.mailbox.clone());
        let outcome = match client.delete_message(&job.item) {
            Ok(()) => Outcome::Moved(String::new()),
            Err(e) if canceled(&e) => {
                result.deletes.push((job.op, Outcome::Failed(e.to_string())));
                break;
            }
            Err(e) => Outcome::Failed(e.to_string()),
        };
        result.deletes.push((job.op, outcome));
    }
    part = crate::speed::Timer::new("Round: read back (unread, labels)");
    for job in &batch.reconcile {
        client.set_mailbox(job.mailbox.clone());
        let asked = std::time::Instant::now();
        let unread = client.unread_ids(job.inbox_only).map_err(|e| e.to_string());
        let mut labels = Vec::new();
        let mut answered = Vec::new();
        if unread.is_ok() {
            for name in &job.labels {
                if let Ok(ids) = client.ids_with_category(name) {
                    labels.push((name.clone(), ids));
                }
            }
            // replied / forwarded elsewhere: the newest messages' last action
            answered = client.last_verbs(job.inbox_only, if job.inbox_only { 500 } else { 2000 }).unwrap_or_default();
        }
        result.reconcile.push(ReconcileResult { job: job.clone(), asked, unread, labels, answered });
    }
    part = crate::speed::Timer::new("Round: contact changes");
    for job in &batch.cards {
        client.set_mailbox(job.mailbox.clone());
        use crate::cards::CardChange;
        let outcome = match &job.what {
            CardChange::Create { book, contact } => client.create_contact(book.as_deref(), contact).map(Some),
            CardChange::Update { server_id, contact } => client.update_contact(server_id, contact).map(Some),
            CardChange::Delete { server_id } => client.delete_contact(server_id).map(|_| None),
        };
        // (a contact already gone there needs no deleting)
        let outcome = match (outcome, &job.what) {
            (Err(Error::Other(e)), crate::cards::CardChange::Delete { .. }) if e.contains(" 404") || e.contains("ErrorItemNotFound") => Ok(None),
            (o, _) => o.map_err(|e| e.to_string()),
        };
        result.cards_done.push(crate::cards::CardDone { op: job.op, card: job.card, result: outcome });
    }
    part = crate::speed::Timer::new("Round: calendar changes");
    for job in &batch.events {
        client.set_mailbox(job.mailbox.clone());
        let outcome = match job.action.as_str() {
            "respond" => client.respond_event(&job.server_id, job.answer.as_deref().unwrap_or("accept"), job.patch.as_ref()).map(|_| None),
            "update" => client.update_event(&job.server_id, job.patch.as_ref().unwrap_or(&serde_json::Value::Null)).map(Some),
            "create" => client.create_event(job.calendar.as_deref().unwrap_or(""), job.patch.as_ref().unwrap_or(&serde_json::Value::Null)).map(Some),
            "cancel" => client.cancel_event(&job.server_id).map(|_| None),
            "delete" => client.delete_event(&job.server_id).map(|_| None),
            "attach" => attach_tickets(client, job).map(|_| None),
            other => Err(Error::Other(format!("unknown event change {other}"))),
        };
        // (an event already gone there needs no deleting, declining or canceling)
        let outcome = match outcome {
            Err(Error::Other(e)) if !["update", "create"].contains(&job.action.as_str()) && (e.contains(" 404") || e.contains("ErrorItemNotFound")) => Ok(None),
            // an organizer who asked for no answers: nothing to send (the answer stays here)
            Err(Error::Other(e)) if job.action == "respond" && e.contains("hasn't requested a response") => Ok(None),
            // a meeting canceled meanwhile: no answer can go; a decline (it was deleted here)
            // takes it off the calendar there too, quietly, as Outlook's Remove from Calendar
            Err(Error::Other(e)) if job.action == "respond" && e.contains("meeting that's been canceled") => {
                if job.answer.as_deref() == Some("decline") { client.delete_event(&job.server_id).map(|_| None).or_else(|e| if e.to_string().contains(" 404") { Ok(None) } else { Err(e.to_string()) }) } else { Ok(None) }
            }
            o => o.map_err(|e| e.to_string()),
        };
        result.events_done.push((job.op, job.event, outcome));
    }
    part = crate::speed::Timer::new("Round: To Do");
    // To Do: the changes made here first, then everything read back
    client.set_mailbox(None);
    let mut default_list: Option<String> = None;
    for job in &batch.todos {
        let list = match (&job.list, job.action.as_str()) {
            (Some(l), _) => Ok(l.clone()),
            (None, _) => match &default_list {
                Some(l) => Ok(l.clone()),
                None => client.todo_default_list().inspect(|l| default_list = Some(l.clone())),
            },
        };
        let outcome = list.and_then(|list| match (job.action.as_str(), &job.server_id) {
            ("create", _) => client.create_task(&list, &job.task).map(|mut v| {
                v["parentListId"] = list.clone().into();
                Some(v)
            }),
            ("update", Some(id)) => client.update_task(&list, id, &job.task).map(Some),
            ("delete", Some(id)) => client.delete_task(&list, id).map(|_| None),
            (_, None) => Ok(None), // (not on the server: nothing to change there)
            (other, _) => Err(Error::Other(format!("unknown task change {other}"))),
        });
        let outcome = match outcome {
            Err(Error::Other(e)) if job.action == "delete" && (e.contains(" 404") || e.contains("NotFound")) => Ok(None),
            o => o.map_err(|e| e.to_string()),
        };
        result.todos_done.push((job.op, job.todo, job.action.clone(), outcome));
    }
    if let Some(account) = batch.todo_fetch {
        result.todo_fetched = Some(crate::todo::TodoFetched { account, lists: client.todo_lists().map_err(|e| e.to_string()) });
    }
    part = crate::speed::Timer::new("Round: folders opened");
    for job in &batch.looks {
        client.set_mailbox(job.mailbox.clone());
        let mut unread = Vec::new();
        let items = client.newest_listed(&job.server_folder, LOOK_NEWEST).and_then(|newest| {
            newest
                .into_iter()
                .filter(|(id, _, _)| !job.known.contains(id))
                .map(|(id, date, is_unread)| {
                    if is_unread {
                        unread.push(id.clone());
                    }
                    client.raw_message(&id).map(|raw| (id, raw, date))
                })
                .collect::<Result<Vec<_>>>()
        });
        result.looked.push(Looked { folder: job.folder, path: job.path.clone(), items: items.map_err(|e| e.to_string()), unread });
    }
    part = crate::speed::Timer::new("Round: address books");
    for job in &batch.contacts {
        client.set_mailbox(job.mailbox.clone());
        let books = client.address_books().map_err(|e| e.to_string());
        result.contacts.push(crate::cards::ContactsFetched { account: job.account, books, source: crate::cards::Source::Graph });
    }
    part = crate::speed::Timer::new("Round: calendars");
    for job in &batch.calendars {
        client.set_mailbox(None);
        let calendars = client.calendars().and_then(|list| {
            list.into_iter().map(|cal| {
                let events = client.calendar_view(cal["id"].as_str().unwrap_or(""), job.from, job.to)?;
                Ok((cal, events))
            }).collect::<Result<Vec<_>>>()
        }).map_err(|e| e.to_string());
        result.calendars.push(crate::calendar::CalendarFetched { account: job.account, from: job.from, to: job.to, calendars });
    }
    drop(part);
    client.set_mailbox(None);
    result.archive_done = archive.join().unwrap_or_default();
    });
    result
}

/// Records a batch's results (the writer's side).
pub fn record_batch(library: &Library, result: &BatchResult) -> Result<BatchRecorded> {
    // (the folders first: moves recorded after may name the folders they made)
    let (folders_changed, folders_failed, folders_error) = crate::folder_changes::record(library, &result.folders_done)?;
    // (messages whose changes reach the server now: a read-back asked before this is older)
    sent_now(library, result.moves.iter().filter(|m| !matches!(&m.outcome, Outcome::Failed(_))).map(|m| m.message));
    let mut done = BatchRecorded { moves: record(library, &result.moves)?, folders_changed, folders_failed, folders_error, ..Default::default() };
    (done.archive_moved, done.archive_failed, done.archive_error) = library.record_archive_moves(&result.archive_done)?;
    let conn = &library.conn;
    library.in_transaction(|| {
        for (ops, count) in [(&result.marks, 0usize), (&result.deletes, 1)] {
            for (op, outcome) in ops.iter() {
                match outcome {
                    Outcome::Moved(_) | Outcome::Gone | Outcome::Elsewhere => {
                        let message: Option<i64> = conn.query_row("SELECT json_extract(payload, '$.message') FROM pending_ops WHERE id = ?1", params![op], |r| r.get(0)).optional()?.flatten();
                        sent_now(library, message);
                        conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                        if count == 0 { done.marked += 1 } else { done.deleted += 1 }
                    }
                    Outcome::Failed(e) if e == "canceled" => {}
                    Outcome::Failed(e) => {
                        conn.execute("UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![op, e])?;
                        done.failed += 1;
                        done.error = Some(e.clone());
                    }
                }
            }
        }
        for r in &result.reconcile {
            let Ok(unread) = &r.unread else { continue };
            let (unread_now, read_now, labels) = apply_reconcile(library, &r.job, unread, &r.labels, &sent_since(library, r.asked))?;
            done.turned_answered += apply_answered(library, &r.job, &r.answered)?;
            done.turned_unread += unread_now;
            done.turned_read += read_now;
            done.labels_changed += labels;
        }
        Ok(())
    })?;
    for sent in &result.cards_done {
        match &sent.result {
            Ok(contact) => {
                if let (Some(card), Some(v)) = (sent.card, contact) {
                    library.conn.execute(
                        "UPDATE cards SET server_id = COALESCE(?2, server_id), version = ?3, original = ?4 WHERE id = ?1",
                        params![card, v["id"].as_str(), v["changeKey"].as_str(), v.to_string()],
                    )?;
                }
                library.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![sent.op])?;
                done.cards_sent += 1;
            }
            Err(e) if e == "canceled" => {}
            Err(e) => {
                library.conn.execute("UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![sent.op, e])?;
                done.cards_failed += 1;
                done.cards_send_error = Some(e.clone());
            }
        }
    }
    for (op, todo, action, outcome) in &result.todos_done {
        match outcome {
            Ok(v) => {
                library.record_todo_done(*op, *todo, action, v.as_ref())?;
                done.todos_sent += 1;
            }
            Err(e) if e == "canceled" => {}
            Err(e) => {
                library.conn.execute("UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![op, e])?;
                done.todos_failed += 1;
                done.todos_send_error = Some(e.clone());
            }
        }
    }
    if let Some(fetched) = &result.todo_fetched {
        match &fetched.lists {
            Ok(lists) => done.todo = library.record_todos(fetched.account, lists)?,
            Err(e) => done.todo_error = Some(e.clone()),
        }
    }
    for (op, event, outcome) in &result.events_done {
        match outcome {
            Ok(v) => {
                if let (Some(event), Some(v)) = (event, v) {
                    // a new event: its server id now (the calendar's own read may have brought it in
                    // already: then that copy stays and the temporary one goes)
                    let id = v["id"].as_str().unwrap_or("");
                    let twin: Option<i64> = library.conn.query_row("SELECT e2.id FROM events e1 JOIN events e2 ON e2.calendar = e1.calendar AND e2.server_id = ?2 AND e2.id <> e1.id WHERE e1.id = ?1", params![event, id], |r| r.get(0)).optional()?;
                    if twin.is_some() || id.is_empty() {
                        if twin.is_some() {
                            library.conn.execute("DELETE FROM events WHERE id = ?1", params![event])?;
                        }
                    } else {
                        library.conn.execute("UPDATE events SET server_id = ?2, version = ?3, original = ?4 WHERE id = ?1", params![event, id, v["changeKey"].as_str(), v.to_string()])?;
                    }
                }
                library.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                done.events_sent += 1;
            }
            Err(e) if e == "canceled" => {}
            Err(e) => {
                // (access refused: trying again won't change it; given up at once, so the
                // calendar's own copy comes back at its next read)
                // (one query whichever it is: it had three values for two placeholders when
                // counting a try, so every round's recording failed, the moves made with it too)
                let give_up = e.contains("AccessDenied") || e.contains("Access is denied");
                library.conn.execute(
                    "UPDATE pending_ops SET attempts = CASE WHEN ?3 THEN ?4 ELSE attempts + 1 END, last_error = ?2 WHERE id = ?1",
                    params![op, e, give_up, GIVE_UP],
                )?;
                done.events_failed += 1;
                done.events_send_error = Some(e.clone());
            }
        }
    }
    for fetched in &result.calendars {
        match &fetched.calendars {
            Ok(list) => {
                let r = library.record_calendars(fetched.account, fetched.from, fetched.to, list)?;
                done.events.added += r.added;
                done.events.changed += r.changed;
                done.events.removed += r.removed;
                // meetings someone else canceled: off the calendar, as Outlook does
                done.events.removed += library.remove_canceled_meetings(fetched.account)?;
            }
            Err(e) => done.events_error = Some(e.clone()),
        }
    }
    for h in &result.gmail_history {
        done.gmail_changed += crate::gmail_changes::record_history(library, h)?;
    }
    for fetched in &result.contacts {
        match &fetched.books {
            Ok(books) => {
                let r = match fetched.source {
                    crate::cards::Source::Google => library.record_google_contacts(fetched.account, books.first().map(|b| b.2.as_slice()).unwrap_or(&[]))?,
                    crate::cards::Source::Dav => library.record_dav_contacts(fetched.account, books)?,
                    crate::cards::Source::Graph => library.record_contacts(fetched.account, books)?,
                };
                done.cards.added += r.added;
                done.cards.changed += r.changed;
                done.cards.removed += r.removed;
            }
            Err(e) => done.cards_error = Some(e.clone()),
        }
    }
    if let (Some(e), None) = (&result.categories_error, &done.error) {
        if !e.contains("403") && !e.contains("Forbidden") && !e.contains("Access") {
            done.error = Some(format!("Outlook's category list: {e}"));
        }
    }
    Ok(done)
}

/// Brings the read state (and labels) of an account's server messages in line with the server.
/// Messages with a change of their own still waiting for the server keep theirs.
/// Messages whose changes reached the server, and when (the last two hours): a read-back asked
/// before a change was made there mustn't undo it, though the change no longer waits.
/// (Kept by library: a program could have more than one open.)
static SENT: std::sync::Mutex<Vec<(std::time::Instant, std::path::PathBuf, i64)>> = std::sync::Mutex::new(Vec::new());

fn sent_now(library: &Library, messages: impl IntoIterator<Item = i64>) {
    let now = std::time::Instant::now();
    let mut sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
    sent.retain(|(at, _, _)| now.duration_since(*at) < std::time::Duration::from_secs(7200));
    sent.extend(messages.into_iter().map(|m| (now, library.root.clone(), m)));
}

fn sent_since(library: &Library, asked: std::time::Instant) -> Vec<i64> {
    SENT.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(at, root, _)| *at >= asked && *root == library.root).map(|(_, _, m)| *m).collect()
}

pub(crate) fn apply_reconcile(library: &Library, job: &ReconcileJob, unread: &[String], labels: &[(String, Vec<String>)], newer: &[i64]) -> Result<(u64, u64, u64)> {
    // mail kept local (`local_only`): read / unread and labels set on the server stay there
    if library.local_only_account(job.account, crate::local_only::Service::Mail)? {
        return Ok((0, 0, 0));
    }
    let conn = &library.conn;
    conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS reconcile_ids (server_id TEXT PRIMARY KEY); DELETE FROM temp.reconcile_ids;")?;
    let fill = |ids: &[String]| -> Result<()> {
        conn.execute("DELETE FROM temp.reconcile_ids", [])?;
        let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO temp.reconcile_ids (server_id) VALUES (?1)")?;
        for id in ids {
            insert.execute(params![id])?;
        }
        Ok(())
    };
    let inbox = if job.inbox_only { "AND (f.role = 'inbox' OR lower(f.path) = 'inbox')" } else { "" };
    // A message in several mailboxes (cc'd to a shared mailbox too) is one message here but has
    // a read flag in each: it follows one of them, or every read-back would undo the last
    // (seen 2026-10-17: the same 1,360 turned unread and read again every five minutes). The
    // user's own mailbox wins over a shared one; between two of a kind, the first account.
    let kind: String = conn.query_row("SELECT kind FROM accounts WHERE id = ?1", params![job.account], |r| r.get(0))?;
    let scope = format!(
        "SELECT l.message FROM locations l JOIN folders f ON f.id = l.folder
         WHERE f.account = {account} AND l.server_id IS NOT NULL AND f.path NOT LIKE 'Online Archive/%' {inbox}
           AND NOT EXISTS (SELECT 1 FROM locations l2 JOIN folders f2 ON f2.id = l2.folder JOIN accounts a2 ON a2.id = f2.account
                           WHERE l2.message = l.message AND f2.account <> {account} AND l2.server_id IS NOT NULL
                             AND a2.kind LIKE 'graph%' AND f2.path NOT LIKE 'Online Archive/%'
                             AND ((a2.kind = 'graph' AND '{kind}' = 'graph-shared') OR (a2.kind = '{kind}' AND f2.account < {account})))",
        account = job.account,
        kind = kind.replace('\'', "")
    );
    let marked = format!("{scope} AND l.server_id IN (SELECT server_id FROM temp.reconcile_ids)");
    let newer: Vec<String> = newer.iter().map(|m| m.to_string()).collect();
    let waiting = format!(
        "SELECT json_extract(payload, '$.message') FROM pending_ops WHERE kind IN ('mark', 'move', 'imap-mark') UNION ALL SELECT value FROM json_each('[{}]')",
        newer.join(",")
    );

    fill(unread)?;
    let turned_unread = conn.execute(&format!("UPDATE messages SET flags = flags | 1 WHERE flags & 1 = 0 AND id IN ({marked}) AND id NOT IN ({waiting})"), [])?;
    let turned_read = conn.execute(
        &format!("UPDATE messages SET flags = flags & ~1 WHERE flags & 1 = 1 AND id IN ({scope}) AND id NOT IN ({marked}) AND id NOT IN ({waiting})"),
        [],
    )?;
    let mut changed = 0;
    for (name, ids) in labels {
        fill(ids)?;
        changed += conn.execute(&format!("INSERT OR IGNORE INTO categories (message, label) SELECT DISTINCT message, ?1 FROM ({marked}) WHERE message NOT IN ({waiting})"), params![name])?;
        changed += conn.execute(
            &format!("DELETE FROM categories WHERE label = ?1 AND message IN ({scope}) AND message NOT IN ({marked}) AND message NOT IN ({waiting})"),
            params![name],
        )?;
    }
    Ok((turned_unread as u64, turned_read as u64, changed as u64))
}

/// Replied / forwarded on the server (Outlook, a phone, the web) marked here too: the mark is
/// added, never taken off (Exchange keeps only the last action, and one done here stays), and a
/// message with its own change still waiting keeps it. Returns how many were marked.
fn apply_answered(library: &Library, job: &ReconcileJob, answered: &[(String, i64)]) -> Result<u64> {
    if answered.is_empty() || library.local_only_account(job.account, crate::local_only::Service::Mail)? {
        return Ok(0);
    }
    let conn = &library.conn;
    let mut done = 0;
    for (bit, verbs) in [(crate::marks::REPLIED, [102, 103]), (crate::marks::FORWARDED, [104, 104])] {
        let ids: Vec<&String> = answered.iter().filter(|(_, v)| verbs.contains(v)).map(|(id, _)| id).collect();
        if ids.is_empty() {
            continue;
        }
        conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS answered_ids (server_id TEXT PRIMARY KEY); DELETE FROM temp.answered_ids;")?;
        {
            let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO temp.answered_ids (server_id) VALUES (?1)")?;
            for id in ids {
                insert.execute(params![id])?;
            }
        }
        done += conn.execute(
            "UPDATE messages SET flags = flags | ?2 WHERE flags & ?2 = 0 AND id IN (
                 SELECT l.message FROM locations l JOIN folders f ON f.id = l.folder
                 WHERE f.account = ?1 AND l.server_id IN (SELECT server_id FROM temp.answered_ids))
               AND id NOT IN (SELECT json_extract(payload, '$.message') FROM pending_ops WHERE kind IN ('mark', 'imap-mark'))",
            params![job.account, bit],
        )? as u64;
    }
    Ok(done)
}

/// IMAP's replied / forwarded marks for one folder's messages (`Chunk::Answered`), added the same
/// way: never taken off, a message's own waiting change kept.
pub fn mark_answered_in(library: &Library, folder: i64, ids: &[(String, i64)]) -> Result<u64> {
    let conn = &library.conn;
    let account: Option<i64> = conn.query_row("SELECT account FROM folders WHERE id = ?1", params![folder], |r| r.get(0)).optional()?;
    if let Some(a) = account {
        if library.local_only_account(a, crate::local_only::Service::Mail)? {
            return Ok(0); // (mail kept local: the server's marks stay there)
        }
    }
    let mut done = 0;
    for bit in [crate::marks::REPLIED, crate::marks::FORWARDED] {
        let mut stmt = conn.prepare_cached(
            "UPDATE messages SET flags = flags | ?3 WHERE flags & ?3 = 0
               AND id IN (SELECT message FROM locations WHERE folder = ?1 AND server_id = ?2)
               AND id NOT IN (SELECT json_extract(payload, '$.message') FROM pending_ops WHERE kind IN ('mark', 'imap-mark'))",
        )?;
        for (id, b) in ids.iter().filter(|(_, b)| *b == bit) {
            done += stmt.execute(params![folder, id, b])? as u64;
        }
    }
    Ok(done)
}

/// The marks and deletes waiting for the server: (waiting, given up).
pub fn waiting_marks(library: &Library) -> Result<(u64, u64)> {
    let (waiting, given_up): (i64, i64) = library.conn.query_row(
        "SELECT COALESCE(SUM(attempts < ?1), 0), COALESCE(SUM(attempts >= ?1), 0) FROM pending_ops WHERE kind IN ('mark', 'delete', 'imap-mark')",
        params![GIVE_UP],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((waiting as u64, given_up as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    /// A library with one Microsoft 365 account (me@x.com): Inbox (one message, on the server
    /// as "item-1"), "Deleted Items" and "Online Archive/Old" known on the server, "Bulk" not.
    fn mailbox() -> (tempfile::TempDir, Library, i64, [i64; 4]) {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let file = dir.path().join("a.eml");
        std::fs::write(&file, "From: ann@a.com\r\nTo: me@x.com\r\nSubject: Hi\r\nMessage-ID: <1@a>\r\n\r\nHello\r\n").unwrap();
        import(&mut library, &[file], &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let conn = &library.conn;
        let inbox: i64 = conn.query_row("SELECT folder FROM locations", [], |r| r.get(0)).unwrap();
        let account: i64 = conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        conn.execute("UPDATE accounts SET kind = 'graph', address = 'me@x.com' WHERE id = ?1", [account]).unwrap();
        // (the import may have put the folder anywhere: make it the account's "Inbox")
        conn.execute("UPDATE folders SET path = 'Inbox', name = 'Inbox', parent = NULL, server_id = 'F-inbox' WHERE id = ?1", [inbox]).unwrap();
        conn.execute("UPDATE locations SET server_id = 'item-1'", []).unwrap();
        let message: i64 = conn.query_row("SELECT id FROM messages", [], |r| r.get(0)).unwrap();
        let trash = crate::db::ensure_folder(conn, account, "Deleted Items").unwrap();
        conn.execute("UPDATE folders SET server_id = 'F-trash' WHERE id = ?1", [trash]).unwrap();
        let archive = crate::db::ensure_folder(conn, account, "Online Archive/Old").unwrap();
        let bulk = crate::db::ensure_folder(conn, account, "Bulk/Marketing").unwrap();
        (dir, library, message, [inbox, trash, archive, bulk])
    }

    #[test]
    fn status_labels_and_priority_go_to_the_server_and_come_back() {
        use crate::marks::{REPLIED, Status, UNREAD};
        let (_dir, library, id, [inbox, _, _, _]) = mailbox();
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        library.set_status(&[id], Status::Unread).unwrap();
        library.set_label(&[id], "Prospects", true).unwrap();
        library.set_priority(&[id], Some(1)).unwrap();
        let batch = plan_batch(&library, "me@x.com", BATCH, None, false).unwrap();
        assert_eq!(batch.marks.len(), 1, "one entry per message, however many changes");
        let mark = &batch.marks[0];
        assert_eq!(mark.targets, vec![(None, "item-1".to_string())]);
        assert_eq!(mark.patch, serde_json::json!({ "isRead": false, "categories": ["Prospects"], "importance": "high" }));
        // another sign-in's account waits
        assert!(plan_batch(&library, "other@x.com", BATCH, None, false).unwrap().marks.is_empty());

        // done on the server
        let done = BatchResult { marks: vec![(mark.op, Outcome::Moved(String::new()))], ..Default::default() };
        assert_eq!(record_batch(&library, &done).unwrap().marked, 1);
        assert!(library.pending_changes().unwrap().is_empty());

        // read in Outlook, the label taken off on the phone: comes back
        let job = ReconcileJob { account, mailbox: None, inbox_only: false, labels: vec!["Prospects".into()] };
        let back = BatchResult { reconcile: vec![ReconcileResult { job: job.clone(), asked: std::time::Instant::now(), unread: Ok(vec![]), labels: vec![("Prospects".into(), vec![])], answered: vec![] }], ..Default::default() };
        let r = record_batch(&library, &back).unwrap();
        assert_eq!((r.turned_read, r.labels_changed), (1, 1));
        assert_eq!(library.flags_of(id).unwrap() & UNREAD, 0);
        assert!(library.labels_on(id).unwrap().is_empty());

        // unread there again; meanwhile read here and not yet sent: the change made here wins
        library.set_status(&[id], Status::Read).unwrap(); // (already read: nothing queued)
        let back = BatchResult { reconcile: vec![ReconcileResult { job: job.clone(), asked: std::time::Instant::now(), unread: Ok(vec!["item-1".into()]), labels: vec![], answered: vec![] }], ..Default::default() };
        assert_eq!(record_batch(&library, &back).unwrap().turned_unread, 1);
        library.set_status(&[id], Status::Read).unwrap();
        let back = BatchResult { reconcile: vec![ReconcileResult { job, asked: std::time::Instant::now(), unread: Ok(vec!["item-1".into()]), labels: vec![], answered: vec![] }], ..Default::default() };
        assert_eq!(record_batch(&library, &back).unwrap().turned_unread, 0, "waiting to go to the server: kept");
        assert_eq!(library.flags_of(id).unwrap() & UNREAD, 0);

        // a long read-back asked before that change reached the server: its answer is older
        let asked = std::time::Instant::now();
        let op: i64 = library.conn.query_row("SELECT id FROM pending_ops WHERE kind = 'mark'", [], |r| r.get(0)).unwrap();
        record_batch(&library, &BatchResult { marks: vec![(op, Outcome::Moved(String::new()))], ..Default::default() }).unwrap();
        assert!(library.pending_changes().unwrap().is_empty());
        let job = ReconcileJob { account, mailbox: None, inbox_only: false, labels: vec![] };
        let late = BatchResult { reconcile: vec![ReconcileResult { job: job.clone(), asked, unread: Ok(vec!["item-1".into()]), labels: vec![], answered: vec![] }], ..Default::default() };
        assert_eq!(record_batch(&library, &late).unwrap().turned_unread, 0, "a read-back older than the change doesn't undo it");
        let fresh = BatchResult { reconcile: vec![ReconcileResult { job, asked: std::time::Instant::now(), unread: Ok(vec!["item-1".into()]), labels: vec![], answered: vec![] }], ..Default::default() };
        assert_eq!(record_batch(&library, &fresh).unwrap().turned_unread, 1, "a newer one counts");

        // replied in Outlook, then forwarded on the phone: both marks here, and they stay
        let job = ReconcileJob { account, mailbox: None, inbox_only: true, labels: vec![] };
        let answered = |verb: i64| BatchResult { reconcile: vec![ReconcileResult { job: job.clone(), asked: std::time::Instant::now(), unread: Ok(vec!["item-1".into()]), labels: vec![], answered: vec![("item-1".into(), verb)] }], ..Default::default() };
        assert_eq!(record_batch(&library, &answered(102)).unwrap().turned_answered, 1);
        assert_eq!(library.flags_of(id).unwrap() & REPLIED, REPLIED);
        assert_eq!(record_batch(&library, &answered(102)).unwrap().turned_answered, 0, "already marked");
        assert_eq!(record_batch(&library, &answered(104)).unwrap().turned_answered, 1);
        assert_eq!(library.flags_of(id).unwrap() & (REPLIED | crate::marks::FORWARDED), REPLIED | crate::marks::FORWARDED);
    }

    #[test]
    fn moves_are_joined_until_the_server_has_them() {
        let (_dir, library, id, [inbox, trash, _, bulk]) = mailbox();
        library.move_messages(&[id], inbox, bulk).unwrap();
        library.move_messages(&[id], bulk, trash).unwrap();
        let jobs = plan(&library, "me@x.com", BATCH, &[]).unwrap();
        assert_eq!(jobs.len(), 1, "Inbox → Bulk → Trash is one move on the server");
        assert_eq!((jobs[0].item.as_str(), jobs[0].to), ("item-1", trash));
        // moved back where it is on the server: nothing to do, and it keeps its server id
        library.move_messages(&[id], trash, inbox).unwrap();
        assert!(plan(&library, "me@x.com", BATCH, &[]).unwrap().is_empty());
        let sid: Option<String> = library.conn.query_row("SELECT server_id FROM locations WHERE message = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(sid.as_deref(), Some("item-1"));
    }

    #[test]
    fn the_online_archive_is_never_changed_on_the_server() {
        let (_dir, library, id, [inbox, _, archive, _]) = mailbox();
        library.move_messages(&[id], inbox, archive).unwrap();
        assert!(library.pending_changes().unwrap().is_empty());
    }

    #[test]
    fn archive_mail_is_filed_like_any_other_but_stays_in_the_archive() {
        let (_dir, library, id, [inbox, trash, _, bulk]) = mailbox();
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        let archive_inbox = crate::db::ensure_folder(&library.conn, account, "Online Archive/Inbox").unwrap();
        let archive_bulk = crate::db::ensure_folder(&library.conn, account, "Online Archive/Bulk/Marketing").unwrap();
        assert_eq!(library.archive_twin(inbox).unwrap(), Some(archive_inbox));
        assert_eq!(library.archive_twin(archive_inbox).unwrap(), Some(inbox));
        assert_eq!(crate::library::shown_path("Online Archive/Inbox_2021"), "Inbox_2021");
        assert_eq!(crate::library::shown_path("Online Archive"), "");
        // the message was pulled from the archive
        library.conn.execute("UPDATE locations SET folder = ?1, server_id = 'arch-1' WHERE message = ?2", [archive_inbox, id]).unwrap();
        library.conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, 'arch-1', ?2)", [archive_inbox, id]).unwrap();
        assert!(library.from_archive(id).unwrap());

        // filed into "Bulk/Marketing" (shown as one with the archive's): the archive's side,
        // nothing for the server
        library.move_messages(&[id], archive_inbox, bulk).unwrap();
        assert_eq!(library.folder_paths(id).unwrap(), vec!["Online Archive/Bulk/Marketing".to_string()]);
        assert!(library.pending_changes().unwrap().is_empty());
        // into a folder only the mailbox has (Deleted Items): filed there, still nothing queued
        library.move_messages(&[id], archive_bulk, trash).unwrap();
        assert_eq!(library.folder_paths(id).unwrap(), vec!["Deleted Items".to_string()]);
        assert!(library.pending_changes().unwrap().is_empty(), "never moved out of the archive on the server");
        // Undo of a move into the merged Inbox, given the mailbox's Inbox id: found on the archive side
        library.move_messages(&[id], trash, inbox).unwrap();
        assert_eq!(library.folder_paths(id).unwrap(), vec!["Online Archive/Inbox".to_string()], "back on the archive's side");
        library.move_messages(&[id], inbox, bulk).unwrap();
        assert_eq!(library.folder_paths(id).unwrap(), vec!["Online Archive/Bulk/Marketing".to_string()]);
        assert!(library.pending_changes().unwrap().is_empty());
    }

    #[test]
    fn mailbox_mail_filed_in_an_archive_folder_stays_put_and_can_move_later() {
        let (_dir, library, id, [inbox, trash, _, _]) = mailbox();
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        let year = crate::db::ensure_folder(&library.conn, account, "Online Archive/Inbox_2021").unwrap();
        library.conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, 'item-1', ?2)", [inbox, id]).unwrap();
        assert!(!library.from_archive(id).unwrap());
        // a move waiting, then filed into an archive-only folder: called off, nothing moves there
        library.move_messages(&[id], inbox, trash).unwrap();
        library.move_messages(&[id], trash, year).unwrap();
        assert!(library.pending_changes().unwrap().is_empty());
        // later into Deleted Items: made on the server, from where it really is
        library.move_messages(&[id], year, trash).unwrap();
        let jobs = plan(&library, "me@x.com", BATCH, &[]).unwrap();
        assert_eq!((jobs.len(), jobs[0].item.as_str(), jobs[0].to), (1, "item-1", trash));
    }

    #[test]
    fn a_new_folder_is_planned_from_the_top_and_recorded_when_made() {
        let (_dir, library, id, [inbox, _, _, bulk]) = mailbox();
        library.move_messages(&[id], inbox, bulk).unwrap();
        let jobs = plan(&library, "me@x.com", BATCH, &[]).unwrap();
        let names: Vec<(&str, bool)> = jobs[0].path.iter().map(|s| (s.name.as_str(), s.server_id.is_some())).collect();
        assert_eq!(names, vec![("Bulk", false), ("Marketing", false)]);
        // another Microsoft account's moves wait for its own sign-in
        assert!(plan(&library, "someone@else.com", BATCH, &[]).unwrap().is_empty());

        let parent = jobs[0].path[0].folder;
        let result = MoveResult { op: jobs[0].op, message: id, to: bulk, folders: vec![(parent, "F-bulk".into()), (bulk, "F-mkt".into())], outcome: Outcome::Moved("item-2".into()) };
        let r = record(&library, &[result]).unwrap();
        assert_eq!((r.moved, r.folders_made.clone()), (1, vec!["Bulk".to_string(), "Bulk/Marketing".to_string()]));
        assert!(library.pending_changes().unwrap().is_empty());
        let (sid, known): (Option<String>, i64) = library
            .conn
            .query_row("SELECT l.server_id, (SELECT COUNT(*) FROM server_items s WHERE s.folder = l.folder AND s.server_id = 'item-2') FROM locations l WHERE l.message = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((sid.as_deref(), known), (Some("item-2"), 1), "the download won't fetch it again");
        let left: i64 = library.conn.query_row("SELECT COUNT(*) FROM server_items WHERE folder = ?1 AND message = ?2", [inbox, id], |r| r.get(0)).unwrap();
        assert_eq!(left, 0, "its old record in the Inbox goes (else the Inbox looks complete and new mail isn't listed)");
        // and from there, a later move starts from its new id
        library.move_messages(&[id], bulk, inbox).unwrap();
        assert_eq!(plan(&library, "me@x.com", BATCH, &[]).unwrap()[0].item, "item-2");
    }

    #[test]
    fn an_opened_folder_is_looked_at_through_its_own_sign_in() {
        let (_dir, library, _id, [inbox, _, archive, bulk]) = mailbox();
        library.conn.execute("UPDATE folders SET server_id = 'F-inbox' WHERE id = ?1", [inbox]).unwrap();
        // (what the download records of what it has there)
        library.conn.execute("INSERT INTO server_items (folder, server_id, message) SELECT folder, server_id, message FROM locations WHERE folder = ?1", [inbox]).unwrap();
        let (jobs, rest) = look_jobs(&library, &[inbox, bulk, archive], "me@x.com").unwrap();
        // the Inbox, with what's here already; Bulk isn't on the server yet; the archive is never looked at
        assert_eq!(jobs.iter().map(|j| (j.folder, j.server_folder.as_str(), j.mailbox.clone())).collect::<Vec<_>>(), vec![(inbox, "F-inbox", None)]);
        assert!(jobs[0].known.contains("item-1"));
        assert!(rest.is_empty());
        // another Microsoft account's sign-in: left for its own pass
        let (jobs, rest) = look_jobs(&library, &[inbox], "someone@else.com").unwrap();
        assert!(jobs.is_empty());
        assert_eq!(rest, vec![inbox]);
    }

    #[test]
    fn a_message_put_back_where_it_was_moved_from_is_taken_out_again() {
        let (_dir, library, id, [inbox, _, _, bulk]) = mailbox();
        library.move_messages(&[id], inbox, bulk).unwrap();
        // the download found the server's copy still in the Inbox and put it back there
        library.conn.execute("INSERT INTO locations (message, folder, date, server_id) SELECT id, ?1, date, 'item-1' FROM messages WHERE id = ?2", [inbox, id]).unwrap();
        assert_eq!(drop_ghost_locations(&library).unwrap(), 1);
        let folders: Vec<i64> = library.conn.prepare("SELECT folder FROM locations WHERE message = ?1").unwrap().query_map([id], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(folders, vec![bulk], "only where it was moved to");
        assert_eq!(drop_ghost_locations(&library).unwrap(), 0);
    }

    #[test]
    fn failures_are_counted_and_given_up_then_retried_on_request() {
        let (_dir, library, id, [inbox, trash, _, _]) = mailbox();
        library.move_messages(&[id], inbox, trash).unwrap();
        let op = plan(&library, "me@x.com", BATCH, &[]).unwrap()[0].op;
        let fail = |e: &str| MoveResult { op, message: id, to: trash, folders: vec![], outcome: Outcome::Failed(e.into()) };
        record(&library, &[fail("canceled")]).unwrap();
        assert_eq!(waiting(&library).unwrap(), (1, 0, None), "a stopped download isn't a try");
        for _ in 0..GIVE_UP {
            record(&library, &[fail("Microsoft Graph 403 Forbidden")]).unwrap();
        }
        assert_eq!(waiting(&library).unwrap(), (0, 1, Some("Microsoft Graph 403 Forbidden".into())));
        assert!(plan(&library, "me@x.com", BATCH, &[]).unwrap().is_empty());
        assert_eq!(retry_given_up(&library).unwrap(), 1);
        assert_eq!(plan(&library, "me@x.com", BATCH, &[]).unwrap().len(), 1);
        // gone from the server meanwhile: dropped
        record(&library, &[MoveResult { outcome: Outcome::Gone, ..fail("") }]).unwrap();
        assert!(library.pending_changes().unwrap().is_empty());
    }

    /// An event change the server refused is counted as a try, with why, and the rest of the
    /// round is recorded (it had failed on a query given three values for two placeholders, so
    /// every round's moves went unrecorded while any event change was failing).
    #[test]
    fn a_refused_event_change_is_counted_and_the_round_still_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let op = |p: &str| {
            library.conn.execute("INSERT INTO pending_ops (kind, payload, created) VALUES ('event-change', ?1, 0)", [p]).unwrap();
            library.conn.last_insert_rowid()
        };
        let (plain, denied) = (op(r#"{"account":1,"action":"respond","answer":"decline","server_id":"A"}"#), op(r#"{"account":1,"action":"respond","answer":"accept","server_id":"B"}"#));
        let result = BatchResult {
            events_done: vec![(plain, None, Err("Microsoft Graph 400 ErrorInvalidRequest: no".into())), (denied, None, Err("Microsoft Graph 403 RemoteCalendarAccessDenied: Access is denied.".into()))],
            ..Default::default()
        };
        record_batch(&library, &result).unwrap();
        let tries = |id: i64| library.conn.query_row("SELECT attempts, last_error FROM pending_ops WHERE id = ?1", [id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).unwrap();
        assert_eq!(tries(plain).0, 1);
        assert!(tries(plain).1.contains("ErrorInvalidRequest"));
        assert_eq!(tries(denied).0, GIVE_UP, "access refused: given up at once");
    }
}
