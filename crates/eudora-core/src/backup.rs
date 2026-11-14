//! Back Up Eudora / Restore Eudora.
//!
//! A backup holds the library (every message's original bytes in the packs, the database with
//! accounts, folders, rules, settings and the window's choices, the filing-sync copies, the web
//! sign-in), optionally the search indexes (quicker restore) and the passwords (IMAP / SMTP and
//! the sync key, from the OS secret store; Microsoft 365 signs in again on the new computer).
//!
//! The file: `EUDBAK01`, a JSON header in the clear (what's inside, sizes, whether a password is
//! needed and the check for it), then the body: records (a file, a secret) and an end mark with
//! the BLAKE3 hash of everything before it. With a password the body is sealed in 1 MiB chunks
//! (AES-256-GCM, key from Argon2id; each chunk numbered, the last one marked, so a missing or
//! reordered chunk is found). Split into parts of a chosen size (`Name.part001.eudora-backup`,
//! `.part002` …) when asked, else one file (`Name.eudora-backup`).
//!
//! Any OS to any OS: paths inside are '/'-separated and checked on the way out,
//! every number has a fixed byte order, SQLite, the packs and Tantivy's files are portable, and
//! passwords go into whichever secret store the restoring computer has.
//!
//! The library is copied with nothing writing to it: the backup holds the library's lock (the
//! window pauses its download first); the database is copied with SQLite's `VACUUM INTO`, which
//! is consistent even if the window changes a setting meanwhile.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::progress::Progress;

const MAGIC: &[u8; 8] = b"EUDBAK01";
const CHUNK: usize = 1 << 20;
/// The smallest part (tests split tiny libraries).
const MIN_PART: u64 = if cfg!(test) { 4096 } else { 1_000_000 };
pub const EXTENSION: &str = "eudora-backup";
/// What a backup must be told to include passwords without a password of its own.
pub const AGREE: &str = "I agree";
/// Argon2id costs (64 MiB, 3 passes): about a second to try one password.
const ARGON_MEMORY_KIB: u32 = 64 * 1024;
const ARGON_PASSES: u32 = 3;

/// What to back up, and how.
#[derive(Clone, Debug, Default)]
pub struct BackupOptions {
    /// The backup's file (`….eudora-backup`; parts are named after it).
    pub dest: PathBuf,
    /// Seal it with this password (None: unprotected).
    pub password: Option<String>,
    /// The search indexes too (a quicker restore), else they're rebuilt after restoring.
    pub indexes: bool,
    /// The passwords (IMAP, SMTP, the sync key). Without a password of its own this needs
    /// `agreed` = `AGREE`: anyone with the file could read them.
    pub passwords: bool,
    pub agreed: String,
    /// Split into parts of at most this many bytes (e.g. 4 GB for a FAT32 USB stick).
    pub part_size: Option<u64>,
}

/// What a backup holds, and how big each kind is (shown before backing up and restoring).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Sizes {
    /// The original messages (packs).
    pub messages: u64,
    pub database: u64,
    pub indexes: u64,
    /// Everything else kept in the library (filing-sync copies, the web sign-in, reports…).
    pub other: u64,
}

/// The backup's header, in the clear.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Header {
    pub version: u32,
    /// When it was made (unix seconds).
    pub created: i64,
    pub app: String,
    pub library: String,
    pub encrypted: bool,
    /// Argon2id salt (hex), and a block sealed with the key: a wrong password is told at once.
    pub salt: String,
    pub check: String,
    pub indexes: bool,
    pub passwords: bool,
    pub sizes: Sizes,
    pub files: u64,
    /// "phone" when made on a phone (iOS, Android), else empty: Restore warns that a phone's
    /// library isn't the whole thing (rules run on computers; perhaps only recent mail) and
    /// should be used only when no computer's backup is at hand.
    #[serde(default)]
    pub device: String,
    /// The phone's download limit then (setting `download-period`, as stored), else empty.
    #[serde(default)]
    pub limit: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BackupReport {
    pub files: u64,
    pub bytes: u64,
    pub secrets: u64,
    /// The backup's files (one, or its parts).
    pub parts: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RestoreReport {
    pub files: u64,
    pub bytes: u64,
    pub secrets: u64,
    /// The search indexes weren't in the backup: they're rebuilt when the library is next opened.
    pub rebuild_indexes: bool,
}

// ------------------------------------------------------------------ what's in a library

/// Library entries never backed up: this computer's own (the lock, the download program's
/// copy, logs, the running download's status) and SQLite's working files (the database is
/// copied whole with VACUUM INTO).
fn skipped(rel: &str, indexes: bool) -> bool {
    let top = rel.split('/').next().unwrap_or("");
    matches!(top, "library.lock" | "bin" | "logs" | "status" | "eudora.db" | "eudora.db-wal" | "eudora.db-shm" | "eudora.db-backup")
        || (!indexes && matches!(top, "index" | "header-index"))
        || rel.ends_with(".part")
}

fn kind_of(rel: &str) -> &'static str {
    match rel.split('/').next().unwrap_or("") {
        "packs" | "dicts" => "messages",
        "index" | "header-index" => "indexes",
        _ => "other",
    }
}

