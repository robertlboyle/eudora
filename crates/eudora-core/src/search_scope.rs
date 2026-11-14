//! Which mailboxes a search looks in.
//!
//! The setting `search-scope` holds the switches the user turned, by key: `shared` (every shared
//! mailbox), `accounts` (the user's own accounts), `account:<id>`, `folder:<id>`; true = searched,
//! false = left out. A folder takes the nearest switch above it: itself, then the folders above it
//! (by path), then its account, then its section; nothing set = searched. So all shared mailboxes
//! can be left out and one of them, or one folder of it, put back.
//!
//! Nothing is taken out of the index. A search leaves out messages found **only** in folders left
//! out (one also in a searched folder still shows), worked out from the database, which always
//! knows where each message is (the index's folder field is where it was first stored). The set
//! is remembered until mail moves into or out of those folders.

use crate::library::Library;
use crate::{Error, Result};
use rusqlite::params;
use std::collections::HashMap;
use std::sync::Arc;

pub const SETTING: &str = "search-scope";

/// The switches the user turned (key → searched).
pub type Scope = HashMap<String, bool>;

/// What the window shows for each folder and level: searched or not, and whether that was set
/// there (a switch of its own) or comes from above.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ScopeState {
    /// The switches turned (key → searched).
    pub switches: Scope,
    /// Folders left out of search (after every switch is applied).
    pub excluded: Vec<i64>,
}

