//! Classic Eudora's attachments: Eudora kept them as files outside the mailbox (its Attach
//! folder, or wherever it was set to save them; pictures in its `embedded` folder) and left a
//! line in the message: `Attachment Converted: "C:\TEMP\Budget.xls"`, `Embedded Content:
//! image001.jpg: 00000001,…`, and in mail the user sent, `X-Attachments: D:\Docs\a.doc;`.
//!
//! Each such reference is recorded (`old_attachments`), and the files are looked for: at the
//! path written, in the classic folder's own Attach and embedded folders, in folders the user
//! points to (old backup drives), and in other copies of the same message (Microsoft 365's,
//! when the mail was imported there while the files still existed). A file found is stored
//! **in the library** (in the packs, once per content), so it shows as a normal attachment,
//! travels in backups and to any OS, and never needs the old Windows path again. One not
//! found stays listed as missing in its message: nothing is dropped silently. Looking again
//! later only fills what's still missing.

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};

use crate::error::{Error, Result};
use crate::library::Library;
use crate::pack::Location;
use crate::progress::Progress;

/// One reference to a file in a classic message.
#[derive(Clone, Debug, PartialEq)]
pub struct OldRef {
    /// "attached" (received), "embedded" (a picture in the message), "sent" (attached when sending).
    pub kind: &'static str,
    /// As written in the message (a Windows path, or an embedded picture's name).
    pub path: String,
    /// The file's name.
    pub name: String,
}

/// The file name in a path written by Windows or the Mac.
fn file_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).trim().to_string()
}

/// `name: 00000001,…` (an embedded picture, stored by name in the `embedded` folder).
fn embedded_name(rest: &str) -> Option<String> {
    let (name, ids) = rest.split_once(": ")?;
    let looks_like_ids = ids.len() >= 8 && ids[..8].chars().all(|c| c.is_ascii_hexdigit());
    (looks_like_ids && !name.contains(['\\', '/'])).then(|| name.trim().to_string())
}

/// The files a classic message refers to, in order.
pub fn references(raw: &[u8]) -> Vec<OldRef> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    let mut in_headers = true;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if in_headers && line.is_empty() {
            in_headers = false;
            continue;
        }
        if in_headers {
            if let Some(rest) = line.strip_prefix("X-Attachments:") {
                for path in rest.split(';').map(str::trim).filter(|p| !p.is_empty() && p.len() > 2) {
                    out.push(OldRef { kind: "sent", path: path.to_string(), name: file_name(path) });
                }
            }
            continue;
        }
        let rest = line.strip_prefix("Attachment Converted:").or_else(|| line.strip_prefix("Embedded Content:"));
        let Some(rest) = rest.map(str::trim) else { continue };
        if rest.is_empty() {
            continue;
        }
        if let Some(quoted) = rest.strip_prefix('"') {
            let path = quoted.split('"').next().unwrap_or("").to_string();
            if !path.is_empty() {
                out.push(OldRef { kind: "attached", name: file_name(&path), path });
            }
        } else if let Some(name) = embedded_name(rest) {
            out.push(OldRef { kind: "embedded", path: name.clone(), name });
        } else {
            out.push(OldRef { kind: "attached", name: file_name(rest), path: rest.to_string() });
        }
    }
    out.retain(|r| !r.name.is_empty());
    out
}

/// A file type by its name (what the viewer shows; the bytes are what they are).
pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "png" => "image/png",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "xls" => "application/vnd.ms-excel",
        "ppt" => "application/vnd.ms-powerpoint",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "zip" => "application/zip",
        "txt" | "log" => "text/plain",
        "htm" | "html" => "text/html",
        "rtf" => "application/rtf",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct FindReport {
    /// Messages read for references (the first time only).
    pub scanned: u64,
    /// References recorded so far, in all.
    pub references: u64,
    /// Found this time: on disk, in another copy of the message.
    pub found_on_disk: u64,
    pub found_in_copies: u64,
    /// Bytes stored (files already in the library aren't stored again).
    pub stored_bytes: u64,
    /// Still missing after this look.
    pub missing: u64,
    /// Of those, ones with several different files of that name (none taken: can't tell which).
    pub ambiguous: u64,
}

