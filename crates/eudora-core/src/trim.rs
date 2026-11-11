//! Keeping a phone within the mail it was told to keep.
//!
//! With a download period (`graph_sync::PERIOD_SETTING`), messages older than it leave the
//! library; with a space limit, the oldest leave until the library takes up 90% of it (so it
//! isn't trimmed again at every check). Only this library's copies go: the servers keep their
//! mail (Microsoft 365, Gmail, IMAP), so nothing is lost and a longer period brings it back.
//! Never trimmed: mail that exists only here (POP3, imported files, classic Eudora archives),
//! flagged messages (To Do), and messages with a change still waiting for the server. And
//! **each message's server is asked first** whether it still has it (`keep_check`): only what a
//! server confirms leaves. The packs that held them are rewritten (Optimize)
//! so the space really comes back.
//!
//! The same choice keeps a computer with little room (a tablet, a small laptop) to recent mail
//! (Settings ▸ Library ▸ Mail kept on this computer): any period, from a slider or typed.

use rusqlite::params;

use crate::error::Result;
use crate::graph_sync;
use crate::library::{Library, now};
use crate::marks::FLAGGED;
use crate::progress::Progress;

/// After trimming by space: the newest message trimmed (unix seconds). Older mail isn't asked
/// of the servers again while the limit is by space (setting).
pub const TRIMMED_BEFORE: &str = "download-trimmed-before";

/// Trimmed down to this share of the space limit.
const SPACE_TARGET: f64 = 0.9;

/// Trimming by space never takes mail from the last week.
const KEEP_NEWEST: i64 = 7 * 86_400;

/// A period's edge is trimmed a day late: the servers' "received" and the message's own date
/// can differ by hours, and a message trimmed must not be downloaded again at once.
const GRACE: i64 = 86_400;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TrimReport {
    pub messages: u64,
    /// Bytes the trimmed messages took in the packs.
    pub bytes: u64,
    /// The library's size before and after (bytes).
    pub before: u64,
    pub after: u64,
    /// Older messages kept because no server confirmed it still has them.
    pub unconfirmed: u64,
    /// What couldn't be checked (for the log).
    pub notes: Vec<String>,
}

/// What keeping only the last `days` would mean (Settings ▸ Library, before it's chosen).
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct KeepPreview {
    /// The edge (unix seconds): mail older than this.
    pub before: i64,
    /// Every message older than that, and their stored size.
    pub older: u64,
    pub older_bytes: u64,
    /// Of those, the ones that would leave once their server confirms it has them.
    pub removable: u64,
    pub removable_bytes: u64,
    /// Kept: mail only on this computer (POP3, imported files, classic Eudora mailboxes).
    pub only_here: u64,
    /// Kept: flagged (To Do) or with a change still waiting for its server.
    pub flagged_or_waiting: u64,
    /// The library now: messages and size on disk.
    pub messages: u64,
    pub library_bytes: u64,
}

/// The library counted by day (unix day) and kind: [could leave, flagged or waiting, only here],
/// each (messages, stored bytes). `Library::keep_counts`.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct KeepCounts {
    pub days: Vec<(i64, [(u64, u64); 3])>,
    pub library_bytes: u64,
}

impl KeepCounts {
    /// What keeping only the last `days` would mean.
    pub fn preview(&self, days: u32) -> KeepPreview {
        let edge = now() - i64::from(days) * 86_400 - GRACE;
        let mut p = KeepPreview { before: edge, library_bytes: self.library_bytes, ..Default::default() };
        for (day, kinds) in &self.days {
            let all: u64 = kinds.iter().map(|k| k.0).sum();
            p.messages += all;
            // (a whole day older than the edge's day: the edge itself is a day late anyway)
            if (day + 1) * 86_400 <= edge {
                p.older += all;
                p.older_bytes += kinds.iter().map(|k| k.1).sum::<u64>();
                p.removable += kinds[0].0;
                p.removable_bytes += kinds[0].1;
                p.flagged_or_waiting += kinds[1].0;
                p.only_here += kinds[2].0;
            }
        }
        p
    }
}

/// Asks the servers which messages they still have (`keep_check::confirm_on_servers`).
pub type Confirm<'a> = dyn FnMut(&Library, &[i64]) -> Result<crate::keep_check::Confirmed> + 'a;

