//! Help ▸ Statistics. Every count comes from an index (dates, the address index, the spam
//! filter's decisions): seconds on millions of messages.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::library::{Library, now};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Period {
    pub label: String,
    pub received: u64,
    pub sent: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Storage {
    /// The database (and its write-ahead log).
    pub database: u64,
    /// The packs: every message's original bytes, compressed (and old attachment files).
    pub packs: u64,
    pub search_index: u64,
    pub header_index: u64,
    /// The messages' own size uncompressed (each message once), and the other originals kept.
    pub messages_uncompressed: u64,
    pub copies_uncompressed: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MailStats {
    pub messages: u64,
    pub oldest: Option<i64>,
    pub newest: Option<i64>,
    pub periods: Vec<Period>,
    /// The busiest days ever: ("2003-04-07", messages).
    pub busiest_days: Vec<(String, u64)>,
    /// Messages per weekday over the last year, Sunday first.
    pub weekdays: Vec<u64>,
    /// The spam filter's decisions (only mail it has sorted): (kind, messages).
    pub kinds: Vec<(String, u64)>,
    pub sorted: u64,
    pub storage: Storage,
    /// When these were counted (Unix seconds).
    pub counted: i64,
}

/// The spam filter's categories as the user thinks of them.
fn kind_of(category: &str) -> &'static str {
    match category {
        "people" | "priority" => "Personal",
        "notification" | "notifications" => "Notifications",
        "bulk" | "marketing" => "Marketing",
        "list" | "lists" => "Mailing lists",
        "spam" | "blocked" => "Spam",
        _ => "Other",
    }
}

fn dir_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries.flatten().map(|e| match e.metadata() {
        Ok(m) if m.is_dir() => dir_size(&e.path()),
        Ok(m) => m.len(),
        Err(_) => 0,
    }).sum()
}

impl Library {
    pub fn mail_stats(&self) -> Result<MailStats> {
        let one = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<u64> { Ok(self.conn.query_row(sql, p, |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0) as u64) };
        let mut s = MailStats { messages: one("SELECT COUNT(*) FROM messages", &[])?, ..Default::default() };
        (s.oldest, s.newest) = self.date_range()?;

        // received / sent: sent = from one of the user's own addresses
        let own = self.address_ids(&self.own_addresses()?)?;
        let marks = vec!["?"; own.len().max(1)].join(",");
        let sent_since = |since: i64| -> Result<u64> {
            if own.is_empty() {
                return Ok(0);
            }
            let sql = format!("SELECT COUNT(DISTINCT ma.message) FROM message_addresses ma JOIN messages m ON m.id = ma.message WHERE ma.role = 0 AND ma.address IN ({marks}) AND m.date >= ?");
            let mut values: Vec<i64> = own.clone();
            values.push(since);
            Ok(self.conn.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get::<_, i64>(0))? as u64)
        };
        let t = now();
        for (label, since) in [("Last 30 days", t - 30 * 86_400), ("Last 12 months", t - 365 * 86_400), ("Ever", i64::MIN)] {
            let all = one("SELECT COUNT(*) FROM messages WHERE date >= ?1", &[&since])?;
            let sent = sent_since(since)?;
            s.periods.push(Period { label: label.into(), received: all.saturating_sub(sent), sent });
        }

        // the busiest days ever, and the weekdays of the last year (local time)
        let mut stmt = self.conn.prepare(
            "SELECT date(date, 'unixepoch', 'localtime') d, COUNT(*) n FROM messages WHERE date > 0 GROUP BY d ORDER BY n DESC LIMIT 10",
        )?;
        s.busiest_days = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?;
        s.weekdays = vec![0; 7];
        let mut stmt = self.conn.prepare("SELECT CAST(strftime('%w', date, 'unixepoch', 'localtime') AS INTEGER), COUNT(*) FROM messages WHERE date >= ?1 GROUP BY 1")?;
        for row in stmt.query_map(params![t - 365 * 86_400], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (day, n) = row?;
            if let Some(slot) = s.weekdays.get_mut(day as usize) {
                *slot = n as u64;
            }
        }

