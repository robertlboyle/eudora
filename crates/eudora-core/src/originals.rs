//! The most original copy of a duplicate is the one a message is shown and searched from. The
//! writer stores the copy that arrives first; this looks at messages that have gained another
//! copy since it last looked and swaps to a more original one (`WriterState::swap_to_copy`).
//! Every copy stays: only which one stands for the message changes.

use rusqlite::params;

use crate::error::Result;
use crate::import::WriterState;
use crate::library::Library;

/// Where it looked up to (the copies' ids).
const MARK: &str = "originals-mark";

/// How original a copy is: lower is more original.
fn rank(kind: Option<&str>, converted: bool) -> u8 {
    if converted {
        return 2; // made by Eudora from an Exchange export
    }
    match kind {
        Some("graph" | "graph-shared" | "gmail" | "imap" | "pop3") => 1, // a server's MIME
        _ => 0, // the user's own files (classic Eudora, mbox, EML)
    }
}

/// Whether bytes are Eudora's own conversion (it marks them in the header).
fn converted(raw: &[u8]) -> bool {
    let head = &raw[..raw.len().min(64 * 1024)];
    let end = head.windows(4).position(|w| w == b"\r\n\r\n").or_else(|| head.windows(2).position(|w| w == b"\n\n")).unwrap_or(head.len());
    format!("\n{}", String::from_utf8_lossy(&head[..end]).to_ascii_lowercase()).contains("\nx-eudora-converted:")
}

/// What one look did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Preferred {
    /// Messages with more than one copy looked at.
    pub looked: u64,
    /// Swapped to a more original copy.
    pub swapped: u64,
}

