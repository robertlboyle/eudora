//! Changes made in Eudora carried out on IMAP servers: moves (Transfer, Delete, the rules, the
//! spam filter) with the folders they go to made there, status (read, replied, forwarded),
//! priority (high = flagged), labels as IMAP keywords, and Empty Trash.
//!
//! The same queue as Microsoft 365's (`pending_ops`: "move" and "delete" of IMAP accounts,
//! "imap-mark"), the same three steps (`server_changes`): the download plans a batch (`plan`), a
//! thread of its own makes the changes (`carry_out`: no database there), and the writer
//! records them with `server_changes::record_batch`. A server id is "<UIDVALIDITY>:<UID>"; a
//! moved message's new UID is found by its Message-ID in the folder it went to.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::error::{Error, Result};
use crate::imap_sync::{Connection, ImapConfig};
use crate::library::Library;
use crate::server_changes::{BatchResult, FolderStep, GIVE_UP, MoveResult, Outcome};

/// Every label Eudora has put on an IMAP server as a keyword (setting; JSON list).
const KEYWORDS_USED: &str = "imap-keywords-used";

/// A queued move in an IMAP account.
#[derive(Clone, Debug)]
pub struct ImapMove {
    pub op: i64,
    pub message: i64,
    /// Its Message-ID (to find its new UID), when it has a real one.
    pub message_id: Option<String>,
    /// The folder it's in on the server (its server name) and its id there.
    pub from: String,
    pub item: String,
    pub to: i64,
    /// The target's path, top first; `server_id` = the server's name for the folder.
    pub path: Vec<FolderStep>,
}

/// A message's flags and keywords, for each place the account has it.
#[derive(Clone, Debug)]
pub struct ImapMark {
    pub op: i64,
    /// (folder's server name, the message's id there)
    pub targets: Vec<(String, String)>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ImapDelete {
    pub op: i64,
    pub folder: String,
    pub item: String,
}

/// One account's changes.
#[derive(Clone, Debug)]
pub struct ImapWork {
    pub config: ImapConfig,
    pub moves: Vec<ImapMove>,
    pub marks: Vec<ImapMark>,
    pub deletes: Vec<ImapDelete>,
    /// Folders made, renamed or deleted in Eudora (done first).
    pub folders: Vec<crate::folder_changes::FolderJob>,
}

#[derive(Clone, Debug, Default)]
pub struct ImapBatch {
    pub accounts: Vec<ImapWork>,
    /// Gmail accounts' changes (`gmail_changes`), on the same thread.
    pub gmail: Vec<crate::gmail_changes::GmailWork>,
    /// Contacts and calendars on CardDAV / CalDAV servers (`dav`), on the same thread.
    pub dav: Vec<crate::dav::DavWork>,
}

impl ImapBatch {
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.gmail.iter().all(|g| g.is_empty()) && self.dav.iter().all(|d| d.is_empty())
    }
}

/// A label as an IMAP keyword (an atom: no spaces or specials).
pub fn keyword(label: &str) -> String {
    label.chars().map(|c| if c.is_ascii_graphic() && !"(){%*\"\\]".contains(c) { c } else { '_' }).collect()
}

/// The flags Eudora sets and takes off for a message's state (others, set in other programs,
/// are left alone): (add, remove).
fn flags_for(state: i64, labels: &[String], every_label: &[String]) -> (Vec<String>, Vec<String>) {
    use crate::marks::{FORWARDED, REPLIED, UNREAD, priority_of};
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    let mut set = |on: bool, flag: &str| if on { add.push(flag.to_string()) } else { remove.push(flag.to_string()) };
    set(state & UNREAD == 0, "\\Seen");
    set(state & REPLIED != 0, "\\Answered");
    set(state & FORWARDED != 0, "$Forwarded");
    // priority set by hand: high = flagged; normal or low takes the flag off; none = left alone
    if let Some(p) = priority_of(state) {
        set(p <= 2, "\\Flagged");
    }
    for l in every_label {
        set(labels.contains(l), &keyword(l));
    }
    (add, remove)
}