/// Account kinds whose mail is on a server (so this copy can go).
const ON_A_SERVER: &str = "('graph', 'graph-shared', 'gmail', 'imap')";

impl Library {
    /// Trims the library to the period or space chosen (nothing when neither is). Run by the
    /// download (the one writer) between checks.
    pub fn trim_to_limit(&self, confirm: &mut Confirm, progress: &dyn Progress) -> Result<TrimReport> {
        let days = graph_sync::download_period(self).flatten();
        let space = graph_sync::download_space_mb(self).map(|mb| mb * 1_000_000);
        self.trim_with(days, space, confirm, progress)
    }

    /// What keeping only the last `days` would remove and keep (nothing is changed).
    pub fn keep_preview(&self, days: u32) -> Result<KeepPreview> {
        Ok(self.keep_counts()?.preview(days))
    }

    /// The library's messages counted by day (their own date or their server's, the later) and
    /// by whether they could leave: once, so a slider's every position is worked out at once.
    pub fn keep_counts(&self) -> Result<KeepCounts> {
        let sql = format!(
            "WITH loc AS (SELECT l.message, MAX(l.date) AS ldate, MAX(a.kind NOT IN {ON_A_SERVER}) AS here
                            FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account GROUP BY l.message)
             SELECT MAX(m.date, COALESCE(loc.ldate, 0)) / 86400 AS day,
                    CASE WHEN loc.message IS NULL OR loc.here THEN 2 WHEN m.flags & ?1 <> 0 THEN 1 ELSE 0 END AS kind,
                    COUNT(*), COALESCE(SUM(m.pack_len), 0)
               FROM messages m LEFT JOIN loc ON loc.message = m.id GROUP BY day, kind"
        );
        let mut days: std::collections::BTreeMap<i64, [(u64, u64); 3]> = std::collections::BTreeMap::new();
        {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![FLAGGED], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?;
            for row in rows {
                let (day, kind, n, bytes) = row?;
                let e = &mut days.entry(day).or_default()[kind.clamp(0, 2) as usize];
                e.0 += n.max(0) as u64;
                e.1 += bytes.max(0) as u64;
            }
        }
        // messages with a change waiting for their server stay too (few: moved by hand)
        let waiting = self.messages_with_changes_waiting()?;
        if !waiting.is_empty() {
            let mut stmt = self.conn.prepare_cached(&format!(
                "SELECT MAX(m.date, COALESCE((SELECT MAX(l.date) FROM locations l WHERE l.message = m.id), 0)) / 86400, m.pack_len FROM messages m
                  WHERE m.id = ?1 AND m.flags & ?2 = 0 AND EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id)
                    AND NOT EXISTS (SELECT 1 FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
                                    WHERE l.message = m.id AND a.kind NOT IN {ON_A_SERVER})"
            ))?;
            for id in waiting {
                if let Some((day, bytes)) = stmt.query_row(params![id, FLAGGED], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).ok() {
                    let e = days.entry(day).or_default();
                    e[0].0 = e[0].0.saturating_sub(1);
                    e[0].1 = e[0].1.saturating_sub(bytes.max(0) as u64);
                    e[1].0 += 1;
                    e[1].1 += bytes.max(0) as u64;
                }
            }
        }
        Ok(KeepCounts { days: days.into_iter().map(|(d, k)| (d, k)).collect(), library_bytes: graph_sync::library_bytes(&self.root) })
    }

    fn trim_with(&self, days: Option<u32>, space: Option<u64>, confirm: &mut Confirm, progress: &dyn Progress) -> Result<TrimReport> {
        let mut report = TrimReport { before: graph_sync::library_bytes(&self.root), ..Default::default() };
        report.after = report.before;
        if days.is_none() && space.is_none() {
            return Ok(report);
        }
        let mut chosen: Vec<(i64, i64, u64)> = Vec::new(); // (id, date, pack bytes)
        if let Some(days) = days {
            let edge = now() - i64::from(days) * 86_400 - GRACE;
            chosen = self.trimmable(Some(edge), None)?;
        }
        if let Some(limit) = space {
            let excess = report.before.saturating_sub((limit as f64 * SPACE_TARGET) as u64);
            if report.before > limit && excess > 0 {
                let mut freed = 0u64;
                // (the last week always stays: a library whose own files outgrow a small limit
                // mustn't lose its newest mail to it)
                for (id, date, bytes) in self.trimmable(Some(now() - KEEP_NEWEST), Some(excess))? {
                    if freed >= excess {
                        break;
                    }
                    freed += bytes;
                    chosen.push((id, date, bytes));
                }
            }
        }
        if chosen.is_empty() {
            return Ok(report);
        }
        // only what a server confirms it still has (`keep_check`)
        let ids: Vec<i64> = chosen.iter().map(|c| c.0).collect();
        let confirmed = confirm(self, &ids)?;
        let asked = chosen.len();
        chosen.retain(|c| confirmed.on_server.contains(&c.0));
        report.unconfirmed = (asked - chosen.len()) as u64;
        report.notes = confirmed.notes;
        if chosen.is_empty() {
            return Ok(report);
        }
        let total = chosen.len();
        for (n, chunk) in chosen.chunks(500).enumerate() {
            if progress.canceled() {
                break;
            }
            progress.report((n * 500) as u64, total as u64, &format!("Trimming older mail: {} of {}", (n * 500).min(total), total));
            self.in_transaction(|| {
                for (id, _, _) in chunk {
                    self.purge_message(*id)?;
                }
                Ok(())
            })?;
            report.messages += chunk.len() as u64;
            report.bytes += chunk.iter().map(|c| c.2).sum::<u64>();
        }
        if space.is_some() {
            if let Some(newest) = chosen.iter().map(|c| c.1).max() {
                let before: i64 = self.setting(TRIMMED_BEFORE)?.and_then(|v| v.parse().ok()).unwrap_or(0);
                self.set_setting(TRIMMED_BEFORE, &newest.max(before).to_string())?;
            }
        }
        // the space back: packs mostly trimmed are rewritten, the search index drops them
        self.optimize(false, progress)?;
        report.after = graph_sync::library_bytes(&self.root);
        Ok(report)
    }

    /// Messages this library may let go, oldest first: on a server, not flagged, no change
    /// waiting; older than `before` when given; with `enough` (bytes), only as many as cover it.
    fn trimmable(&self, before: Option<i64>, enough: Option<u64>) -> Result<Vec<(i64, i64, u64)>> {
        let sql = format!(
            "SELECT m.id, m.date, m.pack_len FROM messages m
             WHERE m.flags & ?1 = 0
               AND (?2 IS NULL OR (m.date < ?2 AND (SELECT MAX(l.date) FROM locations l WHERE l.message = m.id) < ?2))
               AND EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id)
               AND NOT EXISTS (SELECT 1 FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
                               WHERE l.message = m.id AND a.kind NOT IN {ON_A_SERVER})
             ORDER BY m.date, m.id"
        );
        let waiting = self.messages_with_changes_waiting()?;
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![FLAGGED, before])?;
        let mut out = Vec::new();
        let mut covered = 0u64;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if waiting.contains(&id) {
                continue;
            }
            let bytes = r.get::<_, i64>(2)?.max(0) as u64;
            out.push((id, r.get(1)?, bytes));
            covered += bytes;
            if enough.is_some_and(|e| covered >= e) {
                break;
            }
        }
        Ok(out)
    }

    /// Messages named by a change still waiting for a server (moves, marks, deletes…).
    pub(crate) fn messages_with_changes_waiting(&self) -> Result<std::collections::HashSet<i64>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(payload, '$.message') FROM pending_ops WHERE json_valid(payload) AND json_extract(payload, '$.message') IS NOT NULL")?;
        let ids = stmt.query_map([], |r| r.get::<_, Option<i64>>(0))?.filter_map(|r| r.ok().flatten()).collect();
        Ok(ids)
    }
}

