//! A message's status (unread, read, replied, forwarded, redirected), the priority set by hand
//! and its labels: Message ▸ Change Status / Change Priority / Change Label, the rules' actions,
//! and what replying or forwarding does by itself.
//!
//! Status and priority are bits of `messages.flags` (0 = read, no priority set: what every
//! message had before, so nothing old turns unread); labels are rows of `categories`. In a
//! Microsoft 365 mailbox they go to the server too: read = Exchange's read
//! flag, labels = Outlook categories (same names; colors when Outlook's category list may be
//! changed), priority = importance. A change is queued (`pending_ops` "mark", one per message:
//! the server gets the message's state as it is when the change is made there), and the
//! download carries it out (`server_changes`). What changed on the server (read in Outlook,
//! a category given on the phone) comes back the other way (`server_changes::reconcile`).

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::{Library, now};

pub const UNREAD: i64 = 1;
pub const REPLIED: i64 = 2;
pub const FORWARDED: i64 = 4;
pub const REDIRECTED: i64 = 8;
/// Flagged as a To Do: Outlook's flag on Microsoft 365 mail.
pub const FLAGGED: i64 = 1 << 11;
const PRIORITY_SHIFT: i64 = 8;
const PRIORITY_MASK: i64 = 7 << PRIORITY_SHIFT;

/// The priority set by hand: 1 (highest) … 5 (lowest), None when it isn't.
pub fn priority_of(flags: i64) -> Option<u8> {
    match (flags & PRIORITY_MASK) >> PRIORITY_SHIFT {
        0 => None,
        p => Some(p as u8),
    }
}

/// Flags with the priority set (None: not set by hand).
pub fn with_priority(flags: i64, priority: Option<u8>) -> i64 {
    (flags & !PRIORITY_MASK) | (priority.map_or(0, |p| i64::from(p.clamp(1, 5))) << PRIORITY_SHIFT)
}

/// Exchange's importance for a priority.
pub fn importance(priority: Option<u8>) -> &'static str {
    match priority {
        Some(1 | 2) => "high",
        Some(4 | 5) => "low",
        _ => "normal",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Unread,
    Read,
    Replied,
    Forwarded,
    Redirected,
}

impl Status {
    /// A status by name (the menu's, the rules', classic Eudora's).
    pub fn parse(name: &str) -> Option<Status> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            // (classic Eudora's Filters.pce names them by number: summary.h MS_UNREAD … MS_REDIRECT)
            "unread" | "new" | "0" => Status::Unread,
            "read" | "1" => Status::Read,
            "replied" | "2" => Status::Replied,
            "forwarded" | "3" => Status::Forwarded,
            "redirected" | "redirect" | "4" => Status::Redirected,
            _ => return None,
        })
    }

    /// The flags a message has after this status is set.
    fn apply(self, flags: i64) -> i64 {
        match self {
            Status::Unread => flags | UNREAD,
            Status::Read => flags & !UNREAD,
            Status::Replied => (flags & !UNREAD) | REPLIED,
            Status::Forwarded => (flags & !UNREAD) | FORWARDED,
            Status::Redirected => (flags & !UNREAD) | REDIRECTED,
        }
    }
}

/// A label: its name (the Outlook category's) and color ("#rrggbb").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LabelDef {
    pub name: String,
    pub color: String,
}

/// The setting holding the label list (synced between computers).
pub const LABELS_KEY: &str = "labels";

/// Classic Eudora's seven labels as it shipped them (names and colors).
const CLASSIC_LABELS: [(&str, &str); 7] = [
    ("Label 1", "#ff8000"),
    ("Label 2", "#ff0000"),
    ("Label 3", "#ff00ff"),
    ("Label 4", "#0080ff"),
    ("Label 5", "#0000ff"),
    ("Label 6", "#008000"),
    ("Label 7", "#804040"),
];

/// The labels in a classic Eudora settings file (`[Labels]` `Label1=255,128,0,Prospects`).
pub fn classic_labels(ini: &str) -> Vec<LabelDef> {
    let mut in_labels = false;
    let mut found: Vec<(usize, LabelDef)> = Vec::new();
    for line in ini.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_labels = line.eq_ignore_ascii_case("[Labels]");
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_labels) else { continue };
        let Some(n) = key.strip_prefix("Label").and_then(|n| n.parse::<usize>().ok()).filter(|n| (1..=7).contains(n)) else { continue };
        let mut parts = value.splitn(4, ',');
        let rgb: Vec<u8> = parts.by_ref().take(3).filter_map(|p| p.trim().parse().ok()).collect();
        let name = parts.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| format!("Label {n}"));
        let color = if rgb.len() == 3 { format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]) } else { CLASSIC_LABELS[n - 1].1.to_string() };
        found.push((n, LabelDef { name, color }));
    }
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, l)| l).collect()
}