/// The next batch: up to `limit` moves, marks and deletes of each IMAP account whose server
/// Eudora changes.
pub fn plan(library: &Library, limit: usize) -> Result<ImapBatch> {
    let conn = &library.conn;
    let gmail = crate::gmail_changes::plan(library, limit)?;
    let dav = crate::dav::plan(library, limit)?;
    let writable = library.imap_writable()?;
    if writable.is_empty() {
        return Ok(ImapBatch { accounts: Vec::new(), gmail, dav });
    }
    let configs: HashMap<i64, ImapConfig> = library
        .imap_accounts()?
        .into_iter()
        .filter_map(|c| library.imap_account_id(&c.address).ok().flatten().filter(|id| writable.contains(id)).map(|id| (id, c)))
        .collect();
    let mut work: HashMap<i64, ImapWork> = HashMap::new();
    fn entry<'a>(work: &'a mut HashMap<i64, ImapWork>, configs: &HashMap<i64, ImapConfig>, account: i64) -> Option<&'a mut ImapWork> {
        let config = configs.get(&account)?;
        Some(work.entry(account).or_insert_with(|| ImapWork { config: config.clone(), moves: Vec::new(), marks: Vec::new(), deletes: Vec::new(), folders: Vec::new() }))
    }
    let folder = |id: i64| -> Result<Option<(i64, String, Option<String>)>> {
        Ok(conn
            .query_row("SELECT account, path, server_id FROM folders WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    };

    // folders made, renamed or deleted
    for (account, job) in library.folder_jobs(&["imap"], limit, &|_, _| Some(None))? {
        if let Some(w) = entry(&mut work, &configs, account) {
            w.folders.push(job);
        }
    }

    // moves
    let ops: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.payload FROM pending_ops p JOIN folders f ON f.id = json_extract(p.payload, '$.from') JOIN accounts a ON a.id = f.account
             WHERE p.kind = 'move' AND p.attempts < ?1 AND a.kind = 'imap' ORDER BY p.id LIMIT ?2",
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
        let (Some((account, _, Some(from_name))), Some((_, to_path, _))) = (folder(from)?, folder(to)?) else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // a folder that's gone, or not on the server
            continue;
        };
        moving.push(message);
        let mut path = Vec::new();
        let mut so_far = String::new();
        for name in to_path.split('/') {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(name);
            let (folder, server_id): (i64, Option<String>) =
                conn.query_row("SELECT id, server_id FROM folders WHERE account = ?1 AND path = ?2", params![account, so_far], |r| Ok((r.get(0)?, r.get(1)?)))?;
            path.push(FolderStep { folder, name: name.to_string(), server_id });
        }
        let message_id: Option<String> = conn
            .query_row("SELECT message_id FROM messages WHERE id = ?1 AND message_id_made = 0", params![message], |r| r.get(0))
            .optional()?;
        if let Some(w) = entry(&mut work, &configs, account).filter(|w| w.moves.len() < limit) {
            w.moves.push(ImapMove { op, message, message_id, from: from_name, item: item.to_string(), to, path });
        }
    }

    // status, priority, labels
    let ops: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare("SELECT id, json_extract(payload, '$.message') FROM pending_ops WHERE kind = 'imap-mark' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![GIVE_UP, (limit * 2) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    // the labels Eudora manages as keywords: the label list, labels on messages, and every one
    // it has ever put on a server (a label taken off a message, even one not in the list, is
    // taken off there too); keywords other programs set are left alone
    let mut every_label: Vec<String> = library.label_list()?.into_iter().map(|l| l.name).collect();
    let mut used: Vec<String> = library.setting(KEYWORDS_USED)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    let known = used.len();
    {
        let mut stmt = conn.prepare("SELECT DISTINCT label FROM categories")?;
        for label in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let label = label?;
            if !used.contains(&label) {
                used.push(label);
            }
        }
    }
    for l in &used {
        if !every_label.contains(l) {
            every_label.push(l.clone());
        }
    }
    if used.len() != known {
        library.set_setting(KEYWORDS_USED, &serde_json::to_string(&used)?)?;
    }
    for (op, message) in ops {
        if moving.contains(&message) {
            continue; // its id changes with the move: after it
        }
        let Some(state) = conn.query_row("SELECT flags FROM messages WHERE id = ?1", params![message], |r| r.get::<_, i64>(0)).optional()? else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let places: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT f.account, f.server_id, l.server_id FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
                 WHERE l.message = ?1 AND a.kind = 'imap' AND l.server_id IS NOT NULL AND f.server_id IS NOT NULL",
            )?;
            stmt.query_map(params![message], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let places: Vec<_> = places.into_iter().filter(|p| configs.contains_key(&p.0)).collect();
        if places.is_empty() {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // no longer on a server Eudora changes
            continue;
        }
        let (add, remove) = flags_for(state, &library.labels_on(message)?, &every_label);
        // (one entry per message: done in the first account that has it; marks are per
        // message, so the same message in two IMAP accounts gets the state in both)
        let account = places[0].0;
        let targets = places.into_iter().map(|(_, f, item)| (f, item)).collect();
        if let Some(w) = entry(&mut work, &configs, account).filter(|w| w.marks.len() < limit) {
            w.marks.push(ImapMark { op, targets, add, remove });
        }
    }

    // Empty Trash
    let ops: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.payload FROM pending_ops p JOIN accounts a ON a.id = json_extract(p.payload, '$.account')
             WHERE p.kind = 'delete' AND p.attempts < ?1 AND a.kind = 'imap' ORDER BY p.id LIMIT ?2",
        )?;
        stmt.query_map(params![GIVE_UP, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, payload) in ops {
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let (Some(account), Some(item), Some(at)) = (v["account"].as_i64(), v["server_id"].as_str(), v["folder"].as_i64()) else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        let Some((_, _, Some(name))) = folder(at)? else {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        };
        if let Some(w) = entry(&mut work, &configs, account) {
            w.deletes.push(ImapDelete { op, folder: name, item: item.to_string() });
        }
    }
    let mut accounts: Vec<ImapWork> = work.into_values().collect();
    accounts.sort_by(|a, b| a.config.address.cmp(&b.config.address));
    Ok(ImapBatch { accounts, gmail, dav })
}