/// With a space limit: the oldest moment downloaded (the newest message trimmed so far).
pub fn trimmed_before(library: &Library) -> Option<i64> {
    graph_sync::download_space_mb(library)?;
    library.setting(TRIMMED_BEFORE).ok().flatten().and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use crate::graph_sync;
    use crate::library::{Access, Library, now};
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    /// Every message is still on its server.
    fn all_there(_: &Library, ids: &[i64]) -> crate::Result<crate::keep_check::Confirmed> {
        Ok(crate::keep_check::Confirmed { on_server: ids.iter().copied().collect(), notes: Vec::new() })
    }

    fn library_with(dir: &std::path::Path, ages_days: &[i64]) -> (Library, Vec<i64>) {
        let mut library = Library::open(&dir.join("lib"), Access::Write, &Silent).unwrap();
        let mut files = Vec::new();
        for (n, age) in ages_days.iter().enumerate() {
            let when = chrono::DateTime::from_timestamp(now() - age * 86_400, 0).unwrap().to_rfc2822();
            let file = dir.join(format!("{n}.eml"));
            let body = "x".repeat(2_000);
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Hi {n}\r\nDate: {when}\r\nMessage-ID: <{n}@a>\r\n\r\n{body} {n}\r\n")).unwrap();
            files.push(file);
        }
        import(&mut library, &files, &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let conn = &library.conn;
        let account: i64 = conn.query_row("SELECT f.account FROM locations l JOIN folders f ON f.id = l.folder LIMIT 1", [], |r| r.get(0)).unwrap();
        conn.execute("UPDATE accounts SET kind = 'graph', address = 'me@x.com' WHERE id = ?1", [account]).unwrap();
        conn.execute("UPDATE locations SET date = (SELECT date FROM messages WHERE id = locations.message)", []).unwrap();
        let ids = conn.prepare("SELECT id FROM messages ORDER BY date").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        (library, ids)
    }

    #[test]
    fn a_period_trims_older_mail_but_keeps_flagged_and_local_mail() {
        let dir = tempfile::tempdir().unwrap();
        let (library, ids) = library_with(dir.path(), &[400, 200, 100, 10, 1]);
        // ids oldest first: 400, 200, 100, 10, 1 days old; the 200-day one is flagged
        library.conn.execute("UPDATE messages SET flags = flags | ?1 WHERE id = ?2", rusqlite::params![crate::marks::FLAGGED, ids[1]]).unwrap();
        // nothing chosen: nothing trimmed
        assert_eq!(library.trim_to_limit(&mut all_there, &Silent).unwrap().messages, 0);
        graph_sync::set_download_period(&library, Some(90)).unwrap();
        let report = library.trim_to_limit(&mut all_there, &Silent).unwrap();
        assert_eq!(report.messages, 2);
        let left: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY date").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(left, vec![ids[1], ids[3], ids[4]]);
        // a message its server no longer has stays (the 10-day one is gone from the server)
        graph_sync::set_download_period(&library, Some(5)).unwrap();
        let preview = library.keep_preview(5).unwrap();
        assert_eq!((preview.older, preview.removable, preview.only_here, preview.flagged_or_waiting), (2, 1, 0, 1));
        let ids_10 = ids[3];
        let mut gone = |_: &Library, asked: &[i64]| -> crate::Result<crate::keep_check::Confirmed> {
            Ok(crate::keep_check::Confirmed { on_server: asked.iter().copied().filter(|id| *id != ids_10).collect(), notes: vec!["x".into()] })
        };
        let report = library.trim_to_limit(&mut gone, &Silent).unwrap();
        assert_eq!((report.messages, report.unconfirmed), (0, 1));
        assert!(library.conn.query_row("SELECT EXISTS (SELECT 1 FROM messages WHERE id = ?1)", [ids_10], |r| r.get::<_, bool>(0)).unwrap());
        // mail only here (imported files) is never trimmed
        library.conn.execute("UPDATE accounts SET kind = 'files'", []).unwrap();
        graph_sync::set_download_period(&library, Some(1)).unwrap();
        assert_eq!(library.trim_to_limit(&mut all_there, &Silent).unwrap().messages, 0);
    }

    #[test]
    fn a_space_limit_trims_the_oldest_keeps_the_last_week_and_remembers_where() {
        let dir = tempfile::tempdir().unwrap();
        let (library, ids) = library_with(dir.path(), &[50, 40, 30, 3, 1]);
        graph_sync::set_download_space(&library, 1_000).unwrap();
        // a limit a little under the size now: the oldest go first
        let size = graph_sync::library_bytes(&library.root);
        let report = library.trim_with(None, Some(size - 1), &mut all_there, &Silent).unwrap();
        assert!(report.messages >= 1);
        let left: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY date").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert!(!left.contains(&ids[0]), "the oldest went");
        assert_eq!(super::trimmed_before(&library).is_some(), true);
        // a limit nothing could reach: everything older than a week goes, the last week stays
        library.trim_with(None, Some(1), &mut all_there, &Silent).unwrap();
        let left: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY date").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(left, vec![ids[3], ids[4]]);
    }
}
