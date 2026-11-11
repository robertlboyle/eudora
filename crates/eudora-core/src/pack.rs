//! Append-only pack files holding the raw messages, byte for byte.
//!
//! Each message is a record compressed on its own (zstd, with a dictionary trained on the
//! library's own mail once there's enough of it), so any message can be read without its
//! neighbors. Packs are only ever appended to: a crash can at worst leave a half-written
//! record at the end of the last pack, which `start_writing` cuts off.
//!
//! Record layout (little endian):
//! `"EREC"` · body length u32 · codec u8 · dictionary id u32 · raw length u32 ·
//! BLAKE3 of the raw message \[32\] · meta length u32 · meta · payload.
//! The meta part is a little JSON (Message-ID, date, source) so the database can be
//! rebuilt from the packs alone.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use zstd::dict::{DecoderDictionary, EncoderDictionary};

use crate::error::{Error, IoContext, Result};

pub const PACK_HEADER: &[u8; 8] = b"EUDPACK1";
const RECORD_MAGIC: &[u8; 4] = b"EREC";
/// magic, body length, codec, dictionary id, raw length, hash, meta length.
const FIXED_HEADER: usize = 4 + 4 + 1 + 4 + 4 + 32 + 4;
pub const DEFAULT_PACK_LIMIT: u64 = 256 * 1024 * 1024;
/// Largest message we accept (also guards against reading garbage lengths).
pub const MAX_MESSAGE: u32 = 1 << 30;

const CODEC_STORED: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const CODEC_ZSTD_DICT: u8 = 2;

/// Where a record lives: pack number, byte offset, whole record length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub pack: u32,
    pub offset: u64,
    pub len: u32,
}

/// A record ready to be appended (compressed in parallel, written by the single writer).
pub struct EncodedRecord {
    codec: u8,
    dict_id: u32,
    raw_len: u32,
    pub hash: [u8; 32],
    meta: Vec<u8>,
    payload: Vec<u8>,
}

impl EncodedRecord {
    pub fn len(&self) -> usize {
        FIXED_HEADER + self.meta.len() + self.payload.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn stored_len(&self) -> usize {
        self.payload.len()
    }

    fn write_into(&self, out: &mut Vec<u8>) {
        let body_len = (self.len() - 8) as u32;
        out.extend_from_slice(RECORD_MAGIC);
        out.extend_from_slice(&body_len.to_le_bytes());
        out.push(self.codec);
        out.extend_from_slice(&self.dict_id.to_le_bytes());
        out.extend_from_slice(&self.raw_len.to_le_bytes());
        out.extend_from_slice(&self.hash);
        out.extend_from_slice(&(self.meta.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.meta);
        out.extend_from_slice(&self.payload);
    }
}

/// The fixed part of a record, read without its payload (scans, recovery, verify).
#[derive(Clone, Debug)]
pub struct RecordHeader {
    pub codec: u8,
    pub dict_id: u32,
    pub raw_len: u32,
    pub hash: [u8; 32],
    pub meta_len: u32,
    pub body_len: u32,
}

impl RecordHeader {
    fn parse(buf: &[u8; FIXED_HEADER]) -> Option<RecordHeader> {
        if &buf[0..4] != RECORD_MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        let header = RecordHeader {
            body_len: u32_at(4),
            codec: buf[8],
            dict_id: u32_at(9),
            raw_len: u32_at(13),
            hash: buf[17..49].try_into().unwrap(),
            meta_len: u32_at(49),
        };
        let fits = (header.body_len as usize) >= FIXED_HEADER - 8 + header.meta_len as usize;
        (header.codec <= CODEC_ZSTD_DICT && header.raw_len <= MAX_MESSAGE && fits).then_some(header)
    }

    pub fn record_len(&self) -> u64 {
        8 + self.body_len as u64
    }
}

/// A record read back: its meta JSON and the original message bytes (hash checked).
pub struct RawRecord {
    pub meta: Vec<u8>,
    pub raw: Vec<u8>,
    pub hash: [u8; 32],
}

/// A zstd dictionary, prepared once for compression and decompression. The prepared forms
/// are leaked on purpose: a library has one or two dictionaries for the life of the process,
/// and `'static` lets every worker thread keep its own compressor bound to them.
pub struct Dictionary {
    pub id: u32,
    enc: &'static EncoderDictionary<'static>,
    dec: &'static DecoderDictionary<'static>,
}

impl Dictionary {
    pub fn new(id: u32, bytes: &[u8], level: i32) -> Dictionary {
        Dictionary {
            id,
            enc: Box::leak(Box::new(EncoderDictionary::copy(bytes, level))),
            dec: Box::leak(Box::new(DecoderDictionary::copy(bytes))),
        }
    }
}

/// How new records are compressed: zstd level and, once trained, the dictionary.
#[derive(Clone)]
pub struct Compression {
    pub level: i32,
    pub dict: Option<Arc<Dictionary>>,
}

thread_local! {
    // Keyed by the dictionary's address (0 = none) and level: tests run several libraries
    // at once on the same rayon threads, and their dictionaries share id 1.
    static COMPRESSOR: RefCell<Option<(usize, i32, zstd::bulk::Compressor<'static>)>> =
        const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<(usize, zstd::bulk::Decompressor<'static>)>> =
        const { RefCell::new(None) };
}

fn zstd_compress(data: &[u8], level: i32, dict: Option<&'static EncoderDictionary<'static>>) -> io::Result<Vec<u8>> {
    let key = dict.map_or(0, |d| d as *const _ as usize);
    COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if !matches!(&*slot, Some((k, l, _)) if *k == key && *l == level) {
            let compressor = match dict {
                Some(d) => zstd::bulk::Compressor::with_prepared_dictionary(d)?,
                None => zstd::bulk::Compressor::new(level)?,
            };
            *slot = Some((key, level, compressor));
        }
        slot.as_mut().unwrap().2.compress(data)
    })
}

fn zstd_decompress(data: &[u8], raw_len: usize, dict: Option<&'static DecoderDictionary<'static>>) -> io::Result<Vec<u8>> {
    let key = dict.map_or(0, |d| d as *const _ as usize);
    DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if !matches!(&*slot, Some((k, _)) if *k == key) {
            let decompressor = match dict {
                Some(d) => zstd::bulk::Decompressor::with_prepared_dictionary(d)?,
                None => zstd::bulk::Decompressor::new()?,
            };
            *slot = Some((key, decompressor));
        }
        slot.as_mut().unwrap().1.decompress(data, raw_len)
    })
}

