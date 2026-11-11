//! Special ▸ Optimize Library (classic Eudora's "Compact Mailboxes", for today's library): gives
//! back the space of deleted mail and tidies what grows with use.
//!
//! 1. Attachment files no message refers to any more are let go.
//! 2. **Packs mostly holding deleted mail are rewritten**: each record still used is copied
//!    (checksum first) to the pack being written, the copies are on the disk before the
//!    database is changed to point at them (one transaction), and only then is the old pack
//!    deleted. A crash in between leaves the old pack unused: the next run deletes it.
//! 3. The search index's segments are merged into one.
//! 4. The database: SQLite's own optimizer, the write-ahead log emptied; with `vacuum`, the
//!    whole file rewritten (needs as much free space as the database, and takes minutes on a
//!    big library).
//!
//! Needs the library open for writing (the window lets go of its download meanwhile).

use std::collections::{BTreeMap, HashMap};

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::library::{Access, Library};
use crate::pack::Location;
use crate::progress::Progress;

/// A pack is rewritten when at least this share of it is deleted mail.
pub const REWRITE_AT: f64 = 0.25;

#[derive(Clone, Debug, Default, Serialize)]
pub struct OptimizeReport {
    pub attachment_files_dropped: u64,
    pub packs_rewritten: u64,
    pub packs_removed: u64,
    pub records_moved: u64,
    pub bytes_freed: u64,
    pub index_segments: usize,
    pub database_before: u64,
    pub database_after: u64,
}

/// The tables that point into the packs.
const POINTERS: [&str; 3] = ["messages", "copies", "attachment_files"];

/// Setting: Settings ▸ Library ▸ "Keep original messages exactly, for archive and legal purposes"
/// (on unless "off"). Off = Online Archive items don't keep Exchange's export beside the message
/// made from it, and maintenance removes the ones kept. Not synced: each computer chooses.
pub const KEEP_EXPORTS_SETTING: &str = "keep-exchange-exports";

/// The kept exports that aren't also their message's own bytes (by the message's id: quick).
const EXPORTS: &str = "FROM copies c WHERE c.format = 'exchange-export'
    AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = c.message AND m.pack = c.pack AND m.pack_offset = c.pack_offset)";

impl Library {
    /// Whether archive items keep Exchange's export (on unless switched off).
    pub fn keeps_exchange_exports(&self) -> bool {
        self.setting(KEEP_EXPORTS_SETTING).ok().flatten().as_deref() != Some("off")
    }

    /// Switches keeping originals off and removes the exports kept (`remove_exchange_exports`).
    pub fn drop_exchange_exports(&self) -> Result<(u64, u64)> {
        self.set_setting(KEEP_EXPORTS_SETTING, "off")?;
        self.remove_exchange_exports()
    }

    /// Removes every kept Exchange export of Online Archive items. Only the `copies` rows go: a
    /// message's own bytes are never touched (an export that is also a message's bytes is kept),
    /// and "couldn't convert" notes hold their export inside the note itself. The space comes
    /// back when Optimize rewrites the packs. Returns (copies removed, their bytes as stored).
    pub fn remove_exchange_exports(&self) -> Result<(u64, u64)> {
        if self.access != Access::Write {
            return Err(Error::Other("open the library for writing to remove the exports".into()));
        }
        let mut out = (0, 0);
        self.in_transaction(|| {
            out = self.exchange_exports()?;
            self.conn.execute(&format!("DELETE FROM copies WHERE id IN (SELECT c.id {EXPORTS})"), [])?;
            Ok(())
        })?;
        Ok(out)
    }

