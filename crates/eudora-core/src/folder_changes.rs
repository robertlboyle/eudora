//! Mailboxes made, renamed and deleted in Eudora, and on the server.
//!
//! Each change is made in the library at once and queued (`pending_ops` kind `folder-change`)
//! for the server: Microsoft 365 folders are made, renamed, or moved to Deleted Items (as Outlook
//! deletes a folder) by the download's change thread; IMAP folders are made (CREATE), renamed
//! (RENAME) or renamed into the Trash. Folders of accounts with no server (classic Eudora, files)
//! change here only. The special folders (Inbox, Sent, Drafts, Deleted Items, Junk, Outbox,
//! Archive…) are never renamed or deleted, and the Online Archive is never changed.

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::db;
use crate::error::{Error, Result};
use crate::library::{Library, in_online_archive, now};
use crate::server_changes::{FolderStep, GIVE_UP};

pub const KIND: &str = "folder-change";

/// Top-level folders a mail server keeps for itself (by name, whatever role the server gave).
const SPECIAL: &[&str] = &[
    "inbox", "sent items", "sent", "sent messages", "drafts", "deleted items", "deleted messages", "trash", "junk email", "junk", "spam", "outbox", "out",
    "archive", "conversation history", "sync issues", "rss feeds", "rss subscriptions", "calendar", "contacts", "tasks", "notes", "journal", "online archive",
];

/// What happens to the folder on the server.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Create,
    Rename,
    /// Moved into the account's Deleted Items / Trash (as Outlook deletes a folder).
    Delete,
}

/// A queued folder change, ready for the server.
#[derive(Clone, Debug)]
pub struct FolderJob {
    pub op: i64,
    pub folder: i64,
    pub action: Action,
    /// Microsoft 365: a shared mailbox's address (None = the signed-in user's own).
    pub mailbox: Option<String>,
    /// The folder's path as it is now in Eudora, top first, with each step's server id (or IMAP
    /// name) when known: where it's made, or where its parent is.
    pub path: Vec<FolderStep>,
    /// Its id (Microsoft 365) or name (IMAP) on the server, when it's there.
    pub server_id: Option<String>,
    /// Delete: the Trash's id / name on the server (Microsoft 365 uses its well-known name).
    pub trash: Option<String>,
}

/// What a folder change did on the server: the server ids of folders made or moved, or why not
/// ("canceled" = the download stopped meanwhile: no try).
#[derive(Debug)]
pub struct FolderDone {
    pub op: i64,
    pub ids: Vec<(i64, String)>,
    pub outcome: std::result::Result<(), String>,
}

/// Whether a folder (by its path) is one of the server's own.
pub fn special(path: &str, role: Option<&str>) -> bool {
    role.is_some() && !path.contains('/') || !path.contains('/') && SPECIAL.contains(&path.to_lowercase().as_str()) || in_online_archive(path) || path == "Online Archive"
}

/// The kinds of accounts whose folders live on a server Eudora changes.
fn on_server(kind: &str) -> bool {
    matches!(kind, "graph" | "graph-shared" | "imap" | "gmail")
}

impl Library {
    fn folder_row(&self, folder: i64) -> Result<(i64, String, Option<String>, String)> {
        self.conn
            .query_row(
                "SELECT f.account, f.path, f.role, a.kind FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
                params![folder],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| Error::Other("that mailbox no longer exists".into()))
    }