/// Compresses one message into a record. Runs on any thread.
pub fn encode(raw: &[u8], hash: [u8; 32], meta: Vec<u8>, compression: &Compression) -> Result<EncodedRecord> {
    let raw_len = u32::try_from(raw.len())
        .ok()
        .filter(|n| *n <= MAX_MESSAGE)
        .ok_or_else(|| Error::Other(format!("message too large ({} bytes)", raw.len())))?;
    let (codec, dict_id, payload) = match &compression.dict {
        Some(d) => (CODEC_ZSTD_DICT, d.id, zstd_compress(raw, compression.level, Some(d.enc))),
        None => (CODEC_ZSTD, 0, zstd_compress(raw, compression.level, None)),
    };
    let payload = payload.context(|| "compressing a message".into())?;
    let (codec, dict_id, payload) = if payload.len() >= raw.len() {
        (CODEC_STORED, 0, raw.to_vec())
    } else {
        (codec, dict_id, payload)
    };
    Ok(EncodedRecord { codec, dict_id, raw_len, hash, meta, payload })
}

/// Trains a dictionary from sample messages (zstd's recommended ~110 KB).
pub fn train_dictionary(samples: &[Vec<u8>]) -> Result<Vec<u8>> {
    zstd::dict::from_samples(samples, 112 * 1024).context(|| "training the compression dictionary".into())
}

struct WriterState {
    pack: u32,
    file: BufWriter<File>,
    pos: u64,
    buf: Vec<u8>,
}

/// The pack folder: one writer (the library's writer thread), any number of readers.
pub struct PackStore {
    dir: PathBuf,
    dict_dir: PathBuf,
    limit: u64,
    writer: Mutex<Option<WriterState>>,
    readers: Mutex<HashMap<u32, Arc<File>>>,
    dicts: RwLock<HashMap<u32, Arc<Dictionary>>>,
}

pub fn pack_name(pack: u32) -> String {
    format!("pack-{pack:06}.epk")
}

