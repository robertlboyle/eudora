//! Classic Eudora (7.x) files written from the library, for someone going back to the old
//! program.
//!
//! One engine does both jobs: **Export to Classic Eudora** writes a folder classic Eudora opens
//! as its mail folder, and the optional **classic copy kept current** (Settings ▸ Library; off
//! by default) runs the same update again from the download, adding new messages, reflecting
//! moves and deletes, writing changed rules again, and compacting a mailbox when it gets too
//! fragmented. It's one-way: what is changed in classic Eudora never comes back.
//!
//! What goes where (the formats are classic's own, from its released source: `tocdoc.cpp`,
//! `summary.cpp`, `QCMailboxDirector.cpp`, `filtersd.cpp`, `nickdoc.cpp`, `persona.cpp`):
//! - every account's Inbox, Sent, Trash, and Junk / Spam go into classic's system mailboxes
//!   In.mbx, Out.mbx, Trash.mbx, and Junk.mbx (classic has one of each, whatever the number of
//!   personalities); the other folders keep their tree as `.fol` folders and `.mbx` mailboxes,
//!   at the top when one account is exported, else inside a folder per account;
//! - each mailbox has its `.toc` (header 110 bytes, then a 218-byte summary per message: where
//!   it is in the .mbx, its date, status, priority, label, sender, and subject) and each folder
//!   its `descmap.pce` (`Name,File,Type,Status`);
//! - messages are converted the way classic itself stores them (its own importer,
//!   `MAPIImport.cpp`, does the same: classic doesn't decode MIME when it shows a message): the
//!   original headers, the text in Windows-1252 (HTML between `<x-html>` and `</x-html>`, a
//!   plain text with characters Windows-1252 lacks as HTML too, so none is lost), and each
//!   attachment decoded into `Attach` with an `Attachment Converted: "path"` line; pictures the
//!   HTML showed are attachments too (classic's own embedded-picture records aren't made);
//! - the rules as `Filters.pce` (version 3; a rule classic can't express is left out and
//!   named in the report), the address book as `NNdbase.txt`, and a first `eudora.ini`
//!   (return address, real name, personalities, and labels; never written over afterward).
//!
//! What this folder holds is remembered in `Eudora export.db` inside it (each mailbox's file,
//! each message's place in it), so a later update only appends what's new. A mailbox changed
//! by classic Eudora meanwhile (mail fetched there, messages moved) is kept beside as
//! "<name> (changed in classic Eudora <date>)" and written again, so nothing is lost. Nothing is
//! written while classic Eudora has the folder open (its OWNER.LOK).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::cards::Card;
use crate::error::{Error, IoContext, Result};
use crate::library::{Library, now, shown_path};
use crate::marks;
use crate::progress::{Progress, thousands};
use crate::rules::{Action, Rule, Verb};

/// The file inside a classic folder that remembers what Eudora wrote there.
pub const STATE_FILE: &str = "Eudora export.db";
const LOCK_FILE: &str = "Eudora export.lock";
/// Classic Eudora's "I'm running in this folder" file (eudora.cpp `IsEudoraRunning`).
const OWNER_LOCK: &str = "OWNER.LOK";
/// The setting holding the classic copy kept current (this computer only: never synced).
pub const MIRROR_KEY: &str = "classic-mirror";

/// What to export.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassicOptions {
    /// Accounts by id; empty = every account.
    pub accounts: Vec<i64>,
    /// Only these folders (ids); empty = every folder of those accounts.
    pub folders: Vec<i64>,
    /// Leave the rules out (Filters.pce).
    pub no_rules: bool,
    /// Leave the address book out (NNdbase.txt).
    pub no_contacts: bool,
}

/// What an export or an update did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassicReport {
    pub folder: String,
    pub mailboxes: u64,
    /// Messages written this time, and taken out (moved or deleted in Eudora).
    pub written: u64,
    pub removed: u64,
    /// Messages whose status, priority, or label changed in the .toc.
    pub changed: u64,
    /// Messages in the classic folder after this run.
    pub total: u64,
    pub attachments: u64,
    pub compacted: u64,
    /// Messages whose text had characters Windows-1252 lacks: kept by writing it as HTML.
    pub as_html: u64,
    /// Messages Eudora couldn't read: written as stored, whole.
    pub unreadable: u64,
    pub rules_written: u64,
    /// Rules left out, each with why.
    pub rules_left_out: Vec<(String, String)>,
    pub contacts: u64,
    /// Mailboxes changed by classic Eudora since Eudora last wrote them, kept beside under these names.
    pub kept_aside: Vec<String>,
    /// Messages still waiting to be written (an update does a batch at a time).
    pub left: u64,
    /// Something worth saying (an attachment that couldn't be decoded, a label classic lacks…).
    pub notes: Vec<String>,
    /// Set when nothing was written because classic Eudora has the folder open.
    pub waiting: Option<String>,
    pub seconds: f64,
}

// ------------------------------------------------------------------ classic's file formats

/// Classic's mailbox types (tocdoc.h `MailboxType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
enum Kind {
    In,
    Out,
    Junk,
    Trash,
    Regular,
}

impl Kind {
    fn mbt(self) -> i16 {
        match self {
            Kind::In => 0,
            Kind::Out => 1,
            Kind::Junk => 2,
            Kind::Trash => 3,
            Kind::Regular => 4,
        }
    }

    fn file(self) -> Option<&'static str> {
        match self {
            Kind::In => Some("In"),
            Kind::Out => Some("Out"),
            Kind::Junk => Some("Junk"),
            Kind::Trash => Some("Trash"),
            Kind::Regular => None,
        }
    }
}

// summary states (summary.h)
const MS_UNREAD: i16 = 0;
const MS_READ: i16 = 1;
const MS_REPLIED: i16 = 2;
const MS_FORWARDED: i16 = 3;
const MS_REDIRECT: i16 = 4;
const MS_SENT: i16 = 8;
const MSF_HAS_ATTACHMENT: u16 = 0x8000;
const MSF_MIME: u16 = 0x0100;
const MSFEX_HTML: u16 = 0x0002;
const TOC_VERSION: u16 = 0x31;
/// A summary's size in a .toc, and the header's.
pub const SUMMARY_SIZE: usize = 218;
pub const TOC_HEADER_SIZE: usize = 110;

/// One message in a mailbox, as its .toc summary holds it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub offset: u32,
    pub length: u32,
    /// UTC seconds, and the sender's offset from UTC in minutes (east positive).
    pub seconds: i64,
    pub tz_minutes: i16,
    pub state: i16,
    pub flags: u16,
    pub flags_ex: u16,
    pub priority: i8,
    pub label: i16,
    pub date: String,
    pub from: String,
    pub subject: String,
    pub uid: u32,
    pub arrival: i64,
}

/// Text in classic's code page; characters it lacks become "?" (`limit` bytes at most).
fn ansi(text: &str, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for c in text.chars() {
        let mut buf = [0; 4];
        let (bytes, _, bad) = encoding_rs::WINDOWS_1252.encode(c.encode_utf8(&mut buf));
        if bad {
            out.push(b'?');
        } else {
            out.extend_from_slice(&bytes);
        }
        if out.len() >= limit {
            out.truncate(limit);
            break;
        }
    }
    out
}

fn fixed(text: &str, size: usize) -> Vec<u8> {
    let mut b = ansi(&text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>(), size - 1);
    b.resize(size, 0);
    b
}

