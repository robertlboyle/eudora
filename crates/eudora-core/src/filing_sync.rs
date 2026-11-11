//! Filing sync, the format: how mail is filed in Eudora where servers can't hold it (archive
//! mail filed anywhere, mailbox mail filed in archive-only folders, archives with no server),
//! kept per account and shared by every computer that has the account (the project's design notes § 7,
//! "Filing sync").
//!
//! - A message is named by a **key**: 32 bits of the BLAKE3 hash of its Message-ID (the made
//!   one when it has none), the same on every computer and unchanged when Exchange's retention
//!   moves an item into the archive. A message whose 32 bits another message of the account
//!   shares here is named by 64 bits instead (`Key::Long`).
//! - A **change** files a set of keys in one folder (or back where the server has them, or
//!   deleted for good). Keys are stored sorted, Elias–Fano coded: under 2 bytes a message.
//! - An account's **log** = a snapshot (where each key is filed, per folder) + the changes since,
//!   in clock order; changes older than the fold horizon are folded into the snapshot. The later
//!   change wins for a key.
//!
//! Applying a log to a library (keys not found here are kept, never an error) and the
//! transport are elsewhere; this module is pure data.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// The folder of a key deleted for good (Empty Trash): every computer drops it, and the
/// download doesn't bring it back from a server that keeps it (the archive).
pub const DELETED: &str = "\u{0}deleted";
/// Back where the server has it (a filing undone): nothing to keep.
pub const AS_ON_SERVER: &str = "";

/// A message's name in a filing log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    Short(u32),
    Long(u64),
}

/// The 64-bit hash of a Message-ID (angle brackets and case ignored).
pub fn message_hash(message_id: &str) -> u64 {
    let id = message_id.trim().trim_start_matches('<').trim_end_matches('>').trim().to_lowercase();
    let hash = blake3::hash(id.as_bytes());
    u64::from_be_bytes(hash.as_bytes()[..8].try_into().unwrap())
}

/// The short key (the hash's first 32 bits).
pub fn short_of(hash: u64) -> u32 {
    (hash >> 32) as u32
}

/// A set of keys, as stored: the short ones Elias–Fano coded, the long ones delta coded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeySet {
    pub short: Vec<u32>,
    pub long: Vec<u64>,
}

impl KeySet {
    pub fn from_keys(keys: impl IntoIterator<Item = Key>) -> KeySet {
        let mut set = KeySet::default();
        for k in keys {
            match k {
                Key::Short(s) => set.short.push(s),
                Key::Long(l) => set.long.push(l),
            }
        }
        set.short.sort_unstable();
        set.short.dedup();
        set.long.sort_unstable();
        set.long.dedup();
        set
    }

    pub fn keys(&self) -> impl Iterator<Item = Key> + '_ {
        self.short.iter().map(|s| Key::Short(*s)).chain(self.long.iter().map(|l| Key::Long(*l)))
    }

    pub fn len(&self) -> usize {
        self.short.len() + self.long.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn write(&self, out: &mut Vec<u8>) {
        elias_fano(&self.short, out);
        varint(self.long.len() as u64, out);
        let mut last = 0u64;
        for l in &self.long {
            varint(l - last, out);
            last = *l;
        }
    }

    fn read(r: &mut Reader) -> Result<KeySet> {
        let short = elias_fano_read(r)?;
        let n = r.varint()? as usize;
        let mut long = Vec::with_capacity(n.min(1 << 20));
        let mut last = 0u64;
        for _ in 0..n {
            last = last.checked_add(r.varint()?).ok_or_else(|| bad("long key overflow"))?;
            long.push(last);
        }
        Ok(KeySet { short, long })
    }
}

/// One filing change: these keys are now in `folder` (an account-relative path, `DELETED`, or
/// `AS_ON_SERVER`).
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// Order in the account's log (Lamport: one more than the highest seen when it was added).
    pub clock: u64,
    /// The computer that made it (its sync device id).
    pub device: String,
    /// When it was made (unix seconds): changes older than the fold horizon join the snapshot.
    pub at: i64,
    pub folder: String,
    pub keys: KeySet,
}

/// An account's filing log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Log {
    /// The account's address (lowercase).
    pub account: String,
    /// The highest clock folded into the snapshot.
    pub folded: u64,
    /// Where each folded key is filed: folder → keys (`AS_ON_SERVER` too: a computer that
    /// applied a filing and missed its undo puts the message back).
    pub snapshot: BTreeMap<String, KeySet>,
    /// Changes after the snapshot, in clock order.
    pub changes: Vec<Change>,
}

