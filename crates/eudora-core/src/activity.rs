//! What the download is doing in the background, for the window.
//!
//! Each background job (rules on new mail, changes on the servers, sorting into Views, header
//! search, travel mail, …) says what it did each round (`did`); jobs with a backlog say how much
//! is left (`left`). The download writes it all to `status/activity.json` every 2 s (`write`):
//! per job, what it last did, when, how much this session and how much is left.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// One background job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub key: String,
    /// "Sorting new mail with your rules"
    pub name: String,
    /// What it did last time it did something ("3 moved to Bulk / Marketing, 1 to Spam").
    pub said: String,
    /// How many it handled the last time, and since the download started.
    pub last: u64,
    pub total: u64,
    /// When it last did something / last looked (Unix seconds).
    pub active: i64,
    pub checked: i64,
    /// How many are still waiting, when known.
    pub left: Option<u64>,
    /// Working right now (a long job: maintenance), on `now`, `done` of `of`, since `started`.
    pub working: bool,
    pub now: String,
    pub done: u64,
    pub of: u64,
    pub started: i64,
    /// How long its last run took (seconds), for long jobs.
    pub took: Option<i64>,
    /// The user paused it in Activity (filled in when read, `paused_jobs`).
    pub paused: bool,
    /// When it runs next (Unix seconds), shown as "in 10 minutes".
    pub next: Option<i64>,
}

/// Every job's state, as the window reads it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    pub updated: i64,
    pub jobs: Vec<Job>,
}

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());

fn with_job(key: &str, name: &str, f: impl FnOnce(&mut Job)) {
    let Ok(mut jobs) = JOBS.lock() else { return };
    let i = match jobs.iter().position(|j| j.key == key) {
        Some(i) => i,
        None => {
            jobs.push(Job { key: key.into(), name: name.into(), ..Default::default() });
            jobs.len() - 1
        }
    };
    f(&mut jobs[i]);
}

/// How often the download's rounds come (the jobs that run each round look again then).
pub const ROUND_SECS: i64 = 10;

/// A job's round: `count` handled (0 = nothing to do), `said` what it did, in words.
pub fn did(key: &str, name: &str, count: u64, said: &str) {
    let now = crate::library::now();
    with_job(key, name, |j| {
        j.checked = now;
        // (a job on a schedule of its own keeps its time; the others look again next round)
        if j.next.is_none_or(|t| t <= now) {
            j.next = Some(now + ROUND_SECS);
        }
        if count > 0 {
            j.last = count;
            j.total += count;
            j.active = now;
            j.said = said.to_string();
        }
    });
}

/// When a job runs next, for one on a schedule of its own (maintenance, statistics, archive).
pub fn next(key: &str, name: &str, at: Option<i64>) {
    with_job(key, name, |j| j.next = at);
}

/// How many are still waiting for a job (None: not known).
pub fn left(key: &str, name: &str, left: Option<u64>) {
    with_job(key, name, |j| j.left = left);
}

/// The library the download writes the jobs to (for long jobs reporting from inside).
static ROOT: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
static LAST_WRITE: Mutex<Option<std::time::Instant>> = Mutex::new(None);

pub fn set_root(library: &Path) {
    if let Ok(mut r) = ROOT.lock() {
        *r = Some(library.to_path_buf());
    }
}

/// Written now, or (`soon`) at most once a second.
fn write_now(soon: bool) {
    if soon {
        if let Ok(mut last) = LAST_WRITE.lock() {
            if last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1)) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
    }
    let root = ROOT.lock().ok().and_then(|r| r.clone());
    if let Some(root) = root {
        write(&root);
    }
}

/// A long job starts (shown at once: the status bar's spinner, Activity's line).
pub fn begin(key: &str, name: &str, now: &str) {
    let at = crate::library::now();
    with_job(key, name, |j| {
        j.working = true;
        j.now = now.to_string();
        j.done = 0;
        j.of = 0;
        j.started = at;
    });
    write_now(false);
}

