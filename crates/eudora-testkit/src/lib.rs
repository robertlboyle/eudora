//! Synthetic mail corpora for Eudora's tests and benchmarks.
//!
//! `generate` writes a folder tree of mailboxes (mbox, most of them; one written with CRLF line
//! ends, one as mboxcl2 with Content-Length and no "From " escaping; one as EML files) and a
//! `manifest.json` that says exactly what an import must find: messages, duplicates, copies in
//! other folders, and the planted search "needles" with how many messages contain each.
//!
//! The mail spans 1994-2026 and carries what old mail really has: no MIME, raw 8-bit text with
//! no charset, Windows-1252 quotes labeled ISO-8859-1, 8-bit headers, two-digit years, broken
//! and missing dates, missing Message-IDs, uuencoded and BinHex attachments, "From " lines in
//! bodies, then MIME, HTML, encoded words and attachments (some shared between messages).
//! Every message is a pure function of (seed, slot): runs are reproducible and parallel.

pub mod encode;
pub mod rng;
pub mod words;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::rng::{Rng, mix};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// mboxrd, LF line ends
    Mbox,
    /// mboxrd with CRLF line ends (messages written with CRLF too)
    MboxCrlf,
    /// mboxcl2: Content-Length header, no "From " escaping
    MboxCl2,
    /// one .eml file per message
    Eml,
}

pub struct FolderSpec {
    pub path: &'static str,
    pub weight: u32,
    pub format: Format,
}

pub const FOLDERS: &[FolderSpec] = &[
    FolderSpec { path: "Inbox", weight: 36, format: Format::Mbox },
    FolderSpec { path: "Sent", weight: 14, format: Format::Mbox },
    FolderSpec { path: "Archive", weight: 10, format: Format::Mbox },
    FolderSpec { path: "Clients/Acme", weight: 5, format: Format::Mbox },
    FolderSpec { path: "Clients/Globex", weight: 4, format: Format::Mbox },
    FolderSpec { path: "Clients/Initech", weight: 3, format: Format::Mbox },
    FolderSpec { path: "Lists/rust-users", weight: 6, format: Format::Mbox },
    FolderSpec { path: "Lists/eudora-fans", weight: 3, format: Format::Mbox },
    FolderSpec { path: "Receipts", weight: 4, format: Format::Mbox },
    FolderSpec { path: "Family", weight: 3, format: Format::Mbox },
    FolderSpec { path: "Travel", weight: 2, format: Format::Mbox },
    FolderSpec { path: "Old Windows Mail", weight: 4, format: Format::MboxCrlf },
    FolderSpec { path: "Solaris Archive", weight: 3, format: Format::MboxCl2 },
    FolderSpec { path: "EML Export", weight: 3, format: Format::Eml },
];

/// Phrases planted in a known number of messages, for checking search.
pub struct NeedleSpec {
    pub name: &'static str,
    /// Where it's planted: "body", "subject", "from" (an address) or "attachment" (a file name).
    pub field: &'static str,
    pub text: &'static str,
    /// The search that should find exactly the planted messages.
    pub query: &'static str,
    /// About one message in this many.
    pub every: u64,
}

pub const NEEDLES: &[NeedleSpec] = &[
    NeedleSpec { name: "body-rare", field: "body", text: "quokka zeppelin", query: "\"quokka zeppelin\"", every: 50_000 },
    NeedleSpec { name: "body-common", field: "body", text: "marmalade cartographer", query: "\"marmalade cartographer\"", every: 2_000 },
    NeedleSpec { name: "subject", field: "subject", text: "Velociraptor Tuesday", query: "subject:\"velociraptor tuesday\"", every: 20_000 },
    NeedleSpec { name: "sender", field: "from", text: "needle.sender@haystack.example", query: "from:\"needle.sender@haystack.example\"", every: 10_000 },
    NeedleSpec { name: "attachment", field: "attachment", text: "zanzibar-plans.pdf", query: "attachment:\"zanzibar plans\"", every: 30_000 },
];

#[derive(Clone, Debug)]
pub struct Options {
    pub messages: u64,
    pub seed: u64,
    /// Scales attachment sizes (1.0 = realistic; tests use less to stay quick).
    pub attachment_scale: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options { messages: 10_000, seed: 27, attachment_scale: 1.0 }
    }
}

