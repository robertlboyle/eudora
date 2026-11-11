//! Imports mbox and EML files into a library.
//!
//! Three stages run at once: a reader thread cuts the files into messages, a parse stage
//! parses and compresses each batch on every core, and the calling thread (the library's
//! only writer) stores them: pack records first (flushed to disk), then the database rows
//! and the checkpoint in one transaction, then index entries (committed every so often).
//! An import that stops (canceled, crashed, power cut) resumes at its last checkpoint, and
//! duplicates are recognized by checksum, so nothing is stored twice.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use rayon::prelude::*;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::db::{self, REF_IN_REPLY_TO, REF_REFERENCES, ROLE_BCC, ROLE_CC, ROLE_FROM, ROLE_TO};
use crate::error::{Error, IoContext, Result};
use crate::library::{Library, index_text, now};
use crate::mbox::MboxFile;
use crate::pack::{self, Compression, EncodedRecord};
use crate::parse::{self, DateHints, ParseStatus, ParsedMessage};
use crate::progress::{Progress, thousands};
use crate::sources::{self, Source, SourceKind};

/// Messages used to train the compression dictionary (the first ones imported).
const DICT_SAMPLES: usize = 4000;
const DICT_SAMPLE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Folder the import goes under; default "Imports/<name of the first path>".
    pub destination: Option<String>,
    pub batch_messages: usize,
    pub batch_bytes: usize,
    /// Make new messages searchable at least this often.
    pub index_commit_messages: u64,
    pub index_commit_interval: Duration,
    /// Start over instead of resuming an earlier, unfinished import of the same files.
    pub restart: bool,
    /// The account the mail belongs to (its address). Default: a classic Eudora folder's own
    /// address (its eudora.ini), else the "imported files" account.
    pub account: Option<String>,
    /// When that address already has an account from somewhere else: Some(true) adds this
    /// archive to it, Some(false) keeps it as an account of its own, None asks (the import
    /// stops with `Error::AccountExists` saying so). The same archive again always continues.
    pub merge_account: Option<bool>,
}

/// Who a classic Eudora folder belonged to: its main address (the dominant personality's
/// return address), the name, and its other personalities' addresses (from eudora.ini).
#[derive(Clone, Debug, PartialEq)]
pub struct EudoraIdentity {
    pub address: String,
    pub name: String,
    pub personalities: Vec<String>,
}

