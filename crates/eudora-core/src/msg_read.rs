//! Reading Outlook .msg files ([MS-OXMSG]): the reverse of `msg.rs`, for opening a .msg Outlook
//! (or Eudora) saved.
//!
//! A .msg is an OLE compound file of MAPI properties: the message's own in its root, each
//! recipient and attachment in a storage of its own, a message inside it (a forward) in its
//! attachment's `__substg1.0_3701000D` storage. They're read into `fts::Item`, the same shape an
//! Exchange export becomes, and written out as MIME by the same code (`fts::to_mime_as`): the
//! original header block when Outlook kept it (`PR_TRANSPORT_MESSAGE_HEADERS`), the text, the
//! HTML (also the HTML Outlook keeps only inside its RTF, `\fromhtml1`), RTF only when there's
//! neither, every attachment's bytes, messages inside as messages. A signed or encrypted message
//! (`IPM.Note.SMIME…`) is kept whole, as Outlook keeps it: its one attachment is the original.
//!
//! Nothing here trusts the file: sizes are checked against what's there, messages inside
//! messages stop at a depth, and a property that can't be read is left out, never guessed.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, Write};

use crate::error::{Error, Result};
use crate::fts::{Attachment, Item, Name, Prop, Props, Value};

const MESSAGE_CLASS: u16 = 0x001A;
const ATTACH_DATA: u16 = 0x3701;
const ATTACH_METHOD: u16 = 0x3705;
const ATTACH_MIME_TAG: u16 = 0x370E;
const ATTACH_LONG_FILENAME: u16 = 0x3707;
const ATTACH_FILENAME: u16 = 0x3704;
const DISPLAY_NAME: u16 = 0x3001;
const RTF_COMPRESSED: u16 = 0x1009;
const HTML: u16 = 0x1013;
const MESSAGE_CODEPAGE: u16 = 0x3FFD;

/// How deep messages inside messages are read (a file made to nest forever stops here).
const MAX_DEPTH: usize = 12;
/// At most this many recipients or attachments per message (a damaged count can't run away).
const MAX_PARTS: usize = 10_000;

/// What a converted .msg says it was made from (`X-Eudora-Converted`).
pub const MADE_FROM: &str = "Outlook .msg file";

type Cfb<'a> = cfb::CompoundFile<Cursor<&'a [u8]>>;

fn bad(what: impl std::fmt::Display) -> Error {
    Error::Other(format!("this isn't an Outlook message file Eudora can read ({what})"))
}

/// Whether these bytes are an OLE compound file (what every .msg is).
pub fn looks_like_msg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1])
}

/// Reads a .msg file's message.
pub fn read(bytes: &[u8]) -> Result<Item> {
    if !looks_like_msg(bytes) {
        return Err(bad("it isn't an OLE compound file"));
    }
    let mut file = cfb::CompoundFile::open(Cursor::new(bytes)).map_err(bad)?;
    let names = read_names(&mut file);
    read_message(&mut file, "/", &names, 32, None, 0)
}

/// A .msg file as an internet message (MIME), ready for Eudora to show, answer, or save.
pub fn to_eml(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut item = read(bytes)?;
    if let Some(whole) = smime_whole(&item) {
        return Ok(whole);
    }
    add_html_from_rtf(&mut item);
    Ok(crate::fts::to_mime_as(&item, MADE_FROM))
}

// ----------------------------------------------------------------------------- the file itself

fn stream(file: &mut Cfb, path: &str) -> Option<Vec<u8>> {
    let mut s = file.open_stream(path).ok()?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok()?;
    Some(out)
}

