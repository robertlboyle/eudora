//! Checks a library: every message's record against its checksum, every pack's structure,
//! and that the database and the index agree.

use std::collections::HashSet;

use rayon::prelude::*;
use rusqlite::params;

use crate::error::{Error, Result};
use crate::library::Library;
use crate::pack::Location;
use crate::progress::{Progress, thousands};

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub messages: u64,
    pub ok: u64,
    /// (message id, what's wrong), at most 1,000 listed.
    pub damaged: Vec<(i64, String)>,
    pub damaged_count: u64,
    pub packs: u64,
    pub records: u64,
    /// Records no message points to (left by a crash before the database commit; harmless).
    pub unreferenced: u64,
    /// Bytes after the last complete record of a pack.
    pub torn_bytes: u64,
    pub indexed: u64,
    pub index_matches: bool,
}

impl VerifyReport {
    pub fn passed(&self) -> bool {
        self.damaged_count == 0 && self.index_matches
    }
}

const CHUNK: i64 = 4096;

pub fn verify(library: &Library, progress: &dyn Progress) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let total = library.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))? as u64;
    report.messages = total;

    // 1. Every message checked against its checksum, in id order (= the order they were
    //    appended, so the packs are read front to back), a chunk at a time in parallel.
    let mut referenced: HashSet<(u32, u64)> = HashSet::with_capacity(total as usize);
    let mut stmt = library
        .conn
        .prepare("SELECT id, pack, pack_offset, pack_len, content_hash FROM messages WHERE id > ?1 ORDER BY id LIMIT ?2")?;
    let mut last = 0i64;
    let mut done = 0u64;
    loop {
        let chunk: Vec<(i64, Location, Vec<u8>)> = stmt
            .query_map(params![last, CHUNK], |r| {
                Ok((
                    r.get(0)?,
                    Location { pack: r.get::<_, i64>(1)? as u32, offset: r.get::<_, i64>(2)? as u64, len: r.get::<_, i64>(3)? as u32 },
                    r.get(4)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let Some((last_id, _, _)) = chunk.last() else { break };
        last = *last_id;
        let packs = &library.packs;
        let results: Vec<(i64, std::result::Result<(), String>)> = chunk
            .par_iter()
            .map(|(id, location, hash)| {
                let checked = match packs.read(*location) {
                    Ok(record) if record.hash[..] == hash[..] => Ok(()),
                    Ok(_) => Err("the stored message isn't the one the database expects".to_string()),
                    Err(Error::Corrupt { detail, .. }) => Err(detail),
                    Err(e) => Err(e.to_string()),
                };
                (*id, checked)
            })
            .collect();
        for (_, location, _) in &chunk {
            referenced.insert((location.pack, location.offset));
        }
        for (id, checked) in results {
            match checked {
                Ok(()) => report.ok += 1,
                Err(why) => {
                    report.damaged_count += 1;
                    if report.damaged.len() < 1000 {
                        report.damaged.push((id, why));
                    }
                }
            }
        }
        done += chunk.len() as u64;
        progress.report(done, total, &format!("Checking message {} of {}", thousands(done), thousands(total)));
        if progress.canceled() {
            return Err(Error::Canceled);
        }
    }

    // (other records the database points to: kept copies of messages, attachment files)
    for sql in ["SELECT pack, pack_offset FROM copies", "SELECT pack, pack_offset FROM attachment_files"] {
        let mut stmt = library.conn.prepare(sql)?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u64)))? {
            referenced.insert(row?);
        }
    }

    // 2. Pack structure: records nothing points to, torn tails.
    let ids = library.packs.pack_ids()?;
    report.packs = ids.len() as u64;
    for (i, pack) in ids.iter().enumerate() {
        progress.report(i as u64, ids.len() as u64, &format!("Checking pack {} of {}", i + 1, ids.len()));
        let scan = library.packs.scan(*pack)?;
        report.records += scan.records.len() as u64;
        report.unreferenced += scan.records.iter().filter(|(loc, _)| !referenced.contains(&(loc.pack, loc.offset))).count() as u64;
        report.torn_bytes += scan.size - scan.valid_end;
    }

    // 3. The index has one entry per message.
    report.indexed = library.index.num_docs();
    report.index_matches = report.indexed == total;
    Ok(report)
}
