//! Reorganizing the Online Archive on the server.
//!
//! Archive mail filed into another folder in Eudora (by the spam filter, rules or by hand) is
//! moved on the server into the archive folder of the same path ("Lists" → "Online Archive/
//! Lists"), made there when missing. Microsoft's archive API has no move: the item's export
//! (Exchange's own, full-fidelity copy; Eudora keeps it, else it's exported now) is imported into
//! the new folder, then the original is soft-deleted (recoverable). A failed delete after an
//! import is tried again on its own, so nothing is left twice. Off until the user has seen what
//! it will do and agreed (setting `archive-refile` = "on").

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::error::{Error, Result};
use crate::graph::{GRAPH, GraphClient};
use crate::library::{Library, now};

pub const KIND: &str = "archive-move";
/// "on" once the user has agreed to reorganizing the archive on the server.
pub const SETTING: &str = "archive-refile";
/// Folders filed into that stay where they are on the server (the year folders keep their
/// mail): their messages are put back in their archive folder in Eudora instead.
const PUT_BACK: &[&str] = &["inbox"];

/// A queued archive move, ready for the server.
#[derive(Clone, Debug)]
pub struct ArchiveMoveJob {
    pub op: i64,
    pub message: i64,
    /// The archive folder it's in on the server: its id there.
    pub from_server: String,
    pub item: String,
    /// Where it goes, under the archive's top ("Bulk/Marketing").
    pub to_path: String,
    /// Exchange's export of it, when Eudora keeps one.
    pub export: Option<Vec<u8>>,
    /// Imported already (the delete failed): only the delete is left.
    pub imported: Option<String>,
}

/// What an archive move did: (its new id, its new folder's id, the folders made or found on the
/// way: (path under the archive's top, id)), or why not; `imported` when only the delete failed.
#[derive(Debug)]
pub struct ArchiveMoveDone {
    pub op: i64,
    pub outcome: std::result::Result<(String, String, Vec<(String, String)>), String>,
    pub imported: Option<String>,
}

/// What reorganizing will do, before it's switched on: messages per target folder, and those
/// put back where they were.
#[derive(Debug, Default, serde::Serialize)]
pub struct Preview {
    pub to: Vec<(String, u64)>,
    pub put_back: u64,
    pub messages: u64,
}

