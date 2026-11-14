//! Local only: an account's mail, calendar, or contacts kept apart from its server.
//!
//! One settings item per account address, `local-only:<address>` = {mail, calendar, contacts}
//! (true = kept on this computer only; missing = every change goes to the server, as before).
//! It belongs to this computer (not in `settings_sync::SYNCED`): another computer may well keep
//! the same account in step with its server. **To Do follows the calendar switch** of the
//! Microsoft account it syncs through (the first one), as Outlook keeps tasks with the calendar.
//!
//! With a service local only:
//! - **Nothing is queued for the server**: a move, mark, delete, folder change, event or contact
//!   change, or task is made in the library alone. Turning the service local drops the changes
//!   still waiting for that account's server (`set_local_only`), and turning it back on sends
//!   only changes made from then on: nothing made while local is ever replayed.
//! - **Reading from the server goes on**: new mail, new events, new contacts, and new tasks still
//!   come in. Nothing already here is changed or removed by the server: read / unread, labels,
//!   moves, renames, and deletions made there are left out (`forget_gone`, the read-back of
//!   unread lists and labels, Gmail's History, the replied / forwarded marks), events, contacts,
//!   and tasks already here keep their own version, and items deleted on the server stay here
//!   (the copy is independent).
//! - **What's deleted here stays deleted**: mail keeps its `server_items` record, so it isn't
//!   downloaded again; a deleted event, contact, or task is remembered by its server id in
//!   `local-only-deleted:<address>` so the server's copy isn't brought back.
//! - Sending mail (and answering an invitation by email) still sends: that is the user's own act,
//!   not a sync.
//!
//! POP3 has nothing to write back for mail (no folders, flags, or moves), so its mail switch
//! isn't offered.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::Library;

/// `local-only:<address>`.
pub const PREFIX: &str = "local-only:";
/// `local-only-deleted:<address>` = {"events": [server ids], "cards": [...]}.
pub const DELETED_PREFIX: &str = "local-only-deleted:";
/// The addresses already asked about (the window asks once, when an account is added).
pub const ASKED: &str = "local-only-asked";

/// The three things an account may keep local.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Mail,
    /// The account's calendars, and (for the first Microsoft account) To Do.
    Calendar,
    Contacts,
}

impl Service {
    pub fn parse(s: &str) -> Option<Service> {
        match s {
            "mail" => Some(Service::Mail),
            "calendar" | "todo" => Some(Service::Calendar),
            "contacts" => Some(Service::Contacts),
            _ => None,
        }
    }
}

/// An account's choice: true = kept on this computer only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalOnly {
    #[serde(default)]
    pub mail: bool,
    #[serde(default)]
    pub calendar: bool,
    #[serde(default)]
    pub contacts: bool,
}

impl LocalOnly {
    pub fn of(&self, service: Service) -> bool {
        match service {
            Service::Mail => self.mail,
            Service::Calendar => self.calendar,
            Service::Contacts => self.contacts,
        }
    }
}

/// One account as Settings ▸ Accounts lists it: its choice and which services it has.
#[derive(Clone, Debug, Serialize)]
pub struct AccountChoice {
    pub account: i64,
    pub address: String,
    pub kind: String,
    /// The services this account has a server for ("mail", "calendar", "contacts").
    pub services: Vec<&'static str>,
    pub local: LocalOnly,
}