/// "<UIDVALIDITY>:<UID>" → (validity, uid).
fn parse_item(item: &str) -> Option<(u32, u32)> {
    let (v, u) = item.split_once(':')?;
    Some((v.parse().ok()?, u.parse().ok()?))
}

/// A mailbox name as an IMAP quoted string.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A folder name in IMAP's modified UTF-7 (RFC 3501 5.1.3; `imap_sync::decode_utf7` reads it).
pub fn encode_utf7(name: &str) -> String {
    use base64::Engine as _;
    let mut out = String::new();
    let mut run: Vec<u16> = Vec::new();
    let flush = |run: &mut Vec<u16>, out: &mut String| {
        if !run.is_empty() {
            let bytes: Vec<u8> = run.iter().flat_map(|u| u.to_be_bytes()).collect();
            out.push('&');
            out.push_str(&base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes).replace('/', ","));
            out.push('-');
            run.clear();
        }
    };
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut run, &mut out);
            if c == '&' { out.push_str("&-") } else { out.push(c) }
        } else {
            let mut buf = [0u16; 2];
            run.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut run, &mut out);
    out
}

fn err(what: &str, e: imap::Error) -> Error {
    match e {
        imap::Error::Io(e) => Error::Network(format!("{what}: {e}")),
        other => Error::Other(format!("{what}: {other}")),
    }
}

