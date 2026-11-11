//! Seeds: a whole library as one file, to set up another computer without downloading
//! everything again.
//!
//! Made while Eudora runs: the database is a consistent snapshot (`VACUUM INTO`), the packs are
//! copied up to their size after that snapshot (they only grow, and every message the snapshot
//! knows is in them), the search index as of its last commit (a library catches its index up
//! when it opens). Sign-ins are never in it (they stay in each computer's credential store);
//! after restoring, sign in, and the next download fetches only what's new.
//!
//! The file: `EUDORA27-SEED 1\n`, then one entry per file (u16 path length, path, u64 size,
//! the bytes, their BLAKE3), then a JSON summary, its u64 length and `END`.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::progress::{Progress, thousands};

const MAGIC: &[u8] = b"EUDORA27-SEED 1\n";
const END: &[u8] = b"END";

/// What a seed holds (its last part).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub created: i64,
    pub library_id: String,
    pub messages: u64,
    pub files: u64,
    pub bytes: u64,
    /// The search index came along (else it's rebuilt after restoring).
    pub index: bool,
}

struct Writer<W: Write> {
    out: W,
    files: u64,
    bytes: u64,
}

impl<W: Write> Writer<W> {
    /// One entry: `len` bytes from `from`.
    fn entry(&mut self, path: &str, from: &mut dyn Read, len: u64, progress: &dyn Fn(u64)) -> Result<()> {
        let name = path.replace('\\', "/");
        let w = |e: std::io::Error| Error::Io { context: "writing the seed".into(), source: e };
        self.out.write_all(&(name.len() as u16).to_le_bytes()).map_err(w)?;
        self.out.write_all(name.as_bytes()).map_err(w)?;
        self.out.write_all(&len.to_le_bytes()).map_err(w)?;
        let mut hasher = blake3::Hasher::new();
        let mut left = len;
        let mut buf = vec![0u8; 4 << 20];
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = from.read(&mut buf[..want]).context(|| format!("reading {path}"))?;
            if n == 0 {
                return Err(Error::Other(format!("{path} got shorter while it was copied")));
            }
            hasher.update(&buf[..n]);
            self.out.write_all(&buf[..n]).map_err(w)?;
            left -= n as u64;
            self.bytes += n as u64;
            progress(self.bytes);
        }
        self.out.write_all(hasher.finalize().as_bytes()).map_err(w)?;
        self.files += 1;
        Ok(())
    }

    fn file(&mut self, root: &Path, rel: &str, len: Option<u64>, progress: &dyn Fn(u64)) -> Result<()> {
        let path = root.join(rel);
        let mut f = File::open(&path).context(|| format!("opening {}", path.display()))?;
        let len = match len {
            Some(n) => n,
            None => f.metadata().context(|| format!("reading {}", path.display()))?.len(),
        };
        self.entry(rel, &mut f, len, progress)
    }
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.path().strip_prefix(dir.parent().unwrap_or(dir)).ok().map(|p| p.to_string_lossy().replace('\\', "/")))
        .collect();
    out.sort();
    out
}

fn size_of(root: &Path, rel: &str) -> u64 {
    std::fs::metadata(root.join(rel)).map(|m| m.len()).unwrap_or(0)
}