/// The services an account of `kind` writes back (CardDAV / CalDAV hanging on an IMAP or POP3
/// account adds calendar and contacts: `dav`).
pub fn services_of(kind: &str, dav: bool) -> Vec<&'static str> {
    let mut out = match kind {
        "graph" | "gmail" => vec!["mail", "calendar", "contacts"],
        "graph-shared" | "imap" => vec!["mail"],
        _ => vec![],
    };
    if dav || kind == "dav" {
        for s in ["calendar", "contacts"] {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

#[derive(Default, Serialize, Deserialize)]
struct Deleted {
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    cards: Vec<String>,
}

fn key(address: &str) -> String {
    format!("{PREFIX}{}", address.trim().to_lowercase())
}

impl Library {
    /// An address's choice. Before this setting existed, an IMAP account's "Make Eudora's changes
    /// on the server too" switch (`ImapConfig::server_writes`) said it for mail: read from there
    /// until the choice is saved.
    pub fn local_only(&self, address: &str) -> Result<LocalOnly> {
        if let Some(j) = self.setting(&key(address))? {
            return Ok(serde_json::from_str(&j).unwrap_or_default());
        }
        let imap: Option<String> = self.setting(&format!("imap-account:{}", address.trim().to_lowercase()))?;
        let off = imap
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .is_some_and(|v| v["server_writes"].as_bool() == Some(false) && !v["protocol"].as_str().unwrap_or("").eq_ignore_ascii_case("pop3"));
        Ok(LocalOnly { mail: off, ..Default::default() })
    }

    /// Saves an address's choice. A service turned local drops the changes still waiting for
    /// that account's server (they stay here; nothing made while local is ever sent later).
    /// Returns how many waiting changes were dropped.
    pub fn set_local_only(&self, address: &str, choice: LocalOnly) -> Result<u64> {
        if !address.contains('@') {
            return Err(Error::Other("an account's email address is needed".into()));
        }
        let before = self.local_only(address)?;
        self.set_setting(&key(address), &serde_json::to_string(&choice)?)?;
        let mut dropped = 0;
        for (service, now_local, was) in [
            (Service::Mail, choice.mail, before.mail),
            (Service::Calendar, choice.calendar, before.calendar),
            (Service::Contacts, choice.contacts, before.contacts),
        ] {
            if now_local && !was {
                dropped += self.drop_waiting(address, service)?;
            }
        }
        Ok(dropped)
    }

    /// Whether one of the library's accounts keeps a service local.
    pub fn local_only_account(&self, account: i64, service: Service) -> Result<bool> {
        let address: Option<Option<String>> = self.conn.query_row("SELECT address FROM accounts WHERE id = ?1", params![account], |r| r.get(0)).optional()?;
        match address.flatten() {
            Some(a) if !a.is_empty() => Ok(self.local_only(&a)?.of(service)),
            _ => Ok(false),
        }
    }

    /// The accounts keeping a service local.
    pub fn local_only_accounts(&self, service: Service) -> Result<HashSet<i64>> {
        let mut out = HashSet::new();
        let mut stmt = self.conn.prepare("SELECT id, address FROM accounts WHERE address IS NOT NULL AND address <> ''")?;
        let rows: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (id, address) in rows {
            if self.local_only(&address)?.of(service) {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// Whether a message is only in accounts that keep mail local (then a mark of it goes to no
    /// server). A message in no account's folder counts as not local.
    pub fn message_local_only(&self, message: i64) -> Result<bool> {
        let accounts: Vec<i64> = {
            let mut stmt = self.conn.prepare_cached("SELECT DISTINCT f.account FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1")?;
            stmt.query_map(params![message], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        if accounts.is_empty() {
            return Ok(false);
        }
        for a in accounts {
            if !self.local_only_account(a, Service::Mail)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The Microsoft account To Do syncs through (the first one), whose calendar switch it follows.
    pub fn todo_account(&self) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT id FROM accounts WHERE kind = 'graph' ORDER BY id LIMIT 1", [], |r| r.get(0)).optional()?)
    }

    /// Whether To Do is kept local.
    pub fn todo_local_only(&self) -> Result<bool> {
        match self.todo_account()? {
            Some(a) => self.local_only_account(a, Service::Calendar),
            None => Ok(false),
        }
    }

    /// Every account with a server Eudora could write back to, with its choice.
    pub fn local_only_list(&self) -> Result<Vec<AccountChoice>> {
        let dav: HashSet<String> = self.dav_accounts()?.into_iter().map(|c| c.address.to_lowercase()).collect();
        let mut stmt = self.conn.prepare("SELECT id, address, kind FROM accounts WHERE address IS NOT NULL AND address <> '' ORDER BY position, id")?;
        let rows: Vec<(i64, String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (account, address, kind) in rows {
            let services = services_of(&kind, dav.contains(&address.to_lowercase()));
            if services.is_empty() {
                continue;
            }
            let local = self.local_only(&address)?;
            out.push(AccountChoice { account, address, kind, services, local });
        }
        Ok(out)
    }

    /// The accounts added since the window last asked (asked once per account, when it's added).
    /// The first time (no record yet) every account already here counts as asked: people who
    /// had Eudora before aren't asked about accounts they already use.
    pub fn local_only_unasked(&self) -> Result<Vec<AccountChoice>> {
        let list = self.local_only_list()?;
        let Some(j) = self.setting(ASKED)? else {
            let all: Vec<String> = list.iter().map(|c| c.address.to_lowercase()).collect();
            self.set_setting(ASKED, &serde_json::to_string(&all)?)?;
            return Ok(Vec::new());
        };
        let asked: HashSet<String> = serde_json::from_str::<Vec<String>>(&j).unwrap_or_default().into_iter().collect();
        let mut seen = HashSet::new();
        Ok(list.into_iter().filter(|c| !asked.contains(&c.address.to_lowercase()) && seen.insert(c.address.to_lowercase())).collect())
    }

    /// Records that an address was asked about.
    pub fn local_only_mark_asked(&self, address: &str) -> Result<()> {
        let mut asked: Vec<String> = self.setting(ASKED)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let a = address.trim().to_lowercase();
        if !asked.contains(&a) {
            asked.push(a);
            self.set_setting(ASKED, &serde_json::to_string(&asked)?)?;
        }
        Ok(())
    }

    /// Remembers an event or contact deleted here while its service is local, so the server's
    /// copy isn't brought back by the next read (`what`: "events" or "cards").
    pub(crate) fn remember_local_delete(&self, account: i64, what: &str, server_id: &str) -> Result<()> {
        if server_id.is_empty() || server_id.starts_with("local:") {
            return Ok(());
        }
        let Some(address) = self.conn.query_row("SELECT address FROM accounts WHERE id = ?1", params![account], |r| r.get::<_, Option<String>>(0)).optional()?.flatten() else {
            return Ok(());
        };
        let k = format!("{DELETED_PREFIX}{}", address.to_lowercase());
        let mut d: Deleted = self.setting(&k)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let list = if what == "events" { &mut d.events } else { &mut d.cards };
        if !list.iter().any(|s| s == server_id) {
            list.push(server_id.to_string());
            self.set_setting(&k, &serde_json::to_string(&d)?)?;
        }
        Ok(())
    }

    /// The server ids of events or contacts deleted here while local (`what`: "events" / "cards").
    pub(crate) fn local_deleted(&self, account: i64, what: &str) -> Result<HashSet<String>> {
        let Some(address) = self.conn.query_row("SELECT address FROM accounts WHERE id = ?1", params![account], |r| r.get::<_, Option<String>>(0)).optional()?.flatten() else {
            return Ok(HashSet::new());
        };
        let d: Deleted = self.setting(&format!("{DELETED_PREFIX}{}", address.to_lowercase()))?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        Ok(if what == "events" { d.events } else { d.cards }.into_iter().collect())
    }

    /// Drops the changes waiting for an address's server for one service. Returns how many.
    fn drop_waiting(&self, address: &str, service: Service) -> Result<u64> {
        let accounts: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM accounts WHERE lower(address) = lower(?1)")?;
            stmt.query_map(params![address], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0u64;
        for account in accounts {
            n += match service {
                Service::Mail => {
                    let mut n = self.conn.execute(
                        "DELETE FROM pending_ops WHERE kind IN ('move', 'delete') AND json_extract(payload, '$.account') = ?1",
                        params![account],
                    )?;
                    n += self.conn.execute(
                        "DELETE FROM pending_ops WHERE kind = ?2 AND json_extract(payload, '$.folder') IN (SELECT id FROM folders WHERE account = ?1)",
                        params![account, crate::folder_changes::KIND],
                    )?;
                    n += self.conn.execute(
                        "DELETE FROM pending_ops WHERE kind = ?2 AND json_extract(payload, '$.here') IN (SELECT id FROM folders WHERE account = ?1)",
                        params![account, crate::archive_moves::KIND],
                    )?;
                    // marks: only those whose message no server still takes them for
                    let marks: Vec<(i64, i64)> = {
                        let mut stmt = self.conn.prepare(
                            "SELECT p.id, json_extract(p.payload, '$.message') FROM pending_ops p
                             WHERE p.kind IN ('mark', 'imap-mark', 'gmail-mark')
                               AND json_extract(p.payload, '$.message') IN (SELECT l.message FROM locations l JOIN folders f ON f.id = l.folder WHERE f.account = ?1)",
                        )?;
                        stmt.query_map(params![account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                    };
                    for (op, message) in marks {
                        if self.message_local_only(message)? {
                            n += self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                        }
                    }
                    n
                }
                Service::Calendar => {
                    let mut n = self.conn.execute("DELETE FROM pending_ops WHERE kind = 'event-change' AND json_extract(payload, '$.account') = ?1", params![account])?;
                    if self.todo_account()? == Some(account) {
                        n += self.conn.execute("DELETE FROM pending_ops WHERE kind = 'todo-change'", [])?;
                    }
                    n
                }
                Service::Contacts => self.conn.execute(
                    "DELETE FROM pending_ops WHERE (kind = 'card-delete' AND json_extract(payload, '$.account') = ?1)
                        OR (kind IN ('card-update', 'card-create') AND json_extract(payload, '$.card') IN (SELECT id FROM cards WHERE account = ?1))",
                    params![account],
                )?,
            } as u64;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;
    use serde_json::json;

    fn pending(l: &Library) -> i64 {
        l.conn.query_row("SELECT COUNT(*) FROM pending_ops", [], |r| r.get(0)).unwrap()
    }

    fn event(id: &str, key: &str, subject: &str) -> serde_json::Value {
        json!({
            "id": id, "changeKey": key, "subject": subject, "type": "singleInstance",
            "start": { "dateTime": "2099-01-05T14:00:00", "timeZone": "UTC" }, "end": { "dateTime": "2099-01-05T15:00:00", "timeZone": "UTC" },
            "isAllDay": false, "responseStatus": { "response": "organizer" }, "showAs": "busy",
            "organizer": { "emailAddress": { "name": "Me", "address": "me@x.example" } }
        })
    }

    fn calendar() -> serde_json::Value {
        json!({ "id": "CAL", "name": "Calendar", "canEdit": true, "isDefaultCalendar": true })
    }

    /// A Microsoft account (mail, a calendar with one event, a contact) and one message in its Inbox.
    fn setup() -> (tempfile::TempDir, Library, i64, i64, i64) {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("a.eml");
        std::fs::write(&src, "From: ann@y.example\r\nTo: me@x.example\r\nSubject: hi\r\nMessage-ID: <a1@y.example>\r\nDate: Fri, 1 Dec 2023 10:00:00 +0000\r\n\r\nhi\r\n").unwrap();
        crate::import(&mut l, &[src], &crate::ImportOptions::default(), &Silent).unwrap();
        let account = crate::db::graph_account(&l.conn, "graph", "me@x.example", "Me", 0).unwrap();
        let inbox = crate::db::ensure_folder(&l.conn, account, "Inbox").unwrap();
        l.conn.execute("UPDATE folders SET server_id = 'INBOX-ID', role = 'inbox' WHERE id = ?1", [inbox]).unwrap();
        let m: i64 = l.conn.query_row("SELECT id FROM messages", [], |r| r.get(0)).unwrap();
        l.conn.execute("UPDATE locations SET folder = ?1, server_id = 'S1' WHERE message = ?2", params![inbox, m]).unwrap();
        l.conn.execute("INSERT INTO server_items (folder, server_id, message) VALUES (?1, 'S1', ?2)", params![inbox, m]).unwrap();
        l.record_calendars(account, 0, 4_000_000_000, &[(calendar(), vec![event("E1", "k1", "Plan")])]).unwrap();
        let contact = json!({ "id": "C1", "changeKey": "1", "displayName": "Ann", "emailAddresses": [{ "address": "ann@y.example" }] });
        l.record_contacts(account, &[(None, "Contacts".to_string(), vec![contact])]).unwrap();
        (dir, l, account, inbox, m)
    }

    #[test]
    fn a_local_only_account_queues_nothing() {
        let (_dir, l, account, inbox, m) = setup();
        // as before: changes go to the server
        l.set_status(&[m], crate::marks::Status::Replied).unwrap();
        assert!(pending(&l) > 0, "a mark is queued for Microsoft 365");

        // kept local: what was waiting is dropped, and nothing more is queued
        let dropped = l.set_local_only("ME@x.example", LocalOnly { mail: true, calendar: true, contacts: true }).unwrap();
        assert!(dropped > 0);
        assert_eq!(pending(&l), 0);
        assert!(l.local_only_account(account, Service::Mail).unwrap());

        let archive = l.create_folder(account, "Projects").unwrap();
        l.set_status(&[m], crate::marks::Status::Unread).unwrap();
        l.move_messages(&[m], inbox, archive).unwrap();
        l.rename_folder(archive, "Kept").unwrap();
        let event: i64 = l.conn.query_row("SELECT id FROM events", [], |r| r.get(0)).unwrap();
        l.event_update(event, Some(true), None, None).unwrap();
        let card: i64 = l.conn.query_row("SELECT id FROM cards", [], |r| r.get(0)).unwrap();
        let mut c = l.card(card).unwrap().card;
        c.notes = "local note".into();
        l.update_card(card, &c).unwrap();
        l.create_card(account, "", &crate::cards::Card { name: "Bob".into(), ..Default::default() }).unwrap();
        l.todo_add("Call Bob", "", None, None).unwrap();
        assert_eq!(pending(&l), 0, "nothing queued for the server");
        // moved here, still known as on the server (so it isn't downloaded again)
        let known: i64 = l.conn.query_row("SELECT COUNT(*) FROM server_items WHERE message = ?1", [m], |r| r.get(0)).unwrap();
        assert_eq!(known, 1);

        // back on: only changes from then on go
        l.set_local_only("me@x.example", LocalOnly::default()).unwrap();
        assert_eq!(pending(&l), 0, "changes made while local are never replayed");
        l.event_update(event, Some(false), None, None).unwrap();
        assert_eq!(pending(&l), 1);
    }

    #[test]
    fn read_back_leaves_local_changes_alone() {
        let (_dir, mut l, account, inbox, m) = setup();
        l.set_local_only("me@x.example", LocalOnly { mail: true, calendar: true, contacts: true }).unwrap();

        // mail read here; the server says unread and that it left the Inbox
        l.set_status(&[m], crate::marks::Status::Read).unwrap();
        let job = crate::server_changes::ReconcileJob { account, mailbox: None, inbox_only: false, labels: vec![] };
        assert_eq!(crate::server_changes::apply_reconcile(&l, &job, &["S1".to_string()], &[], &[]).unwrap(), (0, 0, 0));
        assert_eq!(crate::graph_sync::forget_gone(&mut l, inbox, &["OTHER".to_string()], None).unwrap(), 0);
        let (flags, here): (i64, i64) = l
            .conn
            .query_row("SELECT flags, (SELECT COUNT(*) FROM locations WHERE message = ?1 AND folder = ?2) FROM messages WHERE id = ?1", params![m, inbox], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(flags & crate::marks::UNREAD, 0, "still read here");
        assert_eq!(here, 1, "still in the Inbox here though gone there");

        // an event deleted here and changed there stays deleted; one here keeps its version
        let e1: i64 = l.conn.query_row("SELECT id FROM events WHERE server_id = 'E1'", [], |r| r.get(0)).unwrap();
        l.event_delete(e1).unwrap();
        l.record_calendars(account, 0, 4_000_000_000, &[(calendar(), vec![event("E1", "k2", "Plan (moved)"), event("E2", "k1", "New")])]).unwrap();
        let subjects: Vec<String> = l.conn.prepare("SELECT subject FROM events ORDER BY subject").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(subjects, vec!["New"], "new events still come in; the deleted one doesn't come back");
        l.record_calendars(account, 0, 4_000_000_000, &[(calendar(), vec![event("E2", "k9", "Renamed there")])]).unwrap();
        let s: String = l.conn.query_row("SELECT subject FROM events WHERE server_id = 'E2'", [], |r| r.get(0)).unwrap();
        assert_eq!(s, "New", "an event here keeps its own version");
        l.record_calendars(account, 0, 4_000_000_000, &[]).unwrap();
        let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "nothing the server removed leaves");

        // a contact edited here keeps the edit; one deleted here doesn't come back
        let card: i64 = l.conn.query_row("SELECT id FROM cards", [], |r| r.get(0)).unwrap();
        let mut c = l.card(card).unwrap().card;
        c.notes = "mine".into();
        l.update_card(card, &c).unwrap();
        let ann = json!({ "id": "C1", "changeKey": "2", "displayName": "Ann Server", "emailAddresses": [{ "address": "ann@y.example" }] });
        let bob = json!({ "id": "C2", "changeKey": "1", "displayName": "Bob", "emailAddresses": [{ "address": "bob@y.example" }] });
        l.record_contacts(account, &[(None, "Contacts".to_string(), vec![ann.clone(), bob.clone()])]).unwrap();
        assert_eq!(l.card(card).unwrap().card.notes, "mine");
        let bob_id: i64 = l.conn.query_row("SELECT id FROM cards WHERE server_id = 'C2'", [], |r| r.get(0)).unwrap();
        l.delete_card(bob_id).unwrap();
        l.record_contacts(account, &[(None, "Contacts".to_string(), vec![ann, bob])]).unwrap();
        let bobs: i64 = l.conn.query_row("SELECT COUNT(*) FROM cards WHERE server_id = 'C2'", [], |r| r.get(0)).unwrap();
        assert_eq!(bobs, 0);
        l.record_contacts(account, &[(None, "Contacts".to_string(), vec![])]).unwrap();
        assert!(l.card(card).is_ok(), "nothing the server removed leaves");
        assert_eq!(pending(&l), 0);
    }

    #[test]
    fn an_imap_accounts_old_switch_becomes_its_mail_choice() {
        let dir = tempfile::tempdir().unwrap();
        let l = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        // as an older Eudora saved it
        l.set_setting("imap-account:me@y.example", r#"{"address":"me@y.example","server":"mail.y.example","protocol":"imap","server_writes":false}"#).unwrap();
        crate::db::import_account(&l.conn, "imap", Some("me@y.example"), "", "mail.y.example:993", false, 0).unwrap();
        assert!(l.local_only("me@y.example").unwrap().mail, "read from the old switch");
        assert!(l.imap_writable().unwrap().is_empty());
        let mut c = l.imap_accounts().unwrap().remove(0);
        assert_eq!(c.server_writes, Some(false));
        c.server_writes = Some(true);
        l.save_imap_account(&c, None, None).unwrap();
        assert!(!l.local_only("me@y.example").unwrap().mail);
        assert_eq!(l.imap_writable().unwrap().len(), 1);
        assert_eq!(l.local_only_list().unwrap()[0].services, vec!["mail"]);
        // asked once: the first look counts every account already here as asked
        assert!(l.local_only_unasked().unwrap().is_empty());
        crate::db::graph_account(&l.conn, "graph", "new@x.example", "", 0).unwrap();
        assert_eq!(l.local_only_unasked().unwrap().len(), 1);
        l.local_only_mark_asked("new@x.example").unwrap();
        assert!(l.local_only_unasked().unwrap().is_empty());
    }
}