/// Every file to back up: (path in the library with '/', its full path, size).
fn library_files(root: &Path, indexes: bool) -> Result<Vec<(String, PathBuf, u64)>> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).context(|| format!("reading {}", dir.display()))? {
            let entry = entry.context(|| format!("reading {}", dir.display()))?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path).components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect::<Vec<_>>().join("/");
            if skipped(&rel, indexes) {
                continue;
            }
            let meta = entry.metadata().context(|| format!("reading {}", path.display()))?;
            if meta.is_dir() {
                dirs.push(path);
            } else if meta.is_file() {
                out.push((rel, path, meta.len()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// What a backup of this library would hold, by kind (the database's size is its file's: the
/// copy is about that or a little less).
pub fn sizes(root: &Path) -> Result<Sizes> {
    let mut s = Sizes::default();
    for (rel, _, size) in library_files(root, true)? {
        match kind_of(&rel) {
            "messages" => s.messages += size,
            "indexes" => s.indexes += size,
            _ => s.other += size,
        }
    }
    s.database = ["eudora.db", "eudora.db-wal"].iter().filter_map(|n| std::fs::metadata(root.join(n)).ok()).map(|m| m.len()).sum();
    Ok(s)
}

/// The secrets a backup carries: each IMAP / POP3 account's password and SMTP password, the sync
/// key, and the keys the user typed in (the Spamhaus key, the Google Maps key, their Zoom app,
/// the Twos token: added 2026-10-26, when backups were checked against
/// everything added since). Never this computer's device name, never a sign-in (Microsoft 365, Google, Zoom: signed
/// in again on the new computer). A secret not on this computer is simply not there.
pub(crate) fn secret_names(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT lower(address) FROM accounts WHERE kind IN ('imap', 'pop3') AND address IS NOT NULL")?;
    let addresses: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut names: Vec<String> = addresses.iter().flat_map(|a| [format!("imap:{a}"), format!("smtp:{a}")]).collect();
    // contacts and calendars servers' passwords
    let mut stmt = conn.prepare("SELECT substr(key, 13) FROM settings_items WHERE key LIKE 'dav-account:%'")?;
    let dav: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    names.extend(dav.iter().map(|a| format!("dav:{}", a.to_lowercase())));
    names.push("sync-key".into());
    names.extend(["spamhaus-dqs", "maps:google", "zoom:app"].map(String::from));
    let twos: Option<String> = conn.query_row("SELECT value FROM settings_items WHERE key = 'twos-user'", [], |r| r.get(0)).ok();
    if let Some(user) = twos.filter(|u| !u.trim().is_empty()) {
        names.push(format!("twos:{user}"));
    }
    Ok(names)
}

/// Whether a backup may restore a secret by this name: the kinds `secret_names` puts in, never a
/// sign-in.
fn secret_allowed(name: &str) -> bool {
    let address = |rest: &str| !rest.is_empty() && rest.len() <= 320 && rest.contains('@') && !rest.chars().any(|c| c.is_control() || c.is_whitespace());
    matches!(name, "sync-key" | "spamhaus-dqs" | "maps:google" | "zoom:app")
        || ["imap:", "smtp:", "dav:"].iter().any(|p| name.strip_prefix(p).is_some_and(address))
        || name.strip_prefix("twos:").is_some_and(|u| !u.is_empty() && u.len() <= 200 && !u.chars().any(char::is_control))
}

// ------------------------------------------------------------------ writing

/// Writes bytes across part files of at most `limit` bytes each.
struct Parts {
    base: PathBuf,
    limit: Option<u64>,
    file: Option<BufWriter<File>>,
    in_file: u64,
    paths: Vec<PathBuf>,
}

/// A backup's file name for part `n` (1-based; `split`: in parts).
pub fn part_path(base: &Path, n: usize, split: bool) -> PathBuf {
    let stem = base.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let stem = stem.strip_suffix(&format!(".{EXTENSION}")).unwrap_or(&stem).to_string();
    if split { base.with_file_name(format!("{stem}.part{n:03}.{EXTENSION}")) } else { base.with_file_name(format!("{stem}.{EXTENSION}")) }
}

impl Parts {
    fn new(base: &Path, limit: Option<u64>) -> Parts {
        Parts { base: base.to_path_buf(), limit: limit.filter(|l| *l >= MIN_PART), file: None, in_file: 0, paths: Vec::new() }
    }

    fn next_file(&mut self) -> Result<()> {
        if let Some(mut f) = self.file.take() {
            f.flush().context(|| "writing the backup".to_string())?;
        }
        let path = part_path(&self.base, self.paths.len() + 1, self.limit.is_some());
        if path.exists() {
            return Err(Error::Other(format!("{} is there already: choose another name", path.display())));
        }
        self.file = Some(BufWriter::with_capacity(1 << 20, File::create(&path).context(|| format!("creating {}", path.display()))?));
        self.paths.push(path);
        self.in_file = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<PathBuf>> {
        if let Some(mut f) = self.file.take() {
            f.flush().context(|| "writing the backup".to_string())?;
            f.get_ref().sync_all().context(|| "writing the backup".to_string())?;
        }
        Ok(self.paths)
    }
}

impl Write for Parts {
    fn write(&mut self, mut buf: &[u8]) -> std::io::Result<usize> {
        let total = buf.len();
        while !buf.is_empty() {
            let room = match self.limit {
                Some(l) if self.file.is_some() && self.in_file >= l => 0,
                Some(l) => (l - self.in_file).min(buf.len() as u64) as usize,
                None => buf.len(),
            };
            if self.file.is_none() || room == 0 {
                self.next_file().map_err(std::io::Error::other)?;
                continue;
            }
            self.file.as_mut().unwrap().write_all(&buf[..room])?;
            self.in_file += room as u64;
            buf = &buf[room..];
        }
        Ok(total)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().map_or(Ok(()), |f| f.flush())
    }
}

/// The body's writer: hashes everything, and seals it in chunks when there's a key.
struct Body<W: Write> {
    out: W,
    key: Option<LessSafeKey>,
    buf: Vec<u8>,
    counter: u64,
    hash: blake3::Hasher,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

fn aad(counter: u64, last: bool) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(&counter.to_be_bytes());
    a[8] = last as u8;
    a
}

impl<W: Write> Body<W> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.hash.update(bytes);
        self.raw(bytes)
    }

    fn raw(&mut self, mut bytes: &[u8]) -> Result<()> {
        if self.key.is_none() {
            return self.out.write_all(bytes).context(|| "writing the backup".to_string());
        }
        while !bytes.is_empty() {
            let room = CHUNK - self.buf.len();
            let n = room.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buf.len() == CHUNK {
                self.seal(false)?;
            }
        }
        Ok(())
    }

    fn seal(&mut self, last: bool) -> Result<()> {
        let key = self.key.as_ref().unwrap();
        let mut chunk = std::mem::take(&mut self.buf);
        key.seal_in_place_append_tag(nonce(self.counter), Aad::from(aad(self.counter, last)), &mut chunk).map_err(|_| Error::Other("sealing failed".into()))?;
        self.out.write_all(&(chunk.len() as u32).to_be_bytes()).context(|| "writing the backup".to_string())?;
        self.out.write_all(&chunk).context(|| "writing the backup".to_string())?;
        self.counter += 1;
        self.buf = Vec::with_capacity(CHUNK);
        Ok(())
    }

    /// The end mark (hash of everything before it), and the last chunk.
    fn end(mut self) -> Result<W> {
        let hash = self.hash.finalize();
        self.raw(b"E")?;
        self.raw(hash.as_bytes())?;
        if self.key.is_some() {
            self.seal(true)?;
        }
        Ok(self.out)
    }
}

fn key_from(password: &str, salt: &[u8]) -> Result<LessSafeKey> {
    let params = argon2::Params::new(ARGON_MEMORY_KIB, ARGON_PASSES, 1, Some(32)).map_err(|e| Error::Other(format!("password key: {e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon.hash_password_into(password.as_bytes(), salt, &mut key).map_err(|e| Error::Other(format!("password key: {e}")))?;
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| Error::Other("password key".into()))?))
}

/// The password check: 32 zero bytes sealed with the key under a number no chunk uses.
fn check_block(key: &LessSafeKey) -> Result<Vec<u8>> {
    let mut b = vec![0u8; 32];
    key.seal_in_place_append_tag(nonce(u64::MAX), Aad::from(aad(u64::MAX, true)), &mut b).map_err(|_| Error::Other("sealing failed".into()))?;
    Ok(b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len() % 2 == 0).then(|| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect::<Option<Vec<u8>>>()).flatten()
}

/// Takes the library's lock for as long as the backup runs (nothing else may write meanwhile).
fn lock(root: &Path) -> Result<File> {
    let path = root.join("library.lock");
    let file = File::create(&path).context(|| format!("opening {}", path.display()))?;
    if file.try_lock().is_err() {
        return Err(Error::Other("the library is being written (a download is running): stop it, or let Eudora pause it, and try again".into()));
    }
    Ok(file)
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Backs up the library at `root`.
pub fn back_up(root: &Path, options: &BackupOptions, progress: &dyn Progress) -> Result<BackupReport> {
    if options.passwords && options.password.is_none() && options.agreed != AGREE {
        return Err(Error::Other(format!("passwords go into an unprotected backup only after you type “{AGREE}”")));
    }
    // (a part size too small for a part was once dropped without a word: one file came out)
    if options.part_size.is_some_and(|p| p < MIN_PART) {
        return Err(Error::Other("parts must be at least 1 MB".into()));
    }
    if options.password.as_deref().is_some_and(|p| p.chars().count() < 8) {
        return Err(Error::Other("the backup's password needs at least 8 characters".into()));
    }
    let _lock = lock(root)?;
    let conn = rusqlite::Connection::open_with_flags(root.join("eudora.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let library_id: String = conn.query_row("SELECT value FROM meta WHERE key = 'library_id'", [], |r| r.get(0)).unwrap_or_default();

    // the database, whole and consistent, beside the backup (deleted when done)
    let db_copy = options.dest.with_file_name(format!("eudora-database-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&db_copy);
    progress.report(0, 0, &format!("Copying the database ({})…", gb(sizes(root)?.database)));
    conn.execute("VACUUM INTO ?1", [db_copy.to_string_lossy()])?;
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _gone = Gone(db_copy.clone());
    let db_size = std::fs::metadata(&db_copy).context(|| "reading the database copy".to_string())?.len();

    let files = library_files(root, options.indexes)?;
    let mut s = Sizes { database: db_size, ..Default::default() };
    for (rel, _, size) in &files {
        match kind_of(rel) {
            "messages" => s.messages += size,
            "indexes" => s.indexes += size,
            _ => s.other += size,
        }
    }
    let secrets: Vec<(String, Vec<u8>)> = if options.passwords {
        secret_names(&conn)?.into_iter().filter_map(|n| crate::graph::load_secret(&n).map(|v| (n, v))).collect()
    } else {
        Vec::new()
    };

    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    let key = match &options.password {
        Some(p) => {
            progress.report(0, 0, "Making the key from the password…");
            Some(key_from(p, &salt)?)
        }
        None => None,
    };
    let header = Header {
        version: 1,
        created: crate::library::now(),
        app: format!("Eudora {}", env!("CARGO_PKG_VERSION")),
        library: library_id,
        encrypted: key.is_some(),
        salt: hex(&salt),
        check: key.as_ref().map(check_block).transpose()?.map(|b| hex(&b)).unwrap_or_default(),
        indexes: options.indexes,
        passwords: !secrets.is_empty(),
        sizes: s.clone(),
        files: files.len() as u64 + 1,
        device: if cfg!(any(target_os = "ios", target_os = "android")) { "phone".into() } else { String::new() },
        limit: rusqlite::Connection::open_with_flags(root.join("eudora.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .and_then(|c| c.query_row("SELECT value FROM settings_items WHERE key = ?1", [crate::graph_sync::PERIOD_SETTING], |r| r.get(0)).ok())
            .unwrap_or_default(),
    };
    let mut parts = Parts::new(&options.dest, options.part_size);
    let head = serde_json::to_vec(&header)?;
    parts.write_all(MAGIC).context(|| "writing the backup".to_string())?;
    parts.write_all(&(head.len() as u32).to_be_bytes()).context(|| "writing the backup".to_string())?;
    parts.write_all(&head).context(|| "writing the backup".to_string())?;

    let total = s.messages + s.database + s.indexes + s.other;
    let mut body = Body { out: parts, key, buf: Vec::with_capacity(CHUNK), counter: 0, hash: blake3::Hasher::new() };
    let mut done = 0u64;
    let mut report = BackupReport::default();
    let mut copy = |body: &mut Body<Parts>, rel: &str, path: &Path, size: u64, n: usize, of: usize| -> Result<()> {
        if progress.canceled() {
            return Err(Error::Canceled);
        }
        let name = rel.as_bytes();
        body.put(b"F")?;
        body.put(&(name.len() as u16).to_be_bytes())?;
        body.put(name)?;
        body.put(&size.to_be_bytes())?;
        let mut file = File::open(path).context(|| format!("reading {}", path.display()))?.take(size);
        let mut buf = vec![0u8; CHUNK];
        let mut left = size;
        while left > 0 {
            let got = file.read(&mut buf).context(|| format!("reading {}", path.display()))?;
            if got == 0 {
                return Err(Error::Other(format!("{} got shorter while it was backed up", path.display())));
            }
            body.put(&buf[..got])?;
            left -= got as u64;
            done += got as u64;
            progress.report(done, total, &format!("Backing up: {} of {} (file {} of {}: {rel})", gb(done), gb(total), n, of));
        }
        report.files += 1;
        report.bytes += size;
        Ok(())
    };
    let of = files.len() + 1;
    copy(&mut body, "eudora.db", &db_copy, db_size, 1, of)?;
    for (i, (rel, path, size)) in files.iter().enumerate() {
        copy(&mut body, rel, path, *size, i + 2, of)?;
    }
    for (name, value) in &secrets {
        body.put(b"S")?;
        body.put(&(name.len() as u16).to_be_bytes())?;
        body.put(name.as_bytes())?;
        body.put(&(value.len() as u32).to_be_bytes())?;
        body.put(value)?;
        report.secrets += 1;
    }
    progress.report(total, total, "Finishing the backup…");
    report.parts = body.end()?.finish()?;
    Ok(report)
}

// ------------------------------------------------------------------ reading

/// The parts of the backup `path` names (any of them, or the single file), in order.
pub fn backup_parts(path: &Path) -> Result<Vec<PathBuf>> {
    let name = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let stem = name.strip_suffix(&format!(".{EXTENSION}")).unwrap_or(&name);
    let split = stem.rfind(".part").filter(|i| stem[i + 5..].chars().all(|c| c.is_ascii_digit()) && stem.len() > i + 5);
    let Some(at) = split else { return Ok(vec![path.to_path_buf()]) };
    let base = path.with_file_name(format!("{}.{EXTENSION}", &stem[..at]));
    let mut parts = Vec::new();
    for n in 1.. {
        let p = part_path(&base, n, true);
        if !p.exists() {
            break;
        }
        parts.push(p);
    }
    if parts.is_empty() {
        return Err(Error::Other(format!("can't find the first part of {}", path.display())));
    }
    Ok(parts)
}

/// Reads one part after another as one stream.
struct Joined {
    parts: Vec<PathBuf>,
    next: usize,
    file: Option<BufReader<File>>,
}

impl Read for Joined {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.file.is_none() {
                let Some(p) = self.parts.get(self.next) else { return Ok(0) };
                self.file = Some(BufReader::with_capacity(1 << 20, File::open(p).map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?));
                self.next += 1;
            }
            let n = self.file.as_mut().unwrap().read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            self.file = None;
        }
    }
}

fn read_header(r: &mut impl Read) -> Result<Header> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(|_| Error::Other("not an Eudora backup".into()))?;
    if &magic != MAGIC {
        return Err(Error::Other("not an Eudora backup (or one from a newer Eudora)".into()));
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len).map_err(|_| Error::Other("the backup is cut short".into()))?;
    let mut head = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut head).map_err(|_| Error::Other("the backup is cut short".into()))?;
    Ok(serde_json::from_slice(&head)?)
}

/// What a backup holds (to show before restoring): its header, and its parts.
pub fn describe(path: &Path) -> Result<(Header, Vec<PathBuf>)> {
    let parts = backup_parts(path)?;
    let mut r = Joined { parts: parts.clone(), next: 0, file: None };
    Ok((read_header(&mut r)?, parts))
}

/// The body's reader: opens chunks when sealed, and hashes what it gives out.
struct Plain<R: Read> {
    input: R,
    key: Option<LessSafeKey>,
    buf: Vec<u8>,
    at: usize,
    counter: u64,
    ended: bool,
}

impl<R: Read> Plain<R> {
    fn fill(&mut self) -> Result<()> {
        let mut len = [0u8; 4];
        self.input.read_exact(&mut len).map_err(|_| Error::Other("the backup is cut short (a part missing?)".into()))?;
        let mut chunk = vec![0u8; u32::from_be_bytes(len) as usize];
        if chunk.len() > CHUNK + 16 {
            return Err(Error::Other("the backup is damaged".into()));
        }
        self.input.read_exact(&mut chunk).map_err(|_| Error::Other("the backup is cut short (a part missing?)".into()))?;
        let key = self.key.as_ref().unwrap();
        // the last chunk says so; any other chunk doesn't
        let plain = match key.open_in_place(nonce(self.counter), Aad::from(aad(self.counter, false)), &mut chunk.clone()) {
            Ok(p) => p.to_vec(),
            Err(_) => {
                let p = key.open_in_place(nonce(self.counter), Aad::from(aad(self.counter, true)), &mut chunk).map_err(|_| Error::Other("the backup is damaged or was changed".into()))?;
                self.ended = true;
                p.to_vec()
            }
        };
        self.counter += 1;
        self.buf = plain;
        self.at = 0;
        Ok(())
    }

    fn take(&mut self, out: &mut [u8]) -> Result<()> {
        if self.key.is_none() {
            return self.input.read_exact(out).map_err(|_| Error::Other("the backup is cut short (a part missing?)".into()));
        }
        let mut got = 0;
        while got < out.len() {
            if self.at == self.buf.len() {
                if self.ended {
                    return Err(Error::Other("the backup is damaged".into()));
                }
                self.fill()?;
                continue;
            }
            let n = (self.buf.len() - self.at).min(out.len() - got);
            out[got..got + n].copy_from_slice(&self.buf[self.at..self.at + n]);
            self.at += n;
            got += n;
        }
        Ok(())
    }
}

/// A path from a backup, checked: relative, plain names only (never `..`).
fn safe_path(dest: &Path, rel: &str) -> Result<PathBuf> {
    let mut p = dest.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':']) {
            return Err(Error::Other(format!("the backup names a file outside the library ({rel})")));
        }
        p.push(part);
    }
    Ok(p)
}

/// What a restore's destination holds.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Destination {
    /// Not there yet, or empty: a restore goes straight in.
    Empty,
    /// A Eudora library (its database or message packs: also what a restore left half done):
    /// it may be replaced, after asking. `bytes`: what it holds, `items`: files and folders.
    Library { bytes: u64, items: u64 },
    /// Other files: never overwritten (a slip of the folder picker mustn't empty Documents).
    Other,
}

/// Looks at a restore's destination.
pub fn destination(dest: &Path) -> Result<Destination> {
    if !dest.exists() {
        return Ok(Destination::Empty);
    }
    let mut entries = std::fs::read_dir(dest).context(|| format!("reading {}", dest.display()))?.peekable();
    if entries.peek().is_none() {
        return Ok(Destination::Empty);
    }
    if !dest.join("eudora.db").is_file() && !dest.join("packs").is_dir() {
        return Ok(Destination::Other);
    }
    let (mut bytes, mut items) = (0u64, 0u64);
    let mut stack = vec![dest.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).context(|| format!("reading {}", dir.display()))?.flatten() {
            items += 1;
            let Ok(meta) = entry.path().symlink_metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                bytes += meta.len();
            }
        }
    }
    Ok(Destination::Library { bytes, items })
}