/// Makes the changes on the servers (the change thread: no database here). Each account signs
/// in once for its batch.
pub fn carry_out(batch: &ImapBatch) -> BatchResult {
    let mut result = BatchResult::default();
    for work in &batch.accounts {
        let fail_all = |result: &mut BatchResult, why: String| {
            for m in &work.moves {
                result.moves.push(MoveResult { op: m.op, message: m.message, to: m.to, folders: Vec::new(), outcome: Outcome::Failed(why.clone()) });
            }
            result.marks.extend(work.marks.iter().map(|m| (m.op, Outcome::Failed(why.clone()))));
            result.deletes.extend(work.deletes.iter().map(|d| (d.op, Outcome::Failed(why.clone()))));
            result.folders_done.extend(work.folders.iter().map(|f| crate::folder_changes::FolderDone { op: f.op, ids: Vec::new(), outcome: Err(why.clone()) }));
        };
        let mut session = match Connection::open(&work.config, None) {
            Ok(Connection::Imap(session, _)) => session,
            Ok(other) => {
                other.logout();
                fail_all(&mut result, "not an IMAP account".into());
                continue;
            }
            Err(e) => {
                // (a network error is no try: the change waits for the connection)
                let why = if matches!(e, Error::Network(_)) { "canceled".to_string() } else { e.to_string() };
                fail_all(&mut result, why);
                continue;
            }
        };
        let caps = session.capabilities().map(|c| (c.has_str("MOVE"), c.has_str("UIDPLUS"))).unwrap_or((false, false));
        let delimiter = session
            .list(Some(""), Some(""))
            .ok()
            .and_then(|l| l.iter().next().and_then(|n| n.delimiter().map(str::to_string)))
            .unwrap_or_else(|| "/".to_string());
        result.folders_done.extend(crate::folder_changes::carry_out_imap(&mut session, &delimiter, &work.folders));
        let mut made: HashMap<i64, String> = HashMap::new();
        for job in &work.moves {
            let mut folders = Vec::new();
            let outcome = move_one(&mut session, job, caps, &delimiter, &mut made, &mut folders).unwrap_or_else(|e| Outcome::Failed(e.to_string()));
            result.moves.push(MoveResult { op: job.op, message: job.message, to: job.to, folders, outcome });
        }
        for job in &work.marks {
            result.marks.push((job.op, mark_one(&mut session, job).unwrap_or_else(|e| Outcome::Failed(e.to_string()))));
        }
        for job in &work.deletes {
            result.deletes.push((job.op, delete_one(&mut session, job, caps.1).unwrap_or_else(|e| Outcome::Failed(e.to_string()))));
        }
        let _ = session.logout();
    }
    // Gmail accounts' changes, through the Gmail API
    let gmail = crate::gmail_changes::carry_out(&batch.gmail);
    result.moves.extend(gmail.moves);
    result.marks.extend(gmail.marks);
    result.contacts.extend(gmail.contacts);
    result.calendars.extend(gmail.calendars);
    result.folders_done.extend(gmail.folders_done);
    result.gmail_history.extend(gmail.gmail_history);
    result.cards_done.extend(gmail.cards_done);
    result.events_done.extend(gmail.events_done);
    // contacts and calendars on CardDAV / CalDAV servers
    let dav = crate::dav::carry_out(&batch.dav);
    result.contacts.extend(dav.contacts);
    result.calendars.extend(dav.calendars);
    result.cards_done.extend(dav.cards_done);
    result.events_done.extend(dav.events_done);
    result
}

type Session = crate::imap_sync::Session;

/// Opens a folder for changing; Err(Gone) when the item's UIDVALIDITY is no longer the folder's
/// (its ids were all renumbered: the download reads it again).
fn open(session: &mut Session, folder: &str, item: &str) -> Result<std::result::Result<u32, Outcome>> {
    let Some((validity, uid)) = parse_item(item) else { return Ok(Err(Outcome::Failed(format!("not an IMAP id: {item}")))) };
    let mailbox = session.select(folder).map_err(|e| err(&format!("opening {folder}"), e))?;
    if mailbox.uid_validity.is_some_and(|v| v != validity) {
        return Ok(Err(Outcome::Gone));
    }
    if session.uid_search(format!("UID {uid}")).map_err(|e| err("looking for the message", e))?.is_empty() {
        return Ok(Err(Outcome::Gone));
    }
    Ok(Ok(uid))
}

