//! What each background job did to each message.
//!
//! One row per message and thing done (`job_log`): the job's key (as in Activity: "rules",
//! "server", "archive", "views", "alerts"…), its run (when the round started: the rows of one
//! round go together), when, the message, and what was done in words ("Rule “Receipts”: moved to
//! Receipts"). Kept 30 days.

use crate::error::Result;
use crate::library::{Library, now};
use rusqlite::params;
use serde::Serialize;
use std::cell::Cell;
use std::sync::atomic::{AtomicI64, Ordering};

/// When the current round of the background jobs started (its rows share it).
static RUN: AtomicI64 = AtomicI64::new(0);
/// When old rows were last cleared out.
static PRUNED: AtomicI64 = AtomicI64::new(0);
const KEEP_DAYS: i64 = 30;

thread_local! {
    /// The job whose work is being done on this thread (the spam filter logs under the job that
    /// ran it: rules on new mail, or the user's own Sort).
    static JOB: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// A new round of background work starts (the download's writer, each round).
pub fn start_run() {
    RUN.store(now(), Ordering::Relaxed);
}

/// Runs `f` as part of `job` (what it logs goes under that job).
pub fn as_job<R>(job: &'static str, f: impl FnOnce() -> R) -> R {
    let before = JOB.with(|j| j.replace(Some(job)));
    let out = f();
    JOB.with(|j| j.set(before));
    out
}

/// The job being done on this thread, if one said so.
pub fn current() -> Option<&'static str> {
    JOB.with(Cell::get)
}

/// One thing a job did to a message, as Activity ▸ Details shows it.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub run: i64,
    pub at: i64,
    pub message: Option<i64>,
    pub what: String,
    /// The message's sender, subject and date (empty when it's gone).
    pub from: String,
    pub subject: String,
    pub date: i64,
}

/// A page of a job's log: its newest runs (or those before `before`), and whether older ones are left.
#[derive(Clone, Debug, Serialize)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// The oldest run on this page (ask for the runs before it to page back), when there are more.
    pub more: Option<i64>,
}

/// Rows a page shows at most (a big run is cut, and says so with More).
const PAGE_ROWS: usize = 500;

impl Library {
    /// Notes what a job did to a message (`message` None: about no one message).
    pub fn log_job(&self, job: &str, message: Option<i64>, what: &str) -> Result<()> {
        let at = now();
        let run = match RUN.load(Ordering::Relaxed) {
            0 => at,
            r => r,
        };
        self.conn.prepare_cached("INSERT INTO job_log (job, run, at, message, what) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute(params![job, run, at, message, what])?;
        // (old rows go now and then)
        if at - PRUNED.load(Ordering::Relaxed) > 3600 {
            PRUNED.store(at, Ordering::Relaxed);
            self.conn.execute("DELETE FROM job_log WHERE at < ?1", [at - KEEP_DAYS * 86400])?;
        }
        Ok(())
    }

    /// Notes it under the job this thread is doing (nothing when none said so).
    pub fn log_here(&self, message: Option<i64>, what: &str) -> Result<()> {
        match current() {
            Some(job) => self.log_job(job, message, what),
            None => Ok(()),
        }
    }

    /// A job's latest run (or the runs before `before`, `runs` of them), each entry with its
    /// message's sender, subject and date.
    pub fn job_log(&self, job: &str, before: Option<i64>, runs: usize) -> Result<Page> {
        let before = before.unwrap_or(i64::MAX);
        let chosen: Vec<i64> = {
            let mut stmt = self.conn.prepare_cached("SELECT DISTINCT run FROM job_log WHERE job = ?1 AND run < ?2 ORDER BY run DESC LIMIT ?3")?;
            stmt.query_map(params![job, before, runs.max(1) as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let Some(&oldest) = chosen.last() else { return Ok(Page { entries: Vec::new(), more: None }) };
        let mut stmt = self.conn.prepare_cached(
            "SELECT j.run, j.at, j.message, j.what, COALESCE(m.from_display, ''), COALESCE(m.subject, ''), COALESCE(m.date, 0)
               FROM job_log j LEFT JOIN messages m ON m.id = j.message
              WHERE j.job = ?1 AND j.run >= ?2 AND j.run < ?3 ORDER BY j.run DESC, j.id LIMIT ?4",
        )?;
        let entries: Vec<Entry> = stmt
            .query_map(params![job, oldest, before, PAGE_ROWS as i64], |r| {
                Ok(Entry { run: r.get(0)?, at: r.get(1)?, message: r.get(2)?, what: r.get(3)?, from: r.get(4)?, subject: r.get(5)?, date: r.get(6)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let older: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM job_log WHERE job = ?1 AND run < ?2)", params![job, oldest], |r| r.get(0))?;
        Ok(Page { entries, more: older.then_some(oldest) })
    }

    /// A folder's path, for the log's words ("moved to Bulk/Marketing").
    pub(crate) fn folder_path_of(&self, folder: i64) -> String {
        self.conn.query_row("SELECT path FROM folders WHERE id = ?1", [folder], |r| r.get::<_, String>(0)).map(|p| crate::library::shown_path(&p).to_string()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    #[test]
    fn a_jobs_runs_page_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let l = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        for (run, what) in [(100, "a"), (100, "b"), (200, "c"), (300, "d")] {
            RUN.store(run, Ordering::Relaxed);
            l.log_job("views", None, what).unwrap();
        }
        l.log_job("other", None, "x").unwrap();
        let first = l.job_log("views", None, 1).unwrap();
        assert_eq!(first.entries.iter().map(|e| e.what.as_str()).collect::<Vec<_>>(), ["d"]);
        assert_eq!(first.more, Some(300));
        let next = l.job_log("views", first.more, 2).unwrap();
        assert_eq!(next.entries.iter().map(|e| e.what.as_str()).collect::<Vec<_>>(), ["c", "a", "b"]);
        assert_eq!(next.more, None);
        // as_job: what's logged goes under that job
        as_job("rules", || l.log_here(None, "moved").unwrap());
        assert_eq!(l.job_log("rules", None, 1).unwrap().entries.len(), 1);
        assert!(current().is_none());
        RUN.store(0, Ordering::Relaxed);
    }
}
