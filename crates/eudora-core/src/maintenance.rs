//! Maintenance.
//!
//! Run by the download (the library's one writer): on a schedule (Off / Daily / Weekly at an hour
//! the user picks; by default daily at 3 AM, and only while Eudora isn't being used), or at once
//! when the window asks (`request`). Each step is reported as it goes (`activity::step`): the
//! window's status bar shows it, Activity lists it with its counts.

use chrono::{Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::Result;
use crate::library::{Library, now};

pub const SETTING: &str = "maintenance";
pub const LAST: &str = "maintenance-last";
const REQUEST_FILE: &str = "status/maintain-now";
/// The window's last sign of the user (written at most once a minute while they use it).
const ACTIVE_FILE: &str = "status/active";
/// Not used for this long = idle.
pub const IDLE_AFTER: i64 = 15 * 60;
/// A scheduled run starts only this long after its time (seconds).
pub const SLOT_WINDOW: i64 = 3 * 3600;
const KEY: &str = "maintenance";

/// A run's limits, as the long step's progress: it reports each step, and asks it to stop at the
/// run's time limit, or (scheduled, idle only) once the user is back in Eudora.
struct Limits {
    until: std::time::Instant,
    idle_only: bool,
    root: std::path::PathBuf,
    /// When the window's sign of the user was last read (every few seconds at most).
    looked: std::sync::Mutex<Option<(std::time::Instant, bool)>>,
    why: std::sync::Mutex<Option<String>>,
}

impl crate::progress::Progress for Limits {
    fn report(&self, done: u64, total: u64, message: &str) {
        crate::activity::step(KEY, NAME, message, done, total);
    }

    fn canceled(&self) -> bool {
        let stop = |why: &str| {
            if let Ok(mut w) = self.why.lock() {
                *w = Some(why.to_string());
            }
            true
        };
        if std::time::Instant::now() >= self.until {
            return stop("its time was up");
        }
        if self.idle_only {
            let Ok(mut looked) = self.looked.lock() else { return false };
            let fresh = looked.is_some_and(|(at, _)| at.elapsed() < std::time::Duration::from_secs(5));
            if !fresh {
                // (the window says it's in use at most once a minute)
                let back = idle_for(&self.root).is_some_and(|idle| idle < 90);
                *looked = Some((std::time::Instant::now(), back));
            }
            if looked.is_some_and(|(_, back)| back) {
                return stop("you came back to Eudora");
            }
        }
        false
    }
}
const NAME: &str = "Maintenance (tidying the library for speed)";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// "off", "daily" or "weekly".
    pub when: String,
    /// The hour it starts (0-23, local time).
    pub hour: u32,
    /// Weekly: the day (0 = Sunday).
    pub weekday: u32,
    /// Only while Eudora isn't being used.
    pub idle_only: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule { when: "daily".into(), hour: 3, weekday: 0, idle_only: true }
    }
}

/// The last run: when, how long, what it did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LastRun {
    pub at: i64,
    pub took: i64,
    pub said: String,
    pub failed: bool,
}

/// The window asks for a run now (the download takes it at its next look, within seconds).
pub fn request(root: &Path) -> Result<()> {
    let file = root.join(REQUEST_FILE);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::error::Error::Other(format!("creating {}: {e}", dir.display())))?;
    }
    std::fs::write(&file, now().to_string()).map_err(|e| crate::error::Error::Other(format!("writing {}: {e}", file.display())))?;
    Ok(())
}

/// Whether a run was asked for (the request used up).
pub fn requested(root: &Path) -> bool {
    std::fs::remove_file(root.join(REQUEST_FILE)).is_ok()
}

/// Whether a run is waiting to be taken.
pub fn waiting(root: &Path) -> bool {
    root.join(REQUEST_FILE).exists()
}

/// The window: the user just did something.
pub fn mark_active(root: &Path) {
    let file = root.join(ACTIVE_FILE);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&file, now().to_string());
}

/// Seconds since the user last used the window (None: no sign of them: the window is closed).
pub fn idle_for(root: &Path) -> Option<i64> {
    let at: i64 = std::fs::read_to_string(root.join(ACTIVE_FILE)).ok()?.trim().parse().ok()?;
    Some(now() - at)
}

/// The most recent scheduled time at or before `at` (None: off).
pub fn last_slot(s: &Schedule, at: i64) -> Option<i64> {
    if s.when == "off" {
        return None;
    }
    let local = Local.timestamp_opt(at, 0).single()?;
    let mut day = local.date_naive();
    let today_slot = day.and_hms_opt(s.hour.min(23), 0, 0)?;
    if local.naive_local() < today_slot {
        day = day.pred_opt()?;
    }
    if s.when == "weekly" {
        while day.weekday().num_days_from_sunday() != s.weekday % 7 {
            day = day.pred_opt()?;
        }
    }
    Local.from_local_datetime(&day.and_hms_opt(s.hour.min(23), 0, 0)?).earliest().map(|d| d.timestamp())
}

