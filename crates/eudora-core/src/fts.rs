//! Exchange exports (Microsoft Graph `exportItems`) → ordinary Internet mail.
//!
//! An export is a list of records (`u32 kind, u32 length, data`); kind 2 holds the item as an
//! MS-OXCFXICS FastTransfer property stream. [`parse`] reads it into an [`Item`] (properties,
//! recipients, attachments, embedded messages) and [`to_mime`] writes the message: the original
//! header block (`TransportMessageHeaders`) when Exchange kept it, else headers made from the
//! properties, then the bodies and attachments as MIME. The export itself is kept by the caller
//! next to the result, so nothing is lost when a property isn't understood here.

use std::fmt::Write as _;

use base64::Engine as _;

use crate::{Error, Result};

// Property ids (MS-OXPROPS).
pub const SUBJECT: u16 = 0x0037;
pub const CLIENT_SUBMIT_TIME: u16 = 0x0039;
pub const SENT_REPRESENTING_NAME: u16 = 0x0042;
pub const SENT_REPRESENTING_EMAIL: u16 = 0x0065;
pub const SENT_REPRESENTING_ADDR_TYPE: u16 = 0x0064;
pub const TRANSPORT_HEADERS: u16 = 0x007D;
pub const RECIPIENT_TYPE: u16 = 0x0C15;
pub const SENDER_NAME: u16 = 0x0C1A;
pub const SENDER_ADDR_TYPE: u16 = 0x0C1E;
pub const SENDER_EMAIL: u16 = 0x0C1F;
pub const DELIVERY_TIME: u16 = 0x0E06;
pub const BODY: u16 = 0x1000;
pub const RTF_COMPRESSED: u16 = 0x1009;
pub const HTML: u16 = 0x1013;
pub const MESSAGE_ID: u16 = 0x1035;
pub const REFERENCES: u16 = 0x1039;
pub const IN_REPLY_TO: u16 = 0x1042;
pub const DISPLAY_NAME: u16 = 0x3001;
pub const ADDR_TYPE: u16 = 0x3002;
pub const EMAIL_ADDRESS: u16 = 0x3003;
pub const ATTACH_DATA: u16 = 0x3701;
pub const ATTACH_FILENAME: u16 = 0x3704;
pub const ATTACH_METHOD: u16 = 0x3705;
pub const ATTACH_LONG_FILENAME: u16 = 0x3707;
pub const ATTACH_MIME_TAG: u16 = 0x370E;
pub const ATTACH_CONTENT_ID: u16 = 0x3712;
pub const SMTP_ADDRESS: u16 = 0x39FE;
pub const INTERNET_CODEPAGE: u16 = 0x3FDE;
pub const SENDER_SMTP: u16 = 0x5D01;
pub const SENT_REPRESENTING_SMTP: u16 = 0x5D02;

// Markers that carry no value (MS-OXCFXICS 2.2.4.1.4).
const NEW_ATTACH: u32 = 0x4000_0003;
const START_EMBED: u32 = 0x4001_0003;
const END_EMBED: u32 = 0x4002_0003;
const START_RECIP: u32 = 0x4003_0003;
const END_TO_RECIP: u32 = 0x4004_0003;
const END_ATTACH: u32 = 0x400E_0003;
const OTHER_MARKERS: [u32; 14] = [
    0x4009_0003, 0x400A_0003, 0x400B_0003, 0x400C_0003, 0x400D_0003, 0x4010_0003, 0x4012_0003,
    0x4013_0003, 0x4014_0003, 0x402F_0003, 0x403A_0003, 0x403B_0003, 0x407D_0003, 0x4015_0003,
];

/// A property value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// FILETIME: 100 ns ticks since 1601-01-01 UTC.
    Time(i64),
    Text(String),
    Bytes(Vec<u8>),
    Guid([u8; 16]),
    Multi(Vec<Value>),
}

/// Where a named property's name comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Name {
    Id(u32),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub id: u16,
    pub ptype: u16,
    /// Named properties (id ≥ 0x8000): property set GUID and name; the id is only local.
    pub named: Option<([u8; 16], Name)>,
    pub value: Value,
}

/// A set of properties, as on a message, a recipient or an attachment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Props(pub Vec<Prop>);

impl Props {
    pub fn get(&self, id: u16) -> Option<&Value> {
        self.0.iter().find(|p| p.id == id && p.named.is_none()).map(|p| &p.value)
    }
    pub fn text(&self, id: u16) -> Option<&str> {
        match self.get(id) {
            Some(Value::Text(t)) => Some(t.trim_end_matches('\0')).filter(|t| !t.is_empty()),
            _ => None,
        }
    }
    pub fn bytes(&self, id: u16) -> Option<&[u8]> {
        match self.get(id) {
            Some(Value::Bytes(b)) => Some(b),
            _ => None,
        }
    }
    pub fn int(&self, id: u16) -> Option<i64> {
        match self.get(id) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        }
    }
    pub fn time(&self, id: u16) -> Option<i64> {
        match self.get(id) {
            Some(Value::Time(t)) => Some(*t),
            _ => None,
        }
    }
    /// A named property by its string name (e.g. "content-type").
    pub fn named(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|p| matches!(&p.named, Some((_, Name::Text(n))) if n.eq_ignore_ascii_case(name))).map(|p| &p.value)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attachment {
    pub props: Props,
    pub embedded: Option<Box<Item>>,
}