    /// The Exchange exports `drop_exchange_exports` would remove: (how many, bytes as stored).
    pub fn exchange_exports(&self) -> Result<(u64, u64)> {
        Ok(self.conn.query_row(&format!("SELECT COUNT(*), COALESCE(SUM(c.pack_len), 0) {EXPORTS}"), [], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))?)
    }

    pub fn optimize(&self, vacuum: bool, progress: &dyn Progress) -> Result<OptimizeReport> {
        if self.access != Access::Write {
            return Err(Error::Other("open the library for writing to optimize it".into()));
        }
        let mut report = OptimizeReport { database_before: self.database_size(), ..Default::default() };

        // 1. attachment files nothing refers to
        progress.report(0, 4, "Optimizing: attachment files no message uses (step 1 of 4)");
        report.attachment_files_dropped = self.conn.execute(
            "DELETE FROM attachment_files WHERE attachment NOT IN (SELECT attachment FROM attachment_refs)
               AND attachment NOT IN (SELECT attachment FROM old_attachments WHERE attachment IS NOT NULL)",
            [],
        )? as u64;

        // 2. packs: what each holds that's still used
        progress.report(1, 4, "Optimizing: measuring the packs (step 2 of 4)");
        self.packs.rotate()?;
        let writing = self.packs.writing_pack();
        let mut used: BTreeMap<u32, Vec<Location>> = BTreeMap::new();
        for table in POINTERS {
            let mut stmt = self.conn.prepare(&format!("SELECT DISTINCT pack, pack_offset, pack_len FROM \"{table}\""))?;
            for row in stmt.query_map([], |r| Ok(Location { pack: r.get::<_, i64>(0)? as u32, offset: r.get::<_, i64>(1)? as u64, len: r.get::<_, i64>(2)? as u32 }))? {
                let loc = row?;
                used.entry(loc.pack).or_default().push(loc);
            }
        }
        let mut chosen = Vec::new();
        for pack in self.packs.pack_ids()? {
            if Some(pack) == writing {
                continue;
            }
            let size = std::fs::metadata(self.packs.pack_path(pack)).map(|m| m.len()).unwrap_or(0);
            let locations = used.remove(&pack).unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            let in_use: u64 = locations.iter().filter(|l| seen.insert(l.offset)).map(|l| l.len as u64).sum();
            let free = size.saturating_sub(in_use + crate::pack::PACK_HEADER.len() as u64);
            if locations.is_empty() || (size > 0 && free as f64 / size as f64 >= REWRITE_AT) {
                chosen.push((pack, size, locations));
            }
        }
        let total_records: usize = chosen.iter().map(|(_, _, l)| l.len()).sum();
        // Pointing the database at the copies finds each row by (pack, offset): without an index
        // that's a scan of millions of rows per record (four records a second on a big library).
        // Made for this step only, so the download's writes never pay for them.
        if !chosen.is_empty() {
            progress.report(1, 4, "Optimizing: preparing to rewrite the packs (step 2 of 4)");
            for table in POINTERS {
                self.conn.execute_batch(&format!("CREATE INDEX IF NOT EXISTS \"optimize_{table}\" ON \"{table}\" (pack, pack_offset)"))?;
            }
        }
        let rewritten = self.rewrite_packs(&chosen, total_records, progress, &mut report);
        for table in POINTERS {
            self.conn.execute_batch(&format!("DROP INDEX IF EXISTS \"optimize_{table}\""))?;
        }
        rewritten?;

        // 3. the search index
        progress.report(2, 4, "Optimizing: merging the search index (step 3 of 4)");
        self.catch_up_index_now(progress)?;
        report.index_segments = self.index.merge_all()?;

        // 4. the database
        progress.report(3, 4, if vacuum { "Optimizing: rewriting the database (step 4 of 4; minutes on a big library)" } else { "Optimizing: the database (step 4 of 4)" });
        self.conn.execute_batch("PRAGMA optimize;")?;
        if vacuum {
            self.conn.execute_batch("VACUUM;")?;
        }
        self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        report.database_after = self.database_size();
        progress.report(4, 4, "Optimized.");
        Ok(report)
    }

    /// Step 2's work: copies what each chosen pack still holds into the pack being written,
    /// points the database at the copies, and removes the old pack.
    fn rewrite_packs(&self, chosen: &[(u32, u64, Vec<Location>)], total_records: usize, progress: &dyn Progress, report: &mut OptimizeReport) -> Result<()> {
        let mut moved_so_far = 0usize;
        for (n, (pack, size, locations)) in chosen.iter().enumerate() {
            let mut moved: HashMap<u64, Location> = HashMap::new();
            for loc in locations {
                if progress.canceled() {
                    return Err(Error::Canceled);
                }
                if moved.contains_key(&loc.offset) {
                    continue;
                }
                moved.insert(loc.offset, self.packs.copy_record(*loc)?);
                moved_so_far += 1;
                if moved_so_far % 200 == 0 {
                    progress.report(
                        moved_so_far as u64,
                        total_records as u64,
                        &format!("Optimizing: rewriting pack {} of {}, record {} of {} (step 2 of 4)", n + 1, chosen.len(), moved_so_far, total_records),
                    );
                }
            }
            // the copies on the disk first, then the database points at them, then the old pack goes
            self.packs.flush(true)?;
            self.in_transaction(|| {
                for (old, new) in &moved {
                    for table in POINTERS {
                        self.conn.execute(
                            &format!("UPDATE \"{table}\" SET pack = ?3, pack_offset = ?4, pack_len = ?5 WHERE pack = ?1 AND pack_offset = ?2"),
                            params![*pack as i64, *old as i64, new.pack as i64, new.offset as i64, new.len as i64],
                        )?;
                    }
                }
                Ok(())
            })?;
            let still: i64 = POINTERS.iter().map(|t| self.conn.query_row(&format!("SELECT COUNT(*) FROM \"{t}\" WHERE pack = ?1"), params![*pack as i64], |r| r.get::<_, i64>(0))).sum::<rusqlite::Result<i64>>()?;
            if still > 0 {
                return Err(Error::Other(format!("pack {pack} is still used by {still} rows after rewriting it: left as it is")));
            }
            self.packs.remove_pack(*pack)?;
            report.records_moved += moved.len() as u64;
            report.bytes_freed += size.saturating_sub(moved.values().map(|l| l.len as u64).sum());
            if moved.is_empty() { report.packs_removed += 1 } else { report.packs_rewritten += 1 }
        }
        Ok(())
    }

    fn database_size(&self) -> u64 {
        ["eudora.db", "eudora.db-wal"].iter().filter_map(|f| std::fs::metadata(self.root.join(f)).ok()).map(|m| m.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use crate::library::{Access, Library};
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    #[test]
    fn deleted_mail_gives_its_space_back_and_the_rest_reads_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let mut files = Vec::new();
        for n in 1..=4 {
            let file = dir.path().join(format!("{n}.eml"));
            let body: String = (0..2000).map(|i| format!("line {i} of message {n}\r\n")).collect();
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Note {n}\r\nMessage-ID: <{n}@a>\r\n\r\n{body}")).unwrap();
            files.push(file);
        }
        import(&mut library, &files, &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let ids: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        let before: Vec<Vec<u8>> = ids.iter().map(|id| library.raw_message(*id).unwrap()).collect();
        // two of four deleted (Empty Trash): half the pack is free
        library.in_transaction(|| { library.purge_message(ids[0])?; library.purge_message(ids[2]) }).unwrap();
        let old_packs = library.packs.pack_ids().unwrap();

        let r = library.optimize(false, &Silent).unwrap();
        assert_eq!((r.packs_rewritten, r.records_moved), (1, 2), "{r:?}");
        assert!(r.bytes_freed > 0);
        assert!(!library.packs.pack_ids().unwrap().contains(&old_packs[0]), "the old pack is gone");
        assert_eq!(library.raw_message(ids[1]).unwrap(), before[1], "the rest reads the same");
        assert_eq!(library.raw_message(ids[3]).unwrap(), before[3]);
        // copies point at the new place too
        let stale: i64 = library.conn.query_row("SELECT COUNT(*) FROM copies WHERE pack = ?1", [old_packs[0] as i64], |r| r.get(0)).unwrap();
        assert_eq!(stale, 0);
        // nothing more to do the second time
        let again = library.optimize(false, &Silent).unwrap();
        assert_eq!(again.packs_rewritten + again.packs_removed, 0, "{again:?}");
    }

    #[test]
    fn exchange_exports_go_and_the_messages_stay() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let mut files = Vec::new();
        for n in 1..=2 {
            let file = dir.path().join(format!("{n}.eml"));
            let body: String = (0..2000).map(|i| format!("line {i} of message {n}\r\n")).collect();
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Note {n}\r\nMessage-ID: <{n}@a>\r\n\r\n{body}")).unwrap();
            files.push(file);
        }
        import(&mut library, &files, &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let ids: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        let before: Vec<Vec<u8>> = ids.iter().map(|id| library.raw_message(*id).unwrap()).collect();
        // message 1 keeps an export of its own (elsewhere in the packs); message 2's "export"
        // is its own bytes, which must stay
        let own = |id: i64| -> crate::pack::Location {
            library.conn.query_row("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1", [id], |r| Ok(crate::pack::Location { pack: r.get::<_, i64>(0)? as u32, offset: r.get::<_, i64>(1)? as u64, len: r.get::<_, i64>(2)? as u32 })).unwrap()
        };
        library.packs.rotate().unwrap();
        let elsewhere = library.packs.copy_record(own(ids[0])).unwrap();
        library.packs.flush(true).unwrap();
        for (id, at, hash) in [(ids[0], elsewhere, b"a"), (ids[1], own(ids[1]), b"b")] {
            library.conn.execute(
                "INSERT INTO copies (message, format, content_hash, size, pack, pack_offset, pack_len, added) VALUES (?1, 'exchange-export', ?2, 1, ?3, ?4, ?5, 0)",
                rusqlite::params![id, hash.to_vec(), at.pack as i64, at.offset as i64, at.len as i64],
            ).unwrap();
        }
        assert!(library.keeps_exchange_exports());
        assert_eq!(library.exchange_exports().unwrap().0, 1);

        let (n, bytes) = library.drop_exchange_exports().unwrap();
        assert_eq!((n, bytes), (1, elsewhere.len as u64));
        assert!(!library.keeps_exchange_exports(), "new ones aren't kept");
        let left: Vec<i64> = library.conn.prepare("SELECT message FROM copies WHERE format = 'exchange-export'").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(left, vec![ids[1]], "a message's own bytes stay");
        library.optimize(false, &Silent).unwrap();
        assert_eq!(library.raw_message(ids[0]).unwrap(), before[0]);
        assert_eq!(library.raw_message(ids[1]).unwrap(), before[1]);
    }
}