/// Empties a Eudora library folder so a backup can be restored there in its place: only a
/// folder `destination` calls a library, and never one in use (its lock held by an open window
/// or a running download). Deleted for good, not to the Trash: a library is ~100 GB, and the
/// Trash would keep the disk full. Links inside are removed, never followed.
pub fn empty_for_restore(dest: &Path, progress: &dyn Progress) -> Result<()> {
    if !matches!(destination(dest)?, Destination::Library { .. }) {
        return Err(Error::Other(format!("{} isn't a Eudora library: it isn't emptied", dest.display())));
    }
    let lock_path = dest.join("library.lock");
    let lock = if lock_path.exists() {
        let file = std::fs::OpenOptions::new().write(true).open(&lock_path).context(|| format!("opening {}", lock_path.display()))?;
        if file.try_lock().is_err() {
            return Err(Error::Other(format!("the library in {} is in use (an open Eudora or its download): close it first", dest.display())));
        }
        Some(file)
    } else {
        None
    };
    let entries: Vec<PathBuf> = std::fs::read_dir(dest).context(|| format!("reading {}", dest.display()))?.flatten().map(|e| e.path()).filter(|p| *p != lock_path).collect();
    let total = entries.len() as u64;
    for (n, path) in entries.iter().enumerate() {
        if progress.canceled() {
            return Err(Error::Canceled);
        }
        progress.report(n as u64, total, &format!("Removing the old library: item {} of {}", n + 1, total));
        let is_dir = path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false);
        if is_dir { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }.context(|| format!("removing {}", path.display()))?;
    }
    drop(lock); // (Windows deletes no file still open)
    if lock_path.exists() {
        std::fs::remove_file(&lock_path).context(|| format!("removing {}", lock_path.display()))?;
    }
    Ok(())
}