fn move_one(session: &mut Session, job: &ImapMove, (has_move, uidplus): (bool, bool), delimiter: &str, made: &mut HashMap<i64, String>, folders: &mut Vec<(i64, String)>) -> Result<Outcome> {
    // the target folder, made where the server doesn't have it yet
    let mut parent: Option<String> = None;
    for step in &job.path {
        let name = match step.server_id.clone().or_else(|| made.get(&step.folder).cloned()) {
            Some(name) => name,
            None => {
                let name = match &parent {
                    Some(p) => format!("{p}{delimiter}{}", encode_utf7(&step.name)),
                    None => encode_utf7(&step.name),
                };
                if let Err(e) = session.create(&name) {
                    // already there (made elsewhere meanwhile) is fine
                    let exists = session.list(Some(""), Some(&name)).map(|l| l.iter().next().is_some()).unwrap_or(false);
                    if !exists {
                        return Err(err(&format!("making the folder {}", step.name), e));
                    }
                }
                let _ = session.subscribe(&name);
                made.insert(step.folder, name.clone());
                folders.push((step.folder, name.clone()));
                name
            }
        };
        parent = Some(name);
    }
    let Some(target) = parent else { return Ok(Outcome::Failed("no folder to move to".into())) };
    let uid = match open(session, &job.from, &job.item)? {
        Ok(uid) => uid,
        Err(outcome) => return Ok(outcome),
    };
    if has_move {
        session.uid_mv(uid.to_string(), &target).map_err(|e| err("moving the message", e))?;
    } else {
        session.uid_copy(uid.to_string(), quoted(&target)).map_err(|e| err("copying the message", e))?;
        session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").map_err(|e| err("removing it from where it was", e))?;
        if uidplus {
            session.uid_expunge(uid.to_string()).map_err(|e| err("removing it from where it was", e))?;
        }
        // (without UIDPLUS it stays marked deleted there: a plain EXPUNGE could remove other
        // messages someone marked deleted)
    }
    // its id in the new folder: found by its Message-ID
    let new_id = match &job.message_id {
        Some(mid) if !mid.is_empty() => {
            let mailbox = session.examine(&target).map_err(|e| err(&format!("opening {target}"), e))?;
            let found = session.uid_search(format!("HEADER Message-ID {}", quoted(mid))).map_err(|e| err("finding the moved message", e))?;
            match (mailbox.uid_validity, found.into_iter().max()) {
                (Some(v), Some(u)) => format!("{v}:{u}"),
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    Ok(Outcome::Moved(new_id))
}

fn mark_one(session: &mut Session, job: &ImapMark) -> Result<Outcome> {
    for (folder, item) in &job.targets {
        let uid = match open(session, folder, item)? {
            Ok(uid) => uid,
            Err(_) => continue, // gone there: nothing to mark
        };
        if !job.add.is_empty() {
            session.uid_store(uid.to_string(), format!("+FLAGS.SILENT ({})", job.add.join(" "))).map_err(|e| err("marking the message", e))?;
        }
        if !job.remove.is_empty() {
            session.uid_store(uid.to_string(), format!("-FLAGS.SILENT ({})", job.remove.join(" "))).map_err(|e| err("marking the message", e))?;
        }
    }
    Ok(Outcome::Moved(String::new()))
}

fn delete_one(session: &mut Session, job: &ImapDelete, uidplus: bool) -> Result<Outcome> {
    let uid = match open(session, &job.folder, &job.item)? {
        Ok(uid) => uid,
        Err(outcome) => return Ok(outcome),
    };
    session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").map_err(|e| err("deleting the message", e))?;
    if uidplus {
        session.uid_expunge(uid.to_string()).map_err(|e| err("deleting the message", e))?;
    } else {
        // Empty Trash: the Trash folder is being emptied, so removing what's marked there is meant
        session.expunge().map_err(|e| err("deleting the message", e))?;
    }
    Ok(Outcome::Moved(String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_go_to_the_server_in_modified_utf7() {
        for name in ["Projects", "Caffè & Co", "日本語", "Bulk/Marketing"] {
            assert_eq!(crate::imap_sync::decode_utf7(&encode_utf7(name)), name);
        }
        assert_eq!(encode_utf7("Caffè"), "Caff&AOg-");
        assert_eq!(encode_utf7("A&B"), "A&-B");
    }

    #[test]
    fn imap_accounts_queue_their_changes_unless_switched_off() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("a.eml");
        std::fs::write(&src, "From: ann@x.com\r\nTo: me@y.com\r\nSubject: hi\r\nMessage-ID: <a1@x.com>\r\nDate: Fri, 1 Dec 2023 10:00:00 +0000\r\n\r\nhi\r\n").unwrap();
        crate::import(&mut library, &[src], &crate::ImportOptions::default(), &Silent).unwrap();
        let mut config = ImapConfig { address: "me@y.com".into(), server: "mail.y.com".into(), ..Default::default() };
        let account = library.save_imap_account(&config, None, None).unwrap();
        let inbox = crate::db::ensure_folder(&library.conn, account, "Inbox").unwrap();
        let archive = crate::db::ensure_folder(&library.conn, account, "Archive").unwrap();
        library.conn.execute("UPDATE folders SET server_id = 'INBOX' WHERE id = ?1", [inbox]).unwrap();
        let id: i64 = library.conn.query_row("SELECT id FROM messages", [], |r| r.get(0)).unwrap();
        library.conn.execute("UPDATE locations SET folder = ?1, server_id = '7:42' WHERE message = ?2", params![inbox, id]).unwrap();

        library.set_status(&[id], crate::marks::Status::Replied).unwrap();
        library.move_messages(&[id], inbox, archive).unwrap();
        let batch = plan(&library, 50).unwrap();
        assert_eq!(batch.accounts.len(), 1);
        let work = &batch.accounts[0];
        assert_eq!((work.moves[0].from.as_str(), work.moves[0].item.as_str()), ("INBOX", "7:42"));
        assert_eq!(work.moves[0].message_id.as_deref(), Some("a1@x.com"));
        assert_eq!(work.moves[0].path[0].server_id, None, "Archive isn't on the server yet: made there");
        assert!(work.marks.is_empty(), "the mark waits for the move (its id changes)");
        let marks: i64 = library.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = 'imap-mark'", [], |r| r.get(0)).unwrap();
        assert_eq!(marks, 1);
        let graph_marks: i64 = library.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = 'mark'", [], |r| r.get(0)).unwrap();
        assert_eq!(graph_marks, 0, "nothing for Microsoft 365");

        // switched off: changes stay in this library
        library.conn.execute("DELETE FROM pending_ops", []).unwrap();
        config.server_writes = Some(false);
        library.save_imap_account(&config, None, None).unwrap();
        library.move_messages(&[id], archive, inbox).unwrap();
        library.set_status(&[id], crate::marks::Status::Unread).unwrap();
        assert!(plan(&library, 50).unwrap().is_empty());
        let queued: i64 = library.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind IN ('move', 'imap-mark')", [], |r| r.get(0)).unwrap();
        assert_eq!(queued, 0);
    }

    #[test]
    fn eudoras_state_becomes_flags_and_keywords() {
        use crate::marks::{REPLIED, UNREAD};
        let labels = vec!["Work stuff".to_string(), "Home".to_string()];
        let (add, remove) = flags_for(REPLIED, &["Home".to_string()], &labels);
        assert!(add.contains(&"\\Seen".into()) && add.contains(&"\\Answered".into()) && add.contains(&"Home".into()));
        assert!(remove.contains(&"Work_stuff".into()) && remove.contains(&"$Forwarded".into()));
        assert!(!add.iter().chain(&remove).any(|f| f == "\\Flagged"), "no priority set: the flag is left alone");
        let (_, remove) = flags_for(UNREAD | (1 << 8), &[], &[]);
        assert!(remove.contains(&"\\Seen".into()));
        assert_eq!(keyword("a (b) \"c\""), "a__b___c_");
    }
}
