//! How fast things are.
//!
//! Each timed thing (a search, a page of a list, storing new mail…) adds its time to a small
//! table in memory: a lock and a few additions, nothing written then. The table is saved at most
//! once a minute (`status/speed-<who>.json`, so it adds up across restarts) and read by
//! Help ▸ About.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// The last this many times of each kind are kept (for the median and the slowest 5%).
const RECENT: usize = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Measure {
    pub count: u64,
    pub total_ms: f64,
    pub max_ms: f64,
    pub last_ms: f64,
    pub last_at: i64,
    /// The latest times, oldest first (milliseconds).
    pub recent: Vec<f32>,
}

/// One kind as shown: how many, the usual time and the slow ones.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub kind: String,
    pub count: u64,
    pub average_ms: f64,
    pub median_ms: f64,
    /// 95 of 100 were at least this quick (of the latest).
    pub p95_ms: f64,
    pub max_ms: f64,
    pub last_ms: f64,
    pub last_at: i64,
}

struct Table {
    kinds: BTreeMap<String, Measure>,
    file: Option<PathBuf>,
    saved: Option<Instant>,
    changed: bool,
}

static TABLE: Mutex<Table> = Mutex::new(Table { kinds: BTreeMap::new(), file: None, saved: None, changed: false });

/// Where this program keeps its table (`<library>/status/speed-<who>.json`); what's there is
/// added to.
pub fn set_file(library: &Path, who: &str) {
    let file = library.join("status").join(format!("speed-{who}.json"));
    let kept: BTreeMap<String, Measure> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if let Ok(mut t) = TABLE.lock() {
        for (k, m) in kept {
            t.kinds.entry(k).or_insert(m);
        }
        t.file = Some(file);
    }
}

/// Adds one time.
pub fn record(kind: &str, ms: f64) {
    let Ok(mut t) = TABLE.lock() else { return };
    let m = t.kinds.entry(kind.to_string()).or_default();
    m.count += 1;
    m.total_ms += ms;
    m.max_ms = m.max_ms.max(ms);
    m.last_ms = ms;
    m.last_at = crate::library::now();
    if m.recent.len() >= RECENT {
        m.recent.remove(0);
    }
    m.recent.push(ms as f32);
    t.changed = true;
    // saved at most once a minute (a small file; the table stays in memory meanwhile)
    if t.saved.is_none_or(|s| s.elapsed().as_secs() >= 60) {
        t.saved = Some(Instant::now());
        save(&t);
    }
}

fn save(t: &Table) {
    let Some(file) = &t.file else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let fresh = file.with_extension("json.new");
    if serde_json::to_vec(&t.kinds).ok().and_then(|b| std::fs::write(&fresh, b).ok()).is_some() {
        let _ = std::fs::rename(&fresh, file);
    }
}

/// Saves what changed now (a program closing).
pub fn flush() {
    if let Ok(mut t) = TABLE.lock() {
        if t.changed {
            save(&t);
            t.changed = false;
        }
    }
}

/// Times what's in its scope: `let _t = speed::Timer::new("Search");`
pub struct Timer {
    kind: &'static str,
    started: Instant,
}

impl Timer {
    pub fn new(kind: &'static str) -> Timer {
        Timer { kind, started: Instant::now() }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        record(self.kind, self.started.elapsed().as_secs_f64() * 1000.0);
    }
}

fn summarize(kinds: &BTreeMap<String, Measure>) -> Vec<Summary> {
    kinds
        .iter()
        .map(|(kind, m)| {
            let mut sorted: Vec<f32> = m.recent.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let at = |q: f64| sorted.get(((sorted.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0.0) as f64;
            Summary {
                kind: kind.clone(),
                count: m.count,
                average_ms: if m.count > 0 { m.total_ms / m.count as f64 } else { 0.0 },
                median_ms: at(0.5),
                p95_ms: at(0.95),
                max_ms: m.max_ms,
                last_ms: m.last_ms,
                last_at: m.last_at,
            }
        })
        .collect()
}

/// This program's table, summarized.
pub fn summary() -> Vec<Summary> {
    TABLE.lock().map(|t| summarize(&t.kinds)).unwrap_or_default()
}

/// Another program's table as it last saved it (`who`: "download", "window").
pub fn read_summary(library: &Path, who: &str) -> Vec<Summary> {
    let file = library.join("status").join(format!("speed-{who}.json"));
    std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<BTreeMap<String, Measure>>(&b).ok()).map(|k| summarize(&k)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_add_up_and_are_summarized() {
        for ms in 1..=100 {
            record("t-search", ms as f64);
        }
        {
            let _t = Timer::new("t-timer");
        }
        let s = summary();
        let search = s.iter().find(|x| x.kind == "t-search").unwrap();
        assert_eq!((search.count, search.max_ms, search.last_ms), (100, 100.0, 100.0));
        assert!((search.average_ms - 50.5).abs() < 1e-9 && (50.0..=51.0).contains(&search.median_ms) && search.p95_ms >= 94.0);
        assert!(s.iter().any(|x| x.kind == "t-timer" && x.count == 1));
    }
}
