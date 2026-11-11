//! Special ▸ Empty Trash: the messages in each account's
//! Trash ("Deleted Items") leave the library, and in a Microsoft 365 mailbox they're deleted on
//! the server too (`pending_ops` "delete", carried out by the download: Exchange keeps them in
//! Recoverable Items for its retention period, as Outlook's Empty Folder does). The Online
//! Archive's Deleted Items are never touched: Eudora never deletes from the archive. Always
//! asked first, with the counts (`trash_summary`).
//!
//! A message that's also in another folder only leaves the Trash. One that was only there is
//! removed from the library: its rows, its labels and tags, its search entries (the writer
//! takes them out: `index_pending`). Its bytes stay in the pack until the library is
//! optimized. What the server had is still remembered (`server_items`), so a later download
//! never brings it back.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::error::Result;
use crate::library::{Library, now};

/// One account's Trash, as Empty Trash would empty it.
#[derive(Clone, Debug, Serialize)]
pub struct TrashSummary {
    pub account: i64,
    /// The account as shown (its name or address).
    pub shown: String,
    pub folder: i64,
    pub path: String,
    pub messages: u64,
    /// Deleted on the server too (Microsoft 365; IMAP accounts Eudora changes).
    pub on_server: bool,
    /// In the Online Archive's Deleted Items: kept.
    pub archive_kept: u64,
}

