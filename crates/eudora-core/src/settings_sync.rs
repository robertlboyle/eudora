//! Settings sync, the library side: which settings travel between the user's computers (rules,
//! the spam filter's, pictures, completion, the window's look and columns, the user's own
//! whitelist and blacklist entries), as items merged one by one: the newest change of an item
//! wins (the project's design notes § 7). What belongs to one computer stays (window sizes, the last folder
//! open, whether it downloads in the background, the arriving-mail mark, sync's own choices).

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::library::Library;

/// The settings that travel.
pub const SYNCED: &[&str] = &[
    "rules",
    "incoming-rules",
    "triage-bulk-folders",
    "triage-languages",
    "triage-like-rules",
    "triage-mailboxes",
    "triage-phrases",
    "triage-rule-settings",
    "pictures",
    "completion",
    "views-accounts",
    crate::marks::LABELS_KEY,
    crate::signatures::SIGNATURES_KEY,
    SENDER_RULES,
];
/// Settings whose keys start with these travel too: the window's ("ui:skin", "ui:columns:eudora"…).
pub const SYNCED_PREFIXES: &[&str] = &["ui:"];
/// The user's own whitelist and blacklist entries (and automatic ones taken out), as one item:
/// the automatic whitelist is made from mail on each computer.
pub const SENDER_RULES: &str = "sender-rules";

pub fn is_synced(key: &str) -> bool {
    SYNCED.contains(&key) || SYNCED_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// One setting as it travels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    pub version: i64,
    /// The computer that changed it last.
    pub device: String,
    /// When (unix seconds).
    pub modified: i64,
    pub value: String,
}

impl Item {
    /// Which of two changes of one item is newer: the later, then the higher version, then the
    /// computer's name (the same answer on every computer).
    pub fn newer_than(&self, other: &Item) -> bool {
        (self.modified, self.version, &self.device) > (other.modified, other.version, &other.device)
    }
}

/// One whitelist / blacklist entry as it travels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    pattern: String,
    kind: String,
    action: String,
    note: String,
    added: i64,
    /// The mailbox it applies in (empty = every mailbox; absent from older computers).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    mailbox: String,
}