/// A message read from a FastTransfer stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub props: Props,
    pub recipients: Vec<Props>,
    pub attachments: Vec<Attachment>,
}

/// Reads an exported item (the records Graph returns, already base64-decoded).
pub fn parse(export: &[u8]) -> Result<Item> {
    let mut stream = Vec::new();
    let mut o = 0;
    while o + 8 <= export.len() {
        let kind = u32_at(export, o)?;
        let len = u32_at(export, o + 4)? as usize;
        let data = export.get(o + 8..o + 8 + len).ok_or_else(|| bad(o, "record runs past the end"))?;
        if kind == 2 {
            stream.extend_from_slice(data);
        }
        o += 8 + len;
    }
    if stream.is_empty() {
        return Err(Error::Other("not an Exchange export: no message data".into()));
    }
    parse_stream(&stream)
}

/// Reads a bare FastTransfer property stream.
pub fn parse_stream(stream: &[u8]) -> Result<Item> {
    let mut reader = Reader { data: stream, at: 0 };
    let item = reader.message(None)?;
    Ok(item)
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek_tag(&self) -> Option<u32> {
        u32_at(self.data, self.at).ok()
    }

    /// Reads message content until `end` (or the end of the stream at the top level).
    fn message(&mut self, end: Option<u32>) -> Result<Item> {
        let mut item = Item::default();
        loop {
            let Some(tag) = self.peek_tag() else {
                return match end {
                    None => Ok(item),
                    Some(_) => Err(bad(self.at, "embedded message not closed")),
                };
            };
            if Some(tag) == end {
                self.at += 4;
                return Ok(item);
            }
            match tag {
                START_RECIP => {
                    self.at += 4;
                    item.recipients.push(self.props_until(&[END_TO_RECIP])?);
                    self.at += 4;
                }
                NEW_ATTACH => {
                    self.at += 4;
                    let props = self.props_until(&[START_EMBED, END_ATTACH])?;
                    let mut attachment = Attachment { props, embedded: None };
                    if self.peek_tag() == Some(START_EMBED) {
                        self.at += 4;
                        attachment.embedded = Some(Box::new(self.message(Some(END_EMBED))?));
                        // Properties may follow the embedded message before EndAttach.
                        let more = self.props_until(&[END_ATTACH])?;
                        attachment.props.0.extend(more.0);
                    }
                    self.at += 4;
                    item.attachments.push(attachment);
                }
                END_EMBED | END_ATTACH | END_TO_RECIP => return Err(bad(self.at, "unexpected end marker")),
                t if OTHER_MARKERS.contains(&t) => self.at += 4,
                _ => item.props.0.push(self.prop()?),
            }
        }
    }

    fn props_until(&mut self, ends: &[u32]) -> Result<Props> {
        let mut props = Props::default();
        loop {
            match self.peek_tag() {
                None => return Err(bad(self.at, "group not closed")),
                Some(t) if ends.contains(&t) => return Ok(props),
                Some(t) if OTHER_MARKERS.contains(&t) => self.at += 4,
                Some(_) => props.0.push(self.prop()?),
            }
        }
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let start = self.at;
        let bytes = self.data.get(start..start + n).ok_or_else(|| bad(start, "value runs past the end"))?;
        self.at += n;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn prop(&mut self) -> Result<Prop> {
        let start = self.at;
        let tag = self.u32()?;
        let (ptype, id) = ((tag & 0xFFFF) as u16, (tag >> 16) as u16);
        let named = if id >= 0x8000 {
            let guid: [u8; 16] = self.take(16)?.try_into().unwrap();
            let kind = self.take(1)?[0];
            let name = if kind == 0 {
                Name::Id(self.u32()?)
            } else {
                let rest = &self.data[self.at..];
                let end = rest.chunks_exact(2).position(|c| c == [0, 0]).ok_or_else(|| bad(self.at, "unterminated name"))?;
                let units: Vec<u16> = rest[..end * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                self.at += end * 2 + 2;
                Name::Text(String::from_utf16_lossy(&units))
            };
            Some((guid, name))
        } else {
            None
        };
        // Several values: 0x1000 (0x2000 = one row per value, same layout). Strings with a code
        // page (0x8000 | code page) carry the flag too: 0x94B0 = a list of UTF-16 strings; their
        // code pages fit in 12 bits.
        let multi = if ptype & 0x8000 != 0 { ptype & 0x1000 != 0 } else { ptype & 0x3000 != 0 };
        let base = if ptype & 0x8000 != 0 { ptype & !0x1000 } else if multi { ptype & !0x3000 } else { ptype };
        let value = if multi {
            let count = self.u32()? as usize;
            if count > self.data.len() {
                return Err(bad(start, "impossible value count"));
            }
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(self.value(base, start)?);
            }
            Value::Multi(values)
        } else {
            self.value(base, start)?
        };
        Ok(Prop { id, ptype, named, value })
    }

    fn value(&mut self, ptype: u16, start: usize) -> Result<Value> {
        Ok(match ptype {
            0x0002 => Value::Int(i16::from_le_bytes(self.take(2)?.try_into().unwrap()) as i64),
            0x0003 | 0x000A => Value::Int(i32::from_le_bytes(self.take(4)?.try_into().unwrap()) as i64),
            0x0004 => Value::Float(f32::from_le_bytes(self.take(4)?.try_into().unwrap()) as f64),
            0x0005 | 0x0007 => Value::Float(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x0006 | 0x0014 => Value::Int(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x000B => Value::Bool(u16::from_le_bytes(self.take(2)?.try_into().unwrap()) != 0),
            0x0040 => Value::Time(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            // Exchange-internal flag 0x4000 on an ordinary type: two bytes, then the value as that
            // type (seen: 0x4040 = a FILETIME as 0x3309 on archived mail, 0x400B = a boolean).
            t if t & 0x4000 != 0 && t & 0x8000 == 0 && t & !0x4000 != 0 => {
                self.take(2)?;
                return self.value(t & !0x4000, start);
            }
            0x0048 => Value::Guid(self.take(16)?.try_into().unwrap()),
            0x001F => {
                let n = self.u32()? as usize;
                Value::Text(utf16(self.take(n)?))
            }
            0x001E => {
                let n = self.u32()? as usize;
                Value::Text(decode(self.take(n)?, 1252))
            }
            0x0102 | 0x000D | 0x00FB => {
                let n = self.u32()? as usize;
                Value::Bytes(self.take(n)?.to_vec())
            }
            t if t & 0x8000 != 0 => {
                let n = self.u32()? as usize;
                Value::Text(decode(self.take(n)?, (t & 0x7FFF) as u32))
            }
            t => return Err(bad(start, &format!("property type 0x{t:04x} not understood"))),
        })
    }
}

fn u32_at(data: &[u8], at: usize) -> Result<u32> {
    data.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| bad(at, "truncated"))
}

fn bad(at: usize, what: &str) -> Error {
    Error::Other(format!("Exchange export: {what} (at byte {at})"))
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

/// The MIME charset name for a Windows code page, when there is a common one.
pub fn charset_name(codepage: u32) -> Option<&'static str> {
    Some(match codepage {
        65001 => "utf-8",
        20127 => "us-ascii",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1252 => "windows-1252",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        874 => "windows-874",
        932 => "shift_jis",
        936 => "gbk",
        949 => "euc-kr",
        950 => "big5",
        20866 => "koi8-r",
        21866 => "koi8-u",
        28591 => "iso-8859-1",
        28592 => "iso-8859-2",
        28595 => "iso-8859-5",
        28597 => "iso-8859-7",
        28599 => "iso-8859-9",
        28605 => "iso-8859-15",
        50220 | 50221 | 50222 => "iso-2022-jp",
        51932 => "euc-jp",
        54936 => "gb18030",
        _ => return None,
    })
}

pub(crate) fn decode(bytes: &[u8], codepage: u32) -> String {
    let text = match codepage {
        1200 => utf16(bytes),
        _ => {
            let label = charset_name(codepage).unwrap_or("windows-1252");
            let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::WINDOWS_1252);
            encoding.decode_without_bom_handling(bytes).0.into_owned()
        }
    };
    text.trim_end_matches('\0').to_string()
}

// ---------------------------------------------------------------- compressed RTF (MS-OXRTFCP)

const RTF_PRELOAD: &[u8] = b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";

/// Decompresses `PR_RTF_COMPRESSED`.
pub fn decompress_rtf(data: &[u8]) -> Result<Vec<u8>> {
    let field = |i: usize| u32_at(data, i);
    let comp_size = field(0)? as usize;
    let raw_size = field(4)? as usize;
    let kind = field(8)?;
    let body = data.get(16..(comp_size + 4).min(data.len())).ok_or_else(|| bad(0, "compressed RTF too short"))?;
    match kind {
        0x414C_454D => return Ok(body.iter().copied().take(raw_size).collect()), // "MELA": stored
        0x7546_5A4C => {}                                                           // "LZFu"
        _ => return Err(bad(8, "unknown compressed RTF kind")),
    }
    let mut dict = [0u8; 4096];
    dict[..RTF_PRELOAD.len()].copy_from_slice(RTF_PRELOAD);
    let mut write = RTF_PRELOAD.len();
    let mut out = Vec::with_capacity(raw_size);
    let mut i = 0;
    'outer: while i < body.len() {
        let control = body[i];
        i += 1;
        for bit in 0..8 {
            if i >= body.len() {
                break 'outer;
            }
            if control & (1 << bit) == 0 {
                let b = body[i];
                i += 1;
                out.push(b);
                dict[write] = b;
                write = (write + 1) % 4096;
            } else {
                let Some(pair) = body.get(i..i + 2) else { break 'outer };
                i += 2;
                let word = u16::from_be_bytes([pair[0], pair[1]]) as usize;
                let (offset, len) = (word >> 4, (word & 0xF) + 2);
                if offset == write {
                    break 'outer;
                }
                for k in 0..len {
                    let b = dict[(offset + k) % 4096];
                    out.push(b);
                    dict[write] = b;
                    write = (write + 1) % 4096;
                }
            }
        }
    }
    out.truncate(raw_size);
    Ok(out)
}

// ---------------------------------------------------------------- MIME

/// Header fields from the original block that describe the old body: ours replace them.
const BODY_FIELDS: [&str; 5] = ["mime-version", "content-type", "content-transfer-encoding", "content-length", "content-disposition"];

/// Writes the item as an RFC 5322 message.
pub fn to_mime(item: &Item) -> Vec<u8> {
    to_mime_as(item, "Exchange export")
}

/// Writes the item as an RFC 5322 message, saying in `X-Eudora-Converted` what it was made from
/// ("Exchange export", "Outlook .msg file").
pub fn to_mime_as(item: &Item, made_from: &str) -> Vec<u8> {
    let mut out = String::new();
    let mut counter = 0u32;
    write_message(item, &mut out, &mut counter, made_from);
    out.into_bytes()
}

fn write_message(item: &Item, out: &mut String, counter: &mut u32, made_from: &str) {
    out.push_str(&header_lines(item, made_from));
    write_body(item, out, counter, made_from);
}

/// The message's header lines (each ending in CRLF): its original header block when it kept one
/// (without the fields describing the old body), else made from its properties; then
/// MIME-Version and `X-Eudora-Converted`. The body's own fields come after them.
pub(crate) fn header_lines(item: &Item, made_from: &str) -> String {
    let mut out = String::new();
    let out = &mut out;
    let p = &item.props;
    match p.text(TRANSPORT_HEADERS) {
        Some(headers) => {
            for field in header_fields(headers) {
                let name = field.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
                if !BODY_FIELDS.contains(&name.as_str()) {
                    out.push_str(&field);
                    out.push_str("\r\n");
                }
            }
        }
        None => made_headers(item, out),
    }
    out.push_str("MIME-Version: 1.0\r\n");
    let _ = write!(out, "X-Eudora-Converted: {made_from}\r\n");
    std::mem::take(out)
}

/// The fields of a header block, each with its folded lines joined by CRLF.
fn header_fields(block: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    for line in block.trim_end_matches(['\0', '\r', '\n']).lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(last) = fields.last_mut() {
                last.push_str("\r\n");
                last.push_str(line);
                continue;
            }
        }
        if line.contains(':') {
            fields.push(line.to_string());
        }
    }
    fields
}