/// The named properties' map (`__nameid_version1.0`): local id (0x8000 on) → (set, name).
fn read_names(file: &mut Cfb) -> HashMap<u16, ([u8; 16], Name)> {
    const PS_MAPI: [u8; 16] = [0x28, 0x03, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
    const PS_PUBLIC_STRINGS: [u8; 16] = [0x29, 0x03, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
    let dir = "/__nameid_version1.0";
    let mut out = HashMap::new();
    let (Some(guids), Some(entries)) = (stream(file, &format!("{dir}/__substg1.0_00020102")), stream(file, &format!("{dir}/__substg1.0_00030102"))) else {
        return out;
    };
    let strings = stream(file, &format!("{dir}/__substg1.0_00040102")).unwrap_or_default();
    for e in entries.chunks_exact(8) {
        let value = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
        let info = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        let index = (info >> 16) as usize;
        let guid_index = ((info >> 1) & 0x7FFF) as usize;
        let guid = match guid_index {
            1 => PS_MAPI,
            2 => PS_PUBLIC_STRINGS,
            n if n >= 3 => match guids.get((n - 3) * 16..(n - 2) * 16) {
                Some(g) => g.try_into().unwrap_or([0; 16]),
                None => continue,
            },
            _ => continue,
        };
        let name = if info & 1 == 0 {
            Name::Id(value)
        } else {
            let at = value as usize;
            let Some(len) = strings.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize) else { continue };
            let Some(text) = strings.get(at + 4..(at + 4).saturating_add(len)) else { continue };
            Name::Text(utf16(text))
        };
        if let Ok(id) = u16::try_from(0x8000 + index) {
            out.insert(id, (guid, name));
        }
    }
    out
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

/// The storages under `dir` whose names start with `prefix`, in order.
fn children(file: &Cfb, dir: &str, prefix: &str) -> Vec<String> {
    let mut found: Vec<String> = match file.read_storage(dir.trim_end_matches('/').to_string() + "/") {
        Ok(entries) => entries.filter(|e| e.is_storage() && e.name().starts_with(prefix)).map(|e| e.name().to_string()).collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    found.truncate(MAX_PARTS);
    found
}

/// One object's properties (`header`: the size of its property stream's header: 32 for the file's
/// message, 24 for a message inside it, 8 for a recipient or an attachment). `codepage`: the
/// message's, for 8-bit strings (a recipient's or an attachment's are written in it).
fn read_props(file: &mut Cfb, dir: &str, names: &HashMap<u16, ([u8; 16], Name)>, header: usize, codepage: Option<u32>) -> Props {
    let table = stream(file, &format!("{dir}__properties_version1.0")).unwrap_or_default();
    let mut raw: Vec<(u16, u16, [u8; 8])> = Vec::new();
    for e in table.get(header..).unwrap_or_default().chunks_exact(16) {
        let tag = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
        raw.push(((tag >> 16) as u16, tag as u16, e[8..16].try_into().unwrap_or([0; 8])));
    }
    // (some programs write a string or binary property's stream and leave it out of the table)
    if let Ok(entries) = file.read_storage(dir.to_string()) {
        let listed: Vec<u32> = raw.iter().map(|(id, t, _)| (u32::from(*id) << 16) | u32::from(*t)).collect();
        for e in entries.filter(|e| e.is_stream()) {
            let Some(hex) = e.name().strip_prefix("__substg1.0_").filter(|h| h.len() == 8) else { continue };
            let Ok(tag) = u32::from_str_radix(hex, 16) else { continue };
            if matches!(tag as u16, 0x001E | 0x001F | 0x0102) && !listed.contains(&tag) {
                raw.push(((tag >> 16) as u16, tag as u16, [0; 8]));
            }
        }
    }
    let codepage = codepage.or_else(|| {
        let at = |want: u16| raw.iter().find(|(id, t, _)| *id == want && *t == 0x0003).map(|(_, _, v)| u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
        // (8-bit strings are in the message's own code page, else Windows' Western one: never the
        // internet code page, which is the body's)
        at(MESSAGE_CODEPAGE)
    });
    let cp = codepage.unwrap_or(1252);
    let mut props = Vec::new();
    for (id, ptype, fixed) in raw {
        let tag = (u32::from(id) << 16) | u32::from(ptype);
        let path = format!("{dir}__substg1.0_{tag:08X}");
        let i64_at = |v: &[u8; 8]| i64::from_le_bytes(*v);
        let value = match ptype {
            0x0002 => Some(Value::Int(i64::from(i16::from_le_bytes([fixed[0], fixed[1]])))),
            0x0003 | 0x000A => Some(Value::Int(i64::from(i32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]])))),
            0x0006 | 0x0014 => Some(Value::Int(i64_at(&fixed))),
            0x0004 => Some(Value::Float(f64::from(f32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]])))),
            0x0005 | 0x0007 => Some(Value::Float(f64::from_le_bytes(fixed))),
            0x000B => Some(Value::Bool(fixed[0] != 0 || fixed[1] != 0)),
            0x0040 => Some(Value::Time(i64_at(&fixed))),
            0x001F => stream(file, &path).map(|b| Value::Text(utf16(&b))),
            0x001E => stream(file, &path).map(|b| Value::Text(crate::fts::decode(&b, cp))),
            0x0102 => stream(file, &path).map(Value::Bytes),
            0x0048 => stream(file, &path).and_then(|b| b.get(..16).and_then(|g| g.try_into().ok())).map(Value::Guid),
            0x101F | 0x101E | 0x1102 => stream(file, &path).map(|lengths| {
                let step = if ptype == 0x1102 { 8 } else { 4 };
                let count = lengths.len() / step;
                Value::Multi(
                    (0..count.min(MAX_PARTS))
                        .filter_map(|i| {
                            let b = stream(file, &format!("{path}-{i:08X}"))?;
                            Some(match ptype {
                                0x101F => Value::Text(utf16(&b)),
                                0x101E => Value::Text(crate::fts::decode(&b, cp)),
                                _ => Value::Bytes(b),
                            })
                        })
                        .collect(),
                )
            }),
            0x1002 | 0x1003 | 0x1014 | 0x1020 | 0x1005 | 0x1040 | 0x1048 => stream(file, &path).map(|b| {
                let size = match ptype {
                    0x1002 => 2,
                    0x1003 => 4,
                    0x1048 => 16,
                    _ => 8,
                };
                Value::Multi(
                    b.chunks_exact(size)
                        .take(MAX_PARTS)
                        .map(|c| match ptype {
                            0x1002 => Value::Int(i64::from(i16::from_le_bytes([c[0], c[1]]))),
                            0x1003 => Value::Int(i64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
                            0x1005 => Value::Float(f64::from_le_bytes(c.try_into().unwrap_or([0; 8]))),
                            0x1040 => Value::Time(i64::from_le_bytes(c.try_into().unwrap_or([0; 8]))),
                            0x1048 => Value::Guid(c.try_into().unwrap_or([0; 16])),
                            _ => Value::Int(i64::from_le_bytes(c.try_into().unwrap_or([0; 8]))),
                        })
                        .collect(),
                )
            }),
            // (an object: a message inside, read with its attachment; anything else unknown)
            _ => None,
        };
        let Some(value) = value else { continue };
        let named = if id >= 0x8000 {
            match names.get(&id) {
                Some(n) => Some(n.clone()),
                None => continue,
            }
        } else {
            None
        };
        props.push(Prop { id, ptype, named, value });
    }
    Props(props)
}