impl Library {
    /// The settings that travel, as items (`device`: this computer's name, for the ones changed
    /// here).
    pub fn synced_items(&self, device: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare("SELECT key, version, device, modified, value FROM settings_items ORDER BY key")?;
        let rows: Vec<Item> = stmt
            .query_map([], |r| Ok(Item { key: r.get(0)?, version: r.get(1)?, device: r.get(2)?, modified: r.get(3)?, value: r.get(4)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter(|i| is_synced(&i.key))
            .map(|mut i| {
                if i.device == "this" {
                    i.device = device.to_string();
                }
                i
            })
            .collect())
    }

    /// Takes in items from other computers: each one newer than this computer's is written
    /// (keeping who changed it and when, so it isn't taken for a change made here). Returns how
    /// many changed.
    pub fn apply_items(&self, items: &[Item], device: &str) -> Result<usize> {
        let mut changed = 0;
        for item in items.iter().filter(|i| is_synced(&i.key)) {
            let here: Option<Item> = self
                .conn
                .query_row("SELECT key, version, device, modified, value FROM settings_items WHERE key = ?1", params![item.key], |r| {
                    Ok(Item { key: r.get(0)?, version: r.get(1)?, device: r.get(2)?, modified: r.get(3)?, value: r.get(4)? })
                })
                .optional()?
                .map(|mut i| {
                    if i.device == "this" {
                        i.device = device.to_string();
                    }
                    i
                });
            if here.as_ref().is_some_and(|h| !item.newer_than(h)) {
                continue;
            }
            let who = if item.device == device { "this" } else { item.device.as_str() };
            self.conn.execute(
                "INSERT INTO settings_items (key, version, device, modified, value) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (key) DO UPDATE SET version = excluded.version, device = excluded.device, modified = excluded.modified, value = excluded.value",
                params![item.key, item.version, who, item.modified, item.value],
            )?;
            if item.key == SENDER_RULES {
                self.load_sender_rules(&item.value)?;
            }
            changed += 1;
        }
        Ok(changed)
    }

    /// Keeps the "sender-rules" item in step with the user's own entries (after each change).
    pub(crate) fn note_sender_rules(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT pattern, kind, action, note, added, mailbox FROM sender_rules WHERE source = 'user' ORDER BY kind, pattern, mailbox")?;
        let entries: Vec<Entry> = stmt
            .query_map([], |r| Ok(Entry { pattern: r.get(0)?, kind: r.get(1)?, action: r.get(2)?, note: r.get(3)?, added: r.get(4)?, mailbox: r.get(5)? }))?
            .collect::<rusqlite::Result<_>>()?;
        self.set_setting(SENDER_RULES, &serde_json::to_string(&entries)?)
    }

    /// The user's own entries, as another computer has them (the automatic ones stay).
    fn load_sender_rules(&self, json: &str) -> Result<()> {
        let entries: Vec<Entry> = serde_json::from_str(json)?;
        self.conn.execute("DELETE FROM sender_rules WHERE source = 'user'", [])?;
        for e in entries {
            // a user's entry replaces an automatic one of the same pattern and kind
            self.conn.execute(
                "INSERT INTO sender_rules (pattern, kind, action, source, note, added, mailbox) VALUES (?1, ?2, ?3, 'user', ?4, ?5, ?6)
                 ON CONFLICT (pattern, kind, mailbox) DO UPDATE SET action = excluded.action, note = excluded.note, source = 'user'",
                params![e.pattern, e.kind, e.action, e.note, e.added, e.mailbox],
            )?;
            if e.kind == "allow" || e.kind == "block" {
                let opposite = if e.kind == "allow" { "block" } else { "allow" };
                self.conn.execute("DELETE FROM sender_rules WHERE pattern = ?1 AND kind = ?2 AND source = 'auto' AND (?3 = '' OR mailbox = ?3)", params![e.pattern, opposite, e.mailbox])?;
            }
            if e.kind == "not-allow" {
                self.conn.execute("DELETE FROM sender_rules WHERE pattern = ?1 AND kind = 'allow' AND source = 'auto' AND (?2 = '' OR mailbox = ?2)", params![e.pattern, e.mailbox])?;
            }
        }
        Ok(())
    }

    /// Settings whose keys start with `prefix` (the window reads its own: "ui:").
    pub fn settings_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings_items WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key")?;
        Ok(stmt.query_map(params![prefix], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }
}

/// Every computer's items merged: for each key, the newest change.
pub fn merge(sets: &[Vec<Item>]) -> Vec<Item> {
    let mut by_key: std::collections::BTreeMap<String, Item> = Default::default();
    for set in sets {
        for item in set {
            match by_key.get(&item.key) {
                Some(have) if !item.newer_than(have) => {}
                _ => {
                    by_key.insert(item.key.clone(), item.clone());
                }
            }
        }
    }
    by_key.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    #[test]
    fn settings_travel_item_by_item_and_the_newest_change_wins() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = Library::open(&da.path().join("lib"), Access::Write, &Silent).unwrap();
        let b = Library::open(&db.path().join("lib"), Access::Write, &Silent).unwrap();
        a.set_setting("ui:skin", "\"gmail\"").unwrap();
        a.set_setting("background-download", "{}").unwrap(); // this computer's own: stays
        a.set_sender_rule("@spam.example", "block", None, "mine").unwrap();
        let items = a.synced_items("pc-a").unwrap();
        assert!(items.iter().all(|i| i.device == "pc-a"));
        assert!(items.iter().any(|i| i.key == SENDER_RULES) && !items.iter().any(|i| i.key == "background-download"));

        assert_eq!(b.apply_items(&items, "pc-b").unwrap(), 2);
        assert_eq!(b.setting("ui:skin").unwrap().as_deref(), Some("\"gmail\""));
        assert!(b.sender_rules().unwrap().iter().any(|r| r.pattern == "@spam.example" && r.kind == "block"));
        assert!(b.apply_items(&items, "pc-b").unwrap() == 0, "nothing newer the second time");
        // B's items it took in aren't B's own changes
        assert!(b.synced_items("pc-b").unwrap().iter().all(|i| i.device == "pc-a"));

        // B changes the look later: B's wins on A; an older change of A's doesn't come back
        let mut later = b.synced_items("pc-b").unwrap();
        for i in &mut later {
            if i.key == "ui:skin" {
                i.value = "\"yahoo\"".into();
                i.modified += 10;
                i.device = "pc-b".into();
            }
        }
        let merged = merge(&[a.synced_items("pc-a").unwrap(), later]);
        a.apply_items(&merged, "pc-a").unwrap();
        assert_eq!(a.setting("ui:skin").unwrap().as_deref(), Some("\"yahoo\""));

        // B takes the blacklist entry out: gone on A too
        let id = b.sender_rules().unwrap().into_iter().find(|r| r.pattern == "@spam.example").unwrap().id;
        b.remove_sender_rule(id).unwrap();
        let mut from_b = b.synced_items("pc-b").unwrap();
        for i in &mut from_b {
            i.modified += 20; // (later than A's)
        }
        a.apply_items(&from_b, "pc-a").unwrap();
        assert!(!a.sender_rules().unwrap().iter().any(|r| r.pattern == "@spam.example"));
    }
}