impl Library {
    pub fn search_scope(&self) -> Result<Scope> {
        Ok(self.setting(SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// Turns one switch: `searched` = Some(on / off), or None to follow the level above again.
    /// A switch that says the same as the level above is dropped (nothing changes, and turning the
    /// level above later carries it along).
    pub fn set_search_scope(&self, key: &str, searched: Option<bool>) -> Result<ScopeState> {
        let valid = key == "shared"
            || key == "accounts"
            || key.strip_prefix("account:").or_else(|| key.strip_prefix("folder:")).is_some_and(|n| n.parse::<i64>().is_ok());
        if !valid {
            return Err(Error::Other(format!("“{key}” isn't a search switch: shared, accounts, account:<id>, or folder:<id>")));
        }
        let mut scope = self.search_scope()?;
        scope.remove(key);
        if let Some(on) = searched {
            if self.inherited(&scope, key)? != on {
                scope.insert(key.to_string(), on);
            }
        }
        self.set_setting(SETTING, &serde_json::to_string(&scope)?)?;
        self.search_scope_state()
    }

    /// The switches and the folders left out, for the window.
    pub fn search_scope_state(&self) -> Result<ScopeState> {
        let switches = self.search_scope()?;
        let excluded = self.excluded_folders(&switches)?;
        Ok(ScopeState { switches, excluded })
    }

    /// What a key would be without a switch of its own: the level above's.
    fn inherited(&self, scope: &Scope, key: &str) -> Result<bool> {
        if key == "shared" || key == "accounts" {
            return Ok(true);
        }
        if let Some(account) = key.strip_prefix("account:").and_then(|n| n.parse::<i64>().ok()) {
            let kind: String = self.conn.query_row("SELECT kind FROM accounts WHERE id = ?1", params![account], |r| r.get(0)).unwrap_or_default();
            return Ok(*scope.get(section(&kind)).unwrap_or(&true));
        }
        let folder: i64 = key.strip_prefix("folder:").and_then(|n| n.parse().ok()).unwrap_or(0);
        let (account, path, kind): (i64, String, String) = self.conn.query_row(
            "SELECT f.account, f.path, a.kind FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
            params![folder],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let paths = self.account_folder_paths(account)?;
        Ok(searched(scope, account, &kind, &path, &paths))
    }

    /// Every folder's path → id in an account (for the folders above one).
    fn account_folder_paths(&self, account: i64) -> Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT path, id FROM folders WHERE account = ?1")?;
        let rows = stmt.query_map(params![account], |r| Ok((r.get::<_, String>(0)?.to_lowercase(), r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The folders left out of search.
    pub(crate) fn excluded_folders(&self, scope: &Scope) -> Result<Vec<i64>> {
        // (no switch turned off: nothing left out)
        if scope.values().all(|on| *on) {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare("SELECT f.id, f.account, f.path, a.kind FROM folders f JOIN accounts a ON a.id = f.account")?;
        let folders: Vec<(i64, i64, String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut by_account: HashMap<i64, HashMap<String, i64>> = HashMap::new();
        for (id, account, path, _) in &folders {
            by_account.entry(*account).or_default().insert(path.to_lowercase(), *id);
        }
        Ok(folders
            .iter()
            .filter(|(id, account, path, kind)| {
                let paths = &by_account[account];
                !searched_folder(scope, *id, *account, kind, path, paths)
            })
            .map(|f| f.0)
            .collect())
    }

    /// The messages a search leaves out: those found only in folders left out. None when nothing
    /// is left out. Worked out once (about a second for a million messages) and then kept up to
    /// date by looking only at the messages mail arrived for since; worked out again when mail
    /// moved into or out of the folders left out, or the switches changed.
    pub fn search_hidden(&self) -> Result<Option<Arc<crate::index::IdSet>>> {
        let scope = self.search_scope()?;
        let excluded = self.excluded_folders(&scope)?;
        if excluded.is_empty() {
            return Ok(None);
        }
        let list = excluded.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let (sum, max): (i64, i64) = self.conn.query_row(
            &format!("SELECT COALESCE((SELECT SUM(messages) FROM folder_counts WHERE folder IN ({list})), 0), (SELECT COALESCE(MAX(rowid), 0) FROM locations)"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let cached = self.search_hidden.lock().ok().and_then(|c| c.clone());
        if let Some(c) = cached.filter(|c| c.list == list) {
            if c.sum == sum && c.max == max {
                return Ok(Some(c.set));
            }
            // only new mail since (every change to those folders' counts is mail that arrived): the
            // messages it arrived for are looked at again
            if max > c.max {
                let added: i64 = self.conn.query_row(&format!("SELECT COUNT(*) FROM locations WHERE rowid > ?1 AND folder IN ({list})"), params![c.max], |r| r.get(0))?;
                if c.sum + added == sum {
                    let mut stmt = self.conn.prepare(&format!(
                        "SELECT message, SUM(folder NOT IN ({list})) = 0 FROM locations
                         WHERE message IN (SELECT message FROM locations WHERE rowid > ?1) GROUP BY message"
                    ))?;
                    let changes: Vec<(i64, bool)> = stmt.query_map(params![c.max], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                    let mut set = (*c.set).clone();
                    for (message, hidden) in changes {
                        set.set(message as u64, hidden);
                    }
                    let set = Arc::new(set);
                    self.remember_hidden(HiddenCache { list, sum, max, set: set.clone() });
                    return Ok(Some(set));
                }
            }
        }
        let _timed = crate::speed::Timer::new("Search: mailboxes left out");
        let mut stmt = self.conn.prepare(&format!("SELECT message FROM locations GROUP BY message HAVING SUM(folder NOT IN ({list})) = 0"))?;
        let ids: Vec<u64> = stmt.query_map([], |r| r.get::<_, i64>(0))?.map(|r| r.map(|i| i as u64)).collect::<rusqlite::Result<_>>()?;
        let set = Arc::new(crate::index::IdSet::new(ids));
        self.remember_hidden(HiddenCache { list, sum, max, set: set.clone() });
        Ok(Some(set))
    }

    fn remember_hidden(&self, cache: HiddenCache) {
        if let Ok(mut c) = self.search_hidden.lock() {
            *c = Some(cache);
        }
    }
}

/// What `search_hidden` worked out, and from what: the folders left out, their message count and
/// the newest location then.
#[derive(Clone, Debug)]
pub struct HiddenCache {
    list: String,
    sum: i64,
    max: i64,
    set: Arc<crate::index::IdSet>,
}

/// The section an account belongs to: other people's mailboxes shared with the user, or the
/// user's own accounts.
fn section(kind: &str) -> &'static str {
    if kind == "graph-shared" { "shared" } else { "accounts" }
}

/// Whether a folder is searched: its own switch, else `searched` from the folders above.
fn searched_folder(scope: &Scope, id: i64, account: i64, kind: &str, path: &str, paths: &HashMap<String, i64>) -> bool {
    match scope.get(&format!("folder:{id}")) {
        Some(on) => *on,
        None => searched(scope, account, kind, path, paths),
    }
}

/// The nearest switch above a folder: the folders above it by path, then its account, then its section; nothing set = searched.
fn searched(scope: &Scope, account: i64, kind: &str, path: &str, paths: &HashMap<String, i64>) -> bool {
    let lower = path.to_lowercase();
    // (the folder's own switch is looked at by the caller)
    let mut at: Option<&str> = lower.rsplit_once('/').map(|(p, _)| p);
    while let Some(p) = at {
        if let Some(on) = paths.get(p).and_then(|id| scope.get(&format!("folder:{id}"))) {
            return *on;
        }
        at = p.rsplit_once('/').map(|(q, _)| q);
    }
    if let Some(on) = scope.get(&format!("account:{account}")) {
        return *on;
    }
    *scope.get(section(kind)).unwrap_or(&true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_switch_above_decides() {
        let paths: HashMap<String, i64> = [("inbox".to_string(), 1), ("inbox/a".to_string(), 2), ("inbox/a/b".to_string(), 3)].into();
        let mut scope = Scope::new();
        assert!(searched_folder(&scope, 3, 7, "graph-shared", "Inbox/A/B", &paths));
        scope.insert("shared".into(), false);
        assert!(!searched_folder(&scope, 3, 7, "graph-shared", "Inbox/A/B", &paths));
        assert!(searched_folder(&scope, 3, 8, "graph", "Inbox/A/B", &paths), "own accounts aren't shared mailboxes");
        scope.insert("account:7".into(), true);
        assert!(searched_folder(&scope, 3, 7, "graph-shared", "Inbox/A/B", &paths), "one shared mailbox put back");
        scope.insert("folder:1".into(), false);
        assert!(!searched_folder(&scope, 3, 7, "graph-shared", "Inbox/A/B", &paths), "a folder above left out");
        scope.insert("folder:3".into(), true);
        assert!(searched_folder(&scope, 3, 7, "graph-shared", "Inbox/A/B", &paths), "its own switch wins");
    }
}

/// Times searches on a real library with every shared mailbox left out (read-only; nothing is
/// changed): `EUDORA_TRY_LIBRARY=<library> cargo test --release -p eudora-core time_real -- --ignored --nocapture`.
#[cfg(test)]
mod timing {
    #[test]
    #[ignore]
    fn time_real_library() {
        let Ok(path) = std::env::var("EUDORA_TRY_LIBRARY") else { return };
        let l = crate::library::Library::open(std::path::Path::new(&path), crate::library::Access::Read, &crate::Silent).unwrap();
        let t = std::time::Instant::now();
        let mut stmt = l.conn.prepare("SELECT message FROM locations GROUP BY message HAVING SUM(folder NOT IN (SELECT f.id FROM folders f JOIN accounts a ON a.id = f.account WHERE a.kind = 'graph-shared')) = 0").unwrap();
        let ids: Vec<u64> = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap().map(|r| r.unwrap() as u64).collect();
        let set = std::sync::Arc::new(crate::index::IdSet::new(ids));
        println!("left out: {} messages, worked out in {:?}", set.len(), t.elapsed());
        for q in ["invoice", "the", "from:alex", "meeting tomorrow", "budget"] {
            for _ in 0..2 {
                let a = l.index.search_filtered(q, 200, crate::SortOrder::Newest, None, None, None).unwrap();
                let b = l.index.search_filtered(q, 200, crate::SortOrder::Newest, None, Some(set.clone()), None).unwrap();
                println!("{q:18} everything: {:>9} in {:>7.1?}   leaving out: {:>9} (+{} hidden) in {:>7.1?}", a.total, a.took, b.total, b.hidden, b.took);
            }
        }
    }
}