/// A .toc for a mailbox (`CTocDoc::Write`, version 3.1): little-endian, as classic wrote it on x86.
pub fn toc_bytes(name: &str, kind_mbt: i16, mbx_size: u64, sums: &[Summary]) -> Vec<u8> {
    let mut b = Vec::with_capacity(TOC_HEADER_SIZE + sums.len() * SUMMARY_SIZE);
    b.extend_from_slice(&TOC_VERSION.to_le_bytes());
    b.extend_from_slice(&[0; 6]); // used version
    b.extend_from_slice(&fixed(name, 32));
    b.extend_from_slice(&kind_mbt.to_le_bytes());
    b.extend_from_slice(&0i16.to_le_bytes()); // group by subject, sorting, file browser
    b.extend_from_slice(&0i16.to_le_bytes()); // needs compact
    b.extend_from_slice(&[0; 8]); // saved window position (empty: classic's default)
    for _ in 0..10 {
        b.extend_from_slice(&(-1i16).to_le_bytes()); // column widths: classic's defaults
    }
    let unread = sums.iter().any(|s| s.state == MS_UNREAD);
    b.extend_from_slice(&(if unread { 1i16 } else { 2 }).to_le_bytes()); // US_YES / US_NO
    let next = sums.iter().map(|s| s.uid).max().unwrap_or(0) + 1;
    b.extend_from_slice(&(next as i32).to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes()); // plug-in id
    b.extend_from_slice(&0i32.to_le_bytes()); // plug-in tag
    b.extend_from_slice(&0i16.to_le_bytes()); // splitter
    b.extend_from_slice(&[0; 11]); // sort columns: not sorted
    b.push(0); // ad failure
    // the .mbx's size + 1 (classic rebuilds a .toc whose mailbox changed behind its back)
    b.extend_from_slice(&((mbx_size as u32).wrapping_add(1)).to_le_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(&(sums.len() as u16).to_le_bytes());
    debug_assert_eq!(b.len(), TOC_HEADER_SIZE);
    for s in sums {
        let start = b.len();
        b.extend_from_slice(&s.offset.to_le_bytes());
        b.extend_from_slice(&s.length.to_le_bytes());
        b.extend_from_slice(&(s.seconds as i32).to_le_bytes());
        b.extend_from_slice(&s.state.to_le_bytes());
        b.extend_from_slice(&s.flags.to_le_bytes());
        b.push(s.priority as u8);
        b.push(0);
        // the date as text (classic makes it again from the seconds), then the arrival time
        b.extend_from_slice(&fixed(&s.date, 28));
        b.extend_from_slice(&(s.arrival as i32).to_le_bytes());
        b.extend_from_slice(&fixed(&s.from, 64));
        b.extend_from_slice(&fixed(&s.subject, 64));
        for v in [-1i16, -1, -1, -1] {
            b.extend_from_slice(&v.to_le_bytes()); // message window position: default
        }
        b.extend_from_slice(&s.label.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // hash (POP / IMAP id)
        b.extend_from_slice(&s.uid.to_le_bytes());
        b.extend_from_slice(&s.flags_ex.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // personality hash (0 = the dominant one)
        b.extend_from_slice(&s.tz_minutes.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // IMAP flags
        b.extend_from_slice(&(((s.length / 1024) + 1).min(u16::MAX as u32) as u16).to_le_bytes());
        b.push(0); // mood
        b.push(0); // junk score
        b.extend_from_slice(&0u32.to_le_bytes()); // junk plug-in
        b.extend_from_slice(&[0; 2]);
        debug_assert_eq!(b.len() - start, SUMMARY_SIZE);
    }
    b
}

/// Reads a .toc back (`CTocDoc::Read`): the mailbox's name, the .mbx size it was written for,
/// and its summaries. For tests and for checking an export.
pub fn read_toc(bytes: &[u8]) -> Option<(String, u64, Vec<Summary>)> {
    if bytes.len() < TOC_HEADER_SIZE {
        return None;
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let i16_at = |i: usize| i16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let text = |b: &[u8]| {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        encoding_rs::WINDOWS_1252.decode_without_bom_handling(&b[..end]).0.into_owned()
    };
    if u16_at(0) > TOC_VERSION {
        return None;
    }
    let name = text(&bytes[8..40]);
    let mbx = u64::from(u32_at(102).wrapping_sub(1));
    let count = u16_at(108) as usize;
    let mut sums = Vec::new();
    let mut at = TOC_HEADER_SIZE;
    while at + SUMMARY_SIZE <= bytes.len() {
        let s = &bytes[at..at + SUMMARY_SIZE];
        let o = at;
        sums.push(Summary {
            offset: u32_at(o),
            length: u32_at(o + 4),
            seconds: i64::from(u32_at(o + 8) as i32),
            state: i16_at(o + 12),
            flags: u16_at(o + 14),
            priority: s[16] as i8,
            date: text(&s[18..42]),
            arrival: i64::from(u32_at(o + 46) as i32),
            from: text(&s[50..114]),
            subject: text(&s[114..178]),
            label: i16_at(o + 186),
            uid: u32_at(o + 192),
            flags_ex: u16_at(o + 196),
            tz_minutes: i16_at(o + 202),
        });
        at += SUMMARY_SIZE;
    }
    (sums.len() % 65536 == count).then_some((name, mbx, sums))
}

/// A file or folder name classic Eudora (and Windows) accepts, from a mailbox's name.
fn legal_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || " -_.()&'!+=#$%@~".contains(c) { c } else { '_' })
        .collect();
    out = out.trim().trim_end_matches(['.', ' ']).to_string();
    if out.len() > 40 {
        out.truncate(40);
        out = out.trim_end_matches(['.', ' ']).to_string();
    }
    if out.is_empty() {
        out = "Mailbox".into();
    }
    let upper = out.to_ascii_uppercase();
    let device = ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || ((upper.starts_with("COM") || upper.starts_with("LPT")) && upper.len() == 4 && upper.as_bytes()[3].is_ascii_digit());
    if device {
        out.push('_');
    }
    out
}

/// A name as descmap.pce lists it (a comma would end it).
fn map_name(name: &str) -> String {
    name.chars().map(|c| if c == ',' || c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

// ------------------------------------------------------------------ messages

/// A message converted to classic's stored form.
struct Converted {
    /// From "From ???@???" to the end, CRLF line endings.
    bytes: Vec<u8>,
    html: bool,
    attachments: u64,
    as_html: bool,
    unreadable: bool,
    notes: Vec<String>,
    tz_minutes: i16,
    seconds: i64,
    from: String,
    subject: String,
    priority: Option<u8>,
}

fn crlf(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + text.len() / 40);
    let mut prev = 0u8;
    for &b in text {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        if prev == b'\r' && b != b'\n' {
            out.push(b'\n');
        }
        out.push(b);
        prev = b;
    }
    if prev == b'\r' {
        out.push(b'\n');
    }
    out
}

/// "From " lines escaped as mbox readers expect (mboxrd: one more ">"), so a line in the body
/// never starts a new message.
fn escape_from_lines(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut line_start = true;
    let mut i = 0;
    while i < text.len() {
        if line_start {
            let quotes = text[i..].iter().take_while(|b| **b == b'>').count();
            if text[i + quotes..].starts_with(b"From ") {
                out.push(b'>');
            }
        }
        out.push(text[i]);
        line_start = text[i] == b'\n';
        i += 1;
    }
    out
}

fn split_head(raw: &[u8]) -> (&[u8], &[u8]) {
    let mut i = 0;
    while i < raw.len() {
        let end = memchr::memchr(b'\n', &raw[i..]).map_or(raw.len(), |n| i + n + 1);
        let line = &raw[i..end];
        if line == b"\n" || line == b"\r\n" {
            return (&raw[..i], &raw[end..]);
        }
        i = end;
    }
    (raw, &[])
}

/// The header block without the lines that describe the original MIME structure (the stored
/// body is no longer that), the one given in their place.
fn headers_for(head: &[u8], content_type: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(head.len() + 64);
    let mut skipping = false;
    for line in head.split_inclusive(|b| *b == b'\n') {
        let continuation = line.first().is_some_and(|b| *b == b' ' || *b == b'\t');
        if !continuation {
            let lower: Vec<u8> = line.iter().take(26).map(u8::to_ascii_lowercase).collect();
            skipping = lower.starts_with(b"content-type:") || lower.starts_with(b"content-transfer-encoding:");
        }
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    out
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c => out.push(c),
        }
    }
    out
}

/// Classic's "From " line ("From ???@??? Mon Jan 03 01:05:34 1996", IDS_FROM_FORMAT), in this
/// computer's time as classic wrote it.
fn from_line(seconds: i64) -> String {
    use chrono::TimeZone;
    let when = chrono::Local.timestamp_opt(seconds.max(1), 0).single().unwrap_or_else(|| chrono::Local.timestamp_opt(1, 0).unwrap());
    format!("From ???@??? {}", when.format("%a %b %d %H:%M:%S %Y"))
}

impl Writer<'_> {
    /// One message in classic's stored form; its attachments decoded into Attach.
    fn convert(&mut self, raw: &[u8], fallback_date: i64, sent: bool) -> Converted {
        let parsed = crate::parse::parse(raw, Default::default(), now());
        let message = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mail_parser::MessageParser::default().parse(raw))).ok().flatten();
        let (seconds, tz_minutes) = message
            .as_ref()
            .and_then(|m| m.date().filter(|d| d.is_valid()).map(|d| {
                let tz = (i16::from(d.tz_hour) * 60 + i16::from(d.tz_minute)) * if d.tz_before_gmt { -1 } else { 1 };
                (d.to_timestamp(), tz)
            }))
            .unwrap_or((if parsed.date > 0 { parsed.date } else { fallback_date }, 0));
        let who = if sent {
            parsed.to.iter().chain(&parsed.cc).map(|a| if a.name.is_empty() { a.address.clone() } else { a.name.clone() }).collect::<Vec<_>>().join(", ")
        } else {
            parsed.from_display()
        };
        let priority = crate::parse::raw_header(raw, "X-Priority")
            .and_then(|v| String::from_utf8_lossy(&v).trim().chars().next().and_then(|c| c.to_digit(10)))
            .filter(|p| (1..=5).contains(p))
            .map(|p| p as u8);
        let mut notes = Vec::new();
        let (head, _) = split_head(raw);
        let Some(message) = message else {
            // unreadable: stored whole as it is, nothing lost
            let mut bytes = format!("{}\r\n", from_line(seconds)).into_bytes();
            bytes.extend(escape_from_lines(&crlf(raw)));
            if !bytes.ends_with(b"\r\n") {
                bytes.extend_from_slice(b"\r\n");
            }
            return Converted { bytes, html: false, attachments: 0, as_html: false, unreadable: true, notes, tz_minutes, seconds, from: who, subject: parsed.subject, priority };
        };
        // the bodies: HTML when there is one (classic shows it between <x-html> tags), else text
        let html: Option<String> = message
            .html_body
            .iter()
            .filter_map(|&id| message.parts.get(id as usize))
            .find(|p| p.is_text_html())
            .and_then(|p| p.text_contents().map(str::to_string));
        let text = crate::parse::display(raw, now()).text;
        let mut as_html = false;
        let (body, is_html): (Vec<u8>, bool) = match html {
            Some(h) => {
                let (bytes, _, _) = encoding_rs::WINDOWS_1252.encode(&h); // (characters it lacks as &#NNNN;)
                let mut b = b"<x-html>\r\n".to_vec();
                b.extend_from_slice(&bytes);
                b.extend_from_slice(b"\r\n</x-html>\r\n");
                (b, true)
            }
            None => {
                let (bytes, _, bad) = encoding_rs::WINDOWS_1252.encode(&text);
                if bad {
                    as_html = true;
                    let html = format!("<x-html>\r\n<div style=\"white-space: pre-wrap\">{}</div>\r\n</x-html>\r\n", escape_html(&text));
                    (encoding_rs::WINDOWS_1252.encode(&html).0.into_owned(), true)
                } else {
                    (bytes.into_owned(), false)
                }
            }
        };
        let content_type = if is_html { "text/html; charset=windows-1252" } else { "text/plain; charset=windows-1252" };
        let mut bytes = format!("{}\r\n", from_line(seconds)).into_bytes();
        bytes.extend(escape_from_lines(&crlf(&headers_for(head, content_type))));
        bytes.extend_from_slice(b"\r\n");
        bytes.extend(escape_from_lines(&crlf(&body)));
        if !bytes.ends_with(b"\r\n") {
            bytes.extend_from_slice(b"\r\n");
        }
        // attachments: decoded into Attach, one line each (classic's "Attachment Converted:")
        let mut lines = Vec::new();
        let mut whole_kept = false;
        for a in &parsed.attachments {
            match crate::compose::part_bytes(raw, &a.part) {
                Some((_, data)) => match self.attach(&a.name, &data) {
                    Ok(path) => lines.push(path),
                    Err(e) => notes.push(format!("“{}”: an attachment couldn't be written ({e})", a.name)),
                },
                None if !whole_kept => {
                    // (one Eudora can't take apart: the whole original goes along instead)
                    whole_kept = true;
                    let name = format!("{}.eml", if parsed.subject.trim().is_empty() { "Message" } else { parsed.subject.trim() });
                    match self.attach(&name, raw) {
                        Ok(path) => {
                            lines.push(path);
                            notes.push(format!("“{}”: attachment “{}” kept inside the whole original message ({name})", parsed.subject, a.name));
                        }
                        Err(e) => notes.push(format!("“{}”: an attachment couldn't be written ({e})", a.name)),
                    }
                }
                None => {}
            }
        }
        for path in &lines {
            bytes.extend_from_slice(b"Attachment Converted: \"");
            bytes.extend_from_slice(&ansi(path, 4096));
            bytes.extend_from_slice(b"\"\r\n");
        }
        Converted { bytes, html: is_html, attachments: lines.len() as u64, as_html, unreadable: false, notes, tz_minutes, seconds, from: who, subject: parsed.subject, priority }
    }

    /// Writes an attachment into Attach once per content; its full path for the message.
    fn attach(&mut self, name: &str, data: &[u8]) -> Result<String> {
        let hash = blake3::hash(data);
        if let Some(file) = self.state.query_row("SELECT file FROM attachments WHERE hash = ?1", params![hash.as_bytes().as_slice()], |r| r.get::<_, String>(0)).optional()? {
            let path = self.root.join("Attach").join(&file);
            if path.is_file() {
                return Ok(classic_path(&path));
            }
        }
        let dir = self.root.join("Attach");
        std::fs::create_dir_all(&dir).context(|| format!("making {}", dir.display()))?;
        let base = legal_name(if name.trim().is_empty() { "attachment" } else { name });
        let (stem, ext) = match base.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() && e.len() <= 8 => (s.to_string(), format!(".{e}")),
            _ => (base.clone(), String::new()),
        };
        let mut file = format!("{stem}{ext}");
        let mut n = 1;
        while dir.join(&file).exists() {
            n += 1;
            file = format!("{stem} {n}{ext}");
        }
        let path = dir.join(&file);
        std::fs::write(&path, data).context(|| format!("writing {}", path.display()))?;
        self.state.execute("INSERT OR REPLACE INTO attachments (hash, file) VALUES (?1, ?2)", params![hash.as_bytes().as_slice(), file])?;
        self.report.attachments += 1;
        Ok(classic_path(&path))
    }
}