impl PackStore {
    pub fn open(dir: &Path, dict_dir: &Path, limit: u64, level: i32) -> Result<PackStore> {
        std::fs::create_dir_all(dir).context(|| format!("creating {}", dir.display()))?;
        std::fs::create_dir_all(dict_dir).context(|| format!("creating {}", dict_dir.display()))?;
        let mut dicts = HashMap::new();
        for entry in std::fs::read_dir(dict_dir).context(|| format!("reading {}", dict_dir.display()))? {
            let path = entry.context(|| "listing dictionaries".into())?.path();
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok());
            if let (Some(id), Some("zdict")) = (id, path.extension().and_then(|e| e.to_str())) {
                let bytes = std::fs::read(&path).context(|| format!("reading {}", path.display()))?;
                dicts.insert(id, Arc::new(Dictionary::new(id, &bytes, level)));
            }
        }
        Ok(PackStore {
            dir: dir.to_path_buf(),
            dict_dir: dict_dir.to_path_buf(),
            limit,
            writer: Mutex::new(None),
            readers: Mutex::new(HashMap::new()),
            dicts: RwLock::new(dicts),
        })
    }

    pub fn dictionary(&self, id: u32) -> Option<Arc<Dictionary>> {
        self.dicts.read().unwrap().get(&id).cloned()
    }

    pub fn newest_dictionary(&self) -> Option<Arc<Dictionary>> {
        let dicts = self.dicts.read().unwrap();
        dicts.keys().max().and_then(|id| dicts.get(id).cloned())
    }

    /// Saves a new dictionary (file first, flushed) and returns it ready to use.
    pub fn add_dictionary(&self, bytes: &[u8], level: i32) -> Result<Arc<Dictionary>> {
        let id = self.dicts.read().unwrap().keys().max().copied().unwrap_or(0) + 1;
        let path = self.dict_dir.join(format!("{id}.zdict"));
        let mut file = File::create(&path).context(|| format!("creating {}", path.display()))?;
        file.write_all(bytes).context(|| format!("writing {}", path.display()))?;
        file.sync_all().context(|| format!("writing {}", path.display()))?;
        let dict = Arc::new(Dictionary::new(id, bytes, level));
        self.dicts.write().unwrap().insert(id, dict.clone());
        Ok(dict)
    }

    pub fn pack_ids(&self) -> Result<Vec<u32>> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.dir).context(|| format!("reading {}", self.dir.display()))? {
            let name = entry.context(|| "listing packs".into())?.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name.strip_prefix("pack-").and_then(|s| s.strip_suffix(".epk")) {
                if let Ok(id) = n.parse() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn pack_path(&self, pack: u32) -> PathBuf {
        self.dir.join(pack_name(pack))
    }

    /// Opens the last pack for appending, cutting off a half-written record left by a crash.
    /// Returns how many bytes were cut off.
    pub fn start_writing(&self) -> Result<u64> {
        let mut writer = self.writer.lock().unwrap();
        if writer.is_some() {
            return Ok(0);
        }
        let (pack, cut) = match self.pack_ids()?.last().copied() {
            Some(last) => {
                let scan = self.scan(last)?;
                let path = self.pack_path(last);
                let size = std::fs::metadata(&path).context(|| format!("reading {}", path.display()))?.len();
                let cut = size - scan.valid_end;
                if cut > 0 {
                    let file = OpenOptions::new().write(true).open(&path).context(|| format!("opening {}", path.display()))?;
                    file.set_len(scan.valid_end).context(|| format!("repairing {}", path.display()))?;
                    file.sync_all().context(|| format!("repairing {}", path.display()))?;
                }
                (last, cut)
            }
            None => (0, 0),
        };
        *writer = Some(if pack == 0 { self.create_pack(1)? } else { self.open_for_append(pack)? });
        Ok(cut)
    }

    fn create_pack(&self, pack: u32) -> Result<WriterState> {
        let path = self.pack_path(pack);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .context(|| format!("creating {}", path.display()))?;
        file.write_all(PACK_HEADER).context(|| format!("writing {}", path.display()))?;
        Ok(WriterState { pack, file: BufWriter::with_capacity(1 << 20, file), pos: PACK_HEADER.len() as u64, buf: Vec::new() })
    }

    fn open_for_append(&self, pack: u32) -> Result<WriterState> {
        let path = self.pack_path(pack);
        let file = OpenOptions::new().append(true).open(&path).context(|| format!("opening {}", path.display()))?;
        let pos = file.metadata().context(|| format!("reading {}", path.display()))?.len();
        Ok(WriterState { pack, file: BufWriter::with_capacity(1 << 20, file), pos, buf: Vec::new() })
    }

    /// Appends a record (buffered until `flush`).
    pub fn append(&self, record: &EncodedRecord) -> Result<Location> {
        let mut guard = self.writer.lock().unwrap();
        let state = guard.as_mut().ok_or_else(|| Error::Other("the pack store isn't open for writing".into()))?;
        let len = record.len() as u64;
        if state.pos > PACK_HEADER.len() as u64 && state.pos + len > self.limit {
            state.file.flush().context(|| format!("writing {}", pack_name(state.pack)))?;
            state.file.get_ref().sync_all().context(|| format!("writing {}", pack_name(state.pack)))?;
            let next = state.pack + 1;
            *state = self.create_pack(next)?;
        }
        state.buf.clear();
        record.write_into(&mut state.buf);
        state.file.write_all(&state.buf).context(|| format!("writing {}", pack_name(state.pack)))?;
        let location = Location { pack: state.pack, offset: state.pos, len: len as u32 };
        state.pos += len;
        Ok(location)
    }

    /// Copies a record as it is (checked against its checksum first) to the pack being written
    /// (Optimize Library: moving what's still used out of a pack mostly holding deleted mail).
    pub fn copy_record(&self, from: Location) -> Result<Location> {
        self.read(from)?; // (the checksum: never copy a damaged record as if it were fine)
        let file = self.reader(from.pack)?;
        let mut buf = vec![0u8; from.len as usize];
        read_at(&file, &mut buf, from.offset).context(|| format!("reading {}", pack_name(from.pack)))?;
        let mut guard = self.writer.lock().unwrap();
        let state = guard.as_mut().ok_or_else(|| Error::Other("the pack store isn't open for writing".into()))?;
        if state.pack == from.pack {
            return Err(Error::Other("a record can't be copied into its own pack".into()));
        }
        if state.pos > PACK_HEADER.len() as u64 && state.pos + buf.len() as u64 > self.limit {
            state.file.flush().context(|| format!("writing {}", pack_name(state.pack)))?;
            state.file.get_ref().sync_all().context(|| format!("writing {}", pack_name(state.pack)))?;
            let next = state.pack + 1;
            *state = self.create_pack(next)?;
        }
        state.file.write_all(&buf).context(|| format!("writing {}", pack_name(state.pack)))?;
        let location = Location { pack: state.pack, offset: state.pos, len: from.len };
        state.pos += buf.len() as u64;
        Ok(location)
    }

    /// Starts a new pack for what's written next (so every older pack can be optimized).
    pub fn rotate(&self) -> Result<u32> {
        let mut guard = self.writer.lock().unwrap();
        let state = guard.as_mut().ok_or_else(|| Error::Other("the pack store isn't open for writing".into()))?;
        if state.pos <= PACK_HEADER.len() as u64 {
            return Ok(state.pack); // empty: already new
        }
        state.file.flush().context(|| format!("writing {}", pack_name(state.pack)))?;
        state.file.get_ref().sync_all().context(|| format!("writing {}", pack_name(state.pack)))?;
        let next = state.pack + 1;
        *state = self.create_pack(next)?;
        Ok(next)
    }

    /// The pack being written to now (None: not open for writing).
    pub fn writing_pack(&self) -> Option<u32> {
        self.writer.lock().unwrap().as_ref().map(|s| s.pack)
    }

    /// Deletes a pack nothing refers to any more (after its records were copied elsewhere and
    /// the database commit pointing at the copies is done).
    pub fn remove_pack(&self, pack: u32) -> Result<()> {
        if self.writing_pack() == Some(pack) {
            return Err(Error::Other("the pack being written can't be removed".into()));
        }
        self.readers.lock().unwrap().remove(&pack); // (Windows can't delete an open file)
        let path = self.pack_path(pack);
        std::fs::remove_file(&path).context(|| format!("removing {}", path.display()))
    }

    /// Writes buffered records out; with `sync`, waits until they're on the disk. Called
    /// before the database commit that refers to them, so the database never points at
    /// data that isn't there.
    pub fn flush(&self, sync: bool) -> Result<()> {
        let mut guard = self.writer.lock().unwrap();
        if let Some(state) = guard.as_mut() {
            state.file.flush().context(|| format!("writing {}", pack_name(state.pack)))?;
            if sync {
                state.file.get_ref().sync_data().context(|| format!("writing {}", pack_name(state.pack)))?;
            }
        }
        Ok(())
    }

    fn reader(&self, pack: u32) -> Result<Arc<File>> {
        let mut readers = self.readers.lock().unwrap();
        if let Some(file) = readers.get(&pack) {
            return Ok(file.clone());
        }
        let path = self.pack_path(pack);
        let file = Arc::new(File::open(&path).context(|| format!("opening {}", path.display()))?);
        readers.insert(pack, file.clone());
        Ok(file)
    }

    fn corrupt(pack: u32, detail: impl Into<String>) -> Error {
        Error::Corrupt { pack: pack_name(pack), detail: detail.into() }
    }

    /// Reads a record and checks the message against its BLAKE3 hash.
    pub fn read(&self, location: Location) -> Result<RawRecord> {
        let file = self.reader(location.pack)?;
        let mut buf = vec![0u8; location.len as usize];
        read_at(&file, &mut buf, location.offset).context(|| format!("reading {}", pack_name(location.pack)))?;
        let fixed: &[u8; FIXED_HEADER] = buf
            .get(..FIXED_HEADER)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Self::corrupt(location.pack, format!("record at {} is too short", location.offset)))?;
        let header = RecordHeader::parse(fixed)
            .ok_or_else(|| Self::corrupt(location.pack, format!("no record at offset {}", location.offset)))?;
        if header.record_len() != location.len as u64 {
            return Err(Self::corrupt(location.pack, format!("record at {} has the wrong length", location.offset)));
        }
        let meta_end = FIXED_HEADER + header.meta_len as usize;
        let meta = buf[FIXED_HEADER..meta_end].to_vec();
        let payload = &buf[meta_end..];
        let raw = self.decode(&header, payload, location.pack)?;
        if *blake3::hash(&raw).as_bytes() != header.hash {
            return Err(Self::corrupt(location.pack, format!("message at offset {} doesn't match its checksum", location.offset)));
        }
        Ok(RawRecord { meta, raw, hash: header.hash })
    }

    fn decode(&self, header: &RecordHeader, payload: &[u8], pack: u32) -> Result<Vec<u8>> {
        let raw_len = header.raw_len as usize;
        let raw = match header.codec {
            CODEC_STORED => payload.to_vec(),
            CODEC_ZSTD => zstd_decompress(payload, raw_len, None)
                .map_err(|e| Self::corrupt(pack, format!("can't decompress: {e}")))?,
            _ => {
                let dict = self
                    .dictionary(header.dict_id)
                    .ok_or_else(|| Self::corrupt(pack, format!("dictionary {} is missing", header.dict_id)))?;
                zstd_decompress(payload, raw_len, Some(dict.dec))
                    .map_err(|e| Self::corrupt(pack, format!("can't decompress: {e}")))?
            }
        };
        if raw.len() != raw_len {
            return Err(Self::corrupt(pack, "message has the wrong length"));
        }
        Ok(raw)
    }

    /// Lists a pack's records from their headers only (no payloads read).
    pub fn scan(&self, pack: u32) -> Result<PackScan> {
        let path = self.pack_path(pack);
        let file = File::open(&path).context(|| format!("opening {}", path.display()))?;
        let size = file.metadata().context(|| format!("reading {}", path.display()))?.len();
        let mut header = [0u8; 8];
        if size < 8 || read_at(&file, &mut header, 0).is_err() || &header != PACK_HEADER {
            return Err(Self::corrupt(pack, "not a pack file"));
        }
        let mut records = Vec::new();
        let mut pos = PACK_HEADER.len() as u64;
        let mut fixed = [0u8; FIXED_HEADER];
        while pos + FIXED_HEADER as u64 <= size {
            if read_at(&file, &mut fixed, pos).is_err() {
                break;
            }
            let Some(h) = RecordHeader::parse(&fixed) else { break };
            if pos + h.record_len() > size {
                break;
            }
            records.push((Location { pack, offset: pos, len: h.record_len() as u32 }, h.clone()));
            pos += h.record_len();
        }
        Ok(PackScan { records, valid_end: pos, size })
    }

    /// Reads a record's meta part only.
    pub fn read_meta(&self, location: Location, header: &RecordHeader) -> Result<Vec<u8>> {
        let file = self.reader(location.pack)?;
        let mut meta = vec![0u8; header.meta_len as usize];
        read_at(&file, &mut meta, location.offset + FIXED_HEADER as u64)
            .context(|| format!("reading {}", pack_name(location.pack)))?;
        Ok(meta)
    }

    /// Total bytes in all packs.
    pub fn disk_size(&self) -> Result<u64> {
        let mut total = 0;
        for id in self.pack_ids()? {
            let path = self.pack_path(id);
            total += std::fs::metadata(&path).context(|| format!("reading {}", path.display()))?.len();
        }
        Ok(total)
    }
}