/// Writes a seed of the library at `root` to `out` (which must not exist yet).
pub fn create(root: &Path, out: &Path, progress: &dyn Progress) -> Result<Summary> {
    if out.exists() {
        return Err(Error::Other(format!("{} is there already: choose another name", out.display())));
    }
    if !root.join("eudora.db").is_file() {
        return Err(Error::NotALibrary(root.to_path_buf()));
    }
    // 1. the database, consistent (a snapshot while other copies of Eudora may write)
    progress.report(0, 0, "Taking a snapshot of the database…");
    let snapshot = out.with_extension("db-snapshot");
    let _ = std::fs::remove_file(&snapshot);
    {
        let conn = rusqlite::Connection::open(root.join("eudora.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
        conn.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
    }
    let snap = rusqlite::Connection::open(&snapshot)?;
    let messages: i64 = snap.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let library_id: String = snap.query_row("SELECT value FROM meta WHERE key = 'library_id'", [], |r| r.get(0)).unwrap_or_default();
    drop(snap);

    // 2. what else to copy, and how much: packs as they are now (after the snapshot)
    let mut plan: Vec<(String, u64)> = Vec::new();
    for dir in ["packs", "dicts", "images", "imports"] {
        if root.join(dir).is_dir() {
            plan.extend(files_in(&root.join(dir)).into_iter().map(|rel| {
                let n = size_of(root, &rel);
                (rel, n)
            }));
        }
    }
    let index_files = index_files(root);
    let total: u64 = std::fs::metadata(&snapshot).map(|m| m.len()).unwrap_or(0)
        + plan.iter().map(|(_, n)| n).sum::<u64>()
        + index_files.as_ref().map_or(0, |f| f.iter().map(|(_, n)| n).sum());

    let file = File::create(out).context(|| format!("creating {}", out.display()))?;
    let mut w = Writer { out: BufWriter::with_capacity(8 << 20, file), files: 0, bytes: 0 };
    w.out.write_all(MAGIC).context(|| "writing the seed".to_string())?;
    let report = |done: u64| progress.report(done / (1 << 20), total / (1 << 20), &format!("Writing the seed: {} of {} MB", thousands(done >> 20), thousands(total >> 20)));
    let result = (|| -> Result<bool> {
        let mut f = File::open(&snapshot).context(|| "reading the database snapshot".to_string())?;
        let len = f.metadata().context(|| "reading the database snapshot".to_string())?.len();
        w.entry("eudora.db", &mut f, len, &report)?;
        for (rel, len) in &plan {
            if progress.canceled() {
                return Err(Error::Canceled);
            }
            w.file(root, rel, Some(*len), &report)?;
        }
        // the index: its files as of its last commit (a merge running meanwhile can remove
        // one: then the seed goes without the index, and it's rebuilt after restoring)
        let mut with_index = false;
        if let Some(files) = index_files {
            with_index = true;
            for (rel, len) in &files {
                match File::open(root.join(rel)) {
                    Ok(mut f) => w.entry(rel, &mut f, *len, &report)?,
                    Err(_) => {
                        with_index = false;
                        break;
                    }
                }
            }
        }
        Ok(with_index)
    })();
    let _ = std::fs::remove_file(&snapshot);
    let index = match result {
        Ok(index) => index,
        Err(e) => {
            drop(w);
            let _ = std::fs::remove_file(out);
            return Err(e);
        }
    };
    let summary = Summary { created: crate::library::now(), library_id, messages: messages as u64, files: w.files, bytes: w.bytes, index };
    let json = serde_json::to_vec(&summary)?;
    let wr = |e: std::io::Error| Error::Io { context: "writing the seed".into(), source: e };
    w.out.write_all(&0u16.to_le_bytes()).map_err(wr)?; // (no more entries)
    w.out.write_all(&json).map_err(wr)?;
    w.out.write_all(&(json.len() as u64).to_le_bytes()).map_err(wr)?;
    w.out.write_all(END).map_err(wr)?;
    w.out.flush().map_err(wr)?;
    Ok(summary)
}

/// The search index's files as of its last commit: meta.json and the files of its segments.
fn index_files(root: &Path) -> Option<Vec<(String, u64)>> {
    let dir = root.join("index");
    let meta = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&meta).ok()?;
    let segments: Vec<String> = v["segments"].as_array()?.iter().filter_map(|s| s["segment_id"].as_str().map(|id| id.replace('-', ""))).collect();
    let mut out = vec![("index/meta.json".to_string(), meta.len() as u64)];
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if segments.iter().any(|s| name.starts_with(s.as_str())) {
            out.push((format!("index/{name}"), entry.metadata().ok()?.len()));
        }
    }
    Some(out)
}