    /// Queues a folder change for the server (accounts with one); one waiting create covers a
    /// rename made before it reached the server.
    fn queue_folder_change(&self, account_kind: &str, folder: i64, action: Action) -> Result<()> {
        if !on_server(account_kind) {
            return Ok(());
        }
        let has_server_id: bool = self.conn.query_row("SELECT server_id IS NOT NULL FROM folders WHERE id = ?1", params![folder], |r| r.get(0))?;
        let waiting_create: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM pending_ops WHERE kind = ?1 AND json_extract(payload, '$.folder') = ?2 AND json_extract(payload, '$.action') = 'create'",
                params![KIND, folder],
                |r| r.get(0),
            )
            .optional()?;
        match action {
            // not on the server yet: made there with its new name, or not at all
            Action::Rename if !has_server_id => return Ok(()),
            Action::Delete if !has_server_id => {
                if let Some(op) = waiting_create {
                    self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                }
                return Ok(());
            }
            _ => {}
        }
        let name = match action {
            Action::Create => "create",
            Action::Rename => "rename",
            Action::Delete => "delete",
        };
        self.conn.execute(
            "INSERT INTO pending_ops (created, kind, payload) VALUES (?1, ?2, ?3)",
            params![now(), KIND, json!({ "folder": folder, "action": name }).to_string()],
        )?;
        Ok(())
    }

    /// Changes a folder's path (and its folders inside'): `to` = the new whole path.
    fn repath_folder(&self, account: i64, folder: i64, from: &str, to: &str, parent: Option<i64>) -> Result<()> {
        let clash: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM folders WHERE account = ?1 AND (lower(path) = lower(?2) OR lower(path) LIKE lower(?2) || '/%') AND id <> ?3 AND path <> ?4 AND path NOT LIKE ?4 || '/%')",
            params![account, to, folder, from],
            |r| r.get(0),
        )?;
        if clash {
            return Err(Error::Other(format!("there's already a mailbox called {to}")));
        }
        let name = to.rsplit('/').next().unwrap_or(to);
        self.in_transaction(|| {
            self.conn.execute("UPDATE folders SET path = ?2, name = ?3, parent = ?4 WHERE id = ?1", params![folder, to, name, parent])?;
            self.conn.execute(
                "UPDATE folders SET path = ?3 || substr(path, length(?2) + 1) WHERE account = ?1 AND path LIKE ?2 || '/%'",
                params![account, from, to],
            )?;
            Ok(())
        })
    }

    /// Makes a mailbox (`path` with "/" for levels) in an account, and queues it for the server.
    pub fn create_folder(&self, account: i64, path: &str) -> Result<i64> {
        let path = path.split('/').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/");
        if path.is_empty() {
            return Err(Error::Other("the mailbox needs a name".into()));
        }
        if in_online_archive(&path) || path == "Online Archive" {
            return Err(Error::Other("mailboxes can't be made in the Online Archive (Eudora never changes it)".into()));
        }
        let exists: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM folders WHERE account = ?1 AND lower(path) = lower(?2))", params![account, path], |r| r.get(0))?;
        if exists {
            return Err(Error::Other(format!("there's already a mailbox called {path}")));
        }
        let kind: String = self.conn.query_row("SELECT kind FROM accounts WHERE id = ?1", params![account], |r| r.get(0))?;
        let id = db::ensure_folder(&self.conn, account, &path)?;
        self.queue_folder_change(&kind, id, Action::Create)?;
        Ok(id)
    }

    /// Renames a mailbox (its last name; the folders inside go with it), here and on the server.
    pub fn rename_folder(&self, folder: i64, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() || name.contains('/') {
            return Err(Error::Other("a mailbox's name can't be empty or hold a \"/\" (that makes levels)".into()));
        }
        let (account, path, role, kind) = self.folder_row(folder)?;
        if special(&path, role.as_deref()) {
            return Err(Error::Other(format!("{path} is one of the mail server's own folders, or in the Online Archive: it can't be renamed")));
        }
        let to = match path.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{name}"),
            None => name.to_string(),
        };
        if to == path {
            return Ok(to);
        }
        let parent: Option<i64> = self.conn.query_row("SELECT parent FROM folders WHERE id = ?1", params![folder], |r| r.get(0))?;
        self.repath_folder(account, folder, &path, &to, parent)?;
        self.queue_folder_change(&kind, folder, Action::Rename)?;
        Ok(to)
    }

    /// Deletes a mailbox as Outlook does: it (with its mail and the folders inside) goes into the
    /// account's Deleted Items / Trash, here and on the server. One already in the Trash is removed
    /// only when it holds nothing (Empty Trash takes the mail). Returns its new path, or None
    /// when it was removed.
    pub fn delete_folder(&self, folder: i64) -> Result<Option<String>> {
        let (account, path, role, kind) = self.folder_row(folder)?;
        if special(&path, role.as_deref()) {
            return Err(Error::Other(format!("{path} is one of the mail server's own folders, or in the Online Archive: it can't be deleted")));
        }
        let trash = self.trash_folder(account)?;
        let trash_path: String = self.conn.query_row("SELECT path FROM folders WHERE id = ?1", params![trash], |r| r.get(0))?;
        if path == trash_path || path.starts_with(&format!("{trash_path}/")) {
            let held: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM locations l JOIN folders f ON f.id = l.folder WHERE f.account = ?1 AND (f.path = ?2 OR f.path LIKE ?2 || '/%')",
                params![account, path],
                |r| r.get(0),
            )?;
            if held > 0 {
                return Err(Error::Other(format!("{path} is already in the Trash and still holds {held} messages: Empty Trash removes them")));
            }
            // empty, in the Trash: gone here; on the server it stays in the Trash until emptied there
            self.in_transaction(|| {
                self.conn.execute(
                    "DELETE FROM pending_ops WHERE kind = ?1 AND json_extract(payload, '$.folder') IN (SELECT id FROM folders WHERE account = ?2 AND (path = ?3 OR path LIKE ?3 || '/%'))",
                    params![KIND, account, path],
                )?;
                self.conn.execute(
                    "DELETE FROM folders WHERE account = ?1 AND (path = ?2 OR path LIKE ?2 || '/%') AND id NOT IN (SELECT folder FROM server_items) AND id NOT IN (SELECT folder FROM listed_folders)",
                    params![account, path],
                )?;
                Ok(())
            })?;
            return Ok(None);
        }
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        // a name not taken in the Trash ("Projects (2)")
        let mut to = format!("{trash_path}/{name}");
        let mut n = 2;
        while self.conn.query_row("SELECT EXISTS (SELECT 1 FROM folders WHERE account = ?1 AND lower(path) = lower(?2))", params![account, to], |r| r.get::<_, bool>(0))? {
            to = format!("{trash_path}/{name} ({n})");
            n += 1;
        }
        self.repath_folder(account, folder, &path, &to, Some(trash))?;
        self.queue_folder_change(&kind, folder, Action::Delete)?;
        // (the Views leave mail in the Trash out by its folders: nothing else to do)
        Ok(Some(to))
    }

    /// The path steps of a folder, top first, each with its server id when known.
    fn folder_steps(&self, folder: i64) -> Result<Vec<FolderStep>> {
        let (account, path): (i64, String) = self.conn.query_row("SELECT account, path FROM folders WHERE id = ?1", params![folder], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut steps = Vec::new();
        let mut so_far = String::new();
        for name in path.split('/') {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(name);
            let (id, server_id): (i64, Option<String>) =
                self.conn.query_row("SELECT id, server_id FROM folders WHERE account = ?1 AND path = ?2", params![account, so_far], |r| Ok((r.get(0)?, r.get(1)?)))?;
            steps.push(FolderStep { folder: id, name: name.to_string(), server_id });
        }
        Ok(steps)
    }

    /// The folder changes waiting for the server, for accounts of these kinds: (account, job).
    /// `mailbox_of` says which Microsoft 365 mailbox an account is (None = leave it for another
    /// sign-in); IMAP passes every account through.
    pub fn folder_jobs(&self, kinds: &[&str], limit: usize, mailbox_of: &dyn Fn(&str, Option<String>) -> Option<Option<String>>) -> Result<Vec<(i64, FolderJob)>> {
        let ops: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = ?1 AND attempts < ?2 ORDER BY id LIMIT ?3")?;
            stmt.query_map(params![KIND, GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut jobs = Vec::new();
        for (op, payload) in ops {
            if jobs.len() >= limit {
                break;
            }
            let v: serde_json::Value = serde_json::from_str(&payload)?;
            let action = match v["action"].as_str() {
                Some("create") => Action::Create,
                Some("rename") => Action::Rename,
                Some("delete") => Action::Delete,
                _ => {
                    self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                    continue;
                }
            };
            let Some(folder) = v["folder"].as_i64() else { continue };
            let row: Option<(i64, String, Option<String>, Option<String>)> = self
                .conn
                .query_row(
                    "SELECT f.account, a.kind, a.address, f.server_id FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
                    params![folder],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((account, kind, address, server_id)) = row else {
                self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // the folder is gone
                continue;
            };
            if !kinds.contains(&kind.as_str()) {
                continue;
            }
            let Some(mailbox) = mailbox_of(&kind, address) else { continue };
            let trash = match action {
                Action::Delete if kind == "imap" => {
                    let t = self.trash_folder(account)?;
                    self.conn.query_row("SELECT server_id FROM folders WHERE id = ?1", params![t], |r| r.get(0))?
                }
                Action::Delete if kind == "gmail" => Some("TRASH".to_string()),
                Action::Delete => Some("deleteditems".to_string()),
                _ => None,
            };
            jobs.push((account, FolderJob { op, folder, action, mailbox, path: self.folder_steps(folder)?, server_id, trash }));
        }
        Ok(jobs)
    }
}

/// Makes the folder changes on Microsoft 365 (the change thread: no database here).
pub fn carry_out_graph(client: &mut crate::graph::GraphClient, jobs: &[FolderJob]) -> Vec<FolderDone> {
    let mut done = Vec::with_capacity(jobs.len());
    for job in jobs {
        client.set_mailbox(job.mailbox.clone());
        let mut ids = Vec::new();
        let outcome = (|| -> Result<()> {
            match (job.action, &job.server_id) {
                // made (or, renamed before it was ever on the server, made with its new name)
                (Action::Create, _) | (Action::Rename, None) => {
                    let mut parent: Option<String> = None;
                    for step in &job.path {
                        let id = match &step.server_id {
                            Some(id) => id.clone(),
                            None => {
                                let id = client.ensure_folder(parent.as_deref(), &step.name)?;
                                ids.push((step.folder, id.clone()));
                                id
                            }
                        };
                        parent = Some(id);
                    }
                    Ok(())
                }
                (Action::Rename, Some(id)) => {
                    let name = job.path.last().map(|s| s.name.as_str()).unwrap_or_default();
                    client.rename_folder(id, name)
                }
                (Action::Delete, Some(id)) => {
                    let to = job.trash.as_deref().unwrap_or("deleteditems");
                    let new_id = client.move_folder(id, to)?;
                    if !new_id.is_empty() {
                        ids.push((job.folder, new_id));
                    }
                    Ok(())
                }
                (Action::Delete, None) => Ok(()),
            }
        })();
        let outcome = match outcome {
            Ok(()) => Ok(()),
            // gone there already (deleted in Outlook): nothing left to do
            Err(Error::Other(e)) if job.action != Action::Create && (e.contains(" 404") || e.contains("ErrorItemNotFound")) => Ok(()),
            Err(e) => Err(e.to_string()),
        };
        let stop = matches!(&outcome, Err(e) if e == "canceled");
        done.push(FolderDone { op: job.op, ids, outcome });
        if stop {
            break;
        }
    }
    client.set_mailbox(None);
    done
}

/// Makes the folder changes on an IMAP server, in a session already open.
pub fn carry_out_imap(session: &mut crate::imap_sync::Session, delimiter: &str, jobs: &[FolderJob]) -> Vec<FolderDone> {
    use crate::imap_changes::encode_utf7;
    let mut done = Vec::with_capacity(jobs.len());
    for job in jobs {
        let mut ids = Vec::new();
        // a name under its parent's server name (the parent made first where it isn't there)
        let name_under = |session: &mut crate::imap_sync::Session, parent_steps: &[FolderStep], name: &str, ids: &mut Vec<(i64, String)>| -> Result<String> {
            let mut parent: Option<String> = None;
            for step in parent_steps {
                let id = match &step.server_id {
                    Some(id) => id.clone(),
                    None => {
                        let n = match &parent {
                            Some(p) => format!("{p}{delimiter}{}", encode_utf7(&step.name)),
                            None => encode_utf7(&step.name),
                        };
                        let _ = session.create(&n);
                        let _ = session.subscribe(&n);
                        ids.push((step.folder, n.clone()));
                        n
                    }
                };
                parent = Some(id);
            }
            Ok(match parent {
                Some(p) => format!("{p}{delimiter}{}", encode_utf7(name)),
                None => encode_utf7(name),
            })
        };
        let (parents, last) = job.path.split_at(job.path.len().saturating_sub(1));
        let name = last.first().map(|s| s.name.clone()).unwrap_or_default();
        let outcome = (|| -> Result<()> {
            match (job.action, &job.server_id) {
                (Action::Create, _) | (Action::Rename, None) => {
                    let full = name_under(session, parents, &name, &mut ids)?;
                    if let Err(e) = session.create(&full) {
                        let exists = session.list(Some(""), Some(&full)).map(|l| l.iter().next().is_some()).unwrap_or(false);
                        if !exists {
                            return Err(Error::Other(format!("making the folder {name}: {e}")));
                        }
                    }
                    let _ = session.subscribe(&full);
                    ids.push((job.folder, full));
                    Ok(())
                }
                (Action::Rename, Some(old)) => {
                    let full = name_under(session, parents, &name, &mut ids)?;
                    session.rename(old, &full).map_err(|e| Error::Other(format!("renaming the folder: {e}")))?;
                    let _ = session.subscribe(&full);
                    ids.push((job.folder, full));
                    Ok(())
                }
                (Action::Delete, Some(old)) => {
                    let trash = job.trash.clone().unwrap_or_else(|| "Trash".into());
                    let full = format!("{trash}{delimiter}{}", encode_utf7(&name));
                    session.rename(old, &full).map_err(|e| Error::Other(format!("moving the folder to the Trash: {e}")))?;
                    let _ = session.subscribe(&full);
                    ids.push((job.folder, full));
                    Ok(())
                }
                (Action::Delete, None) => Ok(()),
            }
        })();
        done.push(FolderDone { op: job.op, ids, outcome: outcome.map_err(|e| e.to_string()) });
    }
    done
}

/// Records what the folder changes did (the writer's side): folders' server ids, the changes
/// done taken off the queue, failures counted. Returns (done, failed, last error).
pub fn record(library: &Library, done: &[FolderDone]) -> Result<(u64, u64, Option<String>)> {
    let conn = &library.conn;
    let (mut ok, mut failed, mut error) = (0, 0, None);
    library.in_transaction(|| {
        for d in done {
            for (folder, id) in &d.ids {
                conn.execute("UPDATE folders SET server_id = ?2 WHERE id = ?1", params![folder, id])?;
            }
            match &d.outcome {
                Ok(()) => {
                    conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![d.op])?;
                    ok += 1;
                }
                Err(e) if e == "canceled" => {}
                Err(e) => {
                    conn.execute("UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![d.op, e])?;
                    failed += 1;
                    error = Some(e.clone());
                }
            }
        }
        Ok(())
    })?;
    Ok((ok, failed, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        library.conn.execute("INSERT INTO accounts (id, kind, address, name, created) VALUES (7, 'graph', 'me@x.com', '', 0)", []).unwrap_or_else(|_| {
            library.conn.execute("INSERT INTO accounts (id, kind, address) VALUES (7, 'graph', 'me@x.com')", []).unwrap()
        });
        (dir, library)
    }

    #[test]
    fn folders_are_made_renamed_and_deleted_here_and_queued() {
        let (_dir, l) = library();
        db::ensure_folder(&l.conn, 7, "Deleted Items").unwrap();
        let id = l.create_folder(7, "Projects/Old").unwrap();
        let ops = |l: &Library| -> Vec<String> {
            let mut stmt = l.conn.prepare("SELECT json_extract(payload, '$.action') FROM pending_ops WHERE kind = 'folder-change' ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(ops(&l), vec!["create"]);
        // renamed before it reached the server: made there with its new name (no rename queued)
        assert_eq!(l.rename_folder(id, "Older").unwrap(), "Projects/Older");
        assert_eq!(ops(&l), vec!["create"]);
        // on the server now: a rename is queued; the folders inside go with it
        l.conn.execute("UPDATE folders SET server_id = 'S' || id", []).unwrap();
        l.conn.execute("DELETE FROM pending_ops", []).unwrap();
        let inside = l.create_folder(7, "Projects/Older/Inner").unwrap();
        l.conn.execute("DELETE FROM pending_ops", []).unwrap();
        let projects: i64 = l.conn.query_row("SELECT id FROM folders WHERE path = 'Projects'", [], |r| r.get(0)).unwrap();
        assert_eq!(l.rename_folder(projects, "Work").unwrap(), "Work");
        let inner: String = l.conn.query_row("SELECT path FROM folders WHERE id = ?1", [inside], |r| r.get(0)).unwrap();
        assert_eq!(inner, "Work/Older/Inner");
        assert_eq!(ops(&l), vec!["rename"]);
        // deleted: into Deleted Items, with what's inside
        assert_eq!(l.delete_folder(projects).unwrap().as_deref(), Some("Deleted Items/Work"));
        let inner: String = l.conn.query_row("SELECT path FROM folders WHERE id = ?1", [inside], |r| r.get(0)).unwrap();
        assert_eq!(inner, "Deleted Items/Work/Older/Inner");
        assert_eq!(ops(&l), vec!["rename", "delete"]);
        // the server's own folders and the Online Archive: never
        let inbox = l.create_folder(7, "Inbox").unwrap();
        assert!(l.rename_folder(inbox, "In").is_err() && l.delete_folder(inbox).is_err());
        assert!(l.create_folder(7, "Online Archive/X").is_err());
        assert!(l.rename_folder(id, "a/b").is_err());
    }

    #[test]
    fn an_empty_folder_in_the_trash_is_removed() {
        let (_dir, l) = library();
        let id = l.create_folder(7, "Deleted Items/Old").unwrap();
        assert_eq!(l.delete_folder(id).unwrap(), None);
        let left: bool = l.conn.query_row("SELECT EXISTS (SELECT 1 FROM folders WHERE id = ?1)", [id], |r| r.get(0)).unwrap();
        assert!(!left);
    }
}