/// A long job's step: what it's doing, `done` of `of` (0 = not counted).
pub fn step(key: &str, name: &str, now: &str, done: u64, of: u64) {
    with_job(key, name, |j| {
        j.working = true;
        j.now = now.to_string();
        j.done = done;
        j.of = of;
    });
    write_now(true);
}

/// A job waiting for its next turn (the download between checks): what it last said, and when it
/// looks again.
pub fn idle(key: &str, name: &str, said: &str, next: Option<i64>) {
    let at = crate::library::now();
    with_job(key, name, |j| {
        if j.working {
            j.active = at;
        }
        j.working = false;
        j.now.clear();
        j.checked = at;
        j.said = said.to_string();
        j.next = next;
    });
    write_now(true);
}

/// A long job ends: what it did, in words, and how many.
pub fn end(key: &str, name: &str, count: u64, said: &str) {
    let at = crate::library::now();
    with_job(key, name, |j| {
        j.working = false;
        j.now.clear();
        j.took = Some(at - j.started);
        j.checked = at;
        j.active = at;
        j.last = count;
        j.total += count;
        j.said = said.to_string();
    });
    write_now(false);
}

/// Writes the jobs for the window (atomically: written aside, then renamed).
pub fn write(library: &Path) {
    let jobs = JOBS.lock().map(|j| j.clone()).unwrap_or_default();
    let activity = Activity { updated: crate::library::now(), jobs };
    let dir = library.join("status");
    let _ = std::fs::create_dir_all(&dir);
    let fresh = dir.join("activity.json.new");
    if serde_json::to_vec(&activity).ok().and_then(|b| std::fs::write(&fresh, b).ok()).is_some() {
        let _ = std::fs::rename(&fresh, dir.join("activity.json"));
    }
}

/// The jobs as the download last wrote them (None when it hasn't), each marked when the user
/// paused it.
pub fn read(library: &Path) -> Option<Activity> {
    let mut a: Activity = std::fs::read(library.join("status").join("activity.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
    let paused = paused_jobs(library);
    for j in &mut a.jobs {
        j.paused = paused.contains(&j.key);
    }
    Some(a)
}

/// The jobs the user paused one by one in Activity: their keys, in `status/paused-jobs.json`,
/// read by the download before each job's round.
pub fn paused_jobs(library: &Path) -> std::collections::HashSet<String> {
    std::fs::read(library.join("status").join("paused-jobs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Pauses or resumes one job.
pub fn set_job_paused(library: &Path, key: &str, paused: bool) -> crate::Result<()> {
    let mut jobs = paused_jobs(library);
    if paused {
        jobs.insert(key.to_string());
    } else {
        jobs.remove(key);
    }
    use crate::error::IoContext;
    let dir = library.join("status");
    std::fs::create_dir_all(&dir).context(|| format!("making {}", dir.display()))?;
    let file = dir.join("paused-jobs.json");
    let tmp = dir.join("paused-jobs.json.new");
    std::fs::write(&tmp, serde_json::to_vec(&jobs)?).context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &file).context(|| format!("writing {}", file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_say_what_they_did_and_add_up() {
        did("t-views", "Sorting into Views", 0, "");
        did("t-views", "Sorting into Views", 2000, "2,000 messages read");
        did("t-views", "Sorting into Views", 500, "500 messages read");
        did("t-views", "Sorting into Views", 0, "");
        left("t-views", "Sorting into Views", Some(12));
        let jobs = JOBS.lock().unwrap().clone();
        let j = jobs.iter().find(|j| j.key == "t-views").unwrap();
        assert_eq!((j.last, j.total, j.said.as_str(), j.left), (500, 2500, "500 messages read", Some(12)));
        let dir = tempfile::tempdir().unwrap();
        write(dir.path());
        assert!(read(dir.path()).unwrap().jobs.iter().any(|j| j.key == "t-views"));
    }
}