/// What an import of the corpus must find (written as manifest.json).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub seed: u64,
    /// Messages written (every slot).
    pub written: u64,
    pub bytes: u64,
    /// Distinct messages an import should store: originals + Message-ID variants.
    pub unique: u64,
    /// Byte-identical repeats in the same folder (skipped as duplicates).
    pub duplicates: u64,
    /// Byte-identical copies in another folder (one message, one more location).
    pub copies: u64,
    /// Same Message-ID, different bytes (extra Received line): separate messages for now.
    pub id_variants: u64,
    /// Folder path (relative to the corpus's mail folder, as an import names it) -> locations.
    pub folders: BTreeMap<String, u64>,
    /// Needle name -> messages containing it (distinct messages).
    pub needles: BTreeMap<String, u64>,
    /// Old-mail features -> how many distinct messages have them.
    pub features: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Original,
    Duplicate(u64),
    Copy(u64),
    Variant(u64),
}

fn slot_kind(s: u64) -> Slot {
    if s < 1000 {
        return Slot::Original;
    }
    match s % 200 {
        37 | 137 => Slot::Duplicate(0),
        81 => Slot::Copy(0),
        163 => Slot::Variant(0),
        _ => Slot::Original,
    }
}

fn folder_of(seed: u64, s: u64) -> usize {
    let total: u64 = FOLDERS.iter().map(|f| f.weight as u64).sum();
    let mut x = mix(seed, s, 1) % total;
    for (i, f) in FOLDERS.iter().enumerate() {
        if x < f.weight as u64 {
            return i;
        }
        x -= f.weight as u64;
    }
    0
}

/// An earlier original slot to repeat (never an mboxcl2 one: its Content-Length header would
/// make it read differently elsewhere).
fn reference(seed: u64, s: u64, stream: u64, avoid_needles: bool) -> u64 {
    let span = (s - 1).max(1).min(20_000);
    let mut k = (s - 1).saturating_sub(mix(seed, s, stream) % span);
    loop {
        let ok = slot_kind(k) == Slot::Original
            && FOLDERS[folder_of(seed, k)].format != Format::MboxCl2
            && !(avoid_needles && !needles_of(seed, k).is_empty());
        if ok || k == 0 {
            return k;
        }
        k -= 1;
    }
}

fn resolve(seed: u64, s: u64) -> Slot {
    match slot_kind(s) {
        Slot::Original => Slot::Original,
        Slot::Duplicate(_) => Slot::Duplicate(reference(seed, s, 2, false)),
        Slot::Copy(_) => Slot::Copy(reference(seed, s, 3, false)),
        Slot::Variant(_) => Slot::Variant(reference(seed, s, 4, true)),
    }
}

fn needles_of(seed: u64, s: u64) -> Vec<usize> {
    if slot_kind(s) != Slot::Original {
        return Vec::new();
    }
    NEEDLES.iter().enumerate().filter(|(i, n)| mix(seed, s, 100 + *i as u64) % n.every == 0).map(|(i, _)| i).collect()
}