fn read_message(file: &mut Cfb, dir: &str, names: &HashMap<u16, ([u8; 16], Name)>, header: usize, codepage: Option<u32>, depth: usize) -> Result<Item> {
    if depth > MAX_DEPTH {
        return Err(bad("messages inside messages go too deep"));
    }
    let props = read_props(file, dir, names, header, codepage);
    let codepage = props.int(MESSAGE_CODEPAGE).map(|c| c as u32).or(codepage);
    let mut item = Item { props, ..Item::default() };
    for sub in children(file, dir, "__recip_version1.0_#") {
        item.recipients.push(read_props(file, &format!("{dir}{sub}/"), names, 8, codepage));
    }
    for sub in children(file, dir, "__attach_version1.0_#") {
        let at = format!("{dir}{sub}/");
        let mut props = read_props(file, &at, names, 8, codepage);
        let object = format!("{at}__substg1.0_3701000D");
        let mut embedded = None;
        if file.is_storage(&object) {
            if props.int(ATTACH_METHOD) == Some(5) || file.is_stream(format!("{object}/__properties_version1.0")) && props.int(ATTACH_METHOD) != Some(6) {
                // a message inside (a forward): read whole; one that can't be read stays as its
                // storage, so it's never dropped
                match read_message(file, &format!("{object}/"), names, 24, codepage, depth + 1) {
                    Ok(inner) => embedded = Some(Box::new(inner)),
                    Err(_) => props.0.push(Prop { id: ATTACH_DATA, ptype: 0x0102, named: None, value: Value::Bytes(storage_as_file(file, &object)?) }),
                }
            } else {
                // an OLE object (a picture or document Outlook embedded): its storage as a
                // compound file of its own
                props.0.retain(|p| p.named.is_some() || p.id != ATTACH_DATA);
                props.0.push(Prop { id: ATTACH_DATA, ptype: 0x0102, named: None, value: Value::Bytes(storage_as_file(file, &object)?) });
                if props.text(ATTACH_LONG_FILENAME).or(props.text(ATTACH_FILENAME)).is_none() {
                    let name = format!("{}.ole", props.text(DISPLAY_NAME).unwrap_or("object"));
                    props.0.push(Prop { id: ATTACH_LONG_FILENAME, ptype: 0x001F, named: None, value: Value::Text(name) });
                }
            }
        }
        // (Outlook often leaves out a file's type: from its name, so it opens as what it is)
        if embedded.is_none() && props.text(ATTACH_MIME_TAG).is_none() {
            let mime = props.text(ATTACH_LONG_FILENAME).or(props.text(ATTACH_FILENAME)).map(crate::old_attachments::mime_of).unwrap_or("application/octet-stream");
            props.0.push(Prop { id: ATTACH_MIME_TAG, ptype: 0x001F, named: None, value: Value::Text(mime.to_string()) });
        }
        item.attachments.push(Attachment { props, embedded });
    }
    Ok(item)
}