/// Where a classic account's own folder is ("C:\OldMail (personalities: …)" → "C:\OldMail").
fn classic_root(source: &str) -> Option<PathBuf> {
    let path = source.split(" (").next()?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Every file under these folders by lowercase name (an old backup drive: one walk).
fn index_folders(folders: &[PathBuf], progress: &dyn Progress) -> HashMap<String, Vec<PathBuf>> {
    let mut by_name: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut dirs: Vec<PathBuf> = folders.to_vec();
    let mut seen = 0u64;
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue }; // (unreadable: skipped)
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(path),
                Ok(t) if t.is_file() => {
                    by_name.entry(entry.file_name().to_string_lossy().to_lowercase()).or_default().push(path);
                    seen += 1;
                    if seen % 5000 == 0 {
                        progress.report(0, 0, &format!("Looking through the folders: {seen} files so far…"));
                    }
                }
                _ => {}
            }
        }
    }
    by_name
}

impl Library {
    /// Records the file references of every classic message not read yet (a message is read
    /// once: the setting "old-attachments-read" is the highest message id done).
    fn read_old_references(&self, progress: &dyn Progress) -> Result<u64> {
        let after: i64 = self.setting("old-attachments-read")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT m.id, m.pack, m.pack_offset, m.pack_len FROM messages m JOIN locations l ON l.message = m.id JOIN folders f ON f.id = l.folder
             JOIN accounts a ON a.id = f.account WHERE a.kind IN ('eudora', 'local', 'mbox') AND m.id > ?1 ORDER BY m.id",
        )?;
        let rows: Vec<(i64, Location)> = stmt.query_map(params![after], |r| Ok((r.get(0)?, Library::location(r, 1)?)))?.collect::<rusqlite::Result<_>>()?;
        let total = rows.len() as u64;
        let finder = [memchr::memmem::Finder::new(b"Attachment Converted:"), memchr::memmem::Finder::new(b"Embedded Content:"), memchr::memmem::Finder::new(b"X-Attachments:")];
        let mut insert = self.conn.prepare_cached("INSERT OR IGNORE INTO old_attachments (message, part, kind, path, name) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let mut done = 0u64;
        let result = (|| -> Result<()> {
            for (id, location) in &rows {
                done += 1;
                if done % 2000 == 0 {
                    progress.report(done, total, &format!("Reading old messages for attachments: {done} of {total}"));
                    if progress.canceled() {
                        return Err(Error::Canceled);
                    }
                }
                let raw = self.packs.read(*location)?.raw;
                if !finder.iter().any(|f| f.find(&raw).is_some()) {
                    continue;
                }
                for (n, r) in references(&raw).into_iter().enumerate() {
                    insert.execute(params![id, format!("eudora:{}", n + 1), r.kind, r.path, r.name])?;
                }
            }
            Ok(())
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        if let Some((last, _)) = rows.last() {
            self.set_setting("old-attachments-read", &last.to_string())?;
        }
        Ok(total)
    }

    /// Stores a file's bytes in the library (once per content) and returns its attachment id.
    fn store_attachment_file(&self, name: &str, bytes: &[u8], stored: &mut u64) -> Result<i64> {
        let hash = blake3::hash(bytes);
        if let Some(id) = self
            .conn
            .query_row("SELECT a.id FROM attachments a JOIN attachment_files f ON f.attachment = a.id WHERE a.hash = ?1", params![&hash.as_bytes()[..]], |r| r.get::<_, i64>(0))
            .optional()?
        {
            return Ok(id);
        }
        self.conn.execute("INSERT OR IGNORE INTO attachments (hash, size, mime) VALUES (?1, ?2, ?3)", params![&hash.as_bytes()[..], bytes.len() as i64, mime_of(name)])?;
        let id: i64 = self.conn.query_row("SELECT id FROM attachments WHERE hash = ?1", params![&hash.as_bytes()[..]], |r| r.get(0))?;
        let meta = serde_json::to_vec(&serde_json::json!({ "attachment": name }))?;
        let record = crate::pack::encode(bytes, *hash.as_bytes(), meta, &self.compression())?;
        let at = self.packs.append(&record)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO attachment_files (attachment, pack, pack_offset, pack_len) VALUES (?1, ?2, ?3, ?4)",
            params![id, at.pack as i64, at.offset as i64, at.len as i64],
        )?;
        *stored += bytes.len() as u64;
        Ok(id)
    }

    /// Reads classic messages for their file references (new ones only), then looks for every
    /// file still missing: at its path, in each classic folder's Attach and embedded folders, in
    /// `folders` (old backup drives…), and in the message's other copies. Needs the library
    /// open for writing.
    pub fn find_old_attachments(&self, folders: &[PathBuf], progress: &dyn Progress) -> Result<FindReport> {
        let mut report = FindReport { scanned: self.read_old_references(progress)?, ..Default::default() };
        // where to look: the classic folders' own, then the user's (the classic folders as they
        // are now: often restored from a backup to another place, so the paths written in
        // messages are read relative to them)
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut stmt = self.conn.prepare("SELECT source FROM accounts WHERE kind = 'eudora' AND source IS NOT NULL")?;
        for source in stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()? {
            if let Some(root) = classic_root(&source).filter(|r| r.is_dir()) {
                roots.push(root);
            }
        }
        let mut places = roots.clone();
        places.extend(folders.iter().cloned());
        let by_name = index_folders(&places, progress);
        // what's missing: (message, part, kind, path, name, the message's date)
        let mut stmt = self.conn.prepare(
            "SELECT o.message, o.part, o.kind, o.path, o.name, m.date FROM old_attachments o JOIN messages m ON m.id = o.message WHERE o.attachment IS NULL ORDER BY o.message",
        )?;
        let missing: Vec<(i64, String, String, String, String, i64)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
        let total = missing.len() as u64;
        let mut stored = 0u64;
        // A wrong attachment is worse than a missing one: the path as written names a folder on
        // a computer long gone (a folder of that name here holds other files), and names like
        // image001.jpg repeat. So a file is taken only when it's certain: the written path read
        // relative to a classic folder or a folder given, or a name exactly one file has (or
        // several with the same content); embedded pictures only from an `embedded` folder.
        let content_of = |paths: &[PathBuf]| -> Option<(Vec<u8>, String)> {
            let mut first: Option<(Vec<u8>, String)> = None;
            for p in paths {
                let bytes = std::fs::read(p).ok()?;
                match &first {
                    None => first = Some((bytes, p.display().to_string())),
                    Some((b, _)) if *b == bytes => {}
                    Some(_) => return None, // different files of that name: can't tell which
                }
            }
            first
        };
        let mut n = 0usize;
        while n < missing.len() {
            // a batch at a time, kept as it goes (stopping keeps what was found so far)
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            let end = (n + 200).min(missing.len());
            let result = (|| -> Result<()> {
                for (message, part, kind, path, name, _date) in &missing[n..end] {
                    let parts: Vec<&str> = path.split(['\\', '/']).filter(|s| !s.is_empty() && !s.ends_with(':')).collect();
                    let mut found: Option<(Vec<u8>, String)> = None;
                    if kind == "embedded" {
                        let named: Vec<PathBuf> = by_name
                            .get(&name.to_lowercase())
                            .map(|v| v.iter().filter(|p| p.parent().and_then(|d| d.file_name()).is_some_and(|d| d.eq_ignore_ascii_case("embedded"))).cloned().collect())
                            .unwrap_or_default();
                        found = content_of(&named);
                    } else {
                        // the written path under a classic folder or a folder given ("c:\internet\
                        // mail\attach\x.doc" → <folder>\internet\mail\attach\x.doc, then
                        // …\mail\attach\x.doc, then …\attach\x.doc): the deepest that's there
                        'bases: for base in roots.iter().chain(folders.iter()) {
                            for i in 0..parts.len().saturating_sub(1) {
                                let p = parts[i..].iter().fold(base.clone(), |p, s| p.join(s));
                                if p.is_file() {
                                    found = content_of(std::slice::from_ref(&p));
                                    break 'bases;
                                }
                            }
                        }
                        if found.is_none() {
                            let named = by_name.get(&name.to_lowercase()).cloned().unwrap_or_default();
                            found = content_of(&named);
                            if found.is_none() && named.len() > 1 {
                                report.ambiguous += 1;
                            }
                        }
                    }
                    if found.is_some() {
                        report.found_on_disk += 1;
                    } else if kind != "sent" {
                        // another copy of the message (Microsoft 365's): an attachment of that name
                        for raw in self.other_copies(*message)? {
                            let parsed = crate::parse::parse(&raw, Default::default(), 0);
                            if let Some(a) = parsed.attachments.iter().find(|a| a.name.eq_ignore_ascii_case(name)) {
                                if let Some((_, bytes)) = crate::compose::part_bytes(&raw, &a.part) {
                                    found = Some((bytes, "another copy of the message (from a server)".into()));
                                    report.found_in_copies += 1;
                                    break;
                                }
                            }
                        }
                    }
                    if let Some((bytes, from)) = found {
                        let id = self.store_attachment_file(name, &bytes, &mut stored)?;
                        self.conn.execute("UPDATE old_attachments SET attachment = ?3, found = ?4 WHERE message = ?1 AND part = ?2", params![message, part, id, from])?;
                    }
                }
                Ok(())
            })();
            self.packs.flush(true)?; // (the files are on disk before the database points to them)
            self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
            result?;
            n = end;
            progress.report(n as u64, total, &format!("Looking for old attachments: {} of {} ({} found so far)", n, total, report.found_on_disk + report.found_in_copies));
            if progress.canceled() {
                return Err(Error::Canceled);
            }
        }
        report.stored_bytes = stored;
        report.references = self.conn.query_row("SELECT COUNT(*) FROM old_attachments", [], |r| r.get::<_, i64>(0))? as u64;
        report.missing = self.conn.query_row("SELECT COUNT(*) FROM old_attachments WHERE attachment IS NULL", [], |r| r.get::<_, i64>(0))? as u64;
        Ok(report)
    }