pub fn eudora_identity(dir: &std::path::Path) -> Option<EudoraIdentity> {
    let ini = std::fs::read(dir.join("eudora.ini")).ok()?;
    let text = parse::decode_8bit(&ini);
    let mut section = String::new();
    let (mut address, mut pop, mut name) = (String::new(), String::new(), String::new());
    let mut personalities = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = s.to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if value.is_empty() {
            continue;
        }
        match (section.as_str(), key.as_str()) {
            ("settings", "returnaddress") => address = value.to_string(),
            ("settings", "popaccount") => pop = value.to_string(),
            ("settings", "realname") => name = value.to_string(),
            (s, "returnaddress") if s.starts_with("persona-") => personalities.push(value.to_string()),
            _ => {}
        }
    }
    let address = if address.contains('@') { address } else { pop };
    if !address.contains('@') {
        return None;
    }
    personalities.retain(|p| !p.eq_ignore_ascii_case(&address));
    personalities.dedup();
    Some(EudoraIdentity { address, name, personalities })
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            destination: None,
            batch_messages: 1000,
            batch_bytes: 32 * 1024 * 1024,
            index_commit_messages: 250_000,
            index_commit_interval: Duration::from_secs(120),
            restart: false,
            account: None,
            merge_account: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    /// Messages read from the files (also those already in the library).
    pub seen: u64,
    pub imported: u64,
    /// Identical to a message already in the same folder.
    pub duplicates: u64,
    /// Identical to a message already in the library, now also in another folder.
    pub new_locations: u64,
    /// Another copy of a message already in the library (same Message-ID, date, sender and
    /// subject, different bytes): only where it is was recorded, the stored copy is kept.
    pub merged: u64,
    /// Original copies kept for messages already stored (a second pass for originals).
    pub copies_kept: u64,
    /// Imported with something fixed or guessed.
    pub fixed: u64,
    /// The parser couldn't read them: kept raw and searchable as text.
    pub unreadable: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProblemSummary {
    pub count: u64,
    pub example: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportReport {
    pub import_id: i64,
    pub destination: String,
    pub sources: usize,
    pub counts: Counts,
    /// Messages done by an earlier run of this import (resumed).
    pub resumed_after: u64,
    pub total: u64,
    pub problems: BTreeMap<String, ProblemSummary>,
    pub skipped_files: Vec<(PathBuf, String)>,
    pub elapsed_secs: f64,
    pub canceled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    /// Source being read.
    source: usize,
    /// mbox: byte offset of the next message's "From " line; EML: index of the next file.
    offset: usize,
    /// Messages done in all sources (for progress).
    done: u64,
    counts: Counts,
}

struct Incoming {
    raw: Vec<u8>,
    hints: DateHints,
    origin: String,
}

struct Batch {
    source: usize,
    messages: Vec<Incoming>,
    /// Where to continue after this batch.
    next: (usize, usize),
}

/// A message parsed and compressed, ready for the writer.
pub(crate) struct Prepared {
    parsed: ParsedMessage,
    record: EncodedRecord,
    origin: String,
    /// Kept only while dictionary samples are being collected.
    sample: Option<Vec<u8>>,
    folder: i64,
    /// The item's id on its server (downloads), recorded in `server_items`.
    pub(crate) server_id: Option<String>,
    /// The message's bytes were made by Eudora from `originals` (an archive item's MIME, made
    /// from Exchange's export): as another copy of a stored message they aren't kept (the
    /// originals are).
    derived: bool,
    /// The source's own bytes, kept as copies of the message (Exchange's export).
    originals: Vec<Original>,
}

impl Prepared {
    /// Keeps `original` (the source's own bytes this message was made from) with it.
    pub(crate) fn made_from(mut self, original: Original) -> Prepared {
        self.derived = true;
        self.originals.push(original);
        self
    }
}

/// A source's own bytes, compressed for the packs, kept as a copy of a message.
pub(crate) struct Original {
    record: EncodedRecord,
    hash: [u8; 32],
    size: u64,
    format: &'static str,
}

/// Prepares a source's own bytes (e.g. Exchange's export, `format` "exchange-export").
pub(crate) fn original(bytes: &[u8], format: &'static str, folder: &str, compression: &Compression) -> Result<Original> {
    let hash = *blake3::hash(bytes).as_bytes();
    let meta = serde_json::to_vec(&serde_json::json!({ "copy": format, "folder": folder }))?;
    Ok(Original { record: pack::encode(bytes, hash, meta, compression)?, hash, size: bytes.len() as u64, format })
}

/// What the writer does with one message.
enum Plan {
    /// The same bytes are stored already (as a message or a copy).
    Same(i64),
    /// The same bytes as an earlier message of this batch.
    SameAsItem(usize),
    /// Another copy of a stored message (Message-ID and the sanity checks agree); its own
    /// bytes are kept as a copy (None: made by Eudora, not kept).
    Copy(i64, Option<pack::Location>),
    /// Another copy of an earlier, new message of this batch.
    CopyOfItem(usize, Option<pack::Location>),
    New(pack::Location),
}

/// The message these bytes are stored for (as its own or as a copy).
fn stored_bytes(conn: &rusqlite::Connection, hash: &[u8; 32]) -> Result<Option<i64>> {
    let mut messages = conn.prepare_cached("SELECT id FROM messages WHERE content_hash = ?1")?;
    if let Some(id) = messages.query_row(params![&hash[..]], |r| r.get(0)).optional()? {
        return Ok(Some(id));
    }
    let mut copies = conn.prepare_cached("SELECT message FROM copies WHERE content_hash = ?1")?;
    Ok(copies.query_row(params![&hash[..]], |r| r.get(0)).optional()?)
}

/// Appends the originals not stored yet; returns where each new one went.
fn append_originals(
    library: &Library,
    item: &Prepared,
    seen: &mut std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<(usize, pack::Location)>> {
    let mut out = Vec::new();
    for (k, o) in item.originals.iter().enumerate() {
        if seen.contains(&o.hash) || stored_bytes(&library.conn, &o.hash)?.is_some() {
            continue;
        }
        seen.insert(o.hash);
        out.push((k, library.packs.append(&o.record)?));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn insert_copy(
    tx: &Transaction,
    message: i64,
    folder: Option<i64>,
    server_id: Option<&str>,
    format: &str,
    hash: &[u8; 32],
    size: u64,
    location: pack::Location,
    added: i64,
) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR IGNORE INTO copies (message, folder, server_id, format, content_hash, size, pack, pack_offset, pack_len, added)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?
    .execute(params![message, folder, server_id, format, &hash[..], size as i64, location.pack as i64, location.offset as i64, location.len as i64, added])?;
    Ok(())
}

struct PreparedBatch {
    items: Vec<Prepared>,
    next: (usize, usize),
}

#[derive(Serialize, Deserialize)]
struct RecordMeta<'a> {
    mid: &'a str,
    date: i64,
    ds: u8,
    folder: &'a str,
}

pub fn import(library: &mut Library, paths: &[PathBuf], options: &ImportOptions, progress: &dyn Progress) -> Result<ImportReport> {
    let started = Instant::now();
    if paths.is_empty() {
        return Err(Error::Other("nothing to import".into()));
    }
    progress.report(0, 0, "Looking for mailboxes…");
    let discovery = sources::discover(paths)?;
    let sources = discovery.sources;
    // Whose mail: the address given, else a classic Eudora folder's own, else "imported files".
    let eudora = paths.iter().find(|p| p.is_dir()).and_then(|p| eudora_identity(p));
    let (kind, address, account_name) = match (&options.account, &eudora) {
        (Some(a), _) => ("mbox", Some(a.clone()), String::new()),
        (None, Some(e)) => ("eudora", Some(e.address.clone()), e.name.clone()),
        (None, None) => ("local", None, String::new()),
    };
    let account_source = match &eudora {
        Some(e) if !e.personalities.is_empty() => format!("{} (personalities: {})", paths[0].display(), e.personalities.join(", ")),
        _ => paths[0].display().to_string(),
    };
    // An account of its own holds its folders at the top; "imported files" groups by import.
    let destination = options.destination.clone().unwrap_or_else(|| {
        if address.is_some() {
            return String::new();
        }
        let name = paths[0].file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Imported".into());
        format!("Imports/{name}")
    });

    // Count first, so progress has a real total.
    let mut per_source = Vec::with_capacity(sources.len());
    for (i, source) in sources.iter().enumerate() {
        progress.report(i as u64, sources.len() as u64, &format!("Counting messages: mailbox {} of {}", i + 1, sources.len()));
        per_source.push(match source.kind {
            SourceKind::Mbox => MboxFile::open(&source.files[0])?.count(),
            SourceKind::Eml => source.files.len() as u64,
        });
    }
    let total: u64 = per_source.iter().sum();

    // Folders, and the import row (new, or the unfinished one being resumed).
    let source_key = serde_json::to_string(&(paths.iter().map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect::<Vec<_>>(), &destination))?;
    let (import_id, checkpoint, folder_ids) = {
        let tx = library.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut separate = false;
        if let Some(address) = &address {
            if let Some((id, source)) = db::find_account(&tx, kind, address)? {
                let same_archive = source.as_deref().is_some_and(|s| s.starts_with(&paths[0].display().to_string()));
                match options.merge_account {
                    _ if same_archive => {}
                    Some(true) => {}
                    Some(false) => separate = true,
                    None => {
                        let messages: i64 = tx.query_row(
                            "SELECT COUNT(DISTINCT l.message) FROM locations l JOIN folders f ON f.id = l.folder WHERE f.account = ?1",
                            params![id],
                            |r| r.get(0),
                        )?;
                        return Err(Error::AccountExists {
                            address: address.clone(),
                            messages: messages as u64,
                            came_from: source.unwrap_or_default(),
                        });
                    }
                }
            }
        }
        let account = db::import_account(&tx, kind, address.as_deref(), &account_name, &account_source, separate, now())?;
        let mut folder_ids = Vec::with_capacity(sources.len());
        for source in &sources {
            let path = match (destination.is_empty(), source.folder.is_empty()) {
                (true, true) => "Imported".to_string(),
                (true, false) => source.folder.clone(),
                (false, true) => destination.clone(),
                (false, false) => format!("{destination}/{}", source.folder),
            };
            folder_ids.push((db::ensure_folder(&tx, account, &path)?, path));
        }
        let earlier: Option<(i64, Option<String>)> = if options.restart {
            None
        } else {
            tx.query_row(
                "SELECT id, checkpoint FROM imports WHERE source_key = ?1 AND status != 'done' ORDER BY id DESC LIMIT 1",
                params![source_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        };
        let (id, checkpoint) = match earlier {
            Some((id, cp)) => {
                tx.execute("UPDATE imports SET status = 'running' WHERE id = ?1", params![id])?;
                (id, cp.and_then(|c| serde_json::from_str::<Checkpoint>(&c).ok()).unwrap_or_default())
            }
            None => {
                let listed: Vec<String> = sources.iter().flat_map(|s| s.files.iter().map(|f| f.display().to_string())).collect();
                tx.execute(
                    "INSERT INTO imports (source_key, sources, started, status) VALUES (?1, ?2, ?3, 'running')",
                    params![source_key, serde_json::to_string(&listed)?, now()],
                )?;
                (tx.last_insert_rowid(), Checkpoint::default())
            }
        };
        tx.commit()?;
        (id, checkpoint, folder_ids)
    };
    let resumed_after = checkpoint.done;

    let compression = RwLock::new(library.compression());
    let need_samples = AtomicBool::new(compression.read().unwrap().dict.is_none());
    let stop = AtomicBool::new(false);
    let mut state = WriterState::new(import_id, checkpoint.clone());
    let mut samples: Vec<Vec<u8>> = Vec::new();
    let mut sample_bytes = 0usize;

    let outcome: Result<()> = std::thread::scope(|scope| {
        let (batch_tx, batch_rx) = bounded::<Batch>(4);
        let (ready_tx, ready_rx) = bounded::<PreparedBatch>(4);
        let sources_ref = &sources;
        let folder_ref = &folder_ids;
        let stop_ref = &stop;
        let start = (checkpoint.source, checkpoint.offset);

        let reader = scope.spawn(move || read_sources(sources_ref, start, options, &batch_tx, stop_ref));
        let compression_ref = &compression;
        let need_ref = &need_samples;
        let parser = scope.spawn(move || {
            for batch in batch_rx {
                let current = compression_ref.read().unwrap().clone();
                let keep_raw = need_ref.load(Ordering::Relaxed);
                let (folder_id, folder) = (folder_ref[batch.source].0, folder_ref[batch.source].1.as_str());
                let items: Vec<Result<Prepared>> = batch
                    .messages
                    .into_par_iter()
                    .map(|m| prepare(m.raw, m.hints, m.origin, folder, folder_id, None, &current, keep_raw))
                    .collect();
                let items = match items.into_iter().collect::<Result<Vec<_>>>() {
                    Ok(items) => items,
                    Err(e) => return Err(e),
                };
                if ready_tx.send(PreparedBatch { items, next: batch.next }).is_err() {
                    break;
                }
            }
            Ok(())
        });

        let mut result = Ok(());
        let mut last_commit = Instant::now();
        let mut since_commit = 0u64;
        for batch in ready_rx.iter() {
            if progress.canceled() {
                stop.store(true, Ordering::SeqCst);
                result = Err(Error::Canceled);
                break;
            }
            if need_samples.load(Ordering::Relaxed) {
                for item in &batch.items {
                    if let Some(raw) = &item.sample {
                        if samples.len() < DICT_SAMPLES && sample_bytes < DICT_SAMPLE_BYTES {
                            sample_bytes += raw.len();
                            samples.push(raw.clone());
                        }
                    }
                }
            }
            let n = batch.items.len() as u64;
            if let Err(e) = state.store_batch(library, batch) {
                stop.store(true, Ordering::SeqCst);
                result = Err(e);
                break;
            }
            since_commit += n;
            progress.report(
                state.checkpoint.done,
                total,
                &format!(
                    "Importing message {} of {} (mailbox {} of {})",
                    thousands(state.checkpoint.done),
                    thousands(total),
                    (state.checkpoint.source + 1).min(sources.len()),
                    sources.len()
                ),
            );
            if need_samples.load(Ordering::Relaxed) && (samples.len() >= DICT_SAMPLES || sample_bytes >= DICT_SAMPLE_BYTES) {
                progress.report(0, 0, &format!("Training the compression dictionary on {} messages…", thousands(samples.len() as u64)));
                match pack::train_dictionary(&samples).and_then(|bytes| library.packs.add_dictionary(&bytes, crate::library::ZSTD_LEVEL)) {
                    Ok(dict) => *compression.write().unwrap() = Compression { level: crate::library::ZSTD_LEVEL, dict: Some(dict) },
                    Err(e) => eprintln!("compression dictionary not trained: {e}"),
                }
                need_samples.store(false, Ordering::Relaxed);
                samples = Vec::new();
            }
            if since_commit >= options.index_commit_messages || last_commit.elapsed() >= options.index_commit_interval {
                if let Err(e) = commit_index(library) {
                    result = Err(e);
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
                since_commit = 0;
                last_commit = Instant::now();
            }
        }
        drop(ready_rx);
        let read_result = reader.join().map_err(|_| Error::Other("the mailbox reader stopped unexpectedly".into()))?;
        let parse_result = parser.join().map_err(|_| Error::Other("the message parser stopped unexpectedly".into()))?;
        result.and(read_result).and(parse_result)
    });

    let canceled = matches!(outcome, Err(Error::Canceled));
    // Whatever happened, make what was stored searchable and record where to resume.
    progress.report(0, 0, "Making the new messages searchable…");
    commit_index(library)?;
    let status = match &outcome {
        Ok(()) => "done",
        Err(Error::Canceled) => "canceled",
        Err(_) => "failed",
    };
    library.conn.execute(
        "UPDATE imports SET status = ?2, finished = ?3, counts = ?4 WHERE id = ?1",
        params![import_id, status, now(), serde_json::to_string(&state.checkpoint.counts)?],
    )?;
    if let Err(e) = outcome {
        if !canceled {
            return Err(e);
        }
    }

    let report = ImportReport {
        import_id,
        destination,
        sources: sources.len(),
        counts: state.checkpoint.counts.clone(),
        resumed_after,
        total,
        problems: state.problems,
        skipped_files: discovery.skipped,
        elapsed_secs: started.elapsed().as_secs_f64(),
        canceled,
    };
    let path = library.root.join("imports").join(format!("import-{import_id}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?).context(|| format!("writing {}", path.display()))?;
    Ok(report)
}

pub(crate) fn commit_index(library: &Library) -> Result<()> {
    let max: i64 = library.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?;
    library.index.commit(max)?;
    library.conn.execute("DELETE FROM index_pending", [])?;
    Ok(())
}

/// Reader thread: cuts the sources into batches, from the checkpoint on.
fn read_sources(sources: &[Source], start: (usize, usize), options: &ImportOptions, out: &crossbeam_channel::Sender<Batch>, stop: &AtomicBool) -> Result<()> {
    for (index, source) in sources.iter().enumerate().skip(start.0) {
        let first = if index == start.0 { start.1 } else { 0 };
        let mut messages = Vec::new();
        let mut bytes = 0usize;
        let send = |messages: Vec<Incoming>, next: (usize, usize)| -> bool { out.send(Batch { source: index, messages, next }).is_ok() };
        match source.kind {
            SourceKind::Mbox => {
                let path = &source.files[0];
                let mbox = MboxFile::open(path)?;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                for span in mbox.spans(first) {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let m = mbox.message(span);
                    bytes += m.raw.len();
                    messages.push(Incoming {
                        raw: m.raw.into_owned(),
                        hints: DateHints { envelope: m.envelope_date, file: None },
                        origin: format!("{name} at byte {}", span.from_line),
                    });
                    if messages.len() >= options.batch_messages || bytes >= options.batch_bytes {
                        let next = if span.next >= mbox.data().len() { (index + 1, 0) } else { (index, span.next) };
                        if !send(std::mem::take(&mut messages), next) {
                            return Ok(());
                        }
                        bytes = 0;
                    }
                }
                if !messages.is_empty() && !send(messages, (index + 1, 0)) {
                    return Ok(());
                }
            }
            SourceKind::Eml => {
                for (i, file) in source.files.iter().enumerate().skip(first) {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let raw = std::fs::read(file).context(|| format!("reading {}", file.display()))?;
                    let modified = std::fs::metadata(file)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64);
                    bytes += raw.len();
                    messages.push(Incoming { raw, hints: DateHints { envelope: None, file: modified }, origin: file.display().to_string() });
                    if messages.len() >= options.batch_messages || bytes >= options.batch_bytes {
                        let next = if i + 1 >= source.files.len() { (index + 1, 0) } else { (index, i + 1) };
                        if !send(std::mem::take(&mut messages), next) {
                            return Ok(());
                        }
                        bytes = 0;
                    }
                }
                if !messages.is_empty() && !send(messages, (index + 1, 0)) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Parse stage, one message (runs on the rayon pool). `folder` is the folder's path (kept in the
/// pack record, so the library can be rebuilt from the packs), `folder_id` its row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    raw: Vec<u8>,
    hints: DateHints,
    origin: String,
    folder: &str,
    folder_id: i64,
    server_id: Option<String>,
    compression: &Compression,
    keep_raw: bool,
) -> Result<Prepared> {
    let parsed = parse::parse(&raw, hints, now());
    let meta = serde_json::to_vec(&RecordMeta { mid: &parsed.message_id, date: parsed.date, ds: parsed.date_source.code(), folder })?;
    let record = pack::encode(&raw, parsed.hash, meta, compression)?;
    Ok(Prepared { parsed, record, origin, sample: keep_raw.then_some(raw), folder: folder_id, server_id, derived: false, originals: Vec::new() })
}

/// Dates closer than this are the same message's (time zones, a missing Date header read
/// from the mbox envelope).
const SAME_MESSAGE_DATES: i64 = 36 * 3600;
/// Dates before 1980 are broken (old clients), so they don't count against a match.
const PLAUSIBLE_DATE: i64 = 315_532_800;

/// A subject for comparing copies: lowercase, without "Re:" / "Fwd:" prefixes, spaces collapsed.
pub(crate) fn subject_key(subject: &str) -> String {
    let mut s = subject.trim().to_lowercase();
    loop {
        let before = s.len();
        for prefix in ["re:", "fwd:", "fw:", "aw:", "sv:", "wg:", "re :", "fw :"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start().to_string();
            }
        }
        if s.len() == before {
            break;
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dates_agree(a: i64, b: i64) -> bool {
    a < PLAUSIBLE_DATE || b < PLAUSIBLE_DATE || (a - b).abs() <= SAME_MESSAGE_DATES
}

fn senders_agree(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a.eq_ignore_ascii_case(b)
}

fn first_sender(m: &ParsedMessage) -> &str {
    m.from.first().map(|a| a.address.as_str()).unwrap_or("")
}

/// Two parsed messages are copies of one message.
fn alike(a: &ParsedMessage, b: &ParsedMessage) -> bool {
    a.message_id == b.message_id
        && a.message_id_made == b.message_id_made
        && dates_agree(a.date, b.date)
        && senders_agree(first_sender(a), first_sender(b))
        && subject_key(&a.subject) == subject_key(&b.subject)
}

/// A stored message this one is another copy of: the same Message-ID, and date, sender and
/// subject agree (Message-IDs do collide: broken old clients, mailing-list copies).
pub(crate) fn same_message(conn: &rusqlite::Connection, m: &ParsedMessage) -> Result<Option<i64>> {
    if m.message_id.is_empty() {
        return Ok(None);
    }
    let mut candidates = conn.prepare_cached("SELECT id, date, subject FROM messages WHERE message_id = ?1 AND message_id_made = ?2 LIMIT 20")?;
    let rows: Vec<(i64, i64, String)> =
        candidates.query_map(params![m.message_id, m.message_id_made], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut sender = conn.prepare_cached(
        "SELECT a.address FROM message_addresses ma JOIN addresses a ON a.id = ma.address WHERE ma.message = ?1 AND ma.role = 0 ORDER BY ma.position LIMIT 1",
    )?;
    let key = subject_key(&m.subject);
    for (id, date, subject) in rows {
        if !dates_agree(date, m.date) || subject_key(&subject) != key {
            continue;
        }
        let stored: String = sender.query_row(params![id], |r| r.get(0)).optional()?.unwrap_or_default();
        if senders_agree(&stored, first_sender(m)) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub(crate) struct WriterState {
    import_id: i64,
    checkpoint: Checkpoint,
    addresses: HashMap<String, i64>,
    problems: BTreeMap<String, ProblemSummary>,
}

impl WriterState {
    pub(crate) fn new(import_id: i64, checkpoint: Checkpoint) -> WriterState {
        WriterState { import_id, checkpoint, addresses: HashMap::new(), problems: BTreeMap::new() }
    }

    pub(crate) fn counts(&self) -> &Counts {
        &self.checkpoint.counts
    }

    fn store_batch(&mut self, library: &mut Library, batch: PreparedBatch) -> Result<()> {
        self.checkpoint.done += batch.items.len() as u64;
        self.checkpoint.source = batch.next.0;
        self.checkpoint.offset = batch.next.1;
        self.store(library, &batch.items)
    }

    /// Stores messages in one transaction (each in its own folder), with the checkpoint.
    pub(crate) fn store(&mut self, library: &mut Library, items: &[Prepared]) -> Result<()> {
        // Decide what each message is, and append the new ones' pack records first: once
        // flushed, the database may point at them.
        let mut plans = Vec::with_capacity(items.len());
        let mut originals: Vec<Vec<(usize, pack::Location)>> = Vec::with_capacity(items.len());
        {
            let mut here_hash: HashMap<[u8; 32], usize> = HashMap::new();
            let mut here_id: HashMap<&str, Vec<usize>> = HashMap::new();
            let mut seen_originals = std::collections::HashSet::new();
            for (i, item) in items.iter().enumerate() {
                let m = &item.parsed;
                // a copy's own bytes are kept, unless Eudora made them (its originals are)
                let keep = |library: &Library| -> Result<Option<pack::Location>> {
                    if item.derived { Ok(None) } else { Ok(Some(library.packs.append(&item.record)?)) }
                };
                let plan = if let Some(id) = stored_bytes(&library.conn, &m.hash)? {
                    Plan::Same(id)
                } else if let Some(&j) = here_hash.get(&m.hash) {
                    Plan::SameAsItem(j)
                } else if let Some(id) = same_message(&library.conn, m)? {
                    here_hash.insert(m.hash, i);
                    Plan::Copy(id, keep(library)?)
                } else if let Some(j) = here_id.get(m.message_id.as_str()).and_then(|js| js.iter().copied().find(|&j| alike(&items[j].parsed, m))) {
                    here_hash.insert(m.hash, i);
                    Plan::CopyOfItem(j, keep(library)?)
                } else {
                    here_hash.insert(m.hash, i);
                    here_id.entry(m.message_id.as_str()).or_default().push(i);
                    Plan::New(library.packs.append(&item.record)?)
                };
                plans.push(plan);
                originals.push(append_originals(library, item, &mut seen_originals)?);
            }
        }
        library.packs.flush(true)?;

        let added = now();
        let tx = library.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut ids: Vec<i64> = vec![0; items.len()];
        for (i, ((item, plan), new_originals)) in items.iter().zip(plans).zip(originals).enumerate() {
            let m = &item.parsed;
            self.checkpoint.counts.seen += 1;
            self.checkpoint.counts.bytes += m.raw_len;
            let server_id = item.server_id.as_deref();
            let (id, copy, kept) = match plan {
                Plan::Same(id) => (id, false, None),
                Plan::SameAsItem(j) => (ids[j], false, None),
                Plan::Copy(id, kept) => (id, true, kept),
                Plan::CopyOfItem(j, kept) => (ids[j], true, kept),
                Plan::New(location) => {
                    let id = self.insert_message(&tx, m, location, item.folder, server_id, added)?;
                    ids[i] = id;
                    insert_copy(&tx, id, Some(item.folder), server_id, "mime", &m.hash, m.raw_len, location, added)?;
                    for (k, at) in new_originals {
                        let o = &item.originals[k];
                        insert_copy(&tx, id, Some(item.folder), server_id, o.format, &o.hash, o.size, at, added)?;
                    }
                    self.note_problems(&tx, m, id, &item.origin)?;
                    let text = index_text(m);
                    library.index.add(&text.as_doc(id, m.date, m.raw_len, m.attached() as u64, &[item.folder]))?;
                    note_server_item(&tx, item, id)?;
                    continue;
                }
            };
            ids[i] = id;
            // this source's own bytes, and what it was made from, as copies of the message
            if let Some(at) = kept {
                insert_copy(&tx, id, Some(item.folder), server_id, "mime", &m.hash, m.raw_len, at, added)?;
            }
            for (k, at) in new_originals {
                let o = &item.originals[k];
                insert_copy(&tx, id, Some(item.folder), server_id, o.format, &o.hash, o.size, at, added)?;
            }
            // moved away from this folder in Eudora, the server not caught up yet: the server's
            // copy here is the same item on its way out, not a message to put back (2026-10-18:
            // mail the rules had filed showed up in the Inbox again as well)
            let moved_away: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM pending_ops WHERE kind = 'move' AND json_extract(payload, '$.message') = ?1 AND json_extract(payload, '$.from') = ?2)",
                params![id, item.folder],
                |r| r.get(0),
            )?;
            if moved_away {
                self.checkpoint.counts.duplicates += 1;
                continue;
            }
            let added_here = tx
                .prepare_cached("INSERT OR IGNORE INTO locations (message, folder, date, server_id) SELECT id, ?2, date, ?3 FROM messages WHERE id = ?1")?
                .execute(params![id, item.folder, item.server_id])?;
            if copy {
                self.checkpoint.counts.merged += 1;
            } else if added_here == 1 {
                self.checkpoint.counts.new_locations += 1;
            } else {
                self.checkpoint.counts.duplicates += 1;
            }
            if added_here == 1 {
                // the index entry lists the message's folders: rewrite it
                tx.prepare_cached("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)")?.execute(params![id])?;
                let folders = folders_of(&tx, id)?;
                let date: i64 = tx.query_row("SELECT date FROM messages WHERE id = ?1", params![id], |r| r.get(0))?;
                let text = index_text(m);
                library.index.replace(&text.as_doc(id, date, m.raw_len, m.attached() as u64, &folders))?;
            }
            note_server_item(&tx, item, id)?;
        }
        tx.execute(
            "UPDATE imports SET checkpoint = ?2 WHERE id = ?1",
            params![self.import_id, serde_json::to_string(&self.checkpoint)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn insert_message(
        &mut self,
        tx: &Transaction,
        m: &ParsedMessage,
        location: pack::Location,
        folder: i64,
        server_id: Option<&str>,
        added: i64,
    ) -> Result<i64> {
        tx.prepare_cached(
            "INSERT INTO messages (content_hash, message_id, message_id_made, date, date_source, from_display, subject, snippet,
                size, attachment_count, list_id, parse_status, pack, pack_offset, pack_len, added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?
        .execute(params![
            &m.hash[..],
            m.message_id,
            m.message_id_made,
            m.date,
            m.date_source.code(),
            m.from_display(),
            m.subject,
            m.snippet,
            m.raw_len as i64,
            m.attached() as i64,
            m.list_id,
            m.status.code(),
            location.pack as i64,
            location.offset as i64,
            location.len as i64,
            added,
        ])?;
        let id = tx.last_insert_rowid();
        match m.status {
            ParseStatus::Ok => {}
            ParseStatus::Fixed => self.checkpoint.counts.fixed += 1,
            ParseStatus::Failed => self.checkpoint.counts.unreadable += 1,
        }
        self.checkpoint.counts.imported += 1;
        tx.prepare_cached("INSERT INTO locations (message, folder, date, server_id) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![id, folder, m.date, server_id])?;
        self.insert_details(tx, id, m)?;
        Ok(id)
    }

    /// Addresses, references and attachments of a message.
    fn insert_details(&mut self, tx: &Transaction, id: i64, m: &ParsedMessage) -> Result<()> {
        for (role, list) in [(ROLE_FROM, &m.from), (ROLE_TO, &m.to), (ROLE_CC, &m.cc), (ROLE_BCC, &m.bcc)] {
            for (position, a) in list.iter().enumerate() {
                let address = self.address_id(tx, &a.address)?;
                tx.prepare_cached("INSERT OR IGNORE INTO message_addresses (message, role, position, address, name) VALUES (?1, ?2, ?3, ?4, ?5)")?
                    .execute(params![id, role, position as i64, address, a.name])?;
            }
        }
        for (kind, list) in [(REF_IN_REPLY_TO, &m.in_reply_to), (REF_REFERENCES, &m.references)] {
            for (position, r) in list.iter().enumerate() {
                tx.prepare_cached("INSERT OR IGNORE INTO message_refs (message, kind, position, ref) VALUES (?1, ?2, ?3, ?4)")?
                    .execute(params![id, kind, position as i64, r])?;
            }
        }
        for a in &m.attachments {
            let attachment: i64 = tx
                .prepare_cached(
                    "INSERT INTO attachments (hash, size, mime) VALUES (?1, ?2, ?3)
                     ON CONFLICT (hash) DO UPDATE SET size = excluded.size RETURNING id",
                )?
                .query_row(params![&a.hash[..], a.size as i64, a.mime], |r| r.get(0))?;
            tx.prepare_cached("INSERT OR IGNORE INTO attachment_refs (message, part, attachment, name, kind) VALUES (?1, ?2, ?3, ?4, ?5)")?
                .execute(params![id, a.part, attachment, a.name, a.kind.code()])?;
        }
        Ok(())
    }

    /// Keeps sources' own bytes for messages already stored (fetched again: earlier only where
    /// they were was recorded). Each item: the message it belongs to, and the copy as fetched
    /// (its bytes kept unless Eudora made them; its originals kept). Returns the copies added.
    pub(crate) fn add_copies(&mut self, library: &mut Library, items: &[(i64, Prepared)]) -> Result<u64> {
        let mut seen = std::collections::HashSet::new();
        let mut plans = Vec::with_capacity(items.len());
        for (_, item) in items {
            let hash = item.parsed.hash;
            let main = if !item.derived && !seen.contains(&hash) && stored_bytes(&library.conn, &hash)?.is_none() {
                seen.insert(hash);
                Some(library.packs.append(&item.record)?)
            } else {
                None
            };
            plans.push((main, append_originals(library, item, &mut seen)?));
        }
        library.packs.flush(true)?;
        let added = now();
        let tx = library.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut n = 0;
        for ((message, item), (main, originals)) in items.iter().zip(plans) {
            let server_id = item.server_id.as_deref();
            if let Some(at) = main {
                insert_copy(&tx, *message, Some(item.folder), server_id, "mime", &item.parsed.hash, item.parsed.raw_len, at, added)?;
                n += 1;
            }
            for (k, at) in originals {
                let o = &item.originals[k];
                insert_copy(&tx, *message, Some(item.folder), server_id, o.format, &o.hash, o.size, at, added)?;
                n += 1;
            }
        }
        tx.commit()?;
        self.checkpoint.counts.copies_kept += n;
        Ok(n)
    }

    /// Makes copy `copy` (one of message `message`'s own copies) the bytes the message is shown
    /// and searched from: the more original copy of a duplicate (`Library::prefer_originals`).
    /// The message keeps its id, folders, server items, Message-ID and date; what's read from
    /// the bytes (subject, preview, addresses, attachments) is read again from the copy, and
    /// the search entry is redone. The bytes it had stay as a copy.
    pub(crate) fn swap_to_copy(&mut self, library: &mut Library, message: i64, copy: i64) -> Result<()> {
        let (hash, location): (Vec<u8>, pack::Location) = library.conn.query_row(
            "SELECT content_hash, pack, pack_offset, pack_len FROM copies WHERE id = ?1 AND message = ?2",
            params![copy, message],
            |r| Ok((r.get(0)?, Library::location(r, 1)?)),
        )?;
        let raw = library.packs.read(location)?.raw;
        let m = parse::parse(&raw, DateHints::default(), now());
        let tx = library.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for table in ["message_addresses", "message_refs", "attachment_refs"] {
            tx.execute(&format!("DELETE FROM {table} WHERE message = ?1"), params![message])?;
        }
        tx.execute(
            "UPDATE messages SET content_hash = ?2, from_display = ?3, subject = ?4, snippet = ?5, size = ?6, attachment_count = ?7, list_id = ?8,
                parse_status = ?9, pack = ?10, pack_offset = ?11, pack_len = ?12 WHERE id = ?1",
            params![
                message,
                hash,
                m.from_display(),
                m.subject,
                m.snippet,
                m.raw_len as i64,
                m.attached() as i64,
                m.list_id,
                m.status.code(),
                location.pack as i64,
                location.offset as i64,
                location.len as i64
            ],
        )?;
        self.insert_details(&tx, message, &m)?;
        tx.execute("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)", params![message])?;
        let folders = folders_of(&tx, message)?;
        let date: i64 = tx.query_row("SELECT date FROM messages WHERE id = ?1", params![message], |r| r.get(0))?;
        tx.commit()?;
        library.index.replace(&index_text(&m).as_doc(message, date, m.raw_len, m.attached() as u64, &folders))?;
        Ok(())
    }

    /// Puts another copy in place of stored message `old` (e.g. an archive item that a later
    /// converter could read). When the copy turns out to be a message the library has already
    /// (same bytes, or same Message-ID and the checks agree), `old`'s folders and server items
    /// move to that message and `old` is removed; otherwise `old` is rewritten from the copy.
    /// Returns the message that now stands for it.
    pub(crate) fn replace_message(&mut self, library: &mut Library, old: i64, item: &Prepared) -> Result<i64> {
        let m = &item.parsed;
        let target = match stored_bytes(&library.conn, &m.hash)?.filter(|&id| id != old) {
            Some(id) => Some(id),
            None => same_message(&library.conn, m)?.filter(|&id| id != old),
        };
        let location = if target.is_none() { Some(library.packs.append(&item.record)?) } else { None };
        let new_originals = append_originals(library, item, &mut std::collections::HashSet::new())?;
        library.packs.flush(true)?;
        // where the old bytes came from: the copies made now come from there too
        let (from_folder, from_server): (Option<i64>, Option<String>) = library
            .conn
            .query_row(
                "SELECT folder, server_id FROM copies WHERE message = ?1 ORDER BY id LIMIT 1",
                params![old],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((Some(item.folder), item.server_id.clone()));
        let added = now();
        let tx = library.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // the old bytes were Eudora's own note (holding the export), not an original
        tx.execute("DELETE FROM copies WHERE message = ?1", params![old])?;
        let id = match (target, location) {
            (Some(target), _) => {
                tx.execute("UPDATE OR IGNORE locations SET message = ?2 WHERE message = ?1", params![old, target])?;
                tx.execute("UPDATE OR IGNORE server_items SET message = ?2 WHERE message = ?1", params![old, target])?;
                // (rows that couldn't move: the target was already in that folder)
                for table in ["locations", "server_items", "message_addresses", "message_refs", "attachment_refs", "index_pending"] {
                    tx.execute(&format!("DELETE FROM {table} WHERE message = ?1"), params![old])?;
                }
                tx.execute("UPDATE import_problems SET message = ?2 WHERE message = ?1", params![old, target])?;
                tx.execute("DELETE FROM messages WHERE id = ?1", params![old])?;
                library.index.delete(old as u64)?;
                self.checkpoint.counts.merged += 1;
                target
            }
            (None, Some(location)) => {
                for table in ["message_addresses", "message_refs", "attachment_refs"] {
                    tx.execute(&format!("DELETE FROM {table} WHERE message = ?1"), params![old])?;
                }
                tx.execute(
                    "UPDATE messages SET content_hash = ?2, message_id = ?3, message_id_made = ?4, date = ?5, date_source = ?6, from_display = ?7,
                        subject = ?8, snippet = ?9, size = ?10, attachment_count = ?11, list_id = ?12, parse_status = ?13, pack = ?14,
                        pack_offset = ?15, pack_len = ?16 WHERE id = ?1",
                    params![
                        old,
                        &m.hash[..],
                        m.message_id,
                        m.message_id_made,
                        m.date,
                        m.date_source.code(),
                        m.from_display(),
                        m.subject,
                        m.snippet,
                        m.raw_len as i64,
                        m.attached() as i64,
                        m.list_id,
                        m.status.code(),
                        location.pack as i64,
                        location.offset as i64,
                        location.len as i64
                    ],
                )?;
                tx.execute("UPDATE locations SET date = ?2 WHERE message = ?1", params![old, m.date])?;
                self.insert_details(&tx, old, m)?;
                insert_copy(&tx, old, from_folder, from_server.as_deref(), "mime", &m.hash, m.raw_len, location, added)?;
                self.checkpoint.counts.imported += 1;
                old
            }
            (None, None) => unreachable!("a copy without a match always gets a pack record"),
        };
        for (k, at) in new_originals {
            let o = &item.originals[k];
            insert_copy(&tx, id, from_folder, from_server.as_deref(), o.format, &o.hash, o.size, at, added)?;
        }
        let folders = folders_of(&tx, id)?;
        let date: i64 = tx.query_row("SELECT date FROM messages WHERE id = ?1", params![id], |r| r.get(0))?;
        tx.commit()?;
        let text = index_text(m);
        library.index.replace(&text.as_doc(id, date, m.raw_len, m.attached() as u64, &folders))?;
        Ok(id)
    }

    fn address_id(&mut self, tx: &Transaction, address: &str) -> Result<i64> {
        let key = address.to_lowercase();
        if let Some(id) = self.addresses.get(&key) {
            return Ok(*id);
        }
        let id: i64 = tx
            .prepare_cached("INSERT INTO addresses (address) VALUES (?1) ON CONFLICT (address) DO UPDATE SET address = excluded.address RETURNING id")?
            .query_row(params![key], |r| r.get(0))?;
        if self.addresses.len() > 2_000_000 {
            self.addresses.clear();
        }
        self.addresses.insert(key, id);
        Ok(id)
    }

    fn note_problems(&mut self, tx: &Transaction, m: &ParsedMessage, id: i64, origin: &str) -> Result<()> {
        for p in &m.problems {
            tx.prepare_cached("INSERT INTO import_problems (import, message, source, kind, detail) VALUES (?1, ?2, ?3, ?4, ?5)")?
                .execute(params![self.import_id, id, origin, p.kind, p.detail])?;
            let entry = self
                .problems
                .entry(p.kind.to_string())
                .or_insert_with(|| ProblemSummary { count: 0, example: format!("{} ({origin})", p.detail) });
            entry.count += 1;
        }
        Ok(())
    }
}

fn note_server_item(tx: &Transaction, item: &Prepared, message: i64) -> Result<()> {
    if let Some(server_id) = &item.server_id {
        tx.prepare_cached("INSERT OR IGNORE INTO server_items (folder, server_id, message) VALUES (?1, ?2, ?3)")?
            .execute(params![item.folder, server_id, message])?;
    }
    Ok(())
}

fn folders_of(tx: &Transaction, id: i64) -> Result<Vec<i64>> {
    let mut stmt = tx.prepare_cached("SELECT folder FROM locations WHERE message = ?1 ORDER BY folder")?;
    Ok(stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    fn message(received: &str, id: &str, subject: &str, date: &str) -> String {
        format!("Received: {received}\r\nFrom: Ann <ann@example.com>\r\nTo: bob@example.com\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{id}>\r\n\r\nHello there.\r\n")
    }

    fn import_files(library: &mut Library, dir: &std::path::Path, files: &[(&str, String)], into: &str) -> Counts {
        let mut paths = Vec::new();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            paths.push(path);
        }
        let options = ImportOptions { destination: Some(into.into()), ..Default::default() };
        import(library, &paths, &options, &Silent).unwrap().counts
    }

    #[test]
    fn copies_with_the_same_message_id_are_merged() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let date = "Fri, 31 Dec 1999 20:38:46 -0500";
        let first = import_files(&mut library, &src, &[("a.eml", message("from a by b", "1@x", "Hello", date))], "Old");
        assert_eq!(first.imported, 1);

        // Another copy (other Received lines, "Re:" doesn't matter, 3 hours off): merged.
        // Same Message-ID but another subject (a collision): kept apart.
        let second = import_files(
            &mut library,
            &src,
            &[
                ("b.eml", message("from c by d", "1@x", "RE: Hello", "Sat, 1 Jan 2000 01:38:46 +0000")),
                ("c.eml", message("from c by d", "1@x", "Something else", date)),
            ],
            "Server",
        );
        assert_eq!((second.imported, second.merged), (1, 1), "{second:?}");
        let locations: i64 = library.conn.query_row("SELECT COUNT(*) FROM locations", [], |r| r.get(0)).unwrap();
        assert_eq!(locations, 3);
        let messages: i64 = library.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(messages, 2);
        // every copy keeps its own bytes: the first copy and the merged one, byte for byte
        let first: i64 = library.conn.query_row("SELECT id FROM messages WHERE subject = 'Hello'", [], |r| r.get(0)).unwrap();
        let copies = library.copies_of(first).unwrap();
        assert_eq!(copies.len(), 2, "{copies:?}");
        assert_eq!(copies[1].folder.as_deref(), Some("Server"));
        let kept = library.copy_bytes(copies[1].id).unwrap();
        assert_eq!(kept, message("from c by d", "1@x", "RE: Hello", "Sat, 1 Jan 2000 01:38:46 +0000").into_bytes());
        // the merged copy is found in its new folder
        let (result, _) = library.search("hello", 10, crate::SortOrder::Newest).unwrap();
        assert_eq!(result.total, 2);
    }

    /// Mailboxes left out of search: their messages are still indexed, but searches leave them
    /// out (and say how many), unless asked to look everywhere; a level's switch applies to what's
    /// under it, and a switch below puts one back.
    #[test]
    fn searches_leave_out_the_mailboxes_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |id: &str, mailer: &str| format!("From: bob@x.com\r\nTo: me@x.com\r\nSubject: note {id}\r\nMessage-ID: <{id}@x>\r\nX-Mailer: {mailer}\r\nDate: Fri, 1 Dec 1999 20:38:46 -0500\r\n\r\nhello\r\n");
        import_files(&mut library, &src, &[("a.eml", mail("1", "Outlook")), ("b.eml", mail("2", "Outlook"))], "Inbox");
        import_files(&mut library, &src, &[("c.eml", mail("3", "Eudora"))], "Lists");
        let found = |l: &Library, q: &str, everywhere: bool| {
            let (r, _) = l.search_scoped(q, 10, crate::SortOrder::Newest, everywhere).unwrap();
            (r.total, r.hidden)
        };
        assert_eq!(found(&library, "hello", false), (3, 0));
        let lists = library.find_folder("Lists").unwrap().unwrap();
        let state = library.set_search_scope(&format!("folder:{lists}"), Some(false)).unwrap();
        assert_eq!(state.excluded, vec![lists]);
        assert_eq!(found(&library, "hello", false), (2, 1), "Lists left out, and counted");
        assert_eq!(found(&library, "hello", true), (3, 0), "Include all mailboxes");
        // new mail is taken into account without working everything out again: one more in Lists
        // (left out), and a copy of a Lists message arriving in the Inbox (shown again)
        import_files(&mut library, &src, &[("d.eml", mail("4", "Eudora"))], "Lists");
        assert_eq!(found(&library, "hello", false), (2, 2));
        import_files(&mut library, &src, &[("e.eml", mail("4", "Eudora"))], "Inbox");
        assert_eq!(found(&library, "hello", false), (3, 1), "a message also in a searched folder shows");
        // header searches too
        while library.update_header_index(10).unwrap() > 0 {}
        library.commit_header_index().unwrap();
        assert_eq!(found(&library, "x-mailer:eudora", false), (1, 1), "the Inbox copy of 4, not 3");
        assert_eq!(found(&library, "x-mailer:outlook", false), (2, 0));
        // the whole account off, then the Inbox put back
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [lists], |r| r.get(0)).unwrap();
        library.set_search_scope(&format!("folder:{lists}"), None).unwrap();
        library.set_search_scope(&format!("account:{account}"), Some(false)).unwrap();
        assert_eq!(found(&library, "hello", false), (0, 4));
        let inbox = library.find_folder("Inbox").unwrap().unwrap();
        library.set_search_scope(&format!("folder:{inbox}"), Some(true)).unwrap();
        assert_eq!(found(&library, "hello", false), (3, 1), "one folder searched again");
        // a switch that says what the level above says is dropped
        library.set_search_scope(&format!("account:{account}"), Some(true)).unwrap();
        assert!(!library.search_scope().unwrap().contains_key(&format!("account:{account}")));
        assert!(library.set_search_scope("folder:x", Some(true)).is_err());
    }

    #[test]
    fn all_mail_can_hide_what_i_sent_and_any_header_can_be_searched() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |from: &str, id: &str, mailer: &str, day: u32| {
            format!("From: {from}\r\nTo: other@x.com\r\nSubject: note {id}\r\nMessage-ID: <{id}@x>\r\nX-Mailer: {mailer}\r\nDate: Fri, {day} Dec 1999 20:38:46 -0500\r\n\r\nhello\r\n")
        };
        import_files(&mut library, &src, &[("in.eml", mail("Bob <bob@x.com>", "1", "Outlook Express 6", 1)), ("in2.eml", mail("carol@x.com", "2", "Thunderbird", 2))], "Inbox");
        import_files(&mut library, &src, &[("out.eml", mail("Me <ME@example.com>", "3", "Eudora 7", 3))], "Out");
        let mine = library.address_ids(&["me@example.com".into(), "nobody@nowhere".into()]).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(library.count_all_except_from(&mine).unwrap(), 2);
        assert_eq!(library.count_all_except_from(&[]).unwrap(), 3);
        let shown: Vec<String> = library.list_all_except_from(&mine, None, 10).unwrap().into_iter().map(|r| r.subject).collect();
        assert_eq!(shown, vec!["note 2", "note 1"]);
        let first = library.cursor_all_except_from(&mine, 1).unwrap().unwrap();
        assert_eq!(library.list_all_except_from(&mine, Some(first), 10).unwrap().len(), 0, "the second row was the last");

        // header search: the background step indexes new mail first, then older
        while library.update_header_index(2).unwrap() > 0 {}
        library.commit_header_index().unwrap();
        let (r, rows) = library.search("x-mailer:outlook", 10, crate::SortOrder::Newest).unwrap();
        assert_eq!((r.total, rows[0].subject.as_str()), (1, "note 1"));
        let (r, _) = library.search("hello -x-mailer:eudora", 10, crate::SortOrder::Newest).unwrap();
        assert_eq!(r.total, 2, "words and header parts together");
        let coverage = library.header_coverage().unwrap();
        assert_eq!((coverage.covered, coverage.total), (3, 3));
        let values = library.header_values(&[rows[0].id], &["X-Mailer".into(), "Message-ID:".into()]).unwrap();
        assert_eq!(values, vec![vec!["Outlook Express 6".to_string(), "<1@x>".to_string()]]);

        // Outlook's search words
        let found = |q: &str| library.search(q, 10, crate::SortOrder::Newest).unwrap().0.total;
        assert_eq!(found("hasattachment:no"), 3);
        assert_eq!(found("hasattachment:yes"), 0);
        assert_eq!(found("received:1999"), 3);
        assert_eq!(found("received:12/2/1999"), 1);
        assert_eq!(found("received:12/1/1999..12/2/1999"), 2);
        assert_eq!(found("sent:2020"), 0);
        assert_eq!(found("size:tiny"), 3);
        assert_eq!(found("size:>5MB"), 0);
        assert_eq!(found("folder:out"), 1);
        assert_eq!(found("in:inbox subject:note"), 2);
        assert_eq!(found("-folder:inbox"), 1);
        assert_eq!(found("participants:bob"), 1);
        assert_eq!(found("note AND NOT carol"), 2);
        assert_eq!(found("folder:nowhere"), 0);
        // From / To / recipients / sender, by name or address
        assert_eq!(found("from:bob@x.com"), 1);
        assert_eq!(found("from:carol"), 1);
        assert_eq!(found("sender:bob"), 1);
        assert_eq!(found("to:other@x.com"), 3);
        assert_eq!(found("recipient:other"), 3);
        assert_eq!(found("from:bob to:other@x.com"), 1);
        assert_eq!(found("recipient:nobody"), 0);
        // a search still being typed: repaired as far as it can be, with notes, never an error
        for q in ["from:", "subject:", "\"unclosed", "(note", "note \"hello", "note AND", "date:[2024 TO", "a ) b"] {
            let (r, _) = library.search(q, 10, crate::SortOrder::Newest).unwrap_or_else(|e| panic!("{q}: {e}"));
            assert!(!r.notes.is_empty(), "{q} has a note");
        }
        assert_eq!(library.search("note \"hello", 10, crate::SortOrder::Newest).unwrap().0.total, 3, "repaired, it still searches");
        assert!(library.search("note", 10, crate::SortOrder::Newest).unwrap().0.notes.is_empty());
        // slips get a suggestion that finds what was meant
        let (r, _) = library.search("form:bob", 10, crate::SortOrder::Newest).unwrap();
        assert_eq!((r.total, r.suggestions[0].query.as_str()), (0, "from:bob"));
        assert_eq!(found(&r.suggestions[0].query), 1);

        // a whole list's ids, for Select All
        assert_eq!(library.list_ids(&[], &[]).unwrap().len(), 3);
        assert_eq!(library.list_ids(&[], &mine).unwrap().len(), 2);
        let inbox = library.find_folder("Inbox").unwrap().unwrap();
        assert!(library.list_ids(&[inbox], &mine).unwrap().iter().all(|(_, f)| *f == inbox));
    }

    #[test]
    fn a_stored_message_can_be_replaced_by_a_better_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let date = "Fri, 31 Dec 1999 20:38:46 -0500";
        import_files(&mut library, &src, &[("a.eml", message("a", "1@x", "Hello", date))], "Old");
        import_files(&mut library, &src, &[("n1.eml", message("n", "n1@note", "Note one", date)), ("n2.eml", message("n", "n2@note", "Note two", date))], "Archive");
        let id = |mid: &str| -> i64 { library.conn.query_row("SELECT id FROM messages WHERE message_id = ?1", params![mid], |r| r.get(0)).unwrap() };
        let (original, note1, note2) = (id("1@x"), id("n1@note"), id("n2@note"));
        let archive = library.find_folder("Archive").unwrap().unwrap();
        let mut writer = WriterState::new(0, Checkpoint::default());
        let compression = library.compression();

        // note 1 turns out to be a copy of the message in "Old": it joins it, the note goes,
        // and Exchange's export it was made from is kept with the message
        let copy = prepare(message("z", "1@x", "Hello", date).into_bytes(), DateHints::default(), "t".into(), "Archive", archive, None, &compression, false)
            .unwrap()
            .made_from(super::original(b"EXCHANGE EXPORT BYTES", "exchange-export", "Archive", &compression).unwrap());
        assert_eq!(writer.replace_message(&mut library, note1, &copy).unwrap(), original);
        assert_eq!(library.folder_paths(original).unwrap(), vec!["Archive".to_string(), "Old".to_string()]);
        let copies = library.copies_of(original).unwrap();
        assert_eq!(copies.iter().map(|c| c.format.as_str()).collect::<Vec<_>>(), vec!["mime", "exchange-export"], "{copies:?}");
        assert_eq!(library.copy_bytes(copies[1].id).unwrap(), b"EXCHANGE EXPORT BYTES");
        assert!(library.copies_of(note1).unwrap().is_empty(), "the note isn't an original");
        let gone: i64 = library.conn.query_row("SELECT COUNT(*) FROM messages WHERE id = ?1", params![note1], |r| r.get(0)).unwrap();
        assert_eq!(gone, 0);

        // note 2 becomes a message of its own, in place
        let fresh = prepare(message("z", "2@x", "Real subject", date).into_bytes(), DateHints::default(), "t".into(), "Archive", archive, None, &compression, false).unwrap();
        assert_eq!(writer.replace_message(&mut library, note2, &fresh).unwrap(), note2);
        commit_index(&library).unwrap();
        assert_eq!(library.rows(&[note2]).unwrap()[0].subject, "Real subject");
        let (found, _) = library.search("subject:real", 10, crate::SortOrder::Newest).unwrap();
        assert_eq!(found.ids, vec![note2 as u64]);
        let (found, _) = library.search("subject:note", 10, crate::SortOrder::Newest).unwrap();
        assert_eq!(found.total, 0);
    }

    fn eudora_folder(dir: &std::path::Path, address: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("eudora.ini"),
            format!("[Settings]\r\nPOPAccount=pat@mail.example.com\r\nRealName=Pat\r\nReturnAddress={address}\r\nSavePasswordText=secret\r\n[Persona-Other]\r\nReturnAddress=other@example.net\r\n"),
        )
        .unwrap();
        std::fs::write(dir.join("In.mbx"), message("a", &format!("{address}-1@x"), "Hi", "Fri, 31 Dec 1999 20:38:46 -0500").replacen("Received", "From ???@??? Fri Dec 31 20:38:46 1999\r\nReceived", 1)).unwrap();
        std::fs::write(dir.join("In.toc"), b"\x00\x01").unwrap();
    }

    #[test]
    fn a_classic_eudora_folder_becomes_its_owners_account() {
        let dir = tempfile::tempdir().unwrap();
        let who = eudora_identity(&{
            eudora_folder(&dir.path().join("mail"), "pat@example.com");
            dir.path().join("mail")
        })
        .unwrap();
        assert_eq!(who, EudoraIdentity { address: "pat@example.com".into(), name: "Pat".into(), personalities: vec!["other@example.net".into()] });

        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        import(&mut library, &[dir.path().join("mail")], &ImportOptions::default(), &Silent).unwrap();
        let accounts = library.accounts().unwrap();
        let eudora = accounts.iter().find(|a| a.kind == "eudora").unwrap();
        assert_eq!((eudora.address.as_deref(), eudora.kind_name, eudora.messages), (Some("pat@example.com"), "classic Eudora", 1));
        assert!(eudora.source.as_deref().unwrap().contains("other@example.net"));
        // its folders are at the account's top, not under "Imports/…"
        assert_eq!(library.folders().unwrap().iter().map(|f| f.path.clone()).collect::<Vec<_>>(), vec!["In".to_string()]);

        // a second archive for the same address: the user is asked, then decides
        eudora_folder(&dir.path().join("mail2"), "pat@example.com");
        let again = import(&mut library, &[dir.path().join("mail2")], &ImportOptions::default(), &Silent);
        assert!(matches!(again, Err(Error::AccountExists { messages: 1, .. })), "{again:?}");
        let apart = ImportOptions { merge_account: Some(false), ..Default::default() };
        import(&mut library, &[dir.path().join("mail2")], &apart, &Silent).unwrap();
        assert_eq!(library.accounts().unwrap().iter().filter(|a| a.kind == "eudora").count(), 2);
        // the same archive again just continues (no question)
        import(&mut library, &[dir.path().join("mail")], &ImportOptions::default(), &Silent).unwrap();

        library.set_account_label(eudora.id, Some("Old Eudora")).unwrap();
        assert_eq!(library.accounts().unwrap()[0].shown(), "Old Eudora");
        library.set_account_label(eudora.id, Some(" ")).unwrap();
        assert_eq!(library.accounts().unwrap()[0].shown(), "pat@example.com");

        // the archive was really a personality's: its address changes, the old one becomes a personality
        assert_eq!(library.set_account_address(eudora.id, "Other@example.net").unwrap(), "pat@example.com");
        let a = library.accounts().unwrap().into_iter().find(|a| a.id == eudora.id).unwrap();
        assert_eq!(a.address.as_deref(), Some("other@example.net"));
        assert!(a.source.as_deref().unwrap().contains("pat@example.com") && !a.source.as_deref().unwrap().contains("other@example.net"));
    }

    #[test]
    fn transfer_delete_and_undo() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let date = "Fri, 31 Dec 1999 20:38:46 -0500";
        import_files(&mut library, &src, &[("a.eml", message("a", "1@x", "Hello", date))], "Inbox");
        import_files(&mut library, &src, &[("b.eml", message("b", "2@x", "Other", date))], "Projects");
        let inbox = library.find_folder("Imports/Inbox").unwrap().or(library.find_folder("Inbox").unwrap()).unwrap();
        let projects = library.find_folder("Imports/Projects").unwrap().or(library.find_folder("Projects").unwrap()).unwrap();
        let id: i64 = library.conn.query_row("SELECT id FROM messages WHERE subject = 'Hello'", [], |r| r.get(0)).unwrap();

        // Transfer within an account: it leaves the Inbox, it's in Projects
        assert_eq!(library.move_messages(&[id], inbox, projects).unwrap(), 1);
        assert_eq!(library.count(&[inbox]).unwrap(), 0);
        assert_eq!(library.count(&[projects]).unwrap(), 2);
        // (an imported account isn't on a server: nothing queued for one)
        assert!(library.pending_changes().unwrap().is_empty());

        // Delete = to the account's Trash (made when there's none); Undo = moving it back
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [projects], |r| r.get(0)).unwrap();
        let trash = library.trash_folder(account).unwrap();
        library.move_messages(&[id], projects, trash).unwrap();
        assert_eq!(library.folder_paths(id).unwrap(), vec!["Trash".to_string()]);
        library.move_messages(&[id], trash, projects).unwrap();
        assert_eq!(library.folder_paths(id).unwrap().len(), 1);

        // Copy To: in both
        library.copy_messages(&[id], inbox).unwrap();
        assert_eq!(library.folder_paths(id).unwrap().len(), 2);
    }

    #[test]
    fn address_completion_from_sent_and_received_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |from: &str, to: &str, id: &str| format!("From: {from}\r\nTo: {to}\r\nSubject: s\r\nMessage-ID: <{id}>\r\n\r\nx\r\n");
        import_files(
            &mut library,
            &src,
            &[
                ("1.eml", mail("Me <me@x.com>", "Ann Smith <ann@a.com>", "1@x")),
                ("2.eml", mail("Me <me@x.com>", "Ann Smith <ann@a.com>", "2@x")),
                ("3.eml", mail("Andy Brown <andy@b.com>", "me@x.com", "3@x")),
                ("4.eml", mail("Me <me@x.com>", "Bob Anders <bob@c.com>", "4@x")),
            ],
            "All",
        );
        assert_eq!(library.rebuild_contacts(&["me@x.com".to_string()]).unwrap(), 3);
        let names = |typed: &str, sent: bool, received: bool| -> Vec<String> {
            library.complete_address(typed, sent, received, 10).unwrap().into_iter().map(|(_, a)| a).collect()
        };
        // written to twice first; a word of the name matches; I'm never offered myself
        assert_eq!(names("an", true, true), vec!["ann@a.com", "bob@c.com", "andy@b.com"]);
        assert_eq!(names("an", true, false), vec!["ann@a.com", "bob@c.com"], "only people I wrote to");
        assert_eq!(names("an", false, true), vec!["andy@b.com"], "only people who wrote to me");
        assert!(names("me", true, true).is_empty());
    }

    #[test]
    fn subjects_compare_without_reply_prefixes() {
        assert_eq!(subject_key("Re: FW:  Budget   2026"), "budget 2026");
        assert_eq!(subject_key("Budget 2026"), "budget 2026");
        assert!(dates_agree(0, 946_690_726));
        assert!(!dates_agree(946_690_726, 946_690_726 + 3 * 86_400));
    }
}