fn made_headers(item: &Item, out: &mut String) {
    let p = &item.props;
    let from = address(
        p.text(SENT_REPRESENTING_NAME).or(p.text(SENDER_NAME)),
        smtp(p, SENT_REPRESENTING_SMTP, SENT_REPRESENTING_ADDR_TYPE, SENT_REPRESENTING_EMAIL)
            .or_else(|| smtp(p, SENDER_SMTP, SENDER_ADDR_TYPE, SENDER_EMAIL)),
    );
    if let Some(from) = from {
        let _ = write!(out, "From: {from}\r\n");
    }
    for (kind, name) in [(1, "To"), (2, "Cc"), (3, "Bcc")] {
        let list: Vec<String> = item
            .recipients
            .iter()
            .filter(|r| r.int(RECIPIENT_TYPE).map(|t| t & 0xF) == Some(kind))
            .filter_map(|r| address(r.text(DISPLAY_NAME), smtp(r, SMTP_ADDRESS, ADDR_TYPE, EMAIL_ADDRESS)))
            .collect();
        if !list.is_empty() {
            let _ = write!(out, "{name}: {}\r\n", list.join(",\r\n "));
        }
    }
    if let Some(subject) = p.text(SUBJECT) {
        let _ = write!(out, "Subject: {}\r\n", encode_words(subject));
    }
    if let Some(date) = p.time(CLIENT_SUBMIT_TIME).or(p.time(DELIVERY_TIME)).and_then(filetime_to_date) {
        let _ = write!(out, "Date: {}\r\n", date.to_rfc2822());
    }
    for (id, name) in [(MESSAGE_ID, "Message-ID"), (IN_REPLY_TO, "In-Reply-To"), (REFERENCES, "References")] {
        if let Some(v) = p.text(id).filter(|v| v.is_ascii()) {
            let _ = write!(out, "{name}: {v}\r\n");
        }
    }
}