impl Library {
    /// Sets the messages' status. Returns how many changed.
    pub fn set_status(&self, ids: &[i64], status: Status) -> Result<u64> {
        self.change_flags(ids, |flags| status.apply(flags))
    }

    /// Sets the priority by hand (1 highest … 5 lowest; None = as the message says).
    pub fn set_priority(&self, ids: &[i64], level: Option<u8>) -> Result<u64> {
        if level.is_some_and(|l| !(1..=5).contains(&l)) {
            return Err(Error::Other("a priority is 1 (highest) to 5 (lowest)".into()));
        }
        let bits = level.map(|l| (l as i64) << PRIORITY_SHIFT).unwrap_or(0);
        self.change_flags(ids, |flags| (flags & !PRIORITY_MASK) | bits)
    }

    /// Raises (`by` < 0) or lowers (`by` > 0) the priority by steps (classic's Raise / Lower
    /// Priority): from Normal (3) when none is set.
    pub fn shift_priority(&self, ids: &[i64], by: i8) -> Result<u64> {
        self.change_flags(ids, |flags| {
            let now = priority_of(flags).unwrap_or(3) as i8;
            let next = (now + by).clamp(1, 5) as i64;
            (flags & !PRIORITY_MASK) | (next << PRIORITY_SHIFT)
        })
    }

    /// Gives the messages a label (`on`) or takes it off. Returns how many changed.
    pub fn set_label(&self, ids: &[i64], label: &str, on: bool) -> Result<u64> {
        let label = label.trim();
        if label.is_empty() {
            return Err(Error::Other("a label needs a name".into()));
        }
        self.in_transaction(|| {
            let mut changed = Vec::new();
            for id in ids {
                let n = if on {
                    self.conn.execute("INSERT OR IGNORE INTO categories (message, label) VALUES (?1, ?2)", params![id, label])?
                } else {
                    self.conn.execute("DELETE FROM categories WHERE message = ?1 AND label = ?2", params![id, label])?
                };
                if n > 0 {
                    changed.push(*id);
                }
            }
            self.queue_marks(&changed)?;
            Ok(changed.len() as u64)
        })
    }

    /// Takes every label off the messages (Change Label ▸ None).
    pub fn clear_labels(&self, ids: &[i64]) -> Result<u64> {
        self.in_transaction(|| {
            let mut changed = Vec::new();
            for id in ids {
                if self.conn.execute("DELETE FROM categories WHERE message = ?1", params![id])? > 0 {
                    changed.push(*id);
                }
            }
            self.queue_marks(&changed)?;
            Ok(changed.len() as u64)
        })
    }

    /// After a reply, forward or redirect was sent: the message it came from is marked Replied,
    /// Forwarded or Redirected (as classic Eudora did; read too).
    pub fn mark_origin(&self, origin: &Option<crate::compose::Origin>) -> Result<()> {
        let Some(origin) = origin else { return Ok(()) };
        let status = match origin.kind.as_str() {
            "reply" => Status::Replied,
            "forward" => Status::Forwarded,
            "redirect" => Status::Redirected,
            _ => return Ok(()),
        };
        self.set_status(&[origin.message], status).map(|_| ())
    }