/// Format version of an encoded log.
const VERSION: u8 = 1;

impl Log {
    pub fn new(account: &str) -> Log {
        Log { account: account.to_lowercase(), ..Default::default() }
    }

    /// The highest clock in the log.
    pub fn clock(&self) -> u64 {
        self.changes.iter().map(|c| c.clock).max().unwrap_or(0).max(self.folded)
    }

    /// Adds a change made on this computer (its clock: one more than any seen).
    pub fn add(&mut self, device: &str, at: i64, folder: &str, keys: KeySet) {
        if keys.is_empty() {
            return;
        }
        let clock = self.clock() + 1;
        self.changes.push(Change { clock, device: device.to_string(), at, folder: folder.to_string(), keys });
    }

    /// Where each key ends up: the snapshot, then every change in order (the later one wins);
    /// `AS_ON_SERVER` for a filing undone.
    pub fn resolve(&self) -> BTreeMap<Key, String> {
        let mut at: BTreeMap<Key, String> = BTreeMap::new();
        for (folder, keys) in &self.snapshot {
            for k in keys.keys() {
                at.insert(k, folder.clone());
            }
        }
        let mut changes: Vec<&Change> = self.changes.iter().collect();
        changes.sort_by(|a, b| (a.clock, &a.device).cmp(&(b.clock, &b.device)));
        for c in changes {
            for k in c.keys.keys() {
                at.insert(k, c.folder.clone());
            }
        }
        at
    }

    /// Folds the changes made before `horizon` (unix seconds) into the snapshot: each key once,
    /// where it ended up.
    pub fn fold(&mut self, horizon: i64) {
        let (old, keep): (Vec<Change>, Vec<Change>) = std::mem::take(&mut self.changes).into_iter().partition(|c| c.at < horizon);
        if old.is_empty() {
            self.changes = keep;
            return;
        }
        let folded = Log { account: String::new(), folded: 0, snapshot: std::mem::take(&mut self.snapshot), changes: old.clone() };
        let mut by_folder: BTreeMap<String, Vec<Key>> = BTreeMap::new();
        for (k, folder) in folded.resolve() {
            by_folder.entry(folder).or_default().push(k);
        }
        self.snapshot = by_folder.into_iter().map(|(f, keys)| (f, KeySet::from_keys(keys))).collect();
        self.folded = self.folded.max(old.iter().map(|c| c.clock).max().unwrap_or(0));
        self.changes = keep;
    }

    /// Joins another computer's copy of this log: the newer snapshot, and every change after
    /// it from either (each once). Changes of ours the other copy's snapshot doesn't hold and
    /// that are older than it (made while this computer was away past the horizon) go on top
    /// with new clocks: the later change wins, and ours were made later than that snapshot.
    pub fn merge(&mut self, other: &Log) {
        if other.folded > self.folded {
            let ours: Vec<Change> = std::mem::take(&mut self.changes);
            self.snapshot = other.snapshot.clone();
            self.folded = other.folded;
            self.changes = other.changes.clone();
            let mut late: Vec<Change> = Vec::new();
            for c in ours {
                if self.changes.iter().any(|x| x.clock == c.clock && x.device == c.device) {
                    continue;
                }
                if c.clock > self.folded {
                    self.changes.push(c);
                } else {
                    late.push(c);
                }
            }
            late.sort_by(|a, b| (a.clock, &a.device).cmp(&(b.clock, &b.device)));
            for mut c in late {
                c.clock = self.clock() + 1;
                self.changes.push(c);
            }
        } else {
            for c in &other.changes {
                if c.clock > self.folded && !self.changes.iter().any(|x| x.clock == c.clock && x.device == c.device) {
                    self.changes.push(c.clone());
                }
            }
        }
        self.changes.sort_by(|a, b| (a.clock, &a.device).cmp(&(b.clock, &b.device)));
    }