/// Restores a backup into `dest` (a new, empty folder: the library there is the one backed
/// up). Passwords go back into this computer's secret store. Without the indexes in the
/// backup, they're rebuilt when the library is next opened for writing.
pub fn restore(backup: &Path, dest: &Path, password: Option<&str>, progress: &dyn Progress) -> Result<RestoreReport> {
    if dest.exists() && std::fs::read_dir(dest).context(|| format!("reading {}", dest.display()))?.next().is_some() {
        return Err(Error::Other(format!("{} isn't empty: restore into an empty folder, or replace the library there", dest.display())));
    }
    let parts = backup_parts(backup)?;
    let mut input = Joined { parts, next: 0, file: None };
    let header = read_header(&mut input)?;
    let key = if header.encrypted {
        let password = password.ok_or_else(|| Error::Other("this backup has a password".into()))?;
        progress.report(0, 0, "Checking the password…");
        let key = key_from(password, &unhex(&header.salt).unwrap_or_default())?;
        let mut check = unhex(&header.check).unwrap_or_default();
        if key.open_in_place(nonce(u64::MAX), Aad::from(aad(u64::MAX, true)), &mut check).is_err() {
            return Err(Error::Other("that isn't the backup's password".into()));
        }
        Some(key)
    } else {
        None
    };
    std::fs::create_dir_all(dest).context(|| format!("creating {}", dest.display()))?;
    let total = header.sizes.messages + header.sizes.database + header.sizes.indexes + header.sizes.other;
    let mut r = Plain { input, key, buf: Vec::new(), at: 0, counter: 0, ended: false };
    let mut hash = blake3::Hasher::new();
    let mut report = RestoreReport { rebuild_indexes: !header.indexes, ..Default::default() };
    let mut done = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if progress.canceled() {
            return Err(Error::Canceled);
        }
        let mut kind = [0u8; 1];
        r.take(&mut kind)?;
        match kind[0] {
            b'E' => {
                let mut want = [0u8; 32];
                r.take(&mut want)?;
                if hash.finalize().as_bytes() != &want {
                    return Err(Error::Other("the backup is damaged (its check doesn't match)".into()));
                }
                break;
            }
            b'F' | b'S' => {
                hash.update(&kind);
                let mut len = [0u8; 2];
                r.take(&mut len)?;
                hash.update(&len);
                let mut name = vec![0u8; u16::from_be_bytes(len) as usize];
                r.take(&mut name)?;
                hash.update(&name);
                let name = String::from_utf8(name).map_err(|_| Error::Other("the backup is damaged".into()))?;
                if kind[0] == b'S' {
                    let mut len = [0u8; 4];
                    r.take(&mut len)?;
                    hash.update(&len);
                    let mut value = vec![0u8; u32::from_be_bytes(len) as usize];
                    r.take(&mut value)?;
                    hash.update(&value);
                    // (only the kinds of secret a backup holds: a changed backup file can't put
                    // anything else, a sign-in for instance, into the credential store)
                    if !secret_allowed(&name) {
                        return Err(Error::Other(format!("the backup holds a secret it shouldn't ({}): not restored", name.chars().take(40).collect::<String>())));
                    }
                    crate::graph::store_secret(&name, &value)?;
                    report.secrets += 1;
                    continue;
                }
                let mut size = [0u8; 8];
                r.take(&mut size)?;
                hash.update(&size);
                let size = u64::from_be_bytes(size);
                let path = safe_path(dest, &name)?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
                }
                let mut out = BufWriter::with_capacity(1 << 20, File::create(&path).context(|| format!("creating {}", path.display()))?);
                let mut left = size;
                while left > 0 {
                    let n = (buf.len() as u64).min(left) as usize;
                    r.take(&mut buf[..n])?;
                    hash.update(&buf[..n]);
                    out.write_all(&buf[..n]).context(|| format!("writing {}", path.display()))?;
                    left -= n as u64;
                    done += n as u64;
                    progress.report(done, total, &format!("Restoring: {} of {} (file {} of {}: {name})", gb(done), gb(total), report.files + 1, header.files));
                }
                out.flush().context(|| format!("writing {}", path.display()))?;
                report.files += 1;
                report.bytes += size;
            }
            _ => return Err(Error::Other("the backup is damaged".into())),
        }
    }
    // the old computer's own paths (attachment files saved there) mean nothing here: they're
    // made again on this computer when attachments are opened (any OS to any OS)
    let conn = rusqlite::Connection::open(dest.join("eudora.db"))?;
    conn.execute("DELETE FROM local_files", [])?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Silent;

    #[test]
    fn only_a_library_not_in_use_is_emptied_for_a_restore() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("lib");
        assert_eq!(destination(&dest).unwrap(), Destination::Empty); // not there yet
        std::fs::create_dir(&dest).unwrap();
        assert_eq!(destination(&dest).unwrap(), Destination::Empty);
        // other files (Documents chosen by mistake): never emptied
        std::fs::write(dest.join("letter.docx"), b"mine").unwrap();
        assert_eq!(destination(&dest).unwrap(), Destination::Other);
        assert!(empty_for_restore(&dest, &Silent).is_err());
        assert!(dest.join("letter.docx").exists());
        // a library (here: what a restore left half done)
        std::fs::create_dir_all(dest.join("packs")).unwrap();
        std::fs::write(dest.join("packs/p1.pack"), [0u8; 100]).unwrap();
        assert_eq!(destination(&dest).unwrap(), Destination::Library { bytes: 104, items: 3 });
        // in use: refused
        let held = File::create(dest.join("library.lock")).unwrap();
        held.lock().unwrap();
        assert!(empty_for_restore(&dest, &Silent).unwrap_err().to_string().contains("in use"));
        drop(held);
        empty_for_restore(&dest, &Silent).unwrap();
        assert_eq!(destination(&dest).unwrap(), Destination::Empty);
        assert!(dest.is_dir(), "the folder itself stays");
    }

    #[test]
    fn a_restore_writes_only_the_kinds_of_secret_a_backup_holds() {
        for ok in ["imap:ann@example.com", "smtp:ann@example.com", "sync-key", "spamhaus-dqs", "maps:google", "zoom:app", "twos:abc123"] {
            assert!(secret_allowed(ok), "{ok}");
        }
        // sign-ins are never restored (signed in again), nor anything else a changed file names
        for no in ["graph:ann@example.com", "graph:default", "google:ann@example.com", "zoom:refresh", "imap:", "imap:not an address", "anything", "imap:a@b\ncom"] {
            assert!(!secret_allowed(no), "{no}");
        }
    }

    fn library(dir: &Path) -> PathBuf {
        let root = dir.join("lib");
        let mut library = crate::Library::open(&root, crate::library::Access::Write, &Silent).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files: Vec<_> = (1..=30)
            .map(|n| {
                let p = src.join(format!("{n}.eml"));
                // varied bodies, so the packs aren't tiny
                let body: String = (0..2000).map(|i| char::from(b'a' + ((i * n) % 26) as u8)).collect();
                std::fs::write(&p, format!("From: a@b.com\r\nSubject: m{n}\r\nMessage-ID: <{n}@b.com>\r\n\r\n{body}\r\n")).unwrap();
                p
            })
            .collect();
        crate::import(&mut library, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        library.set_setting("ui:skin", "\"gmail\"").unwrap();
        library.close(&Silent).unwrap();
        root
    }

    fn same_files(a: &Path, b: &Path, indexes: bool) {
        let fa: Vec<(String, u64)> = library_files(a, indexes).unwrap().into_iter().map(|(r, _, s)| (r, s)).collect();
        let fb: Vec<(String, u64)> = library_files(b, indexes).unwrap().into_iter().map(|(r, _, s)| (r, s)).collect();
        assert_eq!(fa, fb);
        for (rel, _) in fa {
            assert_eq!(std::fs::read(a.join(&rel)).unwrap(), std::fs::read(b.join(&rel)).unwrap(), "{rel}");
        }
    }

    #[test]
    fn a_backup_in_parts_with_a_password_restores_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let root = library(dir.path());
        let dest = dir.path().join("out").join("Eudora 2026-10-14.eudora-backup");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let options = BackupOptions { dest: dest.clone(), password: Some("correct horse".into()), indexes: true, part_size: Some(64 * 1024), ..Default::default() };
        let r = back_up(&root, &options, &Silent).unwrap();
        assert!(r.parts.len() > 2 && r.parts[0].to_string_lossy().ends_with(".part001.eudora-backup"), "{:?}", r.parts);
        // nothing secret in the clear
        let all: Vec<u8> = r.parts.iter().flat_map(|p| std::fs::read(p).unwrap()).collect();
        assert!(!all.windows(4).any(|w| w == b"m17\r"), "messages are sealed");

        let (header, parts) = describe(&r.parts[0]).unwrap();
        assert!(header.encrypted && header.indexes && parts == r.parts);
        assert!(restore(&r.parts[0], &dir.path().join("wrong"), Some("not it at all"), &Silent).is_err());
        let back = dir.path().join("restored");
        let rr = restore(&r.parts[0], &back, Some("correct horse"), &Silent).unwrap();
        assert!(!rr.rebuild_indexes);
        same_files(&root, &back, true);
        // the library opens and has its mail and settings
        let l = crate::Library::open(&back, crate::library::Access::Read, &Silent).unwrap();
        assert_eq!(l.count(&[]).unwrap(), 30);
        assert_eq!(l.setting("ui:skin").unwrap().as_deref(), Some("\"gmail\""));
        // a restore never goes into a folder that has something in it
        assert!(restore(&r.parts[0], &back, Some("correct horse"), &Silent).is_err());
    }

    #[test]
    fn an_unprotected_backup_without_indexes_restores_and_damage_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = library(dir.path());
        let dest = dir.path().join("plain.eudora-backup");
        // passwords without a password: only after "I agree"
        let refused = back_up(&root, &BackupOptions { dest: dest.clone(), passwords: true, ..Default::default() }, &Silent);
        assert!(refused.is_err());
        let r = back_up(&root, &BackupOptions { dest: dest.clone(), ..Default::default() }, &Silent).unwrap();
        assert_eq!(r.parts, vec![dest.clone()]);
        let back = dir.path().join("restored");
        let rr = restore(&dest, &back, None, &Silent).unwrap();
        assert!(rr.rebuild_indexes);
        same_files(&root, &back, false);
        assert!(!back.join("index").exists(), "indexes left out");
        // the library opens for writing and rebuilds its search index
        let l = crate::Library::open(&back, crate::library::Access::Write, &Silent).unwrap();
        assert!(l.search("m17", 10, crate::SortOrder::Newest).unwrap().1.len() >= 1, "searchable again");
        drop(l);

        // a changed byte is found
        let mut bytes = std::fs::read(&dest).unwrap();
        let at = bytes.len() / 2;
        bytes[at] ^= 0x55;
        let damaged = dir.path().join("damaged.eudora-backup");
        std::fs::write(&damaged, &bytes).unwrap();
        assert!(restore(&damaged, &dir.path().join("r2"), None, &Silent).is_err());
    }
}