    /// A message's labels.
    pub fn labels_on(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT label FROM categories WHERE message = ?1 ORDER BY rowid")?;
        Ok(stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// A message's flags (status and priority).
    pub fn flags_of(&self, id: i64) -> Result<i64> {
        Ok(self.conn.query_row("SELECT flags FROM messages WHERE id = ?1", params![id], |r| r.get(0))?)
    }

    /// The labels to choose from: the user's list (as set in Settings, synced), else the
    /// classic Eudora labels of an imported classic Eudora folder, else classic Eudora's own
    /// seven; then any label a message carries that isn't in the list (from Outlook), in gray.
    pub fn label_list(&self) -> Result<Vec<LabelDef>> {
        let mut list: Vec<LabelDef> = match self.setting(LABELS_KEY)? {
            Some(json) => serde_json::from_str(&json).unwrap_or_default(),
            None => self.classic_label_list()?,
        };
        let mut stmt = self.conn.prepare("SELECT DISTINCT label FROM categories ORDER BY label")?;
        for name in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let name = name?;
            if !list.iter().any(|l| l.name.eq_ignore_ascii_case(&name)) {
                list.push(LabelDef { name, color: "#808080".into() });
            }
        }
        Ok(list)
    }

    /// Keeps the label list (Settings ▸ Mail). A renamed label is renamed on every message
    /// that has it (`renamed`: old name → new name), and goes to the server with them.
    pub fn set_label_list(&self, list: &[LabelDef], renamed: &[(String, String)]) -> Result<()> {
        self.in_transaction(|| {
            for (old, new) in renamed {
                let ids: Vec<i64> = {
                    let mut stmt = self.conn.prepare("SELECT message FROM categories WHERE label = ?1")?;
                    stmt.query_map(params![old], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
                };
                self.conn.execute("UPDATE OR IGNORE categories SET label = ?2 WHERE label = ?1", params![old, new])?;
                self.conn.execute("DELETE FROM categories WHERE label = ?1", params![old])?;
                self.queue_marks(&ids)?;
            }
            self.set_setting(LABELS_KEY, &serde_json::to_string(list)?)?;
            Ok(())
        })
    }

    fn classic_label_list(&self) -> Result<Vec<LabelDef>> {
        let mut stmt = self.conn.prepare("SELECT source FROM accounts WHERE kind = 'eudora' AND source IS NOT NULL ORDER BY id")?;
        let sources: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for source in sources {
            // ("C:\OldMail (personalities: a, b)": the folder, then what it held)
            let dir = std::path::Path::new(source.split(" (personalities").next().unwrap_or(&source).trim());
            let ini = if dir.is_dir() { dir.join("eudora.ini") } else { dir.with_file_name("eudora.ini") };
            if let Ok(bytes) = std::fs::read(&ini) {
                let labels = classic_labels(&String::from_utf8_lossy(&bytes));
                if !labels.is_empty() {
                    return Ok(labels);
                }
            }
        }
        Ok(CLASSIC_LABELS.iter().map(|(n, c)| LabelDef { name: n.to_string(), color: c.to_string() }).collect())
    }

    /// Changes the messages' flags with `f`; those whose read state or priority changed are
    /// queued for their server. Unread messages in each folder that has any. Reads every
    /// unread message (~3 s on the user's 2.4 million): asked lazily, in the background.
    pub fn unread_by_folder(&self) -> Result<Vec<(i64, u64)>> {
        // (kept current by the database itself: `folder_counts`, schema 26)
        let mut stmt = self.conn.prepare_cached("SELECT folder, unread FROM folder_counts WHERE unread > 0")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// The unread messages in these folders (Mark All as Read), newest first.
    pub fn unread_in(&self, folders: &[i64]) -> Result<Vec<i64>> {
        if folders.is_empty() {
            return Ok(Vec::new());
        }
        let list = folders.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT m.id FROM locations l JOIN messages m ON m.id = l.message WHERE l.folder IN ({list}) AND m.flags & 1 ORDER BY m.date DESC"
        ))?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn change_flags(&self, ids: &[i64], f: impl Fn(i64) -> i64) -> Result<u64> {
        self.in_transaction(|| {
            let mut read = self.conn.prepare_cached("SELECT flags FROM messages WHERE id = ?1")?;
            let mut write = self.conn.prepare_cached("UPDATE messages SET flags = ?2 WHERE id = ?1")?;
            let (mut changed, mut for_server, mut flag_changed) = (0, Vec::new(), Vec::new());
            for id in ids {
                let Ok(old) = read.query_row(params![id], |r| r.get::<_, i64>(0)) else { continue };
                let new = f(old);
                if new == old {
                    continue;
                }
                write.execute(params![id, new])?;
                changed += 1;
                if (old ^ new) & (UNREAD | REPLIED | FORWARDED | PRIORITY_MASK | FLAGGED) != 0 {
                    for_server.push(*id);
                }
                if (old ^ new) & FLAGGED != 0 {
                    flag_changed.push(*id);
                }
            }
            drop((read, write));
            self.queue_marks(&for_server)?;
            // Outlook's flag goes with a change only when it was flagged or unflagged here (Eudora
            // doesn't read flags back yet: sending it each time would clear flags set in Outlook)
            for id in &flag_changed {
                self.conn.execute("UPDATE pending_ops SET payload = json_set(payload, '$.flag', json('true')) WHERE kind = 'mark' AND json_extract(payload, '$.message') = ?1", params![id])?;
            }
            Ok(changed)
        })
    }

    /// Queues the messages' state for the servers that have them: Microsoft 365 mailboxes (not
    /// the Online Archive: never changed there) as "mark", IMAP accounts whose server Eudora
    /// changes as "imap-mark", Gmail accounts as "gmail-mark" (each side carries out and clears
    /// its own). One entry per message and side.
    pub(crate) fn queue_marks(&self, ids: &[i64]) -> Result<()> {
        let imap = self.imap_writable()?;
        // accounts whose mail is kept local (`local_only`): their marks stay here
        let local = self.local_only_accounts(crate::local_only::Service::Mail)?;
        let mut accounts = self.conn.prepare_cached(
            "SELECT a.id, a.kind FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
             WHERE l.message = ?1 AND l.server_id IS NOT NULL AND (a.kind LIKE 'graph%' OR a.kind IN ('imap', 'gmail')) AND f.path NOT LIKE 'Online Archive/%'",
        )?;
        // what's waiting already, read once (asking per message scanned the whole queue each
        // time: Mark All as Read on a big folder took that many times as long)
        let mut waiting_now: std::collections::HashSet<(String, i64)> = {
            let mut stmt = self.conn.prepare_cached("SELECT kind, json_extract(payload, '$.message') FROM pending_ops WHERE kind IN ('mark', 'imap-mark', 'gmail-mark')")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))?.filter_map(|r| r.ok()).filter_map(|(k, m)| m.map(|m| (k, m))).collect()
        };
        let mut queue = self.conn.prepare_cached("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, ?2, ?3)")?;
        for id in ids {
            let rows: Vec<(i64, String)> = accounts.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter(|(a, _)| !local.contains(a)).collect();
            let sides = [("mark", rows.iter().any(|(_, kind)| kind.starts_with("graph"))), ("imap-mark", rows.iter().any(|(a, kind)| kind == "imap" && imap.contains(a))), ("gmail-mark", rows.iter().any(|(_, kind)| kind == "gmail"))];
            for (kind, on_server) in sides {
                let waiting = waiting_now.contains(&(kind.to_string(), *id));
                if on_server && !waiting {
                    queue.execute(params![now(), kind, serde_json::json!({ "message": id }).to_string()])?;
                    waiting_now.insert((kind.to_string(), *id));
                } else if waiting {
                    // a change after one failed: tried again from the start
                    self.conn.execute(
                        "UPDATE pending_ops SET attempts = 0, last_error = NULL WHERE kind = ?2 AND json_extract(payload, '$.message') = ?1",
                        params![id, kind],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Runs `f` in a transaction of its own (or inside the one already open).
    pub(crate) fn in_transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return f();
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = f();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_priorities_are_bits_that_leave_each_other_alone() {
        let f = Status::Replied.apply(UNREAD | (2 << PRIORITY_SHIFT));
        assert_eq!(f & (UNREAD | REPLIED), REPLIED, "replying reads it");
        assert_eq!(priority_of(f), Some(2));
        assert_eq!(priority_of(0), None);
        assert_eq!(Status::Unread.apply(REPLIED), REPLIED | UNREAD, "unread again keeps that it was answered");
        assert_eq!(Status::parse("Redirected"), Some(Status::Redirected));
        assert_eq!((importance(Some(1)), importance(None), importance(Some(5))), ("high", "normal", "low"));
    }

    #[test]
    fn classic_label_settings_are_read_with_their_colors() {
        let ini = "[Settings]\r\nLabel1=x\r\n[Labels]\r\nLabelCount=7\r\nLabel1=255,128,0,Prospects\r\nLabel2=255,0,0,Important!!!\r\nLabel7=128,64,64,\r\n[Persona-x]\r\n";
        let labels = classic_labels(ini);
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], LabelDef { name: "Prospects".into(), color: "#ff8000".into() });
        assert_eq!(labels[2], LabelDef { name: "Label 7".into(), color: "#804040".into() });
    }
}