/// A storage inside the file, copied as a compound file of its own.
fn storage_as_file(file: &mut Cfb, path: &str) -> Result<Vec<u8>> {
    let io = |e: std::io::Error| Error::Other(format!("copying an embedded object: {e}"));
    let mut out = cfb::CompoundFile::create(Cursor::new(Vec::new())).map_err(io)?;
    let entries: Vec<(String, bool)> = file
        .walk_storage(path)
        .map_err(io)?
        .filter_map(|e| {
            let rel = e.path().to_string_lossy().replace('\\', "/");
            let rel = rel.strip_prefix(path)?.to_string();
            (!rel.is_empty() && rel != "/").then_some((rel, e.is_storage()))
        })
        .take(MAX_PARTS)
        .collect();
    for (rel, is_storage) in entries {
        if is_storage {
            out.create_storage(&rel).map_err(io)?;
        } else if let Some(data) = stream(file, &format!("{path}{rel}")) {
            out.create_stream(&rel).map_err(io)?.write_all(&data).map_err(io)?;
        }
    }
    out.flush().map_err(io)?;
    let mut cursor = out.into_inner();
    cursor.rewind().map_err(io)?;
    Ok(cursor.into_inner())
}

// ----------------------------------------------------------------------------- signed, sealed

/// A signed or encrypted message (`IPM.Note.SMIME…`) as the original it is: Outlook keeps it as
/// one attachment holding the MIME it came as (signed), or the PKCS #7 blob (encrypted). None for
/// any other message.
fn smime_whole(item: &Item) -> Option<Vec<u8>> {
    let class = item.props.text(MESSAGE_CLASS)?;
    if !class.to_ascii_lowercase().starts_with("ipm.note.smime") {
        return None;
    }
    let a = item.attachments.iter().find(|a| a.embedded.is_none() && a.props.bytes(ATTACH_DATA).is_some())?;
    let data = a.props.bytes(ATTACH_DATA)?;
    let entity_headers = mime_headers(data);
    // a whole message (Eudora writes the original as it came): as it is
    if entity_headers.iter().any(|h| ["from", "received", "message-id", "date"].contains(&h.as_str())) {
        return Some(data.to_vec());
    }
    let mut out = crate::fts::header_lines(item, MADE_FROM).into_bytes();
    if entity_headers.iter().any(|h| h == "content-type") {
        // the MIME entity Outlook kept (multipart/signed): its own Content-Type, then its body
        out.extend_from_slice(data);
    } else {
        // the PKCS #7 blob itself: signed or encrypted, by its content type's identifier
        let kind = if data.windows(11).take(64).any(|w| w == [0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02]) { "signed-data" } else { "enveloped-data" };
        let mime = a.props.text(ATTACH_MIME_TAG).filter(|m| m.contains("pkcs7-mime")).unwrap_or("application/pkcs7-mime");
        out.extend_from_slice(
            format!("Content-Type: {mime}; smime-type={kind}; name=\"smime.p7m\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7m\"\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(crate::fts::base64_lines(data).as_bytes());
    }
    Some(out)
}

/// The header names (lowercase) at the start of `data`, when it starts with a header block.
fn mime_headers(data: &[u8]) -> Vec<String> {
    let head = &data[..data.len().min(16 * 1024)];
    let text = String::from_utf8_lossy(head);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        match line.split_once(':') {
            Some((name, _)) if !name.is_empty() && name.bytes().all(|b| b.is_ascii_graphic()) => out.push(name.to_ascii_lowercase()),
            _ => return Vec::new(),
        }
    }
    out
}

// ---------------------------------------------------------------- HTML kept inside the RTF

/// Outlook often keeps an HTML message's HTML only inside its RTF ("encapsulated", [MS-OXRTFEX]):
/// taken out and kept as the HTML when the message has none of its own.
fn add_html_from_rtf(item: &mut Item) {
    if item.props.get(HTML).is_none()
        && let Some(rtf) = item.props.bytes(RTF_COMPRESSED).and_then(|b| crate::fts::decompress_rtf(b).ok())
        && let Some(html) = html_from_rtf(&rtf)
    {
        item.props.0.push(Prop { id: HTML, ptype: 0x001F, named: None, value: Value::Text(html) });
    }
    for a in &mut item.attachments {
        if let Some(inner) = &mut a.embedded {
            add_html_from_rtf(inner);
        }
    }
}

/// The HTML an RTF document carries (`\fromhtml1`), else None.
pub fn html_from_rtf(rtf: &[u8]) -> Option<String> {
    if !rtf.windows(10).take(400).any(|w| w == b"\\fromhtml1") {
        return None;
    }
    let codepage = find_number(rtf, b"\\ansicpg").unwrap_or(1252) as u32;
    let mut out = String::new();
    let mut pending: Vec<u8> = Vec::new();
    let flush = |pending: &mut Vec<u8>, out: &mut String| {
        if !pending.is_empty() {
            out.push_str(&crate::fts::decode(pending, codepage));
            pending.clear();
        }
    };
    // each group: (inside an \htmltag group, \htmlrtf on, a destination to skip, \uc count)
    #[derive(Clone, Copy)]
    struct G {
        tag: bool,
        suppress: bool,
        skip: bool,
        uc: usize,
    }
    let mut stack = vec![G { tag: false, suppress: false, skip: false, uc: 1 }];
    let mut skip_chars = 0usize;
    let mut i = 0;
    let n = rtf.len();
    let mut group_start = false;
    while i < n {
        let c = rtf[i];
        let g = *stack.last()?;
        match c {
            b'{' => {
                stack.push(G { tag: false, ..g });
                group_start = true;
                i += 1;
                continue;
            }
            b'}' => {
                if stack.len() > 1 {
                    stack.pop();
                }
                group_start = false;
                i += 1;
                continue;
            }
            b'\\' => {
                let next = *rtf.get(i + 1)?;
                if next.is_ascii_alphabetic() {
                    let start = i + 1;
                    let mut j = start;
                    while j < n && rtf[j].is_ascii_alphabetic() {
                        j += 1;
                    }
                    let word = &rtf[start..j];
                    let num_start = j;
                    if j < n && (rtf[j] == b'-' || rtf[j].is_ascii_digit()) {
                        j += 1;
                        while j < n && rtf[j].is_ascii_digit() {
                            j += 1;
                        }
                    }
                    let param: Option<i64> = std::str::from_utf8(&rtf[num_start..j]).ok().and_then(|s| s.parse().ok());
                    if j < n && rtf[j] == b' ' {
                        j += 1;
                    }
                    i = j;
                    let first = std::mem::replace(&mut group_start, false);
                    let top = stack.last_mut()?;
                    match word {
                        b"htmltag" if first => {
                            top.tag = true;
                            top.skip = false;
                        }
                        b"htmlrtf" => top.suppress = param != Some(0),
                        b"uc" => top.uc = param.unwrap_or(1).clamp(0, 10) as usize,
                        b"fonttbl" | b"colortbl" | b"stylesheet" | b"info" | b"pict" | b"mhtmltag" | b"listtable" | b"listoverridetable" | b"generator" => top.skip = true,
                        _ => {}
                    }
                    let g = *stack.last()?;
                    if g.skip || (g.suppress && !g.tag) {
                        continue;
                    }
                    match word {
                        b"par" | b"line" => {
                            flush(&mut pending, &mut out);
                            out.push_str("\r\n");
                        }
                        b"tab" => {
                            flush(&mut pending, &mut out);
                            out.push('\t');
                        }
                        b"u" => {
                            flush(&mut pending, &mut out);
                            if let Some(v) = param {
                                let v = if v < 0 { v + 65536 } else { v } as u32;
                                out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                            }
                            skip_chars = g.uc;
                        }
                        _ => {}
                    }
                    continue;
                }
                // a control symbol
                i += 2;
                let first = std::mem::replace(&mut group_start, false);
                if next == b'*' && first {
                    // \* : a destination Eudora doesn't know, unless it's \htmltag (decided by
                    // the next word)
                    let rest = &rtf[i..n.min(i + 9)];
                    if !rest.starts_with(b"\\htmltag") {
                        stack.last_mut()?.skip = true;
                    } else {
                        group_start = true;
                    }
                    continue;
                }
                let g = *stack.last()?;
                if g.skip || (g.suppress && !g.tag) {
                    if next == b'\'' {
                        i += 2;
                    }
                    continue;
                }
                match next {
                    b'\'' => {
                        if let Some(v) = rtf.get(i..i + 2).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                            if skip_chars > 0 {
                                skip_chars -= 1;
                            } else {
                                pending.push(v);
                            }
                        }
                        i += 2;
                    }
                    b'\\' | b'{' | b'}' => pending.push(next),
                    b'~' => {
                        flush(&mut pending, &mut out);
                        out.push('\u{A0}');
                    }
                    b'\r' | b'\n' => {
                        flush(&mut pending, &mut out);
                        out.push_str("\r\n");
                    }
                    _ => {}
                }
                continue;
            }
            b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            _ => {
                group_start = false;
                i += 1;
                if g.skip || (g.suppress && !g.tag) {
                    continue;
                }
                if skip_chars > 0 {
                    skip_chars -= 1;
                    continue;
                }
                pending.push(c);
            }
        }
    }
    flush(&mut pending, &mut out);
    let html = out.trim().to_string();
    (!html.is_empty()).then_some(html)
}