    /// The message's other original copies' bytes (MIME only).
    fn other_copies(&self, message: i64) -> Result<Vec<Vec<u8>>> {
        let mut stmt = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len FROM copies WHERE message = ?1 AND format = 'mime'")?;
        let locations: Vec<Location> = stmt.query_map(params![message], |r| Library::location(r, 0))?.collect::<rusqlite::Result<_>>()?;
        let own: Option<Location> = self
            .conn
            .query_row("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1", params![message], |r| Library::location(r, 0))
            .optional()?;
        Ok(locations.into_iter().filter(|l| Some(*l) != own).filter_map(|l| self.packs.read(l).ok().map(|r| r.raw)).collect())
    }

    /// A message's classic file references: (part, name, kind, path written, size if found).
    pub fn old_attachments_of(&self, message: i64) -> Result<Vec<(String, String, String, String, Option<u64>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT o.part, o.name, o.kind, o.path, a.size FROM old_attachments o LEFT JOIN attachments a ON a.id = o.attachment WHERE o.message = ?1 ORDER BY o.part",
        )?;
        Ok(stmt
            .query_map(params![message], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<i64>>(4)?.map(|s| s as u64))))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// A found classic file's bytes (part "eudora:N"): (its type, the bytes).
    pub fn old_attachment_bytes(&self, message: i64, part: &str) -> Result<Option<(String, String, Vec<u8>)>> {
        let found: Option<(String, String, Location)> = self
            .conn
            .query_row(
                "SELECT o.name, a.mime, f.pack, f.pack_offset, f.pack_len FROM old_attachments o JOIN attachments a ON a.id = o.attachment
                 JOIN attachment_files f ON f.attachment = a.id WHERE o.message = ?1 AND o.part = ?2",
                params![message, part],
                |r| Ok((r.get(0)?, r.get(1)?, Library::location(r, 2)?)),
            )
            .optional()?;
        match found {
            Some((name, mime, location)) => Ok(Some((name, mime, self.packs.read(location)?.raw))),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_of_classic_reference_is_read() {
        let raw = b"From: a@b\r\nX-Attachments: D:\\Docs\\Plan.doc; C:\\x\\Sheet.xls;\r\nSubject: s\r\n\r\nHi\r\nAttachment Converted: \"C:\\TEMP\\Budget 2001.xls\"\r\nAttachment Converted: c:\\eudora\\attach\\OLD.TXT\r\nEmbedded Content: image001.jpg: 00000001,09197423,00000000,00000000\r\nAttachment Converted: 1a3aaf3.jpg: 00000001,0691440c,00000000,00000000\r\n";
        let refs = references(raw);
        let got: Vec<(&str, &str)> = refs.iter().map(|r| (r.kind, r.name.as_str())).collect();
        assert_eq!(
            got,
            vec![("sent", "Plan.doc"), ("sent", "Sheet.xls"), ("attached", "Budget 2001.xls"), ("attached", "OLD.TXT"), ("embedded", "image001.jpg"), ("embedded", "1a3aaf3.jpg")]
        );
        assert_eq!(refs[2].path, "C:\\TEMP\\Budget 2001.xls");
        // an empty X-Attachments, or a bare drive letter, is nothing
        assert!(references(b"X-Attachments: A:\r\nX-Attachments: \r\n\r\nbody").is_empty());
    }

    #[test]
    fn files_are_found_stored_once_and_the_rest_stay_missing() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        // an old Eudora folder with one file kept, and a "backup drive" with another
        let old = dir.path().join("mail");
        std::fs::create_dir_all(old.join("Attach")).unwrap();
        std::fs::write(old.join("Attach").join("Kept.pdf"), b"%PDF kept").unwrap();
        let drive = dir.path().join("backup drive").join("TEMP");
        std::fs::create_dir_all(&drive).unwrap();
        std::fs::write(drive.join("Budget.xls"), b"budget bytes").unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |n: u32, body: &str| {
            let p = src.join(format!("{n}.eml"));
            std::fs::write(&p, format!("From: a@b.com\r\nSubject: m{n}\r\nMessage-ID: <{n}@b>\r\n\r\n{body}\r\n")).unwrap();
            p
        };
        let files = vec![
            mail(1, "Attachment Converted: \"C:\\TEMP\\Budget.xls\""),
            mail(2, "Attachment Converted: \"c:\\eudora\\attach\\Kept.pdf\"\r\nAttachment Converted: \"C:\\TEMP\\Lost.doc\""),
            mail(3, "Attachment Converted: \"C:\\TEMP\\Budget.xls\""),
        ];
        crate::import(&mut library, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        library.conn.execute("UPDATE accounts SET kind = 'eudora', source = ?1", [format!("{} (personalities: x@y)", old.display())]).unwrap();

        // first look: only the classic folder's own Attach
        let r = library.find_old_attachments(&[], &Silent).unwrap();
        assert_eq!((r.scanned, r.references, r.found_on_disk, r.missing), (3, 4, 1, 3), "{r:?}");
        // the backup drive: Budget.xls found for both messages, stored once
        let r = library.find_old_attachments(&[dir.path().join("backup drive")], &Silent).unwrap();
        assert_eq!((r.scanned, r.found_on_disk, r.missing), (0, 2, 1), "{r:?}");
        assert_eq!(r.stored_bytes, b"budget bytes".len() as u64, "the same file twice is kept once");
        let m1: i64 = library.conn.query_row("SELECT id FROM messages WHERE subject = 'm1'", [], |r| r.get(0)).unwrap();
        let (name, mime, bytes) = library.old_attachment_bytes(m1, "eudora:1").unwrap().unwrap();
        assert_eq!((name.as_str(), mime.as_str(), bytes.as_slice()), ("Budget.xls", "application/vnd.ms-excel", &b"budget bytes"[..]));
        let m2: i64 = library.conn.query_row("SELECT id FROM messages WHERE subject = 'm2'", [], |r| r.get(0)).unwrap();
        let listed = library.old_attachments_of(m2).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].4, None, "Lost.doc stays listed as missing");
        // two different files of one name: neither is taken; the same file twice: taken
        let other = dir.path().join("other drive");
        for (sub, bytes) in [("a", &b"one"[..]), ("b", &b"two"[..])] {
            std::fs::create_dir_all(other.join(sub)).unwrap();
            std::fs::write(other.join(sub).join("Lost.doc"), bytes).unwrap();
        }
        let r = library.find_old_attachments(&[other.clone()], &Silent).unwrap();
        assert_eq!((r.found_on_disk, r.ambiguous, r.missing), (0, 1, 1), "{r:?}");
        std::fs::write(other.join("b").join("Lost.doc"), b"one").unwrap();
        let r = library.find_old_attachments(&[other], &Silent).unwrap();
        assert_eq!((r.found_on_disk, r.missing), (1, 0), "{r:?}");
        // the library still checks out (the files are records the database points to)
        let v = crate::verify::verify(&library, &Silent).unwrap();
        assert_eq!(v.unreferenced, 0, "{v:?}");
    }
}