/// Archive mail filed outside the archive in Eudora: (message, its archive folder here, the
/// item's id there, where it's filed now, that folder's path). The moves already waiting are
/// read once (`NOT IN`): checked per row, each parsing every waiting move's JSON, it ran for
/// hours with 21,000 waiting and held the download's writer, so no new mail came in
/// (2026-10-23); now about 15 s on the user's library.
fn filed_elsewhere(library: &Library, limit: i64) -> Result<Vec<(i64, i64, String, i64, String)>> {
    let mut stmt = library.conn.prepare(
        "SELECT si.message, si.folder, si.server_id, l.folder, f2.path FROM server_items si
           JOIN folders f ON f.id = si.folder
           JOIN locations l ON l.message = si.message
           JOIN folders f2 ON f2.id = l.folder AND f2.account = f.account
         WHERE f.path LIKE 'Online Archive/%' AND f2.path NOT LIKE 'Online Archive/%' AND f2.path <> 'Online Archive'
           AND NOT EXISTS (SELECT 1 FROM locations l3 JOIN folders f3 ON f3.id = l3.folder WHERE l3.message = si.message AND f3.path LIKE 'Online Archive/%')
           AND si.message NOT IN (SELECT json_extract(p.payload, '$.message') FROM pending_ops p WHERE p.kind = ?1)
           AND NOT EXISTS (SELECT 1 FROM server_items s2 WHERE s2.message = si.message AND s2.folder = l.folder)
         GROUP BY si.message ORDER BY si.message LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![KIND, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

impl Library {
    /// What reorganizing the archive would do now (nothing changes).
    pub fn archive_refile_preview(&self) -> Result<Preview> {
        let mut p = Preview::default();
        let mut by: HashMap<String, u64> = HashMap::new();
        for (_, _, _, _, path) in filed_elsewhere(self, i64::MAX)? {
            p.messages += 1;
            if PUT_BACK.contains(&path.to_lowercase().as_str()) {
                p.put_back += 1;
            } else {
                *by.entry(path).or_default() += 1;
            }
        }
        p.to = by.into_iter().collect();
        p.to.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(p)
    }

    /// Queues the archive moves of archive mail filed outside the archive (up to `limit`), and
    /// puts back in their archive folder those filed into a folder that stays as it is (the
    /// Inbox). Only when switched on. Returns (queued, put back).
    pub fn queue_archive_refiling(&self, limit: usize) -> Result<(u64, u64)> {
        if self.setting(SETTING)?.as_deref() != Some("on") {
            return Ok((0, 0));
        }
        // Looked for only while few moves wait, and at most every 30 minutes: the search takes
        // seconds, and it runs in the download's writer, ahead of storing new mail.
        let waiting: i64 = self.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = ?1", params![KIND], |r| r.get(0))?;
        if waiting >= 500 {
            return Ok((0, 0));
        }
        {
            static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            if !cfg!(test) && last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(30 * 60)) {
                return Ok((0, 0));
            }
            *last = Some(std::time::Instant::now());
            crate::activity::next("archive", "Reorganizing the Online Archive", Some(now() + 30 * 60));
        }
        let rows = filed_elsewhere(self, limit as i64)?;
        let (mut queued, mut back) = (0, 0);
        self.in_transaction(|| {
            for (message, from, item, here, path) in &rows {
                // an account whose mail is kept local (`local_only`): its archive stays as it is
                let account: i64 = self.conn.query_row("SELECT account FROM folders WHERE id = ?1", params![here], |r| r.get(0))?;
                if self.local_only_account(account, crate::local_only::Service::Mail)? {
                    continue;
                }
                if PUT_BACK.contains(&path.to_lowercase().as_str()) {
                    self.conn.execute("UPDATE OR IGNORE locations SET folder = ?3, server_id = ?4 WHERE message = ?1 AND folder = ?2", params![message, here, from, item])?;
                    self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
                    back += 1;
                    continue;
                }
                self.conn.execute(
                    "INSERT INTO pending_ops (created, kind, payload) VALUES (?1, ?2, ?3)",
                    params![now(), KIND, json!({ "message": message, "from": from, "item": item, "here": here, "to_path": path }).to_string()],
                )?;
                queued += 1;
            }
            Ok(())
        })?;
        Ok((queued, back))
    }

    /// The archive moves waiting, ready for the server (each with its export when Eudora keeps one).
    pub fn archive_move_jobs(&self, limit: usize, account: &str) -> Result<Vec<ArchiveMoveJob>> {
        if self.setting(SETTING)?.as_deref() != Some("on") {
            return Ok(Vec::new());
        }
        let ops: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = ?1 AND attempts < ?2 ORDER BY id LIMIT ?3")?;
            stmt.query_map(params![KIND, crate::server_changes::GIVE_UP, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut jobs = Vec::new();
        for (op, payload) in ops {
            let v: serde_json::Value = serde_json::from_str(&payload)?;
            let (Some(message), Some(from), Some(item), Some(to_path)) = (v["message"].as_i64(), v["from"].as_i64(), v["item"].as_str(), v["to_path"].as_str()) else {
                self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                continue;
            };
            // (the archive of the account signed in here only)
            let owner: Option<String> = self.conn.query_row("SELECT a.address FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1", params![from], |r| r.get(0)).optional()?.flatten();
            if !owner.is_some_and(|o| o.eq_ignore_ascii_case(account)) {
                continue;
            }
            let from_server: Option<String> = self.conn.query_row("SELECT server_id FROM folders WHERE id = ?1", params![from], |r| r.get(0)).optional()?.flatten();
            let Some(from_server) = from_server else {
                self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                continue;
            };
            let copy: Option<i64> = self
                .conn
                .query_row("SELECT id FROM copies WHERE message = ?1 AND format = 'exchange-export' ORDER BY (server_id = ?2) DESC, id LIMIT 1", params![message, item], |r| r.get(0))
                .optional()?;
            let export = match copy {
                Some(c) => self.copy_bytes(c).ok(),
                None => None,
            };
            jobs.push(ArchiveMoveJob {
                op,
                message,
                from_server,
                item: item.to_string(),
                to_path: to_path.to_string(),
                export,
                imported: v["imported"].as_str().map(str::to_string),
            });
        }
        Ok(jobs)
    }

    /// Records what archive moves did (the writer's side): the message in its new archive folder
    /// here, its new id known (so it isn't downloaded again), the change taken off the queue.
    /// Returns (moved, failed, last error).
    pub fn record_archive_moves(&self, done: &[ArchiveMoveDone]) -> Result<(u64, u64, Option<String>)> {
        let (mut moved, mut failed, mut error) = (0, 0, None);
        self.in_transaction(|| {
            for d in done {
                let payload: Option<String> = self.conn.query_row("SELECT payload FROM pending_ops WHERE id = ?1", params![d.op], |r| r.get(0)).optional()?;
                let Some(payload) = payload else { continue };
                let v: serde_json::Value = serde_json::from_str(&payload)?;
                match &d.outcome {
                    Ok((new_id, folder_id, made)) => {
                        let (message, from, here) = (v["message"].as_i64().unwrap_or(0), v["from"].as_i64().unwrap_or(0), v["here"].as_i64().unwrap_or(0));
                        let item = v["item"].as_str().unwrap_or("");
                        let to_path = v["to_path"].as_str().unwrap_or("");
                        let account: i64 = self.conn.query_row("SELECT account FROM folders WHERE id = ?1", params![from], |r| r.get(0))?;
                        // the archive's folders made or found on the way, with their ids
                        for (path, id) in made {
                            let f = crate::db::ensure_folder(&self.conn, account, &format!("Online Archive/{path}"))?;
                            self.conn.execute("UPDATE folders SET server_id = ?2 WHERE id = ?1 AND server_id IS NULL", params![f, id])?;
                        }
                        let target = crate::db::ensure_folder(&self.conn, account, &format!("Online Archive/{to_path}"))?;
                        self.conn.execute("UPDATE folders SET server_id = ?2 WHERE id = ?1 AND server_id IS NULL", params![target, folder_id])?;
                        self.conn.execute("DELETE FROM server_items WHERE folder = ?1 AND server_id = ?2 AND message = ?3", params![from, item, message])?;
                        self.conn.execute("INSERT OR IGNORE INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)", params![target, new_id, message])?;
                        // filed here: from where it was in Eudora into the archive's side of that folder
                        let changed = self.conn.execute("UPDATE OR IGNORE locations SET folder = ?3, server_id = ?4 WHERE message = ?1 AND folder = ?2", params![message, here, target, new_id])?;
                        if changed == 0 {
                            let date: i64 = self.conn.query_row("SELECT date FROM messages WHERE id = ?1", params![message], |r| r.get(0))?;
                            self.conn.execute("INSERT OR IGNORE INTO locations (message, folder, date, server_id) VALUES (?1, ?2, ?3, ?4)", params![message, target, date, new_id])?;
                        }
                        self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
                        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![d.op])?;
                        moved += 1;
                        self.log_job("archive", Some(message), &format!("Moved inside the Online Archive to {}", self.folder_path_of(target)))?;
                    }
                    Err(e) if e == "canceled" => {
                        if let Some(id) = &d.imported {
                            self.conn.execute("UPDATE pending_ops SET payload = json_set(payload, '$.imported', ?2) WHERE id = ?1", params![d.op, id])?;
                        }
                    }
                    Err(e) => {
                        // imported but not yet deleted there: only the delete is tried again
                        if let Some(id) = &d.imported {
                            self.conn.execute("UPDATE pending_ops SET payload = json_set(payload, '$.imported', ?2) WHERE id = ?1", params![d.op, id])?;
                        }
                        self.conn.execute("UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![d.op, e])?;
                        let message: Option<i64> = self.conn.query_row("SELECT json_extract(payload, '$.message') FROM pending_ops WHERE id = ?1", params![d.op], |r| r.get(0)).ok().flatten();
                        self.log_job("archive", message, &format!("Move inside the Online Archive failed, tried again later: {e}"))?;
                        failed += 1;
                        error = Some(e.clone());
                    }
                }
            }
            Ok(())
        })?;
        Ok((moved, failed, error))
    }
}

/// The archive as the change thread knows it: its base URL, and where each of its folders really
/// lives (auto-expanding archives span several mailboxes). Read once an hour.
struct ArchiveMap {
    at: std::time::Instant,
    base: String,
    owners: HashMap<String, String>,
    /// Folders under a parent (None: the top): (name lowercase → id).
    children: HashMap<Option<String>, HashMap<String, String>>,
}

static MAP: std::sync::Mutex<Option<ArchiveMap>> = std::sync::Mutex::new(None);

fn archive_map(client: &mut GraphClient) -> Result<()> {
    let mut map = MAP.lock().map_err(|e| Error::Other(e.to_string()))?;
    if map.as_ref().is_some_and(|m| m.at.elapsed() < std::time::Duration::from_secs(3600)) {
        return Ok(());
    }
    client.set_mailbox(None);
    let (_, archive) = client.mailbox_ids()?;
    let archive = archive.ok_or_else(|| Error::Other("this account has no Online Archive".into()))?;
    let folders = client.mailbox_folders(&archive, &|_| {})?;
    let owners = folders.iter().map(|f| (f.id.clone(), GraphClient::folder_owner(&archive, f))).collect();
    *map = Some(ArchiveMap { at: std::time::Instant::now(), base: format!("{GRAPH}/admin/exchange/mailboxes/{archive}"), owners, children: HashMap::new() });
    Ok(())
}

/// The id of a folder under the archive's top by its path, made where missing (in the archive's
/// main mailbox); the folders found or made on the way.
fn archive_folder(client: &mut GraphClient, path: &str) -> Result<(String, Vec<(String, String)>)> {
    let mut parent: Option<String> = None;
    let mut so_far = String::new();
    let mut made = Vec::new();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        if !so_far.is_empty() {
            so_far.push('/');
        }
        so_far.push_str(name);
        let base = MAP.lock().ok().and_then(|m| m.as_ref().map(|m| m.base.clone())).unwrap_or_default();
        let known = MAP.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.children.get(&parent).cloned()));
        let children = match known {
            Some(c) => c,
            None => {
                let c: HashMap<String, String> = client.mailbox_children(&base, parent.as_deref())?.into_iter().map(|(id, n)| (n.to_lowercase(), id)).collect();
                if let Ok(mut m) = MAP.lock() {
                    if let Some(m) = m.as_mut() {
                        m.children.insert(parent.clone(), c.clone());
                    }
                }
                c
            }
        };
        let id = match children.get(&name.to_lowercase()) {
            Some(id) => id.clone(),
            None => {
                let id = client.mailbox_make_folder(&base, parent.as_deref(), name)?;
                if let Ok(mut m) = MAP.lock() {
                    if let Some(m) = m.as_mut() {
                        m.children.entry(parent.clone()).or_default().insert(name.to_lowercase(), id.clone());
                    }
                }
                id
            }
        };
        made.push((so_far.clone(), id.clone()));
        parent = Some(id);
    }
    let id = parent.ok_or_else(|| Error::Other("no folder to move to".into()))?;
    Ok((id, made))
}

/// Makes the archive moves on the server (the change thread: no database here): each item
/// imported into its folder (from its export), then the original soft-deleted.
pub fn carry_out(client: &mut GraphClient, jobs: &[ArchiveMoveJob]) -> Vec<ArchiveMoveDone> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let fail_all = |why: String| jobs.iter().map(|j| ArchiveMoveDone { op: j.op, outcome: Err(why.clone()), imported: j.imported.clone() }).collect();
    if let Err(e) = archive_map(client) {
        return fail_all(e.to_string());
    }
    let base = MAP.lock().ok().and_then(|m| m.as_ref().map(|m| m.base.clone())).unwrap_or_default();
    // The target folders first, one at a time (two workers never make the same folder twice)
    let mut targets: HashMap<String, std::result::Result<(String, Vec<(String, String)>), String>> = HashMap::new();
    for job in jobs {
        if !targets.contains_key(&job.to_path) {
            let made = archive_folder(client, &job.to_path).map_err(|e| e.to_string());
            targets.insert(job.to_path.clone(), made);
        }
    }
    // Then the items, PARALLEL at once:
    // Exchange takes 4 requests at a time from an app per mailbox; more are throttled.
    let chunks: Vec<Vec<&ArchiveMoveJob>> = (0..PARALLEL).map(|w| jobs.iter().skip(w).step_by(PARALLEL).collect()).collect();
    let results: Vec<Vec<ArchiveMoveDone>> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .into_iter()
            .filter(|c| !c.is_empty())
            .map(|chunk| {
                let mut worker = client.worker();
                let (base, targets) = (&base, &targets);
                s.spawn(move || move_items(&mut worker, base, targets, &chunk))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    let mut done: Vec<ArchiveMoveDone> = results.into_iter().flatten().collect();
    // (jobs a worker didn't reach, being stopped, are simply left for the next round)
    done.sort_by_key(|d| d.op);
    done
}

/// Requests at once for archive moves: Exchange's limit per app and mailbox.
const PARALLEL: usize = 4;

/// One worker's share of the archive moves, one after another.
fn move_items(
    client: &mut GraphClient,
    base: &str,
    targets: &HashMap<String, std::result::Result<(String, Vec<(String, String)>), String>>,
    jobs: &[&ArchiveMoveJob],
) -> Vec<ArchiveMoveDone> {
    let session = match client.import_session(base) {
        Ok(s) => s,
        Err(e) => return jobs.iter().map(|j| ArchiveMoveDone { op: j.op, outcome: Err(e.to_string()), imported: j.imported.clone() }).collect(),
    };
    let base = base.to_string();
    let mut done = Vec::with_capacity(jobs.len());
    for &job in jobs {
        let mut owner = MAP.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.owners.get(&job.from_server).cloned())).unwrap_or_else(|| base.clone());
        // (an archive folder whose mail Exchange moved into an auxiliary mailbox: Microsoft names
        // it in its answer; the request goes there, and the folder is remembered as living there)
        let follow = |error: &Error, owner: &mut String| -> bool {
            match crate::graph::moved_to(&error.to_string()) {
                Some(there) if there != *owner => {
                    *owner = there.clone();
                    if let Ok(mut m) = MAP.lock() {
                        if let Some(m) = m.as_mut() {
                            m.owners.insert(job.from_server.clone(), there);
                        }
                    }
                    true
                }
                _ => false,
            }
        };
        let mut imported = job.imported.clone();
        let outcome = (|| -> Result<(String, String, Vec<(String, String)>)> {
            let (folder, made) = match targets.get(&job.to_path) {
                Some(Ok(t)) => t.clone(),
                Some(Err(e)) => return Err(Error::Other(e.clone())),
                None => archive_folder(client, &job.to_path)?,
            };
            let new_id = match &imported {
                Some(id) => id.clone(),
                None => {
                    let export = match &job.export {
                        Some(e) => e.clone(),
                        None => {
                            let _timed = crate::speed::Timer::new("Archive move: export");
                            let got = match client.export_items(&owner, std::slice::from_ref(&job.item)) {
                                Ok(got) => got,
                                Err(e) if follow(&e, &mut owner) => client.export_items(&owner, std::slice::from_ref(&job.item))?,
                                Err(e) => return Err(e),
                            };
                            let mut item = got.into_iter().next().ok_or_else(|| Error::Other("the archive exported nothing".into()))?;
                            // (Microsoft says "moved" in the item's own answer, not as the
                            // request's error: followed the same way, 2026-10-23)
                            if item.data.is_none() {
                                let said = Error::Other(format!("ErrorArchiveFolderMovedPermanently {}", item.error.clone().unwrap_or_default()));
                                if item.error.as_deref().is_some_and(|e| e.contains("ErrorArchiveFolderMovedPermanently")) && follow(&said, &mut owner) {
                                    item = client.export_items(&owner, std::slice::from_ref(&job.item))?.into_iter().next().ok_or_else(|| Error::Other("the archive exported nothing".into()))?;
                                }
                            }
                            match (item.data, item.error) {
                                (Some(d), _) => d,
                                (None, e) => return Err(Error::Other(format!("the archive wouldn't export it: {}", e.unwrap_or_default()))),
                            }
                        }
                    };
                    let id = {
                        let _timed = crate::speed::Timer::new("Archive move: import");
                        client.import_item(&session, &folder, &export)?
                    };
                    imported = Some(id.clone());
                    id
                }
            };
            let _timed = crate::speed::Timer::new("Archive move: delete");
            match client.mailbox_delete_item(&owner, &job.from_server, &job.item) {
                Ok(()) => {}
                Err(e) if follow(&e, &mut owner) => client.mailbox_delete_item(&owner, &job.from_server, &job.item)?,
                Err(e) => return Err(e),
            }
            Ok((new_id, folder, made))
        })();
        let outcome = outcome.map_err(|e| e.to_string());
        let stop = matches!(&outcome, Err(e) if e == "canceled");
        done.push(ArchiveMoveDone { op: job.op, outcome, imported });
        if stop {
            break;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    #[test]
    fn archive_mail_filed_elsewhere_is_queued_or_put_back_and_recorded_when_moved() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        // two stored messages (imported), then filed as the test needs
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files: Vec<_> = (1..=2)
            .map(|n| {
                let p = src.join(format!("{n}.eml"));
                std::fs::write(&p, format!("From: a@b.example
Subject: s{n}
Message-ID: <{n}@x>
Date: Mon, 1 Jan 2024 10:00:00 +0000

hi
")).unwrap();
                p
            })
            .collect();
        crate::import(&mut l, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        let ids: Vec<i64> = l.conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        l.conn.execute("DELETE FROM locations", []).unwrap();
        let account = crate::db::graph_account(&l.conn, "graph", "me@x.com", "", 0).unwrap();
        let year = crate::db::ensure_folder(&l.conn, account, "Online Archive/Inbox/Inbox_2021").unwrap();
        l.conn.execute("UPDATE folders SET server_id = 'F2021' WHERE id = ?1", [year]).unwrap();
        let lists = crate::db::ensure_folder(&l.conn, account, "Lists").unwrap();
        let inbox = crate::db::ensure_folder(&l.conn, account, "Inbox").unwrap();
        let mail = |n: i64, filed: i64| {
            l.conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)", params![year, format!("I{n}"), n]).unwrap();
            l.conn.execute("INSERT INTO locations (message, folder, date) VALUES (?1, ?2, 1)", params![n, filed]).unwrap();
        };
        mail(ids[0], lists);
        mail(ids[1], inbox);
        let (one, two) = (ids[0], ids[1]);
        let p = l.archive_refile_preview().unwrap();
        assert_eq!((p.messages, p.put_back, p.to.clone()), (2, 1, vec![("Lists".to_string(), 1)]));
        // off until agreed
        assert_eq!(l.queue_archive_refiling(10).unwrap(), (0, 0));
        l.set_setting(SETTING, "on").unwrap();
        assert_eq!(l.queue_archive_refiling(10).unwrap(), (1, 1));
        let back: i64 = l.conn.query_row("SELECT folder FROM locations WHERE message = ?1", [two], |r| r.get(0)).unwrap();
        assert_eq!(back, year, "filed into the Inbox: put back in its year folder");
        assert_eq!(l.queue_archive_refiling(10).unwrap(), (0, 0), "queued once");
        let op: i64 = l.conn.query_row("SELECT id FROM pending_ops WHERE kind = ?1", [KIND], |r| r.get(0)).unwrap();
        // the delete failed after the import: only the delete is left
        l.record_archive_moves(&[ArchiveMoveDone { op, outcome: Err("boom".into()), imported: Some("NEW".into()) }]).unwrap();
        let imported: String = l.conn.query_row("SELECT json_extract(payload, '$.imported') FROM pending_ops WHERE id = ?1", [op], |r| r.get(0)).unwrap();
        assert_eq!(imported, "NEW");
        l.record_archive_moves(&[ArchiveMoveDone { op, outcome: Ok(("NEW".into(), "FLISTS".into(), vec![("Lists".into(), "FLISTS".into())])), imported: Some("NEW".into()) }]).unwrap();
        let (folder, sid): (String, String) = l.conn.query_row("SELECT f.path, l.server_id FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1", [one], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((folder.as_str(), sid.as_str()), ("Online Archive/Lists", "NEW"));
        let known: i64 = l.conn.query_row("SELECT COUNT(*) FROM server_items WHERE message = ?1 AND server_id = 'NEW'", [one], |r| r.get(0)).unwrap();
        assert_eq!(known, 1);
        let left: i64 = l.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = ?1", [KIND], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
    }
}