impl Library {
    /// Every account's Trash with something in it (or in its archive's).
    pub fn trash_summary(&self) -> Result<Vec<TrashSummary>> {
        let imap = self.imap_writable()?;
        let mut stmt = self.conn.prepare("SELECT id, kind, COALESCE(label, address, name) FROM accounts ORDER BY id")?;
        let accounts: Vec<(i64, String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (account, kind, shown) in accounts {
            let Some((folder, path)) = self.existing_trash(account)? else { continue };
            let count = |folder: i64| -> Result<u64> {
                Ok(self.conn.query_row("SELECT COUNT(*) FROM locations WHERE folder = ?1", params![folder], |r| r.get::<_, i64>(0))? as u64)
            };
            let messages = count(folder)?;
            let archive_kept = match self.archive_twin(folder)? {
                Some(twin) => count(twin)?,
                None => 0,
            };
            if messages + archive_kept > 0 {
                out.push(TrashSummary { account, shown, folder, path, messages, on_server: kind.starts_with("graph") || (kind == "imap" && imap.contains(&account)), archive_kept });
            }
        }
        Ok(out)
    }

    /// Empties the Trash of these accounts. Returns how many messages left it.
    pub fn empty_trash(&self, accounts: &[i64]) -> Result<u64> {
        let mut emptied = 0;
        let imap = self.imap_writable()?;
        for account in accounts {
            let Some((folder, _)) = self.existing_trash(*account)? else { continue };
            let kind: String = self.conn.query_row("SELECT kind FROM accounts WHERE id = ?1", params![account], |r| r.get(0))?;
            let ids: Vec<i64> = {
                let mut stmt = self.conn.prepare("SELECT message FROM locations WHERE folder = ?1")?;
                stmt.query_map(params![folder], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            for chunk in ids.chunks(500) {
                emptied += self.in_transaction(|| {
                    for id in chunk {
                        let mut server_id: Option<String> = self
                            .conn
                            .query_row("SELECT server_id FROM locations WHERE message = ?1 AND folder = ?2", params![id, folder], |r| r.get(0))
                            .optional()?
                            .flatten();
                        // a move into the Trash not made on the server yet: deleted from where it is there
                        let waiting: Option<(i64, String)> = self
                            .conn
                            .query_row(
                                "SELECT id, payload FROM pending_ops WHERE kind = 'move' AND json_extract(payload, '$.message') = ?1 AND json_extract(payload, '$.to') = ?2",
                                params![id, folder],
                                |r| Ok((r.get(0)?, r.get(1)?)),
                            )
                            .optional()?;
                        // the folder it's in on the server (IMAP ids are per folder)
                        let mut on_server_in = folder;
                        if let Some((op, payload)) = waiting {
                            self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                            let payload = serde_json::from_str::<serde_json::Value>(&payload)?;
                            server_id = payload["server_id"].as_str().map(str::to_string);
                            on_server_in = payload["from"].as_i64().unwrap_or(folder);
                        }
                        if kind.starts_with("graph") || (kind == "imap" && imap.contains(account)) {
                            // every copy the server has of it there (identical copies are one
                            // message here, each with its own server item)
                            let mut items: Vec<String> = {
                                let mut stmt = self.conn.prepare_cached("SELECT server_id FROM server_items WHERE folder = ?1 AND message = ?2 ORDER BY server_id")?;
                                stmt.query_map(params![on_server_in, id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
                            };
                            if let Some(s) = server_id {
                                if !items.contains(&s) {
                                    items.push(s);
                                }
                            }
                            for server_id in items {
                                let payload = serde_json::json!({ "account": account, "message": id, "server_id": server_id, "folder": on_server_in });
                                self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'delete', ?2)", params![now(), payload.to_string()])?;
                            }
                            self.conn.execute("DELETE FROM server_items WHERE folder = ?1 AND message = ?2", params![on_server_in, id])?;
                        }
                        self.conn.execute("DELETE FROM pending_ops WHERE kind IN ('mark', 'imap-mark', 'gmail-mark') AND json_extract(payload, '$.message') = ?1", params![id])?;
                        self.conn.execute("DELETE FROM locations WHERE message = ?1 AND folder = ?2", params![id, folder])?;
                        let elsewhere: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM locations WHERE message = ?1)", params![id], |r| r.get(0))?;
                        if elsewhere {
                            self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![id])?;
                        } else {
                            self.purge_message(*id)?;
                        }
                    }
                    Ok(chunk.len() as u64)
                })?;
            }
        }
        Ok(emptied)
    }

    /// The account's Trash when it has one (Empty Trash never makes one): not the archive's.
    fn existing_trash(&self, account: i64) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path FROM folders WHERE account = ?1 AND (role = 'trash' OR lower(path) IN ('deleted items', 'trash', 'deleted messages'))
                 AND path NOT LIKE 'Online Archive/%' ORDER BY role IS NULL, id LIMIT 1",
                params![account],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Removes a message from the library: every row that names it (found by the column
    /// `message`, so tables added later are covered), except what the servers had
    /// (`server_items`: a download must not bring it back). The search index drops it when the
    /// writer next brings it up to date.
    pub(crate) fn purge_message(&self, id: i64) -> Result<()> {
        let tables: Vec<String> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT m.name FROM sqlite_master m JOIN pragma_table_info(m.name) p WHERE m.type = 'table' AND p.name = 'message'
                 AND m.name NOT IN ('index_pending')",
            )?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        // (its server items go too: the server deletes it; until it has, the download skips
        // items with a delete waiting, so it never comes back)
        for table in tables {
            self.conn.execute(&format!("DELETE FROM \"{table}\" WHERE message = ?1"), params![id])?;
        }
        self.conn.execute("DELETE FROM links WHERE payment = ?1 OR receipt = ?1", params![id])?;
        self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        self.conn.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::library::{Access, Library};
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    #[test]
    fn empty_trash_deletes_on_the_server_and_keeps_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let mut files = Vec::new();
        for n in 1..=3 {
            let file = dir.path().join(format!("{n}.eml"));
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Hi {n}\r\nMessage-ID: <{n}@a>\r\n\r\nHello\r\n")).unwrap();
            files.push(file);
        }
        import(&mut library, &files, &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let conn = &library.conn;
        let inbox: i64 = conn.query_row("SELECT folder FROM locations LIMIT 1", [], |r| r.get(0)).unwrap();
        let account: i64 = conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        conn.execute("UPDATE accounts SET kind = 'graph', address = 'me@x.com' WHERE id = ?1", [account]).unwrap();
        let trash = crate::db::ensure_folder(conn, account, "Deleted Items").unwrap();
        let archive_trash = crate::db::ensure_folder(conn, account, "Online Archive/Deleted Items").unwrap();
        let ids: Vec<i64> = conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        // 1: in the Trash on the server; 2: in the Trash and also in Inbox; 3: in the archive's Deleted Items
        conn.execute("UPDATE locations SET folder = ?1, server_id = 'srv-1' WHERE message = ?2", [trash, ids[0]]).unwrap();
        conn.execute("INSERT INTO locations (message, folder, date, server_id) SELECT id, ?1, date, 'srv-2' FROM messages WHERE id = ?2", [trash, ids[1]]).unwrap();
        conn.execute("UPDATE locations SET folder = ?1 WHERE message = ?2", [archive_trash, ids[2]]).unwrap();
        library.set_label(&[ids[0]], "Prospects", true).unwrap();
        // as a download leaves them: its server item recorded, and linked to another message
        conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, 'srv-1', ?2)", [trash, ids[0]]).unwrap();
        // the server had it twice (identical copies: one message here)
        conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, 'srv-1b', ?2)", [trash, ids[0]]).unwrap();
        conn.execute("INSERT INTO links (payment, receipt, cents) VALUES (?1, ?2, 1999)", [ids[0], ids[1]]).unwrap();

        let summary = library.trash_summary().unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!((summary[0].messages, summary[0].archive_kept, summary[0].on_server), (2, 1, true));

        assert_eq!(library.empty_trash(&[account]).unwrap(), 2);
        let exists = |id: i64| -> bool { library.conn.query_row("SELECT EXISTS (SELECT 1 FROM messages WHERE id = ?1)", [id], |r| r.get(0)).unwrap() };
        assert!(!exists(ids[0]), "only in the Trash: gone from the library");
        assert!(exists(ids[1]) && exists(ids[2]), "also in the Inbox / in the archive: kept");
        let labels: i64 = library.conn.query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0)).unwrap();
        assert_eq!(labels, 0);
        let deletes: Vec<String> = library
            .conn
            .prepare("SELECT json_extract(payload, '$.server_id') FROM pending_ops WHERE kind = 'delete' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(deletes, vec!["srv-1".to_string(), "srv-1b".to_string(), "srv-2".to_string()], "every copy on the server");
        assert_eq!(library.trash_summary().unwrap()[0].messages, 0, "the archive's are left");
    }
}
