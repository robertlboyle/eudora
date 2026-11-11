//! Filing sync in the library: turning this computer's filing (moves no server holds, noted in
//! `filing_pending` by `Library::move_messages`) into changes of an account's log, and filing
//! messages as a log says. The log's format is `filing_sync`; carrying it between computers is
//! the sync's.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::error::{Error, Result};
use crate::filing_sync::{AS_ON_SERVER, DELETED, Key, KeySet, Log, message_hash, short_of};
use crate::library::Library;

/// What applying a log did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    /// Messages filed (moved here as the log says).
    pub filed: u64,
    /// Already where the log says.
    pub already: u64,
    /// Removed for good (deleted on another computer).
    pub deleted: u64,
    /// Keys with no message here (deleted here, not downloaded yet, an archive not imported
    /// here): kept in the log, applied when the message arrives.
    pub missing: u64,
}

/// An account's messages by key, and their keys (a short key two messages share here names
/// neither: those go by their long keys).
pub struct AccountKeys {
    pub by_key: HashMap<Key, i64>,
    pub of: HashMap<i64, Key>,
}

impl Library {
    /// The account's id for an address (lowercase match).
    pub fn account_by_address(&self, address: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM accounts WHERE lower(address) = lower(?1) ORDER BY id LIMIT 1", params![address], |r| r.get(0))
            .optional()?)
    }

    /// Every message of an account (in any of its folders) by key.
    pub fn account_keys(&self, account: i64) -> Result<AccountKeys> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT m.id, m.message_id FROM locations l JOIN folders f ON f.id = l.folder JOIN messages m ON m.id = l.message WHERE f.account = ?1",
        )?;
        let rows: Vec<(i64, String)> = stmt.query_map(params![account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let hashes: Vec<(i64, u64)> = rows.iter().map(|(id, mid)| (*id, message_hash(mid))).collect();
        let mut shorts: HashMap<u32, u32> = HashMap::new();
        for (_, h) in &hashes {
            *shorts.entry(short_of(*h)).or_default() += 1;
        }
        let mut keys = AccountKeys { by_key: HashMap::with_capacity(hashes.len() * 2), of: HashMap::with_capacity(hashes.len()) };
        for (id, h) in hashes {
            let long = Key::Long(h);
            keys.by_key.insert(long, id);
            let key = if shorts[&short_of(h)] == 1 {
                let short = Key::Short(short_of(h));
                keys.by_key.insert(short, id);
                short
            } else {
                long
            };
            keys.of.insert(id, key);
        }
        Ok(keys)
    }

    /// This computer's filing of an account not yet in its log: added to `log` as changes (one
    /// per folder; each message where it ended up). Returns the pending rows taken: delete them
    /// (`clear_filing_pending`) once the log is safely stored. A message filed and put back
    /// before it was ever synced leaves nothing.
    pub fn take_filing_pending(&self, account: i64, log: &mut Log, device: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id, message, folder, at FROM filing_pending WHERE account = ?1 ORDER BY id")?;
        let rows: Vec<(i64, i64, String, i64)> = stmt.query_map(params![account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let keys = self.account_keys(account)?;
        let known = log.resolve();
        let mut last: HashMap<i64, (String, i64)> = HashMap::new();
        for (_, message, folder, at) in &rows {
            last.insert(*message, (folder.clone(), *at));
        }
        let mut by_folder: HashMap<String, (Vec<Key>, i64)> = HashMap::new();
        for (message, (folder, at)) in last {
            let Some(key) = keys.of.get(&message).copied() else { continue }; // gone meanwhile
            if folder == AS_ON_SERVER && !known.contains_key(&key) {
                continue;
            }
            let entry = by_folder.entry(folder).or_default();
            entry.0.push(key);
            entry.1 = entry.1.max(at);
        }
        let mut folders: Vec<(String, (Vec<Key>, i64))> = by_folder.into_iter().collect();
        folders.sort_by(|a, b| a.1.1.cmp(&b.1.1).then(a.0.cmp(&b.0)));
        for (folder, (list, at)) in folders {
            log.add(device, at, &folder, KeySet::from_keys(list));
        }
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    pub fn clear_filing_pending(&self, ids: &[i64]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached("DELETE FROM filing_pending WHERE id = ?1")?;
        for id in ids {
            stmt.execute(params![id])?;
        }
        Ok(())
    }

    /// Files the account's messages as its log says (the account by the log's address).
    /// Filing done here isn't noted again, and nothing goes to a server: what a log holds is
    /// filing no server keeps. Keys with no message here are counted and left for later.
    pub fn apply_filing(&self, log: &Log) -> Result<Applied> {
        let account = self.account_by_address(&log.account)?.ok_or_else(|| Error::Other(format!("no account {} here", log.account)))?;
        let keys = self.account_keys(account)?;
        let mut done = Applied::default();
        let mut folders: HashMap<String, i64> = HashMap::new();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let mut here_stmt = self.conn.prepare_cached("SELECT l.folder, l.server_id FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1 AND f.account = ?2")?;
            let mut home_stmt = self.conn.prepare_cached("SELECT s.folder FROM server_items s JOIN folders f ON f.id = s.folder WHERE s.message = ?1 AND f.account = ?2 LIMIT 1")?;
            for (key, folder) in log.resolve() {
                let Some(message) = keys.by_key.get(&key).copied() else {
                    done.missing += 1;
                    continue;
                };
                let here: Vec<(i64, Option<String>)> = here_stmt.query_map(params![message, account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                if folder == DELETED {
                    if !here.is_empty() {
                        self.conn.execute("DELETE FROM locations WHERE message = ?1 AND folder IN (SELECT id FROM folders WHERE account = ?2)", params![message, account])?;
                        self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
                        done.deleted += 1;
                    }
                    continue;
                }
                let target = if folder == AS_ON_SERVER {
                    // back where the server has it (the folder the download found it in)
                    match home_stmt.query_row(params![message, account], |r| r.get::<_, i64>(0)).optional()? {
                        Some(f) => f,
                        None => {
                            done.already += 1; // not from a server: nothing to put back
                            continue;
                        }
                    }
                } else {
                    match folders.get(&folder) {
                        Some(id) => *id,
                        None => {
                            let id = crate::db::ensure_folder(&self.conn, account, &folder)?;
                            folders.insert(folder.clone(), id);
                            id
                        }
                    }
                };
                if here.len() == 1 && here[0].0 == target {
                    done.already += 1;
                    continue;
                }
                let server_id = here.iter().find_map(|(_, s)| s.clone());
                self.conn.execute(
                    "DELETE FROM locations WHERE message = ?1 AND folder IN (SELECT id FROM folders WHERE account = ?2) AND folder != ?3",
                    params![message, account, target],
                )?;
                self.conn.execute(
                    "INSERT OR IGNORE INTO locations (message, folder, date, server_id) SELECT id, ?2, date, ?3 FROM messages WHERE id = ?1",
                    params![message, target, server_id],
                )?;
                self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
                done.filed += 1;
            }
            Ok(())
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result.map(|_| done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    /// A library with an IMAP account (me@x.com: read-only on its server, so every move is
    /// filing to sync) holding three messages in its Inbox.
    fn library(dir: &std::path::Path) -> (Library, i64, i64) {
        let mut library = Library::open(&dir.join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files: Vec<_> = (1..=3)
            .map(|n| {
                let p = src.join(format!("{n}.eml"));
                std::fs::write(&p, format!("From: a@b.com\r\nTo: me@x.com\r\nSubject: m{n}\r\nMessage-ID: <{n}@b.com>\r\n\r\nhi\r\n")).unwrap();
                p
            })
            .collect();
        import(&mut library, &files, &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let inbox: i64 = library.conn.query_row("SELECT folder FROM locations LIMIT 1", [], |r| r.get(0)).unwrap();
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        library.conn.execute("UPDATE accounts SET kind = 'imap', address = 'me@x.com' WHERE id = ?1", [account]).unwrap();
        library.conn.execute("UPDATE folders SET path = 'Inbox', name = 'Inbox', parent = NULL WHERE id = ?1", [inbox]).unwrap();
        // the server has them in the Inbox
        library.conn.execute("INSERT INTO server_items (folder, server_id, message) SELECT folder, 'uid' || message, message FROM locations", []).unwrap();
        (library, account, inbox)
    }

    fn id_of(l: &Library, subject: &str) -> i64 {
        l.conn.query_row("SELECT id FROM messages WHERE subject = ?1", [subject], |r| r.get(0)).unwrap()
    }

    #[test]
    fn filing_on_one_computer_is_applied_on_another_and_missing_mail_waits() {
        let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, a_account, a_inbox) = library(a_dir.path());
        let (b, _, b_inbox) = library(b_dir.path());
        // computer A files m1 and m2 in Projects, then puts m2 back, and files m3 and back again
        let projects = crate::db::ensure_folder(&a.conn, a_account, "Projects").unwrap();
        a.move_messages(&[id_of(&a, "m1"), id_of(&a, "m2")], a_inbox, projects).unwrap();
        a.move_messages(&[id_of(&a, "m3")], a_inbox, projects).unwrap();
        a.move_messages(&[id_of(&a, "m3")], projects, a_inbox).unwrap();
        let mut log = Log::new("me@x.com");
        let taken = a.take_filing_pending(a_account, &mut log, "pc-a").unwrap();
        assert_eq!(taken.len(), 4, "every move noted (m3 out and back too)");
        assert_eq!(log.changes.len(), 1, "m3 was put back before syncing: nothing for it");
        assert_eq!(log.changes[0].keys.len(), 2);
        a.clear_filing_pending(&taken).unwrap();

        // computer B doesn't have m2 (deleted there): the rest is filed, m2 waits
        b.conn.execute("DELETE FROM locations WHERE message = ?1", [id_of(&b, "m2")]).unwrap();
        let applied = b.apply_filing(&log).unwrap();
        assert_eq!((applied.filed, applied.missing), (1, 1));
        assert_eq!(b.folder_paths(id_of(&b, "m1")).unwrap(), vec!["Projects".to_string()]);
        assert!(b.conn.query_row("SELECT COUNT(*) FROM filing_pending", [], |r| r.get::<_, i64>(0)).unwrap() == 0, "applying isn't noted again");
        // again: nothing to do
        assert_eq!(b.apply_filing(&log).unwrap().filed, 0);

        // A puts m1 back in its Inbox: B follows
        a.move_messages(&[id_of(&a, "m1")], projects, a_inbox).unwrap();
        a.take_filing_pending(a_account, &mut log, "pc-a").unwrap();
        b.apply_filing(&log).unwrap();
        assert_eq!(b.folder_paths(id_of(&b, "m1")).unwrap(), vec!["Inbox".to_string()]);
        let _ = b_inbox;

        // deleted for good on A: B drops it too
        log.add("pc-a", 9_999_999_999, DELETED, KeySet::from_keys([a.account_keys(a_account).unwrap().of[&id_of(&a, "m3")]]));
        assert_eq!(b.apply_filing(&log).unwrap().deleted, 1);
        assert!(b.folder_paths(id_of(&b, "m3")).unwrap().is_empty());
    }
}