/// When a message was sent: more mail in later years (1994-2026).
fn date_of(seed: u64, s: u64) -> i64 {
    let mut r = Rng::for_item(seed, s, 5);
    let u = (r.next() >> 11) as f64 / (1u64 << 53) as f64;
    let year = (1994 + (u.sqrt() * 33.0) as i32).min(2026);
    let start = chrono::NaiveDate::from_ymd_opt(year, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
    let days = if year >= 2026 { 270 } else { 365 };
    start + r.below(days * 86_400) as i64
}

fn person(seed: u64, n: u64) -> (String, String) {
    let first = words::FIRST[(n % words::FIRST.len() as u64) as usize];
    let last = words::LAST[((n / 7) % words::LAST.len() as u64) as usize];
    let domain = words::DOMAINS[(mix(seed, n, 9) % words::DOMAINS.len() as u64) as usize];
    let local: String = format!("{}.{}{}", first, last, if n > 1000 { (n % 97).to_string() } else { String::new() })
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect::<String>()
        .to_lowercase();
    (format!("{first} {last}"), format!("{local}@{domain}"))
}

const OWNER: (&str, &str) = ("Pat Owner", "pat@owner.example");

fn message_id(seed: u64, s: u64) -> String {
    format!("<{s:x}.{seed:x}@mail.{}>", words::DOMAINS[(s % words::DOMAINS.len() as u64) as usize])
}

fn subject_base(seed: u64, root: u64) -> String {
    let mut r = Rng::for_item(seed, root, 6);
    let n = r.range(2, 7);
    let mut s: Vec<String> = (0..n).map(|_| words::word(&mut r)).collect();
    if let Some(first) = s.first_mut() {
        let mut c = first.chars();
        if let Some(f) = c.next() {
            *first = f.to_uppercase().collect::<String>() + c.as_str();
        }
    }
    s.join(" ")
}

/// A reply's parent (an earlier original in a thread), if this slot is a reply.
fn parent_of(seed: u64, s: u64) -> Option<u64> {
    if s < 50 || mix(seed, s, 7) % 4 != 0 {
        return None;
    }
    let p = reference(seed, s, 8, false);
    (p < s).then_some(p)
}

fn root_of(seed: u64, s: u64) -> u64 {
    let mut cur = s;
    for _ in 0..12 {
        match parent_of(seed, cur) {
            Some(p) => cur = p,
            None => break,
        }
    }
    cur
}

struct Built {
    raw: Vec<u8>,
    features: Vec<&'static str>,
}

fn rfc_date(t: i64, two_digit_year: bool) -> String {
    let dt = chrono::DateTime::from_timestamp(t, 0).unwrap();
    if two_digit_year {
        dt.format("%d %b %y %H:%M GMT").to_string() // two-digit year, no seconds, no weekday
    } else {
        dt.format("%a, %d %b %Y %H:%M:%S +0000").to_string()
    }
}

fn attachment_name(r: &mut Rng) -> (String, &'static str) {
    let kinds: &[(&str, &str)] = &[
        ("pdf", "application/pdf"),
        ("jpg", "image/jpeg"),
        ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        ("zip", "application/zip"),
        ("txt", "text/plain"),
    ];
    let (ext, mime) = *r.pick(kinds);
    (format!("{}-{}.{ext}", words::rare_word(r.below(5000)), r.range(1, 999)), mime)
}

fn attachment_bytes(seed: u64, r: &mut Rng, scale: f64) -> Vec<u8> {
    // 30 % come from a shared pool (the same file sent around), the rest are unique
    let (mut source, size) = if r.chance(0.3) {
        let pool = r.below(3000);
        let mut p = Rng::for_item(seed, pool, 50);
        let size = p.range(1024, 100 * 1024);
        (p, size)
    } else {
        let size = 1024 + r.skewed(150 * 1024);
        (r.clone(), size)
    };
    let size = ((size as f64 * scale) as usize).max(16);
    let mut data = vec![0u8; size];
    source.fill(&mut data);
    data
}

fn push_line(buf: &mut Vec<u8>, text: &[u8], eol: &str) {
    buf.extend_from_slice(text);
    buf.extend_from_slice(eol.as_bytes());
}

/// Builds the message for an original slot.
fn build(seed: u64, s: u64, opts: &Options) -> Built {
    let folder = &FOLDERS[folder_of(seed, s)];
    let eol = if folder.format == Format::MboxCrlf { "\r\n" } else { "\n" };
    let mut r = Rng::for_item(seed, s, 10);
    let date = date_of(seed, s);
    let year: i32 = chrono::DateTime::from_timestamp(date, 0).unwrap().format("%Y").to_string().parse().unwrap();
    let needles = needles_of(seed, s);
    let has = |name: &str| needles.iter().any(|i| NEEDLES[*i].name == name);
    let mut f: Vec<&'static str> = Vec::new();
    let mut head: Vec<u8> = Vec::new();
    let old = year < 1997;
    let mid_era = (1997..2005).contains(&year);

    // Who
    let sent = folder.path == "Sent";
    let other = person(seed, r.skewed(20_000));
    let (from_name, from_addr) = if has("sender") {
        ("Needle Sender".to_string(), "needle.sender@haystack.example".to_string())
    } else if sent {
        (OWNER.0.to_string(), OWNER.1.to_string())
    } else {
        other.clone()
    };
    let to = if sent { other.clone() } else { (OWNER.0.to_string(), OWNER.1.to_string()) };

    // Headers
    let relay = words::DOMAINS[r.below(words::DOMAINS.len() as u64) as usize];
    push_line(&mut head, format!("Received: from mx{}.{relay} by mail.owner.example; {}", r.below(9), rfc_date(date + 5, false)).as_bytes(), eol);
    if old && r.chance(0.04) {
        push_line(&mut head, b"Date: sometime last tuesday", eol);
        f.push("garbage-date");
    } else if old && r.chance(0.02) {
        f.push("no-date");
    } else if old && r.chance(0.3) {
        push_line(&mut head, format!("Date: {}", rfc_date(date, true)).as_bytes(), eol);
        f.push("two-digit-year");
    } else {
        push_line(&mut head, format!("Date: {}", rfc_date(date, false)).as_bytes(), eol);
    }
    if (old || mid_era) && !from_name.is_ascii() && r.chance(0.5) {
        let mut h = b"From: ".to_vec();
        h.extend(encode::windows_1252(&from_name));
        h.extend(format!(" <{from_addr}>").as_bytes());
        push_line(&mut head, &h, eol);
        f.push("header-8bit");
    } else if !from_name.is_ascii() {
        push_line(&mut head, format!("From: {} <{from_addr}>", encode::encoded_word(&from_name)).as_bytes(), eol);
        f.push("encoded-word");
    } else {
        push_line(&mut head, format!("From: {from_name} <{from_addr}>").as_bytes(), eol);
    }
    push_line(&mut head, format!("To: {} <{}>", encode::encoded_word(&to.0), to.1).as_bytes(), eol);
    if r.chance(0.2) {
        let cc = person(seed, r.skewed(20_000));
        push_line(&mut head, format!("Cc: {} <{}>", encode::encoded_word(&cc.0), cc.1).as_bytes(), eol);
    }
    let parent = parent_of(seed, s);
    let root = root_of(seed, s);
    let mut subject = subject_base(seed, root);
    if has("subject") {
        subject = format!("{subject} Velociraptor Tuesday");
    }
    if parent.is_some() {
        subject = format!("Re: {subject}");
    }
    push_line(&mut head, format!("Subject: {subject}").as_bytes(), eol);
    if (old && r.chance(0.03)) || r.chance(0.002) {
        f.push("no-message-id");
    } else {
        push_line(&mut head, format!("Message-ID: {}", message_id(seed, s)).as_bytes(), eol);
    }
    if let Some(p) = parent {
        push_line(&mut head, format!("In-Reply-To: {}", message_id(seed, p)).as_bytes(), eol);
        push_line(&mut head, format!("References: {} {}", message_id(seed, root), message_id(seed, p)).as_bytes(), eol);
    }
    if let Some(list) = folder.path.strip_prefix("Lists/") {
        push_line(&mut head, format!("List-Id: <{list}.lists.example>").as_bytes(), eol);
    }

    // Body text
    let mut paragraphs: Vec<String> = (0..r.range(1, 6)).map(|_| words::paragraph(&mut r)).collect();
    if has("body-rare") {
        paragraphs.push("We saw the quokka zeppelin again near the harbor.".into());
    }
    if has("body-common") {
        paragraphs.push("Ask the marmalade cartographer for the updated map.".into());
    }
    let from_line = r.chance(0.003);
    if parent.is_some() {
        paragraphs.push(format!("> {}", words::sentence(&mut r)));
    }
    let mut text = paragraphs.iter().map(|p| words::wrap(p, eol)).collect::<Vec<_>>().join(&format!("{eol}{eol}"));
    if from_line {
        // a body line starting "From " (mbox has to escape it) and a quoted one
        text.push_str(&format!("{eol}{eol}From the desk of {from_name}: see you then.{eol}>From an older note."));
        f.push("from-line-in-body");
    }

    // Attachments
    let mut attachments: Vec<(String, &'static str, Vec<u8>)> = Vec::new();
    if has("attachment") || r.chance(if old { 0.03 } else { 0.1 }) {
        for i in 0..r.range(1, 2) {
            let (mut name, mime) = attachment_name(&mut r);
            if i == 0 && has("attachment") {
                name = "zanzibar-plans.pdf".into();
            }
            attachments.push((name, mime, attachment_bytes(seed, &mut r, opts.attachment_scale)));
        }
    }

    let mut body: Vec<u8> = Vec::new();
    if old {
        // No MIME: plain text, sometimes raw 8-bit with no charset; attachments uuencoded or BinHex
        if r.chance(0.08) {
            let t = format!("{text}{eol}{eol}Caf\u{e9} \u{201c}d\u{e9}j\u{e0} vu\u{201d} \u{2013} na\u{ef}ve r\u{e9}sum\u{e9}.");
            body.extend(encode::windows_1252(&t));
            f.push("no-charset-8bit");
        } else {
            body.extend(text.as_bytes());
        }
        body.extend(eol.as_bytes());
        for (name, _mime, data) in &attachments {
            body.extend(eol.as_bytes());
            if r.chance(0.2) {
                body.extend(format!("(This file must be converted with BinHex 4.0){eol}").as_bytes());
                const HQX: &[u8; 64] = b"!\"#$%&'()*+,-012345689@ABCDEFGHIJKLMNPQRSTUVXYZ[`abcdefhijklmpqr";
                let mut hqx = String::from(":");
                for (i, b) in data.iter().take(600).enumerate() {
                    hqx.push(HQX[(*b % 64) as usize] as char);
                    if i % 63 == 62 {
                        hqx.push_str(eol);
                    }
                }
                hqx.push(':');
                hqx.push_str(eol);
                body.extend(hqx.as_bytes());
                f.push("binhex");
            } else {
                body.extend(encode::uuencode(name, data, eol).as_bytes());
                f.push("uuencode");
            }
        }
    } else {
        push_line(&mut head, b"MIME-Version: 1.0", eol);
        let latin1 = mid_era && r.chance(0.5);
        let mut text_part = Vec::new();
        let charset = if latin1 {
            let mut t = text.clone();
            if r.chance(0.2) {
                t.push_str(&format!("{eol}{eol}It\u{2019}s \u{201c}fine\u{201d} \u{2013} really."));
                f.push("c1-quotes");
            }
            text_part.extend(encode::windows_1252(&t));
            "iso-8859-1"
        } else {
            text_part.extend(text.as_bytes());
            "utf-8"
        };
        text_part.extend(eol.as_bytes());
        let html = year >= 2003 && r.chance(0.45);
        let html_only = html && r.chance(0.25);
        let html_text = format!("<html><body>{}</body></html>{eol}", paragraphs.iter().map(|p| format!("<p>{p}</p>")).collect::<String>());
        let boundary = format!("=_b{s:x}");
        let alt = format!("=_a{s:x}");
        // (a part = its headers, a blank line, its content)
        let (part_head, part_body): (String, Vec<u8>) = if html_only {
            f.push("html-only");
            (format!("Content-Type: text/html; charset=utf-8{eol}Content-Transfer-Encoding: 8bit{eol}"), html_text.into_bytes())
        } else if html {
            f.push("alternative");
            let mut b = format!("--{alt}{eol}Content-Type: text/plain; charset={charset}{eol}Content-Transfer-Encoding: 8bit{eol}{eol}").into_bytes();
            b.extend(&text_part);
            b.extend(format!("--{alt}{eol}Content-Type: text/html; charset=utf-8{eol}Content-Transfer-Encoding: 8bit{eol}{eol}{html_text}--{alt}--{eol}").as_bytes());
            (format!("Content-Type: multipart/alternative; boundary=\"{alt}\"{eol}"), b)
        } else {
            (format!("Content-Type: text/plain; charset={charset}{eol}Content-Transfer-Encoding: 8bit{eol}"), text_part)
        };
        if attachments.is_empty() {
            head.extend(part_head.as_bytes());
            body.extend(part_body);
        } else {
            f.push("mime-attachment");
            push_line(&mut head, format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"").as_bytes(), eol);
            body.extend(format!("This is a multi-part message in MIME format.{eol}{eol}--{boundary}{eol}{part_head}{eol}").as_bytes());
            body.extend(part_body);
            for (name, mime, data) in &attachments {
                body.extend(format!("--{boundary}{eol}Content-Type: {mime}; name=\"{name}\"{eol}Content-Disposition: attachment; filename=\"{name}\"{eol}Content-Transfer-Encoding: base64{eol}{eol}").as_bytes());
                body.extend(encode::base64_lines(data, eol).as_bytes());
            }
            body.extend(format!("--{boundary}--{eol}").as_bytes());
        }
    }

    if folder.format == Format::MboxCl2 {
        push_line(&mut head, format!("Content-Length: {}", body.len()).as_bytes(), eol);
    }
    let mut raw = head;
    raw.extend(eol.as_bytes());
    raw.extend(body);
    if !raw.ends_with(eol.as_bytes()) {
        raw.extend(eol.as_bytes());
    }
    f.sort_unstable();
    f.dedup();
    Built { raw, features: f }
}

/// The same message with one more Received line (same Message-ID, different bytes).
fn variant(raw: &[u8], eol: &str) -> Vec<u8> {
    let mut v = format!("Received: from relay.variant.example by mail.owner.example; Mon, 1 Jan 2007 00:00:00 +0000{eol}").into_bytes();
    v.extend_from_slice(raw);
    v
}

fn eol_of(folder: usize) -> &'static str {
    if FOLDERS[folder].format == Format::MboxCrlf { "\r\n" } else { "\n" }
}