/// A path as classic writes it in a message (Windows' separators).
fn classic_path(path: &Path) -> String {
    let p = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let s = p.to_string_lossy().into_owned();
    let s = s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s);
    if cfg!(windows) { s } else { s.replace('/', "\\") }
}

// ------------------------------------------------------------------ the plan

/// A mailbox to write: its stable key, the folders above it (key, name), its name, kind, and
/// the library folders whose mail it holds.
#[derive(Clone, Debug)]
struct Planned {
    key: String,
    parents: Vec<(String, String)>,
    name: String,
    kind: Kind,
    folders: Vec<i64>,
    /// Sent mail (Out and its parts): written as sent, the recipients in the summary.
    sent: bool,
}

/// Part `n` (2, 3…) of a mailbox too big for one file: a mailbox of its own beside it, "In (2)"
/// (classic Eudora has only one In, so the parts of a system mailbox are ordinary ones).
fn part_of(p: &Planned, n: usize) -> Planned {
    Planned { key: format!("{}#{n}", p.key), parents: p.parents.clone(), name: format!("{} ({n})", p.name), kind: Kind::Regular, folders: p.folders.clone(), sent: p.sent }
}

fn system_kind(path: &str, role: Option<&str>) -> Option<Kind> {
    if path.contains('/') {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    match (role, lower.as_str()) {
        (Some("inbox"), _) | (_, "inbox") => Some(Kind::In),
        (Some("sent"), _) | (_, "sent" | "sent items" | "sent messages" | "sent mail") => Some(Kind::Out),
        (Some("trash"), _) | (_, "trash" | "deleted items" | "deleted messages") => Some(Kind::Trash),
        (Some("junk"), _) | (_, "junk" | "junk email" | "junk e-mail" | "spam") => Some(Kind::Junk),
        _ => None,
    }
}

impl Library {
    /// The mailboxes an export of these accounts / folders writes.
    fn classic_plan(&self, options: &ClassicOptions) -> Result<Vec<Planned>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.account, f.path, f.role, COALESCE(a.label, ''), COALESCE(a.address, ''), a.name
             FROM folders f JOIN accounts a ON a.id = f.account ORDER BY a.position, a.id, f.path",
        )?;
        let rows: Vec<(i64, i64, String, Option<String>, String, String, String)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| options.accounts.is_empty() || options.accounts.contains(&r.1))
            .filter(|r| options.folders.is_empty() || options.folders.contains(&r.0))
            .filter(|r| !crate::sync_stores::is_sync_folder(&r.2) && !shown_path(&r.2).is_empty())
            .collect();
        let accounts: Vec<i64> = {
            let mut a: Vec<i64> = rows.iter().map(|r| r.1).collect();
            a.dedup();
            a.sort();
            a.dedup();
            a
        };
        let several = accounts.len() > 1;
        let mut plan: BTreeMap<String, Planned> = BTreeMap::new();
        for kind in [Kind::In, Kind::Out, Kind::Trash] {
            let key = format!("sys:{:?}", kind).to_ascii_lowercase();
            plan.insert(key.clone(), Planned { key, parents: Vec::new(), name: kind.file().unwrap().into(), kind, folders: Vec::new(), sent: kind == Kind::Out });
        }
        for (id, account, path, role, label, address, name) in rows {
            let shown = shown_path(&path).to_string();
            if let Some(kind) = system_kind(&shown, role.as_deref()) {
                let key = format!("sys:{:?}", kind).to_ascii_lowercase();
                plan.entry(key.clone())
                    .or_insert_with(|| Planned { key, parents: Vec::new(), name: kind.file().unwrap().into(), kind, folders: Vec::new(), sent: kind == Kind::Out })
                    .folders
                    .push(id);
                continue;
            }
            let mut parents = Vec::new();
            if several {
                let shown_account = [label, address, name].into_iter().find(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("Account {account}"));
                parents.push((format!("dir:{account}"), shown_account));
            }
            let parts: Vec<&str> = shown.split('/').filter(|p| !p.is_empty()).collect();
            for depth in 1..parts.len() {
                parents.push((format!("dir:{account}:{}", parts[..depth].join("/")), parts[depth - 1].to_string()));
            }
            let key = format!("mbx:{account}:{shown}");
            plan.entry(key.clone())
                .or_insert_with(|| Planned { key, parents, name: parts.last().unwrap_or(&"Mailbox").to_string(), kind: Kind::Regular, folders: Vec::new(), sent: false })
                .folders
                .push(id);
        }
        let mut list: Vec<Planned> = plan.into_values().collect();
        list.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.parents.iter().map(|p| &p.1).cmp(b.parents.iter().map(|p| &p.1))).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(list)
    }

    /// The settings for the classic copy kept current (None: off).
    pub fn classic_mirror(&self) -> Result<Option<MirrorSettings>> {
        Ok(self.setting(MIRROR_KEY)?.and_then(|s| serde_json::from_str::<MirrorSettings>(&s).ok()).filter(|m| m.on && !m.folder.is_empty()))
    }

    /// Keeps a classic copy current (Some) or stops (None). The folder is checked first.
    pub fn set_classic_mirror(&self, settings: Option<&MirrorSettings>) -> Result<()> {
        match settings {
            Some(m) => {
                check_folder(self, Path::new(&m.folder))?;
                self.set_setting(MIRROR_KEY, &serde_json::to_string(&MirrorSettings { on: true, ..m.clone() })?)
            }
            None => {
                let mut old: MirrorSettings = self.setting(MIRROR_KEY)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
                old.on = false;
                self.set_setting(MIRROR_KEY, &serde_json::to_string(&old)?)
            }
        }
    }

    /// The settings as last saved, on or off (for the window to show the folder chosen before).
    pub fn classic_mirror_saved(&self) -> Result<MirrorSettings> {
        Ok(self.setting(MIRROR_KEY)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }
}

/// The classic copy kept current.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MirrorSettings {
    pub on: bool,
    pub folder: String,
    /// Accounts by id; empty = every account.
    pub accounts: Vec<i64>,
    pub no_rules: bool,
    pub no_contacts: bool,
}

/// Why classic Eudora's folder can't be written now: classic Eudora has it open (None: free).
pub fn classic_is_open(dir: &Path) -> Option<String> {
    let lock = dir.join(OWNER_LOCK);
    let bytes = std::fs::read(&lock).ok()?;
    // classic writes its window's handle there and deletes the file when it quits; a file left
    // by a crash names a window that no longer exists
    #[cfg(windows)]
    {
        if bytes.len() >= 4 {
            let hwnd = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if hwnd != 0 && !window_exists(hwnd) {
                return None;
            }
        }
    }
    let _ = bytes;
    Some(format!("classic Eudora has this folder open ({} is there); Eudora writes once classic Eudora is closed", lock.display()))
}