/// An SMTP address from the SMTP property, or the email property when its type is SMTP.
fn smtp<'a>(p: &'a Props, smtp_id: u16, type_id: u16, email_id: u16) -> Option<&'a str> {
    p.text(smtp_id).or_else(|| {
        let email = p.text(email_id)?;
        let is_smtp = p.text(type_id).map_or(email.contains('@'), |t| t.eq_ignore_ascii_case("SMTP"));
        is_smtp.then_some(email)
    })
}

fn address(name: Option<&str>, email: Option<&str>) -> Option<String> {
    let name = name.map(str::trim).filter(|n| !n.is_empty() && Some(*n) != email);
    match (name, email) {
        (Some(n), Some(e)) => Some(format!("{} <{e}>", display_name(n))),
        (None, Some(e)) => Some(e.to_string()),
        // (a recipient never looked up, as in a message not sent yet: its name is the address)
        (Some(n), None) if n.contains('@') && !n.contains([' ', '<', '>', ',']) => Some(n.to_string()),
        (Some(n), None) => Some(format!("{}:;", display_name(n))), // a group: name only, no address
        (None, None) => None,
    }
}

pub(crate) fn display_name(name: &str) -> String {
    if !name.is_ascii() {
        encode_words(name)
    } else if name.chars().all(|c| c.is_ascii_alphanumeric() || " !#$%&'*+-/=?^_`{|}~".contains(c)) {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// RFC 2047 encoded words (UTF-8, base64) for non-ASCII header text.
pub(crate) fn encode_words(text: &str) -> String {
    if text.is_ascii() {
        return text.to_string();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    words.push(chunk);
    let b64 = base64::engine::general_purpose::STANDARD;
    words.iter().map(|w| format!("=?utf-8?B?{}?=", b64.encode(w))).collect::<Vec<_>>().join("\r\n ")
}

pub fn filetime_to_date(ft: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    let secs = ft.div_euclid(10_000_000) - 11_644_473_600;
    let nanos = (ft.rem_euclid(10_000_000) * 100) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
}

/// One MIME part, ready to write.
pub(crate) struct Part {
    pub(crate) headers: Vec<String>,
    pub(crate) body: String,
}

fn write_body(item: &Item, out: &mut String, counter: &mut u32, made_from: &str) {
    let part = body_part(item, counter, made_from);
    for h in &part.headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.push_str(&part.body);
}

fn body_part(item: &Item, counter: &mut u32, made_from: &str) -> Part {
    let p = &item.props;
    let codepage = p.int(INTERNET_CODEPAGE).map(|c| c as u32);
    let html = html_body(p, codepage);
    let text = p.text(BODY).map(|t| text_part("text/plain", t));
    let rtf = if html.is_none() && text.is_none() {
        p.bytes(RTF_COMPRESSED).and_then(|b| decompress_rtf(b).ok()).map(|rtf| bytes_part("text/rtf", None, &rtf, None))
    } else {
        None
    };

    let (inline, attached): (Vec<&Attachment>, Vec<&Attachment>) = item
        .attachments
        .iter()
        .partition(|a| html.is_some() && a.props.text(ATTACH_CONTENT_ID).is_some() && a.embedded.is_none());

    let html = html.map(|h| {
        if inline.is_empty() {
            h
        } else {
            let mut parts = vec![h];
            parts.extend(inline.iter().map(|a| attachment_part(a, true, counter, made_from)));
            multipart("related", parts, counter)
        }
    });
    let main = match (text, html, rtf) {
        (Some(t), Some(h), _) => multipart("alternative", vec![t, h], counter),
        (Some(t), None, _) => t,
        (None, Some(h), _) => h,
        (None, None, Some(r)) => r,
        (None, None, None) => text_part("text/plain", ""),
    };
    if attached.is_empty() {
        return main;
    }
    let mut parts = vec![main];
    parts.extend(attached.iter().map(|a| attachment_part(a, false, counter, made_from)));
    multipart("mixed", parts, counter)
}

/// The HTML body, in its own code page when it's stored as bytes.
fn html_body(p: &Props, codepage: Option<u32>) -> Option<Part> {
    match p.get(HTML)? {
        Value::Bytes(b) if !b.is_empty() => {
            let charset = match codepage.and_then(charset_name) {
                Some(c) => c,
                None if std::str::from_utf8(b).is_ok() => "utf-8",
                None => "windows-1252",
            };
            Some(bytes_part("text/html", Some(charset), b, None))
        }
        Value::Text(t) if !t.is_empty() => Some(text_part("text/html", t)),
        _ => None,
    }
}

pub(crate) fn text_part(mime: &str, text: &str) -> Part {
    bytes_part(mime, Some("utf-8"), text.as_bytes(), None)
}

pub(crate) fn bytes_part(mime: &str, charset: Option<&str>, data: &[u8], extra: Option<Vec<String>>) -> Part {
    let mut headers = vec![match charset {
        Some(c) => format!("Content-Type: {mime}; charset=\"{c}\""),
        None => format!("Content-Type: {mime}"),
    }];
    headers.extend(extra.unwrap_or_default());
    let body = if mime.starts_with("text/") {
        headers.push("Content-Transfer-Encoding: quoted-printable".into());
        quoted_printable(data)
    } else {
        headers.push("Content-Transfer-Encoding: base64".into());
        base64_lines(data)
    };
    Part { headers, body }
}

fn attachment_part(a: &Attachment, inline: bool, counter: &mut u32, made_from: &str) -> Part {
    let p = &a.props;
    let filename = p.text(ATTACH_LONG_FILENAME).or(p.text(ATTACH_FILENAME));
    if let Some(embedded) = &a.embedded {
        let mut body = String::new();
        write_message(embedded, &mut body, counter, made_from);
        let mut headers = vec!["Content-Type: message/rfc822".to_string()];
        headers.push(disposition("attachment", filename.or(embedded.props.text(SUBJECT)).map(|s| format!("{s}.eml")).as_deref()));
        return Part { headers, body };
    }
    let mime = p.text(ATTACH_MIME_TAG).filter(|m| m.contains('/') && m.is_ascii()).unwrap_or("application/octet-stream");
    let data = p.bytes(ATTACH_DATA).unwrap_or_default();
    let mut extra = vec![disposition(if inline { "inline" } else { "attachment" }, filename)];
    if let Some(cid) = p.text(ATTACH_CONTENT_ID).filter(|c| c.is_ascii()) {
        extra.push(format!("Content-ID: <{}>", cid.trim_matches(['<', '>'])));
    }
    // Attachments are kept byte for byte: never as text with a charset.
    let mut part = bytes_part(mime, None, data, Some(extra));
    if mime.starts_with("text/") {
        part.headers.retain(|h| !h.starts_with("Content-Transfer-Encoding"));
        part.headers.push("Content-Transfer-Encoding: base64".into());
        part.body = base64_lines(data);
    }
    part
}

pub(crate) fn disposition(kind: &str, filename: Option<&str>) -> String {
    match filename {
        None => format!("Content-Disposition: {kind}"),
        Some(f) if f.is_ascii() && !f.contains(['"', '\\']) => format!("Content-Disposition: {kind}; filename=\"{f}\""),
        Some(f) => {
            let mut encoded = String::new();
            for b in f.bytes() {
                if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
                    encoded.push(b as char);
                } else {
                    let _ = write!(encoded, "%{b:02X}");
                }
            }
            format!("Content-Disposition: {kind}; filename*=utf-8''{encoded}")
        }
    }
}

pub(crate) fn multipart(subtype: &str, parts: Vec<Part>, counter: &mut u32) -> Part {
    *counter += 1;
    let boundary = format!("=_eudora_{subtype}_{}_{:08x}", *counter, parts.len() as u32 ^ 0x5eed_0000);
    let mut body = String::new();
    for part in parts {
        let _ = write!(body, "--{boundary}\r\n");
        for h in &part.headers {
            body.push_str(h);
            body.push_str("\r\n");
        }
        body.push_str("\r\n");
        body.push_str(&part.body);
        if !part.body.ends_with("\r\n") {
            body.push_str("\r\n");
        }
    }
    let _ = write!(body, "--{boundary}--\r\n");
    Part { headers: vec![format!("Content-Type: multipart/{subtype}; boundary=\"{boundary}\"")], body }
}

pub(crate) fn base64_lines(data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 38);
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push_str("\r\n");
    }
    out
}