/// The result of scanning a pack's headers.
pub struct PackScan {
    pub records: Vec<(Location, RecordHeader)>,
    /// End of the last complete record; anything after it is a torn write.
    pub valid_end: u64,
    pub size: u64,
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path, limit: u64) -> PackStore {
        PackStore::open(&dir.join("packs"), &dir.join("dicts"), limit, 3).unwrap()
    }

    fn record(raw: &[u8], compression: &Compression) -> EncodedRecord {
        encode(raw, *blake3::hash(raw).as_bytes(), b"{}".to_vec(), compression).unwrap()
    }

    #[test]
    fn round_trip_with_and_without_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let packs = store(dir.path(), DEFAULT_PACK_LIMIT);
        packs.start_writing().unwrap();
        let plain = Compression { level: 3, dict: None };
        let messages: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("From: person{i}@example.com\r\nSubject: Hello number {i}\r\n\r\nBody text {i} repeated repeated repeated.\r\n").into_bytes())
            .collect();
        let dict = packs.add_dictionary(&train_dictionary(&messages).unwrap(), 3).unwrap();
        let with_dict = Compression { level: 3, dict: Some(dict) };
        let mut locations = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            let c = if i % 2 == 0 { &plain } else { &with_dict };
            locations.push(packs.append(&record(m, c)).unwrap());
        }
        packs.append(&record(b"", &plain)).unwrap();
        packs.flush(true).unwrap();
        for (m, loc) in messages.iter().zip(&locations) {
            assert_eq!(&packs.read(*loc).unwrap().raw, m);
        }
        // A fresh store finds the dictionary on disk.
        let again = store(dir.path(), DEFAULT_PACK_LIMIT);
        assert_eq!(&again.read(locations[1]).unwrap().raw, &messages[1]);
        assert_eq!(again.scan(1).unwrap().records.len(), 201);
    }

    #[test]
    fn rolls_over_to_new_packs() {
        let dir = tempfile::tempdir().unwrap();
        let packs = store(dir.path(), 4096);
        packs.start_writing().unwrap();
        let plain = Compression { level: 1, dict: None };
        let mut locations = Vec::new();
        for i in 0..50u32 {
            // (random-looking bytes: they don't compress, so 50 records need several 4 KB packs)
            let mut raw = vec![0u8; 500];
            blake3::Hasher::new().update(&i.to_le_bytes()).finalize_xof().fill(&mut raw);
            locations.push((packs.append(&record(&raw, &plain)).unwrap(), raw));
        }
        packs.flush(false).unwrap();
        assert!(packs.pack_ids().unwrap().len() > 5);
        for (loc, raw) in &locations {
            assert_eq!(&packs.read(*loc).unwrap().raw, raw);
        }
    }

    #[test]
    fn torn_write_is_cut_off_and_damage_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let plain = Compression { level: 3, dict: None };
        let first;
        {
            let packs = store(dir.path(), DEFAULT_PACK_LIMIT);
            packs.start_writing().unwrap();
            first = packs.append(&record(b"Subject: one\r\n\r\nfirst", &plain)).unwrap();
            packs.append(&record(b"Subject: two\r\n\r\nsecond message body", &plain)).unwrap();
            packs.flush(true).unwrap();
        }
        // Simulate a crash in the middle of the second record.
        let path = dir.path().join("packs").join(pack_name(1));
        let size = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new().write(true).open(&path).unwrap().set_len(size - 5).unwrap();
        let packs = store(dir.path(), DEFAULT_PACK_LIMIT);
        assert!(packs.start_writing().unwrap() > 0);
        assert_eq!(packs.scan(1).unwrap().records.len(), 1);
        let third = packs.append(&record(b"Subject: three\r\n\r\nthird", &plain)).unwrap();
        packs.flush(true).unwrap();
        assert_eq!(packs.read(first).unwrap().raw, b"Subject: one\r\n\r\nfirst");
        assert_eq!(packs.read(third).unwrap().raw, b"Subject: three\r\n\r\nthird");

        // Flip a byte inside the stored payload: the checksum catches it.
        let mut bytes = std::fs::read(&path).unwrap();
        let at = (first.offset + first.len as u64 - 2) as usize;
        bytes[at] ^= 0xFF;
        drop(packs);
        std::fs::write(&path, &bytes).unwrap();
        let packs = store(dir.path(), DEFAULT_PACK_LIMIT);
        assert!(matches!(packs.read(first), Err(Error::Corrupt { .. })));
    }
}