impl Library {
    /// Looks at up to `limit` copies added since the last look; each message among them with
    /// several MIME copies is shown from its most original one. `looked` 0 = nothing new.
    pub(crate) fn prefer_originals(&mut self, writer: &mut WriterState, limit: usize) -> Result<Preferred> {
        let mark: i64 = self.setting(MARK)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let rows: Vec<(i64, i64)> = {
            let mut stmt = self.conn.prepare("SELECT id, message FROM copies WHERE id > ?1 ORDER BY id LIMIT ?2")?;
            stmt.query_map(params![mark, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let Some(&(last, _)) = rows.last() else { return Ok(Preferred::default()) };
        let mut messages: Vec<i64> = rows.iter().map(|r| r.1).collect();
        messages.sort_unstable();
        messages.dedup();
        let mut done = Preferred::default();
        for message in messages {
            if let Some(copy) = self.more_original_copy(message)? {
                writer.swap_to_copy(self, message, copy)?;
                done.swapped += 1;
            }
            done.looked += 1;
        }
        self.set_setting(MARK, &last.to_string())?;
        Ok(done)
    }

    /// Every message with a more original copy is shown from it (the command line; the
    /// download does this a batch at a time). Returns what was done over everything.
    pub fn prefer_originals_all(&mut self, progress: &dyn crate::progress::Progress) -> Result<Preferred> {
        let mut writer = WriterState::new(0, Default::default());
        let total: i64 = self.conn.query_row("SELECT COUNT(*) FROM copies", [], |r| r.get(0))?;
        let mut all = Preferred::default();
        let mut seen = 0u64;
        loop {
            let done = self.prefer_originals(&mut writer, 5000)?;
            if done.looked == 0 {
                break;
            }
            seen += 5000;
            all.looked += done.looked;
            all.swapped += done.swapped;
            progress.report(seen.min(total as u64), total as u64, &format!("Looking for the most original copies: {} of {} copies ({} messages changed)", crate::progress::thousands(seen.min(total as u64)), crate::progress::thousands(total as u64), all.swapped));
        }
        crate::import::commit_index(self)?;
        Ok(all)
    }

    /// The copy message `message` should be shown from, when it isn't already.
    fn more_original_copy(&self, message: i64) -> Result<Option<i64>> {
        let copies: Vec<(i64, Vec<u8>, Option<String>)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT c.id, c.content_hash, a.kind FROM copies c LEFT JOIN folders f ON f.id = c.folder LEFT JOIN accounts a ON a.id = f.account
                 WHERE c.message = ?1 AND c.format = 'mime' ORDER BY c.id",
            )?;
            stmt.query_map(params![message], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        if copies.len() < 2 {
            return Ok(None);
        }
        let current: Vec<u8> = self.conn.query_row("SELECT content_hash FROM messages WHERE id = ?1", params![message], |r| r.get(0))?;
        let Some(now) = copies.iter().find(|c| c.1 == current) else { return Ok(None) };
        // the best of the others (the oldest of equals, so it never swaps back and forth);
        // kept copies are never Eudora's conversions (the writer doesn't keep those)
        let Some(best) = copies.iter().filter(|c| c.0 != now.0).min_by_key(|c| (rank(c.2.as_deref(), false), c.0)) else { return Ok(None) };
        let best_rank = rank(best.2.as_deref(), false);
        let mut now_rank = rank(now.2.as_deref(), false);
        if best_rank >= now_rank && now_rank == 1 {
            // a server's copy: is it really Eudora's conversion of an export?
            let raw = self.copy_bytes(now.0)?;
            now_rank = rank(now.2.as_deref(), converted(&raw));
        }
        Ok((best_rank < now_rank).then_some(best.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_users_own_files_come_first_then_servers_then_conversions() {
        assert!(rank(Some("eudora"), false) < rank(Some("graph"), false));
        assert!(rank(None, false) < rank(Some("imap"), false));
        assert!(rank(Some("graph"), false) < rank(Some("graph"), true));
        assert!(converted(b"From: a@b.c\r\nX-Eudora-Converted: Exchange export\r\n\r\nhi"));
        assert!(!converted(b"From: a@b.c\r\nSubject: hi\r\n\r\nX-Eudora-Converted: in the body"));
    }

    #[test]
    fn a_duplicate_is_shown_from_the_users_own_file() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // the same message twice: as a server gave it, and as the user's own file has it
        let server = "From: ann@x.com\r\nTo: me@y.com\r\nSubject: Hello\r\nMessage-ID: <h1@x.com>\r\nDate: Fri, 1 Dec 2023 10:00:00 +0000\r\nX-MS-Exchange-Organization-SCL: 1\r\n\r\nhello\r\n";
        let own = "From: ann@x.com\r\nTo: me@y.com\r\nSubject: RE: Hello\r\nMessage-ID: <h1@x.com>\r\nDate: Fri, 1 Dec 2023 10:00:00 +0000\r\n\r\nhello\r\n";
        let mut paths = Vec::new();
        for (name, text) in [("1.eml", server), ("2.eml", own)] {
            std::fs::write(src.join(name), text).unwrap();
            paths.push(src.join(name));
        }
        crate::import(&mut library, &paths, &crate::ImportOptions::default(), &Silent).unwrap();
        let (id, first): (i64, Vec<u8>) = library.conn.query_row("SELECT id, content_hash FROM messages", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(library.conn.query_row("SELECT COUNT(*) FROM copies WHERE message = ?1", [id], |r| r.get::<_, i64>(0)).unwrap(), 2);
        // the copy stored first came from Microsoft 365
        let graph = crate::db::graph_account(&library.conn, "graph", "me@y.com", "Me", 0).unwrap();
        let inbox = crate::db::ensure_folder(&library.conn, graph, "Inbox").unwrap();
        library.conn.execute("UPDATE copies SET folder = ?1 WHERE content_hash = ?2", params![inbox, first]).unwrap();
        let done = library.prefer_originals_all(&Silent).unwrap();
        assert_eq!((done.looked, done.swapped), (1, 1));
        let (hash, subject): (Vec<u8>, String) = library.conn.query_row("SELECT content_hash, subject FROM messages WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_ne!(hash, first);
        assert_eq!(subject, "RE: Hello", "read again from the user's own copy");
        assert_eq!(library.raw_message(id).unwrap(), own.as_bytes());
        // looked at once: nothing new, nothing done
        assert_eq!(library.prefer_originals_all(&Silent).unwrap(), Preferred::default());
    }
}