impl Library {
    pub fn maintenance_schedule(&self) -> Schedule {
        self.setting(SETTING).ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    pub fn set_maintenance_schedule(&self, s: &Schedule) -> Result<()> {
        self.set_setting(SETTING, &serde_json::to_string(s)?)
    }

    /// When maintenance runs next on its schedule (None: only when asked).
    pub fn maintenance_next(&self) -> Option<i64> {
        let s = self.maintenance_schedule();
        let period = if s.when == "weekly" { 7 * 86400 } else { 86400 };
        let now = now();
        let current = last_slot(&s, now)?;
        // (today's slot not run yet, and still within its window: due now)
        let ran = self.maintenance_last().map(|l| l.at).unwrap_or(0);
        if ran < current && now - current <= SLOT_WINDOW {
            return Some(now);
        }
        last_slot(&s, now + period)
    }

    pub fn maintenance_last(&self) -> Option<LastRun> {
        self.setting(LAST).ok().flatten().and_then(|j| serde_json::from_str(&j).ok())
    }

    /// Whether the schedule says it's time: not run since its last slot, within three hours of
    /// it (a computer off at 3 AM waits for the next night: the first run started at 2 PM and held
    /// new mail for four hours, 2026-10-22), and idle when asked to be.
    pub fn maintenance_due(&self) -> bool {
        let s = self.maintenance_schedule();
        let Some(slot) = last_slot(&s, now()) else { return false };
        let last = self.maintenance_last().map(|l| l.at).unwrap_or(0);
        if last >= slot || now() - slot > SLOT_WINDOW {
            return false;
        }
        !s.idle_only || idle_for(&self.root).is_none_or(|idle| idle >= IDLE_AFTER)
    }

    /// One run, each step reported as it goes. The library must be open for writing. `asked`:
    /// Run Maintenance Now (an hour at most) rather than the schedule (half an hour, and it stops
    /// when the user comes back). The download stores no mail meanwhile, so a run never goes on
    /// for long: the long step stops at its limit and the next run carries on.
    pub fn maintain(&self, asked: bool) -> Result<LastRun> {
        let started = now();
        crate::activity::begin(KEY, NAME, "Starting");
        let limits = Limits {
            until: std::time::Instant::now() + std::time::Duration::from_secs(if asked { 3600 } else { 1800 }),
            idle_only: !asked && self.maintenance_schedule().idle_only,
            root: self.root.clone(),
            looked: std::sync::Mutex::new(None),
            why: std::sync::Mutex::new(None),
        };
        let result = self.maintain_steps(&limits);
        let (said, failed) = match &result {
            Ok(said) => (said.clone(), false),
            Err(e) => (format!("Stopped: {e}"), true),
        };
        let run = LastRun { at: started, took: now() - started, said: said.clone(), failed };
        let _ = self.set_setting(LAST, &serde_json::to_string(&run)?);
        crate::activity::end(KEY, NAME, 1, &said);
        result.map(|_| run)
    }

    fn maintain_steps(&self, limits: &Limits) -> Result<String> {
        let step = |n: u64, what: &str| crate::activity::step(KEY, NAME, &format!("Step {n} of 6: {what}"), n - 1, 6);
        let mut said = Vec::new();

        // the quick steps first
        // 1. the folders' counts (kept by triggers: checked against the messages themselves)
        step(1, "checking every folder's message and unread counts");
        let fixed = self.recount_folders()?;
        said.push(if fixed == 0 { "counts right".to_string() } else { format!("{fixed} folder counts corrected") });

        // 2. every stored text value valid (a broken byte, the disk's or memory's, once stopped
        //    every check of new mail: `text_repair`)
        step(2, "checking that all stored text is valid");
        let repaired = self.repair_text(&crate::progress::Silent)?;
        said.push(if repaired == 0 { "all text valid".to_string() } else { format!("{repaired} broken text values repaired") });

        // 3. what the database's planner knows (a sample per index: seconds, not minutes)
        step(3, "updating the database's statistics for its indexes");
        self.conn.execute_batch("PRAGMA analysis_limit = 2000; ANALYZE; PRAGMA optimize;")?;

        // 4. Help ▸ Statistics, counted again
        step(4, "counting Help ▸ Statistics");
        let s = self.mail_stats()?;
        crate::stats::write_counted(&self.root, &s);
        said.push(format!("{} messages counted", crate::progress::thousands(s.messages)));

        // 5-6. deleted mail's space and the search index (Optimize Library without rewriting the
        // database): the long step, stopped at the run's limit or when the user comes back
        step(5, "giving back the space of deleted mail");
        // (Settings ▸ Library ▸ Keep original messages exactly: off = Exchange's exports go)
        if !self.keeps_exchange_exports() {
            let (n, bytes) = self.remove_exchange_exports()?;
            if n > 0 {
                said.push(format!("{n} kept Exchange exports removed ({:.1} GB)", bytes as f64 / 1e9));
            }
        }
        match self.optimize(false, limits) {
            Ok(r) => said.push(format!("{:.0} MB given back, search index merged from {} parts", r.bytes_freed as f64 / 1e6, r.index_segments)),
            Err(crate::error::Error::Canceled) => {
                let why = limits.why.lock().ok().and_then(|w| w.clone()).unwrap_or_else(|| "stopped".into());
                said.push(format!("giving back deleted mail's space stopped ({why}); the next run carries on"));
            }
            Err(e) => return Err(e),
        }
        self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(said.join("; "))
    }

    /// The folders' message and unread counts made right from the messages (returns how many
    /// folders were off).
    pub fn recount_folders(&self) -> Result<u64> {
        self.in_transaction(|| {
            self.conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS recount (folder INTEGER PRIMARY KEY, messages INTEGER, unread INTEGER);
                 DELETE FROM recount;
                 INSERT INTO recount SELECT l.folder, COUNT(*), SUM(m.flags & 1) FROM locations l JOIN messages m ON m.id = l.message GROUP BY l.folder;",
            )?;
            let off: i64 = self.conn.query_row(
                "SELECT (SELECT COUNT(*) FROM recount r LEFT JOIN folder_counts c ON c.folder = r.folder WHERE c.folder IS NULL OR c.messages <> r.messages OR c.unread <> r.unread)
                      + (SELECT COUNT(*) FROM folder_counts c WHERE (c.messages <> 0 OR c.unread <> 0) AND c.folder NOT IN (SELECT folder FROM recount))",
                [],
                |r| r.get(0),
            )?;
            if off > 0 {
                self.conn.execute_batch("DELETE FROM folder_counts; INSERT INTO folder_counts (folder, messages, unread) SELECT folder, messages, unread FROM recount;")?;
            }
            self.conn.execute_batch("DROP TABLE recount;")?;
            Ok(off as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schedule_finds_its_last_slot() {
        let at = Local.with_ymd_and_hms(2026, 10, 22, 10, 30, 0).unwrap().timestamp(); // a Thursday
        let daily = Schedule::default();
        assert_eq!(last_slot(&daily, at), Some(Local.with_ymd_and_hms(2026, 10, 22, 3, 0, 0).unwrap().timestamp()));
        let early = Local.with_ymd_and_hms(2026, 10, 22, 2, 0, 0).unwrap().timestamp();
        assert_eq!(last_slot(&daily, early), Some(Local.with_ymd_and_hms(2026, 10, 21, 3, 0, 0).unwrap().timestamp()), "before 3 AM: yesterday's");
        let weekly = Schedule { when: "weekly".into(), weekday: 0, ..Default::default() };
        assert_eq!(last_slot(&weekly, at), Some(Local.with_ymd_and_hms(2026, 10, 18, 3, 0, 0).unwrap().timestamp()), "the Sunday before");
        assert_eq!(last_slot(&Schedule { when: "off".into(), ..Default::default() }, at), None);
    }

    #[test]
    fn a_run_reports_and_fixes_counts() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("m.eml");
        std::fs::write(&src, "From: a@b.example\r\nSubject: hi\r\nMessage-ID: <1@x>\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\n\r\nhi\r\n").unwrap();
        crate::import(&mut library, &[src], &crate::ImportOptions::default(), &Silent).unwrap();
        assert_eq!(library.recount_folders().unwrap(), 0);
        library.conn.execute_batch("UPDATE folder_counts SET messages = messages + 5").unwrap();
        assert_eq!(library.recount_folders().unwrap(), 1, "a wrong count is found and fixed");
        assert_eq!(library.recount_folders().unwrap(), 0);
        let run = library.maintain(true).unwrap();
        assert!(!run.failed && run.said.contains("1 messages counted"), "{}", run.said);
        assert!(!library.maintenance_due() || library.maintenance_last().is_some());
        assert!(crate::stats::read_counted(&library.root).is_some());
    }

    #[test]
    fn a_request_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        request(dir.path()).unwrap();
        assert!(waiting(dir.path()) && requested(dir.path()) && !requested(dir.path()));
        assert_eq!(idle_for(dir.path()), None);
        mark_active(dir.path());
        assert!(idle_for(dir.path()).unwrap() < 5);
    }
}