fn find_number(data: &[u8], word: &[u8]) -> Option<i64> {
    let at = data.windows(word.len()).take(2000).position(|w| w == word)? + word.len();
    let digits: Vec<u8> = data[at..].iter().take_while(|b| b.is_ascii_digit()).copied().collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIL: &[u8] = b"Received: from mx.example.com by mail.example.org; Tue, 3 Nov 2026 10:00:00 +0000\r\n\
From: Ann Smith <ann@example.com>\r\n\
To: Bob Jones <bob@example.org>, carol@example.org\r\n\
Cc: Dan <dan@example.org>\r\n\
Subject: Re: The contract\r\n\
Date: Tue, 3 Nov 2026 10:00:00 +0000\r\n\
Message-ID: <abc123@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: multipart/alternative; boundary=\"b2\"\r\n\
\r\n\
--b2\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Signed copy attached. Caf\xc3\xa9.\r\n\
--b2\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>Signed copy attached. Caf\xc3\xa9.</p>\r\n\
--b2--\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"contract.pdf\"\r\n\
Content-Disposition: attachment; filename=\"contract.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQKJcfsj6IK\r\n\
--b1\r\n\
Content-Type: message/rfc822\r\n\
\r\n\
From: Bob <bob@example.org>\r\n\
Subject: The contract\r\n\
Date: Mon, 2 Nov 2026 09:00:00 +0000\r\n\
\r\n\
Please sign.\r\n\
--b1--\r\n";

    #[test]
    fn a_msg_written_by_eudora_reads_back() {
        let msg = crate::msg::write(&crate::msg::from_mime(MAIL)).unwrap();
        let item = read(&msg).unwrap();
        assert_eq!(item.props.text(0x0037), Some("Re: The contract"));
        assert_eq!(item.recipients.len(), 3);
        assert_eq!(item.attachments.len(), 2);
        let eml = to_eml(&msg).unwrap();
        let d = crate::parse::display(&eml, 0);
        assert_eq!(d.parsed.subject, "Re: The contract");
        assert_eq!(d.parsed.from[0].address, "ann@example.com");
        assert_eq!(d.parsed.to.len(), 2);
        assert_eq!(d.parsed.message_id, "abc123@example.com");
        assert!(d.html.as_deref().unwrap().contains("Signed copy attached. Café."), "{:?}", d.html);
        assert!(d.text.contains("Signed copy attached. Café."));
        // the PDF byte for byte, the forward as a message
        let pdf = d.parsed.attachments.iter().find(|a| a.name == "contract.pdf").unwrap();
        assert_eq!(crate::compose::part_bytes(&eml, &pdf.part).unwrap().1, b"%PDF-1.4\n%\xc7\xec\x8f\xa2\n");
        assert!(d.parsed.attachments.iter().any(|a| a.mime == "message/rfc822"), "{:?}", d.parsed.attachments);
        assert!(String::from_utf8_lossy(&eml).contains("X-Eudora-Converted: Outlook .msg file"));
    }

    #[test]
    fn a_signed_msg_is_the_original_again() {
        let raw = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: Signed\r\nMIME-Version: 1.0\r\nContent-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256; boundary=\"s\"\r\n\r\n--s\r\nContent-Type: text/plain\r\n\r\nHello\r\n--s\r\nContent-Type: application/pkcs7-signature; name=smime.p7s\r\nContent-Transfer-Encoding: base64\r\n\r\nAAAA\r\n--s--\r\n";
        let msg = crate::msg::write(&crate::msg::from_mime(raw)).unwrap();
        assert_eq!(to_eml(&msg).unwrap(), raw);
    }

    #[test]
    fn outlooks_signed_entity_gets_the_headers_back() {
        // as Outlook keeps one: the attachment is the signed entity, the headers are the message's
        let entity = b"Content-Type: multipart/signed; boundary=\"s\"\r\n\r\n--s\r\nContent-Type: text/plain\r\n\r\nHi\r\n--s--\r\n";
        let mut item = Item::default();
        item.props.0.push(Prop { id: MESSAGE_CLASS, ptype: 0x001F, named: None, value: Value::Text("IPM.Note.SMIME.MultipartSigned".into()) });
        item.props.0.push(Prop { id: 0x007D, ptype: 0x001F, named: None, value: Value::Text("From: a@example.com\r\nSubject: Hi\r\nContent-Type: text/plain\r\n\r\n".into()) });
        item.attachments.push(Attachment { props: Props(vec![Prop { id: ATTACH_DATA, ptype: 0x0102, named: None, value: Value::Bytes(entity.to_vec()) }]), embedded: None });
        let out = smime_whole(&item).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("From: a@example.com\r\nSubject: Hi\r\nMIME-Version: 1.0\r\n"), "{text}");
        assert!(text.ends_with(std::str::from_utf8(entity).unwrap()));
        assert!(!text.contains("Content-Type: text/plain\r\nMIME"));
    }

    #[test]
    fn html_kept_inside_rtf_comes_out() {
        let rtf = br"{\rtf1\ansi\ansicpg1252\fromhtml1 \deff0{\fonttbl{\f0\fswiss Arial;}}{\*\htmltag19 <html>}{\*\htmltag50 <body>}\htmlrtf {\htmlrtf0 Caf\'e9 {\*\htmltag84 <b>}bold{\*\htmltag92 </b>}\htmlrtf\par\htmlrtf0 \u8364?\htmlrtf}{\*\htmltag58 </body>}{\*\htmltag27 </html>}}";
        let html = html_from_rtf(rtf).unwrap();
        assert_eq!(html, "<html><body>Café <b>bold</b>€</body></html>");
        assert_eq!(html_from_rtf(br"{\rtf1\ansi plain}"), None);
    }

    #[test]
    fn not_a_msg_is_said() {
        assert!(read(b"From: a@example.com\r\n\r\nhi").is_err());
        assert!(read(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0]).is_err());
    }

    /// A real Outlook .msg, when one is given (`EUDORA_MSG=path`): read, converted, and parsed.
    #[test]
    #[ignore]
    fn try_real() {
        for path in std::env::var("EUDORA_MSG").unwrap_or_default().split(';').filter(|p| !p.is_empty()) {
            let bytes = std::fs::read(path).unwrap();
            let t = std::time::Instant::now();
            let eml = to_eml(&bytes).unwrap();
            let own_html = read(&bytes).unwrap().props.get(HTML).is_some();
            if let Ok(out) = std::env::var("EUDORA_MSG_OUT") {
                std::fs::write(std::path::Path::new(&out).join(format!("{}.eml", std::path::Path::new(path).file_stem().unwrap().to_string_lossy())), &eml).unwrap();
            }
            let d = crate::parse::display(&eml, 0);
            println!("{path}: {} ms, subject {:?}, from {:?}, {} attachments, html {} (its own: {own_html}), text {} chars", t.elapsed().as_millis(), d.parsed.subject, d.parsed.from, d.parsed.attachments.len(), d.html.is_some(), d.text.len());
        }
    }
}