struct Entry {
    folder: usize,
    raw: Vec<u8>,
    envelope: i64,
    sender: String,
    slot: u64,
}

/// Writes the corpus into `out` (created; mailboxes under `out/mail`) and returns its manifest
/// (also written as `out/manifest.json`). `progress(done, total)` after every chunk.
pub fn generate(out: &Path, opts: &Options, progress: &(dyn Fn(u64, u64) + Sync)) -> std::io::Result<Manifest> {
    std::fs::create_dir_all(out)?;
    let mail = out.join("mail");
    let mut writers: Vec<Option<BufWriter<File>>> = (0..FOLDERS.len()).map(|_| None).collect();
    let mut manifest = Manifest { seed: opts.seed, ..Default::default() };
    let chunk = 2048u64;
    let mut start = 0u64;
    while start < opts.messages {
        let end = (start + chunk).min(opts.messages);
        let entries: Vec<(Entry, Vec<&'static str>, Vec<usize>, Slot)> = (start..end)
            .into_par_iter()
            .map(|s| {
                let seed = opts.seed;
                let kind = resolve(seed, s);
                let base = match kind {
                    Slot::Original => s,
                    Slot::Duplicate(k) | Slot::Copy(k) | Slot::Variant(k) => k,
                };
                let built = build(seed, base, opts);
                let home = folder_of(seed, base);
                let (folder, raw) = match kind {
                    Slot::Original | Slot::Duplicate(_) => (home, built.raw),
                    Slot::Copy(_) => {
                        // another folder (not mboxcl2); the message keeps its own line ends, as a
                        // CRLF message copied into an LF mailbox does in real life
                        let mut f = (home + 1 + (mix(seed, s, 11) % (FOLDERS.len() as u64 - 1)) as usize) % FOLDERS.len();
                        while f == home || FOLDERS[f].format == Format::MboxCl2 {
                            f = (f + 1) % FOLDERS.len();
                        }
                        (f, built.raw)
                    }
                    Slot::Variant(_) => (home, variant(&built.raw, eol_of(home))),
                };
                let sender = if FOLDERS[home].path == "Sent" { OWNER.1.to_string() } else { person(seed, base).1 };
                let needles = if kind == Slot::Original { needles_of(seed, s) } else { Vec::new() };
                (Entry { folder, raw, envelope: date_of(seed, base), sender, slot: s }, built.features, needles, kind)
            })
            .collect();
        for (entry, features, needles, kind) in entries {
            manifest.written += 1;
            manifest.bytes += entry.raw.len() as u64;
            if !matches!(kind, Slot::Duplicate(_)) {
                *manifest.folders.entry(FOLDERS[entry.folder].path.to_string()).or_default() += 1;
            }
            match kind {
                Slot::Original => {
                    manifest.unique += 1;
                    for f in features {
                        *manifest.features.entry(f.to_string()).or_default() += 1;
                    }
                    for n in needles {
                        *manifest.needles.entry(NEEDLES[n].name.to_string()).or_default() += 1;
                    }
                }
                Slot::Variant(_) => {
                    manifest.unique += 1;
                    manifest.id_variants += 1;
                }
                Slot::Duplicate(_) => manifest.duplicates += 1,
                Slot::Copy(_) => manifest.copies += 1,
            }
            write_entry(&mail, &mut writers, &entry)?;
        }
        start = end;
        progress(start, opts.messages);
    }
    for w in writers.iter_mut().flatten() {
        w.flush()?;
    }
    for n in NEEDLES {
        manifest.needles.entry(n.name.to_string()).or_default();
    }
    std::fs::write(out.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(manifest)
}

fn write_entry(mail: &Path, writers: &mut [Option<BufWriter<File>>], e: &Entry) -> std::io::Result<()> {
    let spec = &FOLDERS[e.folder];
    if spec.format == Format::Eml {
        let dir = mail.join(spec.path);
        std::fs::create_dir_all(&dir)?;
        return std::fs::write(dir.join(format!("{:08}.eml", e.slot)), &e.raw);
    }
    if writers[e.folder].is_none() {
        let path: PathBuf = mail.join(format!("{}.mbox", spec.path));
        std::fs::create_dir_all(path.parent().unwrap())?;
        writers[e.folder] = Some(BufWriter::with_capacity(1 << 20, File::create(path)?));
    }
    let w = writers[e.folder].as_mut().unwrap();
    let eol = eol_of(e.folder);
    let when = chrono::DateTime::from_timestamp(e.envelope, 0).unwrap().format("%a %b %e %H:%M:%S %Y");
    write!(w, "From {} {when}{eol}", e.sender)?;
    if spec.format == Format::MboxCl2 {
        w.write_all(&e.raw)?;
    } else {
        // mboxrd: every line that is ">*From " gets one more '>'
        let mut line_start = 0;
        while line_start < e.raw.len() {
            let line_end = e.raw[line_start..].iter().position(|b| *b == b'\n').map_or(e.raw.len(), |n| line_start + n + 1);
            let line = &e.raw[line_start..line_end];
            let quotes = line.iter().take_while(|b| **b == b'>').count();
            if line[quotes..].starts_with(b"From ") {
                w.write_all(b">")?;
            }
            w.write_all(line)?;
            line_start = line_end;
        }
    }
    w.write_all(eol.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_bytes_and_counts_add_up() {
        let dir = std::env::temp_dir().join(format!("eudora-corpus-test-{}", std::process::id()));
        // (30,000: old mail is ~1 % of the corpus, and the rare old-mail cases need some of it)
        let opts = Options { messages: 30_000, seed: 7, attachment_scale: 0.05 };
        let a = generate(&dir.join("a"), &opts, &|_, _| {}).unwrap();
        let b = generate(&dir.join("b"), &opts, &|_, _| {}).unwrap();
        assert_eq!(std::fs::read(dir.join("a/mail/Inbox.mbox")).unwrap(), std::fs::read(dir.join("b/mail/Inbox.mbox")).unwrap());
        assert_eq!((a.written, a.unique), (b.written, b.unique));
        assert_eq!(a.written, 30_000);
        assert_eq!(a.unique + a.duplicates + a.copies, a.written);
        assert!(a.duplicates > 0 && a.copies > 0 && a.id_variants > 0, "{a:?}");
        assert_eq!(a.folders.values().sum::<u64>(), a.written - a.duplicates);
        for f in ["no-message-id", "two-digit-year", "uuencode", "no-charset-8bit", "mime-attachment"] {
            assert!(a.features.get(f).copied().unwrap_or(0) > 0, "{f}: {:?}", a.features);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
