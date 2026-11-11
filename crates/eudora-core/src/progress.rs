//! Progress reporting with real numbers ("Importing message 120,000 of 1,412,000").

use std::sync::atomic::{AtomicBool, Ordering};

/// Receives progress from long jobs. `total` 0 means the step can't be counted;
/// `stage` then says what it is.
pub trait Progress: Sync {
    fn report(&self, done: u64, total: u64, stage: &str);

    /// Polled between batches; returning true stops the job at the next safe point.
    fn canceled(&self) -> bool {
        false
    }

    /// Something worth telling the user apart from the counts ("Connection lost at 10:58…",
    /// "Connection back after 3 min; continuing."): a line of its own, an alert in the UI.
    fn event(&self, _text: &str) {}
}

/// Discards all progress (tests, quiet runs).
pub struct Silent;

impl Progress for Silent {
    fn report(&self, _done: u64, _total: u64, _stage: &str) {}
}

impl<F: Fn(u64, u64, &str) + Sync> Progress for F {
    fn report(&self, done: u64, total: u64, stage: &str) {
        self(done, total, stage)
    }
}

/// A cancel flag that can be set from another thread (Ctrl+C in the CLI).
#[derive(Default)]
pub struct CancelFlag(AtomicBool);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 1234567 → "1,234,567".
/// "30 minutes", "an hour", "2 hours", "90 minutes".
pub fn minutes_text(m: u32) -> String {
    match m {
        60 => "an hour".into(),
        m if m % 60 == 0 => format!("{} hours", m / 60),
        1 => "minute".into(),
        m => format!("{m} minutes"),
    }
}

pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::thousands;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_412_000), "1,412,000");
    }
}

/// A job the window runs and shows (the spam filter on many messages): its count, and a Stop.
#[derive(Default)]
pub struct Job {
    pub done: std::sync::atomic::AtomicU64,
    pub total: std::sync::atomic::AtomicU64,
    stop: AtomicBool,
}

impl Job {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Progress for Job {
    fn report(&self, done: u64, total: u64, _stage: &str) {
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }
    fn canceled(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Runs `work` on half the processor's cores (at least one), so a long job beside the window
/// never takes them all and the window stays quick.
pub fn on_half_the_cores<R: Send>(work: impl FnOnce() -> R + Send) -> R {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    match rayon::ThreadPoolBuilder::new().num_threads((cores / 2).max(1)).build() {
        Ok(pool) => pool.install(work),
        Err(_) => work(),
    }
}