#[cfg(windows)]
fn window_exists(hwnd: u32) -> bool {
    // (IsWindow from user32, declared here: one call isn't worth a crate feature)
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsWindow(hwnd: isize) -> i32;
    }
    // SAFETY: IsWindow only checks a handle; any value is allowed.
    unsafe { IsWindow(hwnd as i32 as isize) != 0 }
}

/// A folder may hold a classic copy: not inside a sync service's folder (their file locks
/// break live mailboxes), not inside the library, and empty or made by an earlier export.
pub fn check_folder(library: &Library, dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() {
        return Err(Error::Other("choose a folder for the classic Eudora files".into()));
    }
    if let Some(service) = crate::library::in_sync_folder(dir) {
        return Err(Error::Other(format!("{} is inside {service}: choose a folder outside it (its file locking damages mailboxes classic Eudora has open)", dir.display())));
    }
    // (a folder not made yet: its nearest existing parent, resolved, plus the rest)
    let abs = |p: &Path| {
        let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let mut rest = Vec::new();
        let mut at = p.as_path();
        loop {
            if let Ok(real) = std::fs::canonicalize(at) {
                return rest.iter().rev().fold(real, |acc: PathBuf, part| acc.join(part));
            }
            match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_os_string());
                    at = parent;
                }
                _ => return p.clone(),
            }
        }
    };
    let (d, lib) = (abs(dir), abs(&library.root));
    if d.starts_with(&lib) || lib.starts_with(&d) {
        return Err(Error::Other("choose a folder outside Eudora's library".into()));
    }
    if dir.is_dir() && !dir.join(STATE_FILE).is_file() {
        let mut entries = std::fs::read_dir(dir).context(|| format!("reading {}", dir.display()))?;
        if entries.next().is_some() {
            return Err(Error::Other(format!(
                "{} isn't empty and wasn't made by Eudora's export: choose an empty folder (Eudora never writes into a classic Eudora folder it didn't make)",
                dir.display()
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ the writer

struct Writer<'a> {
    library: &'a Library,
    root: PathBuf,
    state: Connection,
    report: ClassicReport,
    /// The most a mailbox file may hold (`MBX_LIMIT`; smaller in tests).
    limit: u64,
}

/// How big Eudora lets a classic mailbox grow before carrying on in "<name> (2)". Classic
/// Eudora keeps every place in a mailbox as a signed 32-bit number (summary.h `long m_Offset`,
/// `JJFile::Seek(long)`), so it can't read past 2 GB (2,147,483,647 bytes): a summary there is
/// negative, `CTocDoc::Corrupt` rejects the .toc, and the mail beyond is out of reach. The
/// limit leaves about 340 MB under that for mail classic Eudora fetches or moves in itself.
pub const MBX_LIMIT: u64 = 1_800_000_000;
/// The largest file classic Eudora can read at all (one message alone can't be bigger).
const CLASSIC_MAX: u64 = i32::MAX as u64;

const STATE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS names (key TEXT PRIMARY KEY, rel TEXT NOT NULL UNIQUE COLLATE NOCASE, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mailboxes (key TEXT PRIMARY KEY, size INTEGER NOT NULL, toc_size INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS entries (
    key TEXT NOT NULL, message INTEGER NOT NULL, offset INTEGER NOT NULL, length INTEGER NOT NULL,
    seconds INTEGER NOT NULL, tz INTEGER NOT NULL, state INTEGER NOT NULL, flags INTEGER NOT NULL,
    flags_ex INTEGER NOT NULL, priority INTEGER NOT NULL, label INTEGER NOT NULL, from_ TEXT NOT NULL,
    subject TEXT NOT NULL, uid INTEGER NOT NULL, arrival INTEGER NOT NULL,
    PRIMARY KEY (key, message));
CREATE TABLE IF NOT EXISTS attachments (hash BLOB PRIMARY KEY, file TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// Exports into a folder classic Eudora opens as its mail folder (or brings an earlier export
/// up to date). Every message is written: a whole library takes as long as reading it all.
pub fn export(library: &Library, dest: &Path, options: &ClassicOptions, progress: &dyn Progress) -> Result<ClassicReport> {
    let report = update(library, dest, options, None, progress)?;
    if report.waiting.is_none() {
        write_atomically(&dest.join("Eudora export report.txt"), report_text(&report).as_bytes())?;
    }
    Ok(report)
}

/// Brings a classic folder up to date with the library: new messages appended, moved and
/// deleted ones taken out (a mailbox compacted when over a quarter of it is unused), status,
/// priority, and label changes written, rules and the address book written again when they
/// changed. `budget`: at most this many messages written this time (the download does a batch
/// each round); None = all.
pub fn update(library: &Library, dest: &Path, options: &ClassicOptions, budget: Option<u64>, progress: &dyn Progress) -> Result<ClassicReport> {
    update_with(library, dest, options, budget, MBX_LIMIT, progress)
}

fn update_with(library: &Library, dest: &Path, options: &ClassicOptions, budget: Option<u64>, limit: u64, progress: &dyn Progress) -> Result<ClassicReport> {
    let started = Instant::now();
    check_folder(library, dest)?;
    std::fs::create_dir_all(dest).context(|| format!("making {}", dest.display()))?;
    let mut report = ClassicReport { folder: dest.display().to_string(), ..Default::default() };
    if let Some(why) = classic_is_open(dest) {
        report.waiting = Some(why);
        return Ok(report);
    }
    // one writer at a time (an export from the window and the download's copy never overlap)
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dest.join(LOCK_FILE)).context(|| format!("opening {}", dest.display()))?;
    if lock.try_lock().is_err() {
        report.waiting = Some("another export into this folder is running".into());
        return Ok(report);
    }
    let state = Connection::open(dest.join(STATE_FILE))?;
    state.execute_batch(STATE_SCHEMA)?;
    let mut w = Writer { library, root: dest.to_path_buf(), state, report, limit };
    w.run(options, budget, progress)?;
    w.report.seconds = started.elapsed().as_secs_f64();
    drop(lock);
    Ok(w.report)
}

impl Writer<'_> {
    fn run(&mut self, options: &ClassicOptions, budget: Option<u64>, progress: &dyn Progress) -> Result<()> {
        progress.report(0, 0, "Classic Eudora: looking at the mailboxes…");
        let plan = self.library.classic_plan(options)?;
        let labels = self.library.label_list()?;
        let label_index: HashMap<String, i16> = labels.iter().take(7).enumerate().map(|(i, l)| (l.name.to_lowercase(), i as i16 + 1)).collect();
        let mut wider = HashSet::new();
        // each mailbox's file (stable once chosen) and what it should hold
        let mut work: Vec<(Planned, String, Vec<(i64, i64, i64)>)> = Vec::new();
        for p in &plan {
            let rel = self.file_for(p)?;
            let ids = if p.folders.is_empty() {
                Vec::new()
            } else {
                let list = p.folders.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
                let mut stmt = self.library.conn.prepare(&format!(
                    "SELECT m.id, m.flags, m.date FROM messages m WHERE m.id IN (SELECT DISTINCT message FROM locations WHERE folder IN ({list})) ORDER BY m.date, m.id"
                ))?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<(i64, i64, i64)>>>()?
            };
            work.push((p.clone(), rel, ids));
        }
        // how many will be written, for counted progress (a mailbox over the size limit carries
        // on in parts: "In (2)", "In (3)"…)
        let mut to_write = 0u64;
        let mut parts_of: Vec<Vec<String>> = Vec::new();
        for (p, _, ids) in &work {
            let parts = self.part_keys(&p.key)?;
            // (first, any part classic Eudora changed is kept beside: its mail is written again)
            for k in &parts {
                let rel: Option<String> = self.state.query_row("SELECT rel FROM names WHERE key = ?1", [k], |r| r.get(0)).optional()?;
                if let Some(rel) = rel {
                    self.keep_aside_if_changed(k, &rel)?;
                }
            }
            let mut have = HashSet::new();
            for k in &parts {
                have.extend(self.entry_ids(k)?);
            }
            to_write += ids.iter().filter(|(id, _, _)| !have.contains(id)).count() as u64;
            parts_of.push(parts);
        }
        let mut allowance = budget.unwrap_or(u64::MAX);
        let mut done = 0u64;
        let count = work.len();
        let mut every: Vec<Planned> = Vec::new();
        for (n, ((p, rel, ids), mut parts)) in work.iter().zip(parts_of).enumerate() {
            if progress.canceled() {
                return Err(Error::Canceled);
            }
            let mut have = HashSet::new();
            for k in &parts {
                have.extend(self.entry_ids(k)?);
            }
            let mut candidates: Vec<(i64, i64, i64)> = ids.iter().filter(|(id, _, _)| !have.contains(id)).copied().collect();
            let mut i = 0;
            loop {
                let part = if i == 0 { p.clone() } else { part_of(p, i + 1) };
                let part_rel = if i == 0 { rel.clone() } else { self.file_for(&part)? };
                let overflow = self.mailbox(&part, &part_rel, ids, &candidates, &label_index, &mut wider, &mut allowance, &mut done, to_write, (n + 1, count), progress)?;
                every.push(part);
                i += 1;
                if !overflow.is_empty() && i >= parts.len() {
                    parts.push(format!("{}#{}", p.key, i + 1));
                }
                candidates = overflow;
                if i >= parts.len() {
                    break;
                }
            }
        }
        self.report.left = to_write.saturating_sub(done);
        self.report.mailboxes = every.len() as u64;
        // mailboxes no longer exported (a folder deleted in Eudora)
        let planned: HashSet<String> = every.iter().map(|p| p.key.clone()).collect();
        self.forget_others(&planned, &every)?;
        self.descmaps(&every)?;
        if !wider.is_empty() {
            self.report.notes.push(format!(
                "Classic Eudora has seven labels: {} beyond the seventh aren't shown there",
                wider.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if !options.no_rules {
            self.rules(&work)?;
        }
        if !options.no_contacts {
            self.contacts(options)?;
        }
        self.settings_file(options, &labels)?;
        self.report.total = self.state.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))? as u64;
        Ok(())
    }

    /// A mailbox classic Eudora changed since Eudora wrote it (its size isn't what Eudora left:
    /// mail fetched there, messages moved): renamed "<name> (changed in classic Eudora <date>)"
    /// so nothing is lost, and forgotten, so Eudora writes its own again. True when it was.
    fn keep_aside_if_changed(&mut self, key: &str, rel: &str) -> Result<bool> {
        let mbx = self.path(rel);
        let toc = mbx.with_extension("toc");
        let recorded: Option<i64> = self.state.query_row("SELECT size FROM mailboxes WHERE key = ?1", [key], |r| r.get(0)).optional()?;
        let on_disk = std::fs::metadata(&mbx).ok().map(|m| m.len() as i64);
        let changed = match (recorded, on_disk) {
            (Some(size), Some(now)) => size != now,
            (None, Some(now)) => now > 0,
            _ => false,
        };
        if changed {
            let stamp = chrono::Local::now().format("%Y-%m-%d %H%M").to_string();
            let stem = mbx.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let aside = mbx.with_file_name(format!("{stem} (changed in classic Eudora {stamp}).mbx"));
            std::fs::rename(&mbx, &aside).context(|| format!("keeping {}", mbx.display()))?;
            if toc.exists() {
                let _ = std::fs::rename(&toc, aside.with_extension("toc"));
            }
            self.report.kept_aside.push(aside.display().to_string());
            self.state.execute("DELETE FROM entries WHERE key = ?1", [key])?;
            self.state.execute("DELETE FROM mailboxes WHERE key = ?1", [key])?;
        }
        Ok(changed)
    }

    /// A mailbox's parts written so far, in order: its own key, then "<key>#2", "<key>#3"….
    fn part_keys(&self, key: &str) -> Result<Vec<String>> {
        let mut stmt = self.state.prepare_cached("SELECT key FROM names WHERE substr(key, 1, length(?1) + 1) = ?1 || '#'")?;
        let mut more: Vec<(u32, String)> = stmt
            .query_map([key], |r| r.get::<_, String>(0))?
            .filter_map(|k| k.ok())
            .filter_map(|k| k[key.len() + 1..].parse::<u32>().ok().map(|n| (n, k)))
            .collect();
        more.sort();
        Ok(std::iter::once(key.to_string()).chain(more.into_iter().map(|(_, k)| k)).collect())
    }

    fn entry_ids(&self, key: &str) -> Result<HashSet<i64>> {
        let mut stmt = self.state.prepare_cached("SELECT message FROM entries WHERE key = ?1")?;
        Ok(stmt.query_map([key], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// The mailbox's file below the folder ("Clients.fol\Acme.mbx"), chosen once.
    fn file_for(&mut self, p: &Planned) -> Result<String> {
        let mut dir = String::new();
        for (key, name) in &p.parents {
            dir = self.name_in(key, name, &dir, ".fol")?;
        }
        match p.kind.file() {
            Some(file) => {
                let rel = format!("{file}.mbx");
                self.state.execute("INSERT OR IGNORE INTO names (key, rel, name) VALUES (?1, ?2, ?3)", params![p.key, rel, file])?;
                Ok(rel)
            }
            None => self.name_in(&p.key, &p.name, &dir, ".mbx"),
        }
    }

    fn name_in(&mut self, key: &str, name: &str, dir: &str, ext: &str) -> Result<String> {
        if let Some(rel) = self.state.query_row("SELECT rel FROM names WHERE key = ?1", [key], |r| r.get::<_, String>(0)).optional()? {
            self.state.execute("UPDATE names SET name = ?2 WHERE key = ?1", params![key, name])?;
            return Ok(rel);
        }
        let base = legal_name(name);
        let mut n = 1;
        loop {
            let stem = if n == 1 { base.clone() } else { format!("{base} {n}") };
            let at_root_reserved = dir.is_empty() && ["in", "out", "trash", "junk"].contains(&stem.to_ascii_lowercase().as_str());
            let rel = if dir.is_empty() { format!("{stem}{ext}") } else { format!("{dir}\\{stem}{ext}") };
            let taken = at_root_reserved
                || self.state.query_row("SELECT 1 FROM names WHERE rel = ?1", [&rel], |_| Ok(())).optional()?.is_some()
                || self.path(&rel).exists();
            if !taken {
                self.state.execute("INSERT INTO names (key, rel, name) VALUES (?1, ?2, ?3)", params![key, rel, name])?;
                return Ok(rel);
            }
            n += 1;
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        rel.split('\\').fold(self.root.clone(), |p, part| p.join(part))
    }

    /// Brings one mailbox file up to date: `ids` is everything the mailbox (all its parts)
    /// should hold, `candidates` the messages not written anywhere yet to add here. Returns the
    /// candidates that didn't fit (the file reached the size limit) for the next part.
    #[allow(clippy::too_many_arguments)]
    fn mailbox(
        &mut self,
        p: &Planned,
        rel: &str,
        ids: &[(i64, i64, i64)],
        candidates: &[(i64, i64, i64)],
        labels: &HashMap<String, i16>,
        wider: &mut HashSet<String>,
        allowance: &mut u64,
        done: &mut u64,
        to_write: u64,
        (n, count): (usize, usize),
        progress: &dyn Progress,
    ) -> Result<Vec<(i64, i64, i64)>> {
        let mbx = self.path(rel);
        let toc = mbx.with_extension("toc");
        if let Some(parent) = mbx.parent() {
            std::fs::create_dir_all(parent).context(|| format!("making {}", parent.display()))?;
        }
        // changed by classic Eudora since Eudora wrote it: kept beside, written again
        let changed_outside = self.keep_aside_if_changed(&p.key, rel)?;
        let recorded: Option<(i64, i64)> = self.state.query_row("SELECT size, toc_size FROM mailboxes WHERE key = ?1", [&p.key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let on_disk = std::fs::metadata(&mbx).ok().map(|m| m.len() as i64);
        let toc_on_disk = std::fs::metadata(&toc).ok().map(|m| m.len() as i64);
        let mut entries: Vec<Summary> = Vec::new();
        let mut by_message: HashMap<i64, usize> = HashMap::new();
        {
            let mut stmt = self.state.prepare_cached(
                "SELECT message, offset, length, seconds, tz, state, flags, flags_ex, priority, label, from_, subject, uid, arrival FROM entries WHERE key = ?1 ORDER BY offset",
            )?;
            let rows = stmt.query_map([&p.key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Summary {
                        offset: r.get::<_, i64>(1)? as u32,
                        length: r.get::<_, i64>(2)? as u32,
                        seconds: r.get(3)?,
                        tz_minutes: r.get(4)?,
                        state: r.get(5)?,
                        flags: r.get::<_, i64>(6)? as u16,
                        flags_ex: r.get::<_, i64>(7)? as u16,
                        priority: r.get::<_, i64>(8)? as i8,
                        label: r.get(9)?,
                        from: r.get(10)?,
                        subject: r.get(11)?,
                        uid: r.get::<_, i64>(12)? as u32,
                        arrival: r.get(13)?,
                        date: String::new(),
                    },
                ))
            })?;
            for row in rows {
                let (message, s) = row?;
                by_message.insert(message, entries.len());
                entries.push(s);
            }
        }
        let mut messages: Vec<i64> = vec![0; entries.len()];
        for (m, i) in &by_message {
            messages[*i] = *m;
        }
        let wanted: HashMap<i64, i64> = ids.iter().map(|(id, flags, _)| (*id, *flags)).collect();
        // every label on this mailbox's mail, read at once
        let mut on: HashMap<i64, Vec<String>> = HashMap::new();
        if !p.folders.is_empty() {
            let list = p.folders.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let mut stmt = self.library.conn.prepare(&format!(
                "SELECT message, label FROM categories WHERE message IN (SELECT message FROM locations WHERE folder IN ({list})) ORDER BY rowid"
            ))?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (m, l) = row?;
                on.entry(m).or_default().push(l);
            }
        }
        let mut dirty = changed_outside || toc_on_disk.is_none() || on_disk.is_none();
        if let (Some((_, toc_size)), Some(t)) = (recorded, toc_on_disk) {
            dirty |= toc_size != t; // (classic rewrote the .toc: Eudora's view goes back)
        }
        // taken out: moved or deleted in Eudora
        let before = entries.len();
        let mut keep_entries = Vec::with_capacity(entries.len());
        let mut keep_messages = Vec::with_capacity(entries.len());
        for (s, m) in entries.into_iter().zip(messages) {
            if wanted.contains_key(&m) {
                keep_entries.push(s);
                keep_messages.push(m);
            } else {
                self.state.execute("DELETE FROM entries WHERE key = ?1 AND message = ?2", params![p.key, m])?;
            }
        }
        let removed = (before - keep_entries.len()) as u64;
        self.report.removed += removed;
        dirty |= removed > 0;
        let (mut entries, mut messages) = (keep_entries, keep_messages);
        // status, priority, and label changes
        for (s, m) in entries.iter_mut().zip(&messages) {
            let (state, priority, label) = marks_of(wanted[m], p.sent, s.priority, on.get(m).map(Vec::as_slice).unwrap_or(&[]), labels, wider);
            if (state, priority, label) != (s.state, s.priority, s.label) {
                s.state = state;
                s.priority = priority;
                s.label = label;
                self.state.execute(
                    "UPDATE entries SET state = ?3, priority = ?4, label = ?5 WHERE key = ?1 AND message = ?2",
                    params![p.key, m, state, priority, label],
                )?;
                self.report.changed += 1;
                dirty = true;
            }
        }
        // new ones, appended
        let have: HashSet<i64> = messages.iter().copied().collect();
        let new: Vec<(i64, i64, i64)> = candidates.iter().filter(|(id, _, _)| !have.contains(id)).copied().collect();
        let mut overflow: Vec<(i64, i64, i64)> = Vec::new();
        let mut size = std::fs::metadata(&mbx).map(|m| m.len()).unwrap_or(0);
        if !new.is_empty() && *allowance > 0 {
            let file = OpenOptions::new().create(true).append(true).open(&mbx).context(|| format!("opening {}", mbx.display()))?;
            let mut out = BufWriter::new(file);
            let mut uid = entries.iter().map(|s| s.uid).max().unwrap_or(0);
            self.state.execute_batch("BEGIN")?; // (one save for the batch)
            let mut rest = new.into_iter();
            while let Some((id, flags, date)) = rest.next() {
                if *allowance == 0 {
                    break;
                }
                if progress.canceled() {
                    break;
                }
                // full: the rest goes on in the next part (classic can't read past 2 GB)
                if size >= self.limit {
                    overflow.push((id, flags, date));
                    overflow.extend(rest);
                    break;
                }
                let raw = self.library.raw_message(id)?;
                let c = self.convert(&raw, date, p.sent);
                let len = c.bytes.len() as u64;
                if size + len > self.limit && size > 0 {
                    overflow.push((id, flags, date));
                    overflow.extend(rest);
                    break;
                }
                if len > CLASSIC_MAX {
                    // (one message alone bigger than classic Eudora can read: said, not written)
                    self.report.notes.push(format!("“{}” is {} MB, bigger than classic Eudora can read (2 GB), so it wasn't written", c.subject, len / 1_000_000));
                    continue;
                }
                out.write_all(&c.bytes).context(|| format!("writing {}", mbx.display()))?;
                uid += 1;
                let (state, priority, label) = marks_of(flags, p.sent, c.priority.map_or(3, |x| x as i8), on.get(&id).map(Vec::as_slice).unwrap_or(&[]), labels, wider);
                let s = Summary {
                    offset: size as u32,
                    length: c.bytes.len() as u32,
                    seconds: c.seconds,
                    tz_minutes: c.tz_minutes,
                    state,
                    flags: if c.attachments > 0 { MSF_HAS_ATTACHMENT | MSF_MIME } else { 0 },
                    flags_ex: if c.html { MSFEX_HTML } else { 0 },
                    priority,
                    label,
                    date: String::new(),
                    from: c.from.clone(),
                    subject: c.subject.clone(),
                    uid,
                    arrival: c.seconds,
                };
                self.state.execute(
                    "INSERT OR REPLACE INTO entries (key, message, offset, length, seconds, tz, state, flags, flags_ex, priority, label, from_, subject, uid, arrival)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![p.key, id, s.offset, s.length, s.seconds, s.tz_minutes, s.state, s.flags, s.flags_ex, s.priority, s.label, s.from, s.subject, s.uid, s.arrival],
                )?;
                size += c.bytes.len() as u64;
                entries.push(s);
                messages.push(id);
                self.report.written += 1;
                self.report.as_html += u64::from(c.as_html);
                self.report.unreadable += u64::from(c.unreadable);
                self.report.notes.extend(c.notes);
                *allowance -= 1;
                *done += 1;
                dirty = true;
                if *done % 50 == 0 || *done == to_write {
                    progress.report(*done, to_write, &format!("Classic Eudora: writing message {} of {} (mailbox {n} of {count}: {})", thousands(*done), thousands(to_write), p.name));
                }
            }
            out.flush().context(|| format!("writing {}", mbx.display()))?;
            drop(out);
            self.state.execute_batch("COMMIT")?;
        }
        if !dirty && on_disk.is_some() {
            return Ok(overflow);
        }
        // compact: rewrite the mailbox when over a quarter of it is mail taken out
        let used: u64 = entries.iter().map(|s| u64::from(s.length)).sum();
        let unused = size.saturating_sub(used);
        if unused > 0 && (unused * 4 > size || entries.is_empty()) {
            self.compact(&p.key, &mbx, &mut entries, &messages)?;
            size = std::fs::metadata(&mbx).map(|m| m.len()).unwrap_or(0);
            self.report.compacted += 1;
        }
        if !mbx.exists() {
            File::create(&mbx).context(|| format!("making {}", mbx.display()))?;
        }
        // the .toc last, so it's newer than its mailbox (classic rebuilds one older than it)
        let bytes = toc_bytes(&p.name, p.kind.mbt(), size, &entries);
        write_atomically(&toc, &bytes)?;
        self.state.execute(
            "INSERT OR REPLACE INTO mailboxes (key, size, toc_size) VALUES (?1, ?2, ?3)",
            params![p.key, size as i64, bytes.len() as i64],
        )?;
        Ok(overflow)
    }

    /// Rewrites a mailbox with only the messages it still holds (classic's Compact).
    fn compact(&mut self, key: &str, mbx: &Path, entries: &mut [Summary], messages: &[i64]) -> Result<()> {
        let tmp = mbx.with_extension("mbx.compacting");
        {
            let mut from = File::open(mbx).context(|| format!("reading {}", mbx.display()))?;
            let mut out = BufWriter::new(File::create(&tmp).context(|| format!("writing {}", tmp.display()))?);
            let mut at = 0u64;
            let mut buf = Vec::new();
            for s in entries.iter_mut() {
                buf.resize(s.length as usize, 0);
                from.seek(SeekFrom::Start(u64::from(s.offset))).context(|| format!("reading {}", mbx.display()))?;
                from.read_exact(&mut buf).context(|| format!("reading {}", mbx.display()))?;
                out.write_all(&buf).context(|| format!("writing {}", tmp.display()))?;
                s.offset = at as u32;
                at += u64::from(s.length);
            }
            out.flush().context(|| format!("writing {}", tmp.display()))?;
            out.get_ref().sync_all().context(|| format!("writing {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, mbx).context(|| format!("replacing {}", mbx.display()))?;
        let tx = self.state.unchecked_transaction()?;
        for (s, m) in entries.iter().zip(messages) {
            tx.execute("UPDATE entries SET offset = ?3 WHERE key = ?1 AND message = ?2", params![key, m, s.offset])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Mailboxes written before that aren't exported any more: removed when classic Eudora
    /// didn't change them, else kept under a name of their own.
    fn forget_others(&mut self, planned: &HashSet<String>, plan: &[Planned]) -> Result<()> {
        let old: Vec<(String, String, i64)> = {
            let mut stmt = self.state.prepare("SELECT n.key, n.rel, COALESCE(m.size, -1) FROM names n LEFT JOIN mailboxes m ON m.key = n.key WHERE n.rel LIKE '%.mbx'")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (key, rel, size) in old {
            if planned.contains(&key) {
                continue;
            }
            let mbx = self.path(&rel);
            let now = std::fs::metadata(&mbx).map(|m| m.len() as i64).unwrap_or(-1);
            if now >= 0 && now != size {
                self.report.kept_aside.push(mbx.display().to_string());
            } else {
                let _ = std::fs::remove_file(&mbx);
                let _ = std::fs::remove_file(mbx.with_extension("toc"));
            }
            self.state.execute("DELETE FROM entries WHERE key = ?1", [&key])?;
            self.state.execute("DELETE FROM mailboxes WHERE key = ?1", [&key])?;
            self.state.execute("DELETE FROM names WHERE key = ?1", [&key])?;
        }
        // folders (.fol) no mailbox sits in any more: removed when empty
        let used: HashSet<String> = plan.iter().flat_map(|p| p.parents.iter().map(|(k, _)| k.clone())).collect();
        let dirs: Vec<(String, String)> = {
            let mut stmt = self.state.prepare("SELECT key, rel FROM names WHERE rel LIKE '%.fol' ORDER BY length(rel) DESC")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (key, rel) in dirs {
            if used.contains(&key) {
                continue;
            }
            let at = self.path(&rel);
            let _ = std::fs::remove_file(at.join("descmap.pce"));
            if std::fs::remove_dir(&at).is_err() && at.exists() {
                let _ = std::fs::write(at.join("descmap.pce"), b"");
                self.report.notes.push(format!("{} is no longer exported but holds other files, so it was left there", at.display()));
            }
            self.state.execute("DELETE FROM names WHERE key = ?1", [&key])?;
        }
        Ok(())
    }

    /// descmap.pce in the folder and every .fol below it: each mailbox's and folder's name.
    fn descmaps(&mut self, plan: &[Planned]) -> Result<()> {
        // directory rel → lines
        let mut dirs: BTreeMap<String, Vec<(u8, String)>> = BTreeMap::new();
        dirs.insert(String::new(), Vec::new());
        let mut seen_dirs = HashSet::new();
        for p in plan {
            let rel: String = self.state.query_row("SELECT rel FROM names WHERE key = ?1", [&p.key], |r| r.get(0))?;
            let mut parent = String::new();
            for (key, name) in &p.parents {
                let drel: String = self.state.query_row("SELECT rel FROM names WHERE key = ?1", [key], |r| r.get(0))?;
                if seen_dirs.insert(drel.clone()) {
                    let file = drel.rsplit('\\').next().unwrap_or(&drel).to_string();
                    dirs.entry(parent.clone()).or_default().push((2, format!("{},{},F,N", map_name(name), file)));
                    dirs.entry(drel.clone()).or_default();
                }
                parent = drel;
            }
            let file = rel.rsplit('\\').next().unwrap_or(&rel).to_string();
            let (order, kind) = match p.kind {
                Kind::Regular => (1, 'M'),
                _ => (0, 'S'),
            };
            dirs.entry(parent).or_default().push((order, format!("{},{},{kind},U", map_name(&p.name), file)));
        }
        for (dir, mut lines) in dirs {
            lines.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
            let mut text = Vec::new();
            for (_, line) in lines {
                text.extend(ansi(&line, 255));
                text.extend_from_slice(b"\r\n");
            }
            let at = if dir.is_empty() { self.root.clone() } else { self.path(&dir) };
            std::fs::create_dir_all(&at).context(|| format!("making {}", at.display()))?;
            write_atomically(&at.join("descmap.pce"), &text)?;
        }
        Ok(())
    }

    fn changed(&self, key: &str, content: &[u8]) -> Result<bool> {
        let hash = blake3::hash(content).to_hex().to_string();
        let old: Option<String> = self.state.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?;
        if old.as_deref() == Some(hash.as_str()) {
            return Ok(false);
        }
        self.state.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![key, hash])?;
        Ok(true)
    }

    fn rules(&mut self, work: &[(Planned, String, Vec<(i64, i64, i64)>)]) -> Result<()> {
        let rules = self.library.rules()?;
        let labels = self.library.label_list()?;
        // each exported folder by (account address, path) → its mailbox file
        let mut files: HashMap<(String, String), String> = HashMap::new();
        let mut by_folder: HashMap<i64, String> = HashMap::new();
        for (p, rel, _) in work {
            for f in &p.folders {
                by_folder.insert(*f, rel.clone());
            }
        }
        {
            let mut stmt = self.library.conn.prepare("SELECT f.id, lower(COALESCE(a.address, '')), f.path FROM folders f JOIN accounts a ON a.id = f.account")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            for row in rows {
                let (id, address, path) = row?;
                if let Some(rel) = by_folder.get(&id) {
                    files.insert((address.clone(), path.to_lowercase()), rel.clone());
                    files.insert((address, shown_path(&path).to_lowercase()), rel.clone());
                }
            }
        }
        let find = |account: &Option<String>, folder: &str| -> Option<String> {
            let folder = folder.to_lowercase();
            match account {
                Some(a) => files.get(&(a.to_lowercase(), folder.clone())).cloned(),
                None => files.iter().find(|((_, f), _)| *f == folder).map(|(_, r)| r.clone()),
            }
        };
        let (text, written, left_out) = classic_filters(&rules, &find, &labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>());
        self.report.rules_written = written;
        self.report.rules_left_out = left_out;
        let path = self.root.join("Filters.pce");
        if self.changed("filters", &text)? || !path.exists() {
            write_atomically(&path, &text)?;
        }
        Ok(())
    }

    fn contacts(&mut self, options: &ClassicOptions) -> Result<()> {
        let cards: Vec<Card> = {
            let mut stmt = self.library.conn.prepare("SELECT account, card FROM cards ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (account, json) = row?;
                if !options.accounts.is_empty() && !options.accounts.contains(&account) {
                    continue;
                }
                if let Ok(card) = serde_json::from_str::<Card>(&json) {
                    out.push(card);
                }
            }
            out
        };
        let (text, count) = nicknames(&cards);
        self.report.contacts = count;
        let path = self.root.join("NNdbase.txt");
        if self.changed("nicknames", &text)? || !path.exists() {
            write_atomically(&path, &text)?;
        }
        Ok(())
    }

    /// A first eudora.ini (never written over: classic keeps its own settings there).
    fn settings_file(&mut self, options: &ClassicOptions, labels: &[crate::marks::LabelDef]) -> Result<()> {
        let path = self.root.join("eudora.ini");
        if path.exists() {
            return Ok(());
        }
        let accounts: Vec<(String, String)> = self
            .library
            .accounts()?
            .into_iter()
            .filter(|a| options.accounts.is_empty() || options.accounts.contains(&a.id))
            .filter(|a| a.kind != "graph-shared")
            .filter_map(|a| a.address.clone().filter(|x| x.contains('@')).map(|x| (x, a.name.clone())))
            .collect();
        let mut ini = String::from("; Written by Eudora's Export to Classic Eudora. Classic Eudora keeps its own settings here from now on.\r\n");
        if let Some((address, name)) = accounts.first() {
            ini.push_str(&format!("[Settings]\r\nReturnAddress={address}\r\nPOPAccount={address}\r\nRealName={name}\r\n"));
        }
        if accounts.len() > 1 {
            ini.push_str("[Personalities]\r\n");
            for (i, (address, _)) in accounts.iter().skip(1).enumerate() {
                ini.push_str(&format!("Persona{i}=Persona-{address}\r\n"));
            }
            for (address, name) in accounts.iter().skip(1) {
                ini.push_str(&format!("[Persona-{address}]\r\nReturnAddress={address}\r\nPOPAccount={address}\r\nRealName={name}\r\n"));
            }
        }
        ini.push_str("[Labels]\r\n");
        for (i, l) in labels.iter().take(7).enumerate() {
            let hex = l.color.trim_start_matches('#');
            let c = |k: usize| u8::from_str_radix(hex.get(k..k + 2).unwrap_or("00"), 16).unwrap_or(0);
            ini.push_str(&format!("Label{}={},{},{},{}\r\n", i + 1, c(0), c(2), c(4), l.name));
        }
        write_atomically(&path, &ansi(&ini, usize::MAX))
    }
}

/// A message's status, priority, and label as classic keeps them (`on`: its labels).
fn marks_of(flags: i64, sent: bool, priority_now: i8, on: &[String], labels: &HashMap<String, i16>, wider: &mut HashSet<String>) -> (i16, i8, i16) {
    let state = if sent {
        MS_SENT
    } else if flags & marks::UNREAD != 0 {
        MS_UNREAD
    } else if flags & marks::REDIRECTED != 0 {
        MS_REDIRECT
    } else if flags & marks::FORWARDED != 0 {
        MS_FORWARDED
    } else if flags & marks::REPLIED != 0 {
        MS_REPLIED
    } else {
        MS_READ
    };
    let priority = marks::priority_of(flags).map_or(priority_now, |p| p as i8);
    let mut label = 0;
    for name in on {
        match labels.get(&name.to_lowercase()) {
            Some(i) if label == 0 => label = *i,
            Some(_) => {}
            None => {
                wider.insert(name.clone());
            }
        }
    }
    (state, priority, label)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.writing", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::write(&tmp, bytes).context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).context(|| format!("writing {}", path.display()))
}

// ------------------------------------------------------------------ rules

fn classic_verb(v: Verb) -> &'static str {
    match v {
        Verb::Contains => "contains",
        Verb::NotContains => "!contains",
        Verb::Is => "is",
        Verb::IsNot => "!is",
        Verb::StartsWith => "starts",
        Verb::EndsWith => "ends",
        Verb::Appears => "appears",
        Verb::NotAppears => "!appears",
        Verb::Regex => "regex",
        Verb::RegexIgnoreCase => "regex_icase",
        Verb::Greater => "greater",
        Verb::Less => "less",
    }
}

fn classic_status(name: &str) -> Option<i16> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "unread" | "new" | "0" => MS_UNREAD,
        "read" | "1" => MS_READ,
        "replied" | "2" => MS_REPLIED,
        "forwarded" | "3" => MS_FORWARDED,
        "redirected" | "redirect" | "4" => MS_REDIRECT,
        _ => return None,
    })
}

/// The rules as classic's Filters.pce (`CFiltersDoc::Write`, version 3, Windows-1252), how many
/// were written, and those left out with why. `mailbox` finds an exported folder's file.
pub fn classic_filters(rules: &[Rule], mailbox: &dyn Fn(&Option<String>, &str) -> Option<String>, labels: &[String]) -> (Vec<u8>, u64, Vec<(String, String)>) {
    let mut text = String::from("3\r\n");
    let mut written = 0;
    let mut left_out = Vec::new();
    for rule in rules {
        let name = if rule.name.trim().is_empty() { "Untitled".to_string() } else { rule.name.trim().replace(['\r', '\n'], " ") };
        let mut why: Option<String> = None;
        if !rule.enabled {
            why = Some("it's switched off in Eudora (classic Eudora has no off switch: it would run)".into());
        } else if rule.tests.len() > 2 {
            why = Some(format!("it has {} tests; classic Eudora's rules have two", rule.tests.len()));
        } else if rule.tests.first().is_some_and(|t| t.unless) {
            why = Some("its first test is an “unless” test, which classic Eudora can't start with".into());
        } else if rule.tests.get(1).is_some_and(|t| t.unless) && !rule.all {
            why = Some("it mixes “any of the tests” with an “unless” test".into());
        } else if rule.actions.len() > 5 {
            why = Some(format!("it has {} actions; classic Eudora's rules have five", rule.actions.len()));
        }
        let mut actions = Vec::new();
        if why.is_none() {
            for action in &rule.actions {
                let line = match action {
                    Action::Transfer { account, folder } => mailbox(account, folder).map(|f| format!("transfer {f}")).ok_or_else(|| format!("its mailbox “{folder}” isn't in the export")),
                    Action::Copy { account, folder } => mailbox(account, folder).map(|f| format!("copy {f}")).ok_or_else(|| format!("its mailbox “{folder}” isn't in the export")),
                    Action::Delete => Ok("transfer Trash.mbx".into()),
                    Action::Label { label } => {
                        let n = label.trim().parse::<usize>().ok().filter(|n| (1..=7).contains(n)).or_else(|| labels.iter().take(7).position(|l| l.eq_ignore_ascii_case(label.trim())).map(|i| i + 1));
                        n.map(|n| format!("label {n}")).ok_or_else(|| format!("its label “{label}” isn't one of classic Eudora's seven"))
                    }
                    Action::Priority { level: -1 } => Ok("raise".into()),
                    Action::Priority { level: -2 } => Ok("lower".into()),
                    Action::Priority { level } if (1..=5).contains(level) => Ok(format!("priority {level}")),
                    Action::Priority { level } => Err(format!("priority {level} isn't one classic Eudora has")),
                    Action::Status { status } => classic_status(status).map(|s| format!("status {s}")).ok_or_else(|| format!("the status “{status}” isn't one classic Eudora has")),
                    Action::Junk => Ok("junk ".into()),
                    Action::Subject { text } => Ok(format!("subject {text}")),
                    Action::Forward { to } => Ok(format!("forward {to}")),
                    Action::Redirect { to } => Ok(format!("redirect {to}")),
                    Action::Reply { with } if Path::new(with.trim()).is_file() => Ok(format!("reply {}", with.trim())),
                    Action::Reply { .. } => Err("its reply is words of its own; classic Eudora replies with a stationery file".into()),
                    Action::Notify => Ok("notifyUser 1".into()),
                    Action::Speak { who, subject } => Ok(format!("speak {} ", u8::from(*who) | (u8::from(*subject) << 1))),
                    Action::Sound { file } => Ok(format!("sound {file}")),
                    Action::Stop => Ok("stop ".into()),
                    Action::Sort => Err("it runs Eudora's spam filter, which classic Eudora doesn't have".into()),
                    Action::Plugin { plugin, .. } => Err(format!("it runs the plugin “{plugin}”, which classic Eudora doesn't have")),
                };
                match line {
                    Ok(l) => actions.push(l),
                    Err(e) => {
                        why = Some(e);
                        break;
                    }
                }
            }
        }
        if let Some(why) = why {
            left_out.push((name, why));
            continue;
        }
        text.push_str(&format!("rule {name}\r\n"));
        for a in actions {
            text.push_str(&a.replace(['\r', '\n'], " "));
            text.push_str("\r\n");
        }
        if rule.incoming {
            text.push_str("incoming\r\n");
        }
        if rule.outgoing {
            text.push_str("outgoing\r\n");
        }
        if rule.manual {
            text.push_str("manual\r\n");
        }
        let empty = crate::rules::Test::default();
        let first = rule.tests.first().unwrap_or(&empty);
        let second = rule.tests.get(1);
        let conjunction = match second {
            None => "ignore",
            Some(t) if t.unless => "unless",
            Some(_) if rule.all => "and",
            Some(_) => "or",
        };
        for (k, t) in [Some(first), second].into_iter().enumerate() {
            let t = t.unwrap_or(&empty);
            text.push_str(&format!("header {}\r\n", t.field.replace(['\r', '\n'], " ")));
            text.push_str(&format!("verb {}\r\n", classic_verb(t.verb)));
            text.push_str(&format!("value {}\r\n", t.value.replace(['\r', '\n'], " ")));
            if k == 0 {
                text.push_str(&format!("conjunction {conjunction}\r\n"));
            }
        }
        written += 1;
    }
    (ansi(&text, usize::MAX), written, left_out)
}

// ------------------------------------------------------------------ the address book

/// The address book as classic's NNdbase.txt (`alias` and `note` lines; a note's line breaks as
/// character 3, as nickdoc.cpp keeps them), and how many entries.
pub fn nicknames(cards: &[Card]) -> (Vec<u8>, u64) {
    let mut used: HashSet<String> = HashSet::new();
    let mut out = String::new();
    let mut count = 0;
    let clean = |s: &str| s.replace(['<', '>'], " ").replace("\r\n", "\u{3}").replace(['\r', '\n'], "\u{3}");
    for card in cards {
        let emails: Vec<&str> = card.emails.iter().map(|e| e.value.trim()).filter(|e| !e.is_empty()).collect();
        if emails.is_empty() && card.shown().trim().is_empty() {
            continue;
        }
        let base = [card.nickname.trim(), card.shown().trim()].into_iter().find(|s| !s.is_empty()).unwrap_or("contact").replace('"', "'");
        let mut nick = base.clone();
        let mut n = 1;
        while !used.insert(nick.to_lowercase()) {
            n += 1;
            nick = format!("{base} {n}");
        }
        let quoted = if nick.contains([' ', '\t']) { format!("\"{nick}\"") } else { nick.clone() };
        out.push_str(&format!("alias {quoted} {}\r\n", emails.first().copied().unwrap_or("")));
        let mut fields = vec![("name", card.shown()), ("first", card.given.clone()), ("last", card.family.clone())];
        if emails.len() > 1 {
            fields.push(("otheremail", emails[1..].join("\u{3}")));
        }
        let phone = |kind: &str| card.phones.iter().find(|p| p.kind.eq_ignore_ascii_case(kind)).map(|p| p.value.clone());
        if let Some(p) = phone("mobile").or_else(|| phone("cell")) {
            fields.push(("mobile", p));
        }
        let home = phone("home").or_else(|| card.phones.iter().find(|p| !["mobile", "cell", "work", "business", "fax"].contains(&p.kind.to_ascii_lowercase().as_str())).map(|p| p.value.clone()));
        if let Some(p) = home {
            fields.push(("phone", p));
        }
        if let Some(p) = phone("work").or_else(|| phone("business")) {
            fields.push(("phone2", p));
        }
        if let Some(p) = phone("fax") {
            fields.push(("fax", p));
        }
        for (i, a) in card.addresses.iter().take(2).enumerate() {
            fields.push((if i == 0 { "address" } else { "address2" }, a.street.clone()));
            let pick = |k: &'static str, v: &str| (k, v.to_string());
            let (city, state, zip, country) = if i == 0 { ("city", "state", "zip", "country") } else { ("city2", "state2", "zip2", "country2") };
            fields.push(pick(city, &a.city));
            fields.push(pick(state, &a.region));
            fields.push(pick(zip, &a.postal_code));
            fields.push(pick(country, &a.country));
        }
        if let Some(web) = card.websites.first() {
            fields.push(("web", web.clone()));
        }
        fields.push(("title", card.title.clone()));
        fields.push(("company", card.company.clone()));
        let mut note = String::new();
        for (k, v) in fields {
            if !v.trim().is_empty() {
                note.push_str(&format!("<{k}:{}>", clean(v.trim())));
            }
        }
        note.push_str(&clean(card.notes.trim()));
        if !note.is_empty() {
            out.push_str(&format!("note {quoted} {note}\r\n"));
        }
        count += 1;
    }
    (ansi(&out, usize::MAX), count)
}

// ------------------------------------------------------------------ the copy kept current

/// One round of the classic copy kept current (the download, the one writer, calls it):
/// at most `budget` messages written. None when it's off.
pub fn mirror_round(library: &Library, budget: u64) -> Result<Option<ClassicReport>> {
    let Some(m) = library.classic_mirror()? else { return Ok(None) };
    let options = ClassicOptions { accounts: m.accounts.clone(), folders: Vec::new(), no_rules: m.no_rules, no_contacts: m.no_contacts };
    update(library, Path::new(&m.folder), &options, Some(budget), &crate::progress::Silent).map(Some)
}

/// A short account of a report, for Activity, the window, and the command line.
pub fn said(r: &ClassicReport) -> String {
    if let Some(w) = &r.waiting {
        return format!("Waiting: {w}");
    }
    let mut parts = vec![format!("{} written, {} taken out", thousands(r.written), thousands(r.removed))];
    if r.changed > 0 {
        parts.push(format!("{} marked anew", thousands(r.changed)));
    }
    if r.left > 0 {
        parts.push(format!("{} still to write", thousands(r.left)));
    }
    parts.push(format!("{} in {} mailboxes", thousands(r.total), r.mailboxes));
    if !r.rules_left_out.is_empty() {
        parts.push(format!("{} rules left out", r.rules_left_out.len()));
    }
    parts.join("; ")
}

/// The report as text a person reads (the file `Eudora export report.txt` beside the mail).
pub fn report_text(r: &ClassicReport) -> String {
    let mut t = String::new();
    t.push_str(&format!("Eudora: export to classic Eudora\r\nFolder: {}\r\nWhen: {}\r\n\r\n", r.folder, chrono::Local::now().format("%Y-%m-%d %H:%M")));
    if let Some(w) = &r.waiting {
        t.push_str(&format!("Nothing was written: {w}.\r\n"));
        return t;
    }
    t.push_str(&format!(
        "Mailboxes: {}\r\nMessages written: {}\r\nMessages taken out (moved or deleted in Eudora): {}\r\nMessages marked anew (status, priority, label): {}\r\nMessages in the classic folder: {}\r\nAttachments written into Attach: {}\r\nMailboxes compacted: {}\r\n",
        r.mailboxes, thousands(r.written), thousands(r.removed), thousands(r.changed), thousands(r.total), thousands(r.attachments), r.compacted
    ));
    if r.left > 0 {
        t.push_str(&format!("Still to write: {} (the next update writes them)\r\n", thousands(r.left)));
    }
    if r.as_html > 0 {
        t.push_str(&format!("Messages whose text has characters classic Eudora's character set lacks, written as HTML so none is lost: {}\r\n", thousands(r.as_html)));
    }
    if r.unreadable > 0 {
        t.push_str(&format!("Messages Eudora couldn't read, written whole as stored: {}\r\n", thousands(r.unreadable)));
    }
    t.push_str(&format!("Rules written to Filters.pce: {}\r\nAddress book entries written to NNdbase.txt: {}\r\n", r.rules_written, r.contacts));
    if !r.rules_left_out.is_empty() {
        t.push_str("\r\nRules left out (classic Eudora can't do them):\r\n");
        for (name, why) in &r.rules_left_out {
            t.push_str(&format!("  {name}: {why}\r\n"));
        }
    }
    if !r.kept_aside.is_empty() {
        t.push_str("\r\nMailboxes changed in classic Eudora, kept as they were under these names:\r\n");
        for k in &r.kept_aside {
            t.push_str(&format!("  {k}\r\n"));
        }
    }
    if !r.notes.is_empty() {
        t.push_str("\r\nNotes:\r\n");
        for n in &r.notes {
            t.push_str(&format!("  {n}\r\n"));
        }
    }
    t.push_str("\r\nOpen this folder in classic Eudora as its mail folder (classic Eudora 7: start it with this folder, or choose it when it asks where your mail is). Changes made in classic Eudora don't come back to Eudora.\r\n");
    t
}

#[cfg(test)]
mod tests;