    /// The log as bytes (zstd-compressed; what the sync envelope carries).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        text(&self.account, &mut out);
        varint(self.folded, &mut out);
        varint(self.snapshot.len() as u64, &mut out);
        for (folder, keys) in &self.snapshot {
            text(folder, &mut out);
            keys.write(&mut out);
        }
        varint(self.changes.len() as u64, &mut out);
        for c in &self.changes {
            varint(c.clock, &mut out);
            text(&c.device, &mut out);
            varint(c.at.max(0) as u64, &mut out);
            text(&c.folder, &mut out);
            c.keys.write(&mut out);
        }
        zstd::bulk::compress(&out, 19).unwrap_or(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Log> {
        let raw = zstd::stream::decode_all(bytes).map_err(|e| bad(&format!("not a filing log ({e})")))?;
        let mut r = Reader { bytes: &raw, at: 0 };
        if r.byte()? != VERSION {
            return Err(bad("a filing log from a newer Eudora"));
        }
        let account = r.text()?;
        let folded = r.varint()?;
        let n = r.varint()? as usize;
        let mut snapshot = BTreeMap::new();
        for _ in 0..n {
            let folder = r.text()?;
            snapshot.insert(folder, KeySet::read(&mut r)?);
        }
        let n = r.varint()? as usize;
        let mut changes = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            changes.push(Change { clock: r.varint()?, device: r.text()?, at: r.varint()? as i64, folder: r.text()?, keys: KeySet::read(&mut r)? });
        }
        Ok(Log { account, folded, snapshot, changes })
    }
}

// ------------------------------------------------------------------ coding

fn bad(why: &str) -> Error {
    Error::Other(format!("filing sync: {why}"))
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn text(s: &str, out: &mut Vec<u8>) {
    varint(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.bytes.get(self.at).ok_or_else(|| bad("cut short"))?;
        self.at += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(bad("number too long"))
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.bytes.len()).ok_or_else(|| bad("cut short"))?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn text(&mut self) -> Result<String> {
        let n = self.varint()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("text isn't UTF-8"))
    }
}

/// Bits, written least significant first.
struct Bits {
    bytes: Vec<u8>,
    len: u64,
}

impl Bits {
    fn new() -> Bits {
        Bits { bytes: Vec::new(), len: 0 }
    }

    fn push(&mut self, bit: bool) {
        if self.len % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            *self.bytes.last_mut().unwrap() |= 1 << (self.len % 8);
        }
        self.len += 1;
    }

    fn push_low(&mut self, value: u64, bits: u32) {
        for i in 0..bits {
            self.push(value >> i & 1 == 1);
        }
    }
}

fn bit(bytes: &[u8], i: u64) -> bool {
    bytes.get((i / 8) as usize).is_some_and(|b| b >> (i % 8) & 1 == 1)
}

/// Elias–Fano: sorted 32-bit values as `low` bits each (written as is) plus their high parts
/// in unary: about 2 + log2(2^32 / n) bits a value.
fn elias_fano(values: &[u32], out: &mut Vec<u8>) {
    let n = values.len() as u64;
    varint(n, out);
    if n == 0 {
        return;
    }
    let low = (((1u64 << 32) / n).max(1)).ilog2().min(31);
    out.push(low as u8);
    let mut lows = Bits::new();
    let mut highs = Bits::new();
    let mut last_high = 0u64;
    for v in values {
        let v = u64::from(*v);
        lows.push_low(v, low);
        let high = v >> low;
        for _ in last_high..high {
            highs.push(false);
        }
        highs.push(true);
        last_high = high;
    }
    varint(lows.bytes.len() as u64, out);
    out.extend_from_slice(&lows.bytes);
    varint(highs.len, out);
    varint(highs.bytes.len() as u64, out);
    out.extend_from_slice(&highs.bytes);
}