        // what the spam filter decided (the mail it has sorted)
        let mut kinds: Vec<(String, u64)> = Vec::new();
        let mut stmt = self.conn.prepare("SELECT category, COUNT(*) FROM triage GROUP BY category")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (category, n) = row?;
            let k = kind_of(&category).to_string();
            match kinds.iter_mut().find(|(name, _)| *name == k) {
                Some((_, c)) => *c += n as u64,
                None => kinds.push((k, n as u64)),
            }
        }
        kinds.sort_by(|a, b| b.1.cmp(&a.1));
        s.sorted = kinds.iter().map(|(_, n)| n).sum();
        s.kinds = kinds;

        // storage
        s.storage = Storage {
            database: ["eudora.db", "eudora.db-wal"].iter().filter_map(|n| std::fs::metadata(self.root.join(n)).ok()).map(|m| m.len()).sum(),
            packs: self.packs.disk_size()?,
            search_index: dir_size(&self.root.join("index")),
            header_index: dir_size(&self.root.join("header-index")),
            messages_uncompressed: one("SELECT SUM(size) FROM messages", &[])?,
            copies_uncompressed: one("SELECT SUM(c.size) FROM copies c JOIN messages m ON m.id = c.message WHERE c.content_hash <> m.content_hash", &[])?,
        };
        s.counted = now();
        Ok(s)
    }
}

/// Help ▸ Statistics comes up at once: the download counts them hourly on a read-only
/// connection of its own (never holding up its writing) and keeps them in `status/stats.json`;
/// the window shows that copy and when it was counted.
pub const STATS_EVERY: i64 = 3600;

fn stats_file(library: &std::path::Path) -> std::path::PathBuf {
    library.join("status").join("stats.json")
}

/// The statistics as last counted (None: never yet).
pub fn read_counted(library: &std::path::Path) -> Option<MailStats> {
    std::fs::read(stats_file(library)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Keeps them for the window (written aside, then renamed).
pub fn write_counted(library: &std::path::Path, s: &MailStats) {
    let file = stats_file(library);
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(library));
    let fresh = file.with_extension("json.new");
    if serde_json::to_vec(s).ok().and_then(|b| std::fs::write(&fresh, b).ok()).is_some() {
        let _ = std::fs::rename(&fresh, &file);
    }
}

/// Counts them again in a thread of its own when the kept copy is older than an hour (or
/// missing); one count at a time.
pub fn refresh_in_background(library: &std::path::Path) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static COUNTING: AtomicBool = AtomicBool::new(false);
    let counted = read_counted(library).map(|s| s.counted);
    if let Some(at) = counted {
        crate::activity::next("stats", "Counting Help ▸ Statistics", Some(at + STATS_EVERY));
    }
    if counted.is_some_and(|at| now() - at < STATS_EVERY) || COUNTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let root = library.to_path_buf();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let counted = Library::open(&root, crate::library::Access::Read, &crate::progress::Silent).and_then(|l| l.mail_stats());
        if let Ok(s) = &counted {
            write_counted(&root, s);
            crate::activity::did("stats", "Counting Help ▸ Statistics", s.messages, &format!("{} messages counted in {:.0} s", crate::progress::thousands(s.messages), started.elapsed().as_secs_f64()));
        }
        COUNTING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    #[test]
    fn received_sent_days_and_sizes_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let day = chrono::Utc::now().format("%a, %d %b %Y 10:00:00 +0000").to_string();
        let files: Vec<_> = (1..=4)
            .map(|n| {
                let from = if n == 4 { "Me <me@x.com>" } else { "Ann <ann@y.com>" };
                let p = src.join(format!("{n}.eml"));
                std::fs::write(&p, format!("From: {from}\r\nTo: me@x.com\r\nSubject: m{n}\r\nDate: {day}\r\nMessage-ID: <{n}@x>\r\n\r\nhello\r\n")).unwrap();
                p
            })
            .collect();
        crate::import(&mut library, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        library.conn.execute("UPDATE accounts SET address = 'me@x.com'", []).unwrap();
        library.conn.execute("INSERT INTO triage (message, category, reasons, decided) SELECT id, CASE WHEN id = 1 THEN 'bulk' ELSE 'people' END, '[]', 0 FROM messages", []).unwrap();
        let s = library.mail_stats().unwrap();
        assert_eq!(s.messages, 4);
        let month = &s.periods[0];
        assert_eq!((month.received, month.sent), (3, 1), "{:?}", s.periods);
        assert_eq!(s.busiest_days[0].1, 4);
        assert_eq!(s.weekdays.iter().sum::<u64>(), 4);
        assert_eq!(s.kinds, vec![("Personal".to_string(), 3), ("Marketing".to_string(), 1)]);
        assert!(s.storage.packs > 0 && s.storage.messages_uncompressed > 0);
    }
}