/// Unpacks a seed into `target` (a new, empty folder: never over a library), checking every
/// file. Then open it for writing once (Eudora does: it catches the index up) and sign in.
pub fn restore(seed: &Path, target: &Path, progress: &dyn Progress) -> Result<Summary> {
    if target.exists() && std::fs::read_dir(target).context(|| format!("reading {}", target.display()))?.next().is_some() {
        return Err(Error::Other(format!("{} isn't empty: restore into a new folder", target.display())));
    }
    let size = std::fs::metadata(seed).context(|| format!("reading {}", seed.display()))?.len();
    let mut input = BufReader::with_capacity(8 << 20, File::open(seed).context(|| format!("opening {}", seed.display()))?);
    let r = |e: std::io::Error| Error::Io { context: format!("reading {}", seed.display()), source: e };
    let mut magic = vec![0u8; MAGIC.len()];
    input.read_exact(&mut magic).map_err(r)?;
    if magic != MAGIC {
        return Err(Error::Other(format!("{} isn't an Eudora seed", seed.display())));
    }
    let partial = target.with_extension("restoring");
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).context(|| format!("creating {}", partial.display()))?;
    let mut done = MAGIC.len() as u64;
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let mut two = [0u8; 2];
        input.read_exact(&mut two).map_err(r)?;
        let name_len = u16::from_le_bytes(two) as usize;
        if name_len == 0 {
            break;
        }
        let mut name = vec![0u8; name_len];
        input.read_exact(&mut name).map_err(r)?;
        let name = String::from_utf8(name).map_err(|_| Error::Other("a damaged seed (a file name)".into()))?;
        // only plain relative paths inside the library
        let rel = PathBuf::from(&name);
        if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(Error::Other(format!("a damaged seed (a file outside the library: {name})")));
        }
        let mut eight = [0u8; 8];
        input.read_exact(&mut eight).map_err(r)?;
        let mut left = u64::from_le_bytes(eight);
        let path = partial.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).context(|| format!("creating {}", path.display()))?;
        let mut out = BufWriter::with_capacity(8 << 20, File::create(&path).context(|| format!("creating {}", path.display()))?);
        let mut hasher = blake3::Hasher::new();
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = input.read(&mut buf[..want]).map_err(r)?;
            if n == 0 {
                return Err(Error::Other(format!("the seed ends in the middle of {name}: it's incomplete")));
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).context(|| format!("writing {}", path.display()))?;
            left -= n as u64;
            done += n as u64;
            progress.report(done >> 20, size >> 20, &format!("Restoring: {} of {} MB", thousands(done >> 20), thousands(size >> 20)));
        }
        out.flush().context(|| format!("writing {}", path.display()))?;
        let mut sum = [0u8; 32];
        input.read_exact(&mut sum).map_err(r)?;
        if hasher.finalize().as_bytes() != &sum {
            return Err(Error::Other(format!("{name} is damaged in the seed (its checksum doesn't match)")));
        }
    }
    let mut rest = Vec::new();
    input.read_to_end(&mut rest).map_err(r)?;
    if !rest.ends_with(END) || rest.len() < END.len() + 8 {
        return Err(Error::Other("the seed's last part is missing: it's incomplete".into()));
    }
    let json = &rest[..rest.len() - END.len() - 8];
    let summary: Summary = serde_json::from_slice(json)?;
    if !summary.index {
        let _ = std::fs::remove_dir_all(partial.join("index")); // (rebuilt when it opens)
    }
    std::fs::rename(&partial, target).or_else(|_| {
        // (the target folder may exist empty)
        let _ = std::fs::remove_dir(target);
        std::fs::rename(&partial, target)
    })
    .context(|| format!("moving the restored library to {}", target.display()))?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    #[test]
    fn a_library_goes_through_a_seed_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        let mut library = crate::Library::open(&root, Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files: Vec<PathBuf> = (0..30)
            .map(|i| {
                let p = src.join(format!("{i}.eml"));
                std::fs::write(&p, format!("From: a{i}@x.com\r\nTo: me@x.com\r\nSubject: note {i}\r\nMessage-ID: <{i}@x>\r\n\r\nseed body {i}\r\n")).unwrap();
                p
            })
            .collect();
        crate::import(&mut library, &files, &crate::ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        library.set_setting("rules", "[]").unwrap();
        library.close(&Silent).unwrap();

        // made while another copy has the library open for writing
        let busy = crate::Library::open(&root, Access::Write, &Silent).unwrap();
        let seed = dir.path().join("library.eudora-seed");
        let summary = create(&root, &seed, &Silent).unwrap();
        assert_eq!(summary.messages, 30);
        assert!(create(&root, &seed, &Silent).is_err(), "never over an existing file");
        drop(busy);

        let target = dir.path().join("restored");
        restore(&seed, &target, &Silent).unwrap();
        let restored = crate::Library::open(&target, Access::Write, &Silent).unwrap();
        assert_eq!(restored.count(&[]).unwrap(), 30);
        let (found, _) = restored.search("seed", 50, crate::SortOrder::Newest).unwrap();
        assert_eq!(found.total, 30, "the index came along (or was caught up)");
        assert_eq!(restored.setting("rules").unwrap().as_deref(), Some("[]"));
        let id: i64 = restored.connection().query_row("SELECT id FROM messages LIMIT 1", [], |r| r.get(0)).unwrap();
        assert!(!restored.raw_message(id).unwrap().is_empty());

        // a damaged seed is refused
        let mut bytes = std::fs::read(&seed).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xFF;
        let bad = dir.path().join("bad.eudora-seed");
        std::fs::write(&bad, bytes).unwrap();
        assert!(restore(&bad, &dir.path().join("restored2"), &Silent).is_err());
        assert!(restore(&seed, &target, &Silent).is_err(), "never into a library");
    }
}