fn elias_fano_read(r: &mut Reader) -> Result<Vec<u32>> {
    let n = r.varint()?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let low = u32::from(r.byte()?);
    if low > 31 {
        return Err(bad("bad key list"));
    }
    let lows_len = r.varint()? as usize;
    let lows = r.take(lows_len)?.to_vec();
    let high_bits = r.varint()?;
    let highs_len = r.varint()? as usize;
    let highs = r.take(highs_len)?.to_vec();
    if (lows.len() as u64) * 8 < n * u64::from(low) || (highs.len() as u64) * 8 < high_bits {
        return Err(bad("bad key list"));
    }
    let mut values = Vec::with_capacity(n.min(1 << 24) as usize);
    let mut high = 0u64;
    let mut i = 0u64;
    for pos in 0..high_bits {
        if bit(&highs, pos) {
            let mut l = 0u64;
            for b in 0..u64::from(low) {
                if bit(&lows, i * u64::from(low) + b) {
                    l |= 1 << b;
                }
            }
            let v = high << low | l;
            values.push(u32::try_from(v).map_err(|_| bad("bad key"))?);
            i += 1;
            if i == n {
                break;
            }
        } else {
            high += 1;
        }
    }
    if i != n {
        return Err(bad("bad key list"));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_keys(n: usize, seed: u64) -> Vec<u32> {
        let mut x = seed;
        let mut v: Vec<u32> = (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u32
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    #[test]
    fn keys_ignore_brackets_and_case() {
        assert_eq!(message_hash("<ABC@Example.com>"), message_hash("abc@example.com"));
        assert_ne!(message_hash("a@x"), message_hash("b@x"));
    }

    #[test]
    fn key_lists_round_trip_and_stay_small() {
        for (n, limit) in [(0usize, 8usize), (1, 16), (100_000, 230_000), (500_000, 1_000_000)] {
            let set = KeySet { short: random_keys(n, 7 + n as u64), long: vec![3, 1 << 40, u64::MAX] };
            let mut out = Vec::new();
            set.write(&mut out);
            assert!(out.len() <= limit + 40, "{n} keys took {} bytes", out.len());
            let back = KeySet::read(&mut Reader { bytes: &out, at: 0 }).unwrap();
            assert_eq!(back, set);
        }
        // the extremes of the range
        let set = KeySet { short: vec![0, 1, u32::MAX - 1, u32::MAX], long: vec![] };
        let mut out = Vec::new();
        set.write(&mut out);
        assert_eq!(KeySet::read(&mut Reader { bytes: &out, at: 0 }).unwrap(), set);
    }

    #[test]
    fn the_later_change_wins_and_an_undo_is_kept() {
        let k = |n: u32| KeySet::from_keys([Key::Short(n)]);
        let mut log = Log::new("Me@x.com");
        log.add("pc1", 100, "Bulk", KeySet::from_keys([Key::Short(1), Key::Short(2), Key::Long(9)]));
        log.add("pc1", 200, "Projects", k(2));
        log.add("pc1", 300, AS_ON_SERVER, k(1));
        log.add("pc1", 400, DELETED, k(3));
        let at = log.resolve();
        assert_eq!(at.get(&Key::Short(1)).map(String::as_str), Some(AS_ON_SERVER), "moved back: undone");
        assert_eq!(at.get(&Key::Short(2)).map(String::as_str), Some("Projects"));
        assert_eq!(at.get(&Key::Long(9)).map(String::as_str), Some("Bulk"));
        assert_eq!(at.get(&Key::Short(3)).map(String::as_str), Some(DELETED));

        // folding changes before 350 gives the same result, with fewer changes kept
        let mut folded = log.clone();
        folded.fold(350);
        assert_eq!(folded.resolve(), at);
        assert_eq!((folded.changes.len(), folded.folded), (1, 3));
        // and survives being written and read
        assert_eq!(Log::decode(&folded.encode()).unwrap(), folded);
        assert_eq!(folded.account, "me@x.com");
    }

    #[test]
    fn two_computers_merge_and_a_long_absence_goes_on_top() {
        let k = |n: u32| KeySet::from_keys([Key::Short(n)]);
        let mut shared = Log::new("me@x.com");
        shared.add("pc1", 100, "A", k(1));
        let mut pc2 = shared.clone();
        // both file message 2 meanwhile: pc2's change comes in later, so it wins
        shared.add("pc1", 200, "B", k(2));
        pc2.add("pc2", 210, "C", k(2));
        let mut pc1 = shared.clone();
        pc1.merge(&pc2);
        pc2.merge(&pc1);
        assert_eq!(pc1.resolve(), pc2.resolve(), "both computers agree");
        assert_eq!(pc1.changes.len(), 3, "each change once");

        // pc3 was away past the horizon: the others folded; its own old change goes on top
        let mut pc3 = Log::new("me@x.com");
        pc3.add("pc3", 50, "Old", k(1));
        let mut now = pc1.clone();
        now.fold(1_000);
        pc3.merge(&now);
        assert_eq!(pc3.resolve().get(&Key::Short(1)).map(String::as_str), Some("Old"));
        assert_eq!(pc3.resolve().get(&Key::Short(2)), now.resolve().get(&Key::Short(2)));
    }

    #[test]
    fn damaged_logs_are_refused() {
        let mut log = Log::new("me@x.com");
        log.add("pc1", 1, "A", KeySet::from_keys((0..1000).map(|i| Key::Short(i * 7919))));
        let mut bytes = log.encode();
        assert!(Log::decode(&bytes[..bytes.len() / 2]).is_err());
        let raw = zstd::stream::decode_all(&bytes[..]).unwrap();
        bytes = zstd::bulk::compress(&raw[..raw.len() - 5], 3).unwrap();
        assert!(Log::decode(&bytes).is_err());
    }
}