/// Quoted-printable (RFC 2045), keeping the text's own line breaks.
fn quoted_printable(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() + data.len() / 8);
    let lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    for (n, line) in lines.iter().enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let mut width = 0;
        for (i, &b) in line.iter().enumerate() {
            let last = i + 1 == line.len();
            let plain = (b == b' ' || b == b'\t') && !last || (33..=126).contains(&b) && b != b'=';
            let piece = if plain { (b as char).to_string() } else { format!("={b:02X}") };
            if width + piece.len() > 75 {
                out.push_str("=\r\n");
                width = 0;
            }
            width += piece.len();
            out.push_str(&piece);
        }
        if n + 1 < lines.len() {
            out.push_str("\r\n");
        }
    }
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};

    /// Builds FastTransfer streams for tests.
    #[derive(Default)]
    struct Stream(Vec<u8>);

    impl Stream {
        fn tag(&mut self, id: u16, ptype: u16) -> &mut Self {
            self.0.extend_from_slice(&(((id as u32) << 16) | ptype as u32).to_le_bytes());
            self
        }
        fn marker(&mut self, m: u32) -> &mut Self {
            self.0.extend_from_slice(&m.to_le_bytes());
            self
        }
        fn text(&mut self, id: u16, s: &str) -> &mut Self {
            let mut b: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
            b.extend([0, 0]);
            self.tag(id, 0x84B0).sized(&b)
        }
        fn bytes(&mut self, id: u16, b: &[u8]) -> &mut Self {
            self.tag(id, 0x0102).sized(b)
        }
        fn int(&mut self, id: u16, n: i32) -> &mut Self {
            self.tag(id, 0x0003);
            self.0.extend_from_slice(&n.to_le_bytes());
            self
        }
        fn time(&mut self, id: u16, ft: i64) -> &mut Self {
            self.tag(id, 0x0040);
            self.0.extend_from_slice(&ft.to_le_bytes());
            self
        }
        fn sized(&mut self, b: &[u8]) -> &mut Self {
            self.0.extend_from_slice(&(b.len() as u32).to_le_bytes());
            self.0.extend_from_slice(b);
            self
        }
        fn export(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for (kind, data) in [(1u32, &[0u8; 8][..]), (4, &[0u8; 6][..]), (2, &self.0[..])] {
                out.extend_from_slice(&kind.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(data);
            }
            out
        }
    }

    fn recipient(s: &mut Stream, kind: i32, name: &str, email: &str) {
        s.marker(START_RECIP).int(RECIPIENT_TYPE, kind).text(DISPLAY_NAME, name).text(ADDR_TYPE, "SMTP").text(EMAIL_ADDRESS, email).marker(END_TO_RECIP);
    }

    const JAN_2000: i64 = 125_911_584_000_000_000; // 2000-01-01T00:00:00Z

    #[test]
    fn original_headers_are_kept_and_body_replaced() {
        let mut s = Stream::default();
        s.text(SUBJECT, "Hello")
            .text(TRANSPORT_HEADERS, "From: A <a@example.com>\r\nTo: b@example.com\r\nSubject: Hello\r\nMessage-Id: <1@x>\r\nX-Eudora-Signature: <Standard>\r\nContent-Type: text/html; charset=\"us-ascii\"\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\0")
            .int(INTERNET_CODEPAGE, 1252);
        // Exchange's own two-byte-prefixed time (seen as 0x3309) must not stop the reader.
        s.tag(0x3309, 0x4040);
        s.0.extend_from_slice(&[0, 2]);
        s.0.extend_from_slice(&JAN_2000.to_le_bytes());
        s.bytes(HTML, b"<html>caf\xe9</html>\r\n");
        recipient(&mut s, 1, "B", "b@example.com");
        let item = parse(&s.export()).unwrap();
        assert_eq!(item.recipients.len(), 1);
        let mime = to_mime(&item);
        let text = String::from_utf8(mime.clone()).unwrap();
        assert!(text.contains("X-Eudora-Signature: <Standard>"));
        assert!(!text.contains("8bit"));
        let m = MessageParser::default().parse(&mime).unwrap();
        assert_eq!(m.message_id(), Some("1@x"));
        assert_eq!(m.subject(), Some("Hello"));
        assert_eq!(m.body_html(0).unwrap().trim(), "<html>café</html>");
    }

    #[test]
    fn headers_made_from_properties_when_none_were_kept() {
        let mut s = Stream::default();
        s.text(SUBJECT, "Grüße")
            .text(SENDER_NAME, "Ann Smith, Jr.")
            .text(SENDER_ADDR_TYPE, "SMTP")
            .text(SENDER_EMAIL, "ann@example.com")
            .time(CLIENT_SUBMIT_TIME, JAN_2000)
            .text(MESSAGE_ID, "<abc@example.com>")
            .text(BODY, "Plain text\r\nsecond line");
        recipient(&mut s, 1, "Bob", "bob@example.com");
        recipient(&mut s, 2, "Cy", "cy@example.com");
        let m_bytes = to_mime(&parse(&s.export()).unwrap());
        let m = MessageParser::default().parse(&m_bytes).unwrap();
        assert_eq!(m.subject(), Some("Grüße"));
        assert_eq!(m.from().unwrap().first().unwrap().name(), Some("Ann Smith, Jr."));
        assert_eq!(m.to().unwrap().first().unwrap().address(), Some("bob@example.com"));
        assert_eq!(m.cc().unwrap().first().unwrap().address(), Some("cy@example.com"));
        assert_eq!(m.date().unwrap().year, 2000);
        assert_eq!(m.message_id(), Some("abc@example.com"));
        assert_eq!(m.body_text(0).unwrap().trim_end(), "Plain text\r\nsecond line");
    }

    #[test]
    fn attachments_inline_pictures_and_embedded_messages() {
        let mut s = Stream::default();
        s.text(SUBJECT, "With files").text(BODY, "see attached").bytes(HTML, b"<img src=\"cid:pic1\">");
        let picture = [0x89u8, b'P', b'N', b'G', 0, 1, 2, 3];
        s.marker(NEW_ATTACH).int(ATTACH_METHOD, 1).text(ATTACH_LONG_FILENAME, "Résumé.pdf").text(ATTACH_MIME_TAG, "application/pdf").bytes(ATTACH_DATA, b"%PDF-1.4 data").marker(END_ATTACH);
        s.marker(NEW_ATTACH).int(ATTACH_METHOD, 1).text(ATTACH_LONG_FILENAME, "pic.png").text(ATTACH_MIME_TAG, "image/png").text(ATTACH_CONTENT_ID, "pic1").bytes(ATTACH_DATA, &picture).marker(END_ATTACH);
        s.marker(NEW_ATTACH).int(ATTACH_METHOD, 5).marker(START_EMBED).text(SUBJECT, "Inner").text(BODY, "inner body");
        recipient(&mut s, 1, "Dee", "dee@example.com");
        s.marker(END_EMBED).marker(END_ATTACH);
        let item = parse(&s.export()).unwrap();
        assert_eq!(item.attachments.len(), 3);
        assert!(item.attachments[2].embedded.is_some());
        let bytes = to_mime(&item);
        let m = MessageParser::default().parse(&bytes).unwrap();
        assert_eq!(m.body_text(0).unwrap().trim(), "see attached");
        let names: Vec<_> = m.attachments().map(|a| a.attachment_name().unwrap_or("").to_string()).collect();
        assert!(names.contains(&"Résumé.pdf".to_string()), "{names:?}");
        let pdf = m.attachments().find(|a| a.attachment_name() == Some("Résumé.pdf")).unwrap();
        assert_eq!(pdf.contents(), b"%PDF-1.4 data");
        let png = m.attachments().find(|a| a.content_id() == Some("pic1")).unwrap();
        assert_eq!(png.contents(), &picture);
        let inner = m.attachments().find_map(|a| a.message()).unwrap();
        assert_eq!(inner.subject(), Some("Inner"));
        assert_eq!(inner.to().unwrap().first().unwrap().address(), Some("dee@example.com"));
    }

    #[test]
    fn named_and_multi_valued_properties() {
        let mut s = Stream::default();
        s.0.extend_from_slice(&0x836B_84B0u32.to_le_bytes());
        s.0.extend_from_slice(&[7; 16]);
        s.0.push(1);
        s.0.extend("content-type".encode_utf16().flat_map(u16::to_le_bytes));
        s.0.extend([0, 0]);
        let value: Vec<u8> = "text/plain\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        s.sized(&value);
        s.tag(0x8001, 0x0003);
        s.0.extend_from_slice(&[7; 16]);
        s.0.push(0);
        s.0.extend_from_slice(&0x85D7u32.to_le_bytes());
        s.0.extend_from_slice(&1i32.to_le_bytes());
        s.tag(0x3A71, 0x101F); // multi-valued Unicode: two strings
        s.0.extend_from_slice(&2u32.to_le_bytes());
        for word in ["one", "two"] {
            let b: Vec<u8> = word.encode_utf16().flat_map(u16::to_le_bytes).collect();
            s.sized(&b);
        }
        s.tag(0x3311, 0x400B); // Exchange's 0x4000 flag on a boolean: two bytes, then the value
        s.0.extend_from_slice(&[0, 2, 1, 0]);
        s.tag(0x3A72, 0x94B0); // multi-valued string in code page 1200 (seen on archived mail)
        s.0.extend_from_slice(&2u32.to_le_bytes());
        for word in ["a", "b"] {
            let mut b: Vec<u8> = word.encode_utf16().flat_map(u16::to_le_bytes).collect();
            b.extend([0, 0]);
            s.sized(&b);
        }
        s.int(SUBJECT, 7);
        let item = parse_stream(&s.0).unwrap();
        assert_eq!(item.props.get(0x3311), Some(&Value::Bool(true)));
        assert_eq!(item.props.get(0x3A72), Some(&Value::Multi(vec![Value::Text("a".into()), Value::Text("b".into())])));
        assert_eq!(item.props.named("Content-Type"), Some(&Value::Text("text/plain".into())));
        assert_eq!(item.props.0[1].named, Some(([7; 16], Name::Id(0x85D7))));
        assert_eq!(item.props.get(0x3A71), Some(&Value::Multi(vec![Value::Text("one".into()), Value::Text("two".into())])));
    }

    #[test]
    fn damaged_streams_are_errors_not_panics() {
        let mut s = Stream::default();
        s.text(SUBJECT, "x");
        let full = s.0.clone();
        for cut in 1..full.len() {
            let _ = parse_stream(&full[..cut]);
        }
        let mut odd = Stream::default();
        odd.tag(0x1234, 0x0099);
        assert!(parse_stream(&odd.0).is_err());
        assert!(parse(b"nothing here").is_err());
    }

    #[test]
    fn compressed_rtf() {
        assert_eq!(RTF_PRELOAD.len(), 207);
        // MS-OXRTFCP 3.1.1, example 1.
        let compressed = [
            0x2d, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x00, 0x00, 0x4c, 0x5a, 0x46, 0x75, 0xf1, 0xc5, 0xc7, 0xa7, 0x03, 0x00, 0x0a, 0x00,
            0x72, 0x63, 0x70, 0x67, 0x31, 0x32, 0x35, 0x42, 0x32, 0x0a, 0xf3, 0x20, 0x68, 0x65, 0x6c, 0x09, 0x00, 0x20, 0x62,
            0x77, 0x05, 0xb0, 0x6c, 0x64, 0x7d, 0x0a, 0x80, 0x0f, 0xa0,
        ];
        assert_eq!(decompress_rtf(&compressed).unwrap(), b"{\\rtf1\\ansi\\ansicpg1252\\pard hello world}\r\n");
        let mut stored = vec![0u8; 16];
        stored[0..4].copy_from_slice(&(12u32 + 5).to_le_bytes());
        stored[4..8].copy_from_slice(&5u32.to_le_bytes());
        stored[8..12].copy_from_slice(&0x414C_454Du32.to_le_bytes());
        stored.extend_from_slice(b"{\\rtf");
        assert_eq!(decompress_rtf(&stored).unwrap(), b"{\\rtf");
    }

    #[test]
    fn quoted_printable_round_trip() {
        let text = "a line with = sign and trailing space \r\n".to_string() + &"x".repeat(200) + "\r\nlast";
        let qp = quoted_printable(text.as_bytes());
        assert!(qp.lines().all(|l| l.len() <= 76));
        let mime = format!("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{qp}");
        let m = MessageParser::default().parse(mime.as_bytes()).unwrap();
        assert_eq!(m.body_text(0).unwrap().replace("\r\n", "\n").trim_end(), text.replace("\r\n", "\n"));
    }
}
