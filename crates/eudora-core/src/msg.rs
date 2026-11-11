//! Outlook .msg files ([MS-OXMSG]): a message written as Outlook saves one, for dragging a
//! message out of Eudora into a folder.
//!
//! A .msg is an OLE compound file holding a message's MAPI properties: Outlook's own fields, not
//! the message's internet bytes. Outlook itself keeps no more than that: Exchange turns each
//! arriving message into these properties, keeping its header block word for word
//! (`PR_TRANSPORT_MESSAGE_HEADERS`) and every attachment byte for byte. Eudora writes them from:
//!
//! - **Exchange's own export** of the message, when Eudora kept one (the Online Archive's items):
//!   the very properties Exchange holds, written as they are, which is what Outlook would save;
//! - else the **original message** (MIME), converted as Exchange converts arriving mail: the
//!   header block exactly, the text, the HTML, every attachment's exact bytes, forwarded messages
//!   as messages inside it, and a signed or encrypted message (S/MIME) whole and unchanged as
//!   Outlook stores those, so its signature can still be checked.
//!
//! The exact original bytes are what an .eml holds (dragging with Ctrl or Shift).

use std::collections::HashMap;
use std::io::{Cursor, Write};

use mail_parser::{MessageParser, MimeHeaders};

use crate::error::{Error, Result};
use crate::fts::{Attachment, Item, Name, Prop, Props, Value};

// Property ids used here (the type is chosen from the value when written).
const MESSAGE_CLASS: u16 = 0x001A;
const SUBJECT: u16 = 0x0037;
const IMPORTANCE: u16 = 0x0017;
const CLIENT_SUBMIT_TIME: u16 = 0x0039;
const SENT_REPRESENTING_ENTRYID: u16 = 0x0041;
const SENT_REPRESENTING_NAME: u16 = 0x0042;
const REPLY_RECIPIENT_ENTRIES: u16 = 0x004F;
const REPLY_RECIPIENT_NAMES: u16 = 0x0050;
const SENT_REPRESENTING_ADDRTYPE: u16 = 0x0064;
const SENT_REPRESENTING_EMAIL: u16 = 0x0065;
const CONVERSATION_TOPIC: u16 = 0x0070;
const TRANSPORT_HEADERS: u16 = 0x007D;
const SENDER_ENTRYID: u16 = 0x0C19;
const SENDER_NAME: u16 = 0x0C1A;
const SENDER_ADDRTYPE: u16 = 0x0C1E;
const SENDER_EMAIL: u16 = 0x0C1F;
const RECIPIENT_TYPE: u16 = 0x0C15;
const DISPLAY_BCC: u16 = 0x0E02;
const DISPLAY_CC: u16 = 0x0E03;
const DISPLAY_TO: u16 = 0x0E04;
const DELIVERY_TIME: u16 = 0x0E06;
const MESSAGE_FLAGS: u16 = 0x0E07;
const NORMALIZED_SUBJECT: u16 = 0x0E1D;
const ATTACH_SIZE: u16 = 0x0E20;
const ATTACH_NUM: u16 = 0x0E21;
const OBJECT_TYPE: u16 = 0x0FFE;
const ENTRYID: u16 = 0x0FFF;
const BODY: u16 = 0x1000;
const HTML: u16 = 0x1013;
const NATIVE_BODY: u16 = 0x1016;
const INTERNET_MESSAGE_ID: u16 = 0x1035;
const INTERNET_REFERENCES: u16 = 0x1039;
const IN_REPLY_TO: u16 = 0x1042;
const ROWID: u16 = 0x3000;
const DISPLAY_NAME: u16 = 0x3001;
const ADDRTYPE: u16 = 0x3002;
const EMAIL_ADDRESS: u16 = 0x3003;
const CREATION_TIME: u16 = 0x3007;
const LAST_MODIFICATION_TIME: u16 = 0x3008;
const SEARCH_KEY: u16 = 0x300B;
const STORE_SUPPORT_MASK: u16 = 0x340D;
const ATTACH_DATA: u16 = 0x3701;
const ATTACH_EXTENSION: u16 = 0x3703;
const ATTACH_FILENAME: u16 = 0x3704;
const ATTACH_METHOD: u16 = 0x3705;
const ATTACH_LONG_FILENAME: u16 = 0x3707;
const RENDERING_POSITION: u16 = 0x370B;
const ATTACH_MIME_TAG: u16 = 0x370E;
const ATTACH_CONTENT_ID: u16 = 0x3712;
const ATTACH_FLAGS: u16 = 0x3714;
const DISPLAY_TYPE: u16 = 0x3900;
const SMTP_ADDRESS: u16 = 0x39FE;
const INTERNET_CPID: u16 = 0x3FDE;
const SENDER_SMTP: u16 = 0x5D01;
const SENT_REPRESENTING_SMTP: u16 = 0x5D02;
const RECIPIENT_FLAGS: u16 = 0x5FFD;
const RECIPIENT_DISPLAY_NAME: u16 = 0x5FF6;
const ATTACHMENT_HIDDEN: u16 = 0x7FFE;

/// Unicode strings throughout (`STORE_UNICODE_OK`), so Outlook reads 0x001F strings.
const STORE_UNICODE_OK: i64 = 0x0004_0000;
const MSGFLAG_READ: i64 = 0x01;
const MSGFLAG_HASATTACH: i64 = 0x10;
/// How deep forwarded messages inside messages are written (a loop of them stops here).
const MAX_DEPTH: usize = 12;

fn prop(id: u16, ptype: u16, value: Value) -> Prop {
    Prop { id, ptype, named: None, value }
}

fn text(id: u16, value: &str) -> Prop {
    prop(id, 0x001F, Value::Text(value.to_string()))
}

fn int(id: u16, value: i64) -> Prop {
    prop(id, 0x0003, Value::Int(value))
}

fn time(id: u16, value: i64) -> Prop {
    prop(id, 0x0040, Value::Time(value))
}

fn bytes(id: u16, value: Vec<u8>) -> Prop {
    prop(id, 0x0102, Value::Bytes(value))
}

/// Unix seconds as a FILETIME (100 ns ticks since 1601).
fn filetime(unix: i64) -> i64 {
    (unix + 11_644_473_600) * 10_000_000
}

/// Outlook's "one-off" entry id for an internet address (what Reply uses).
fn one_off_entry_id(name: &str, address: &str) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0];
    out.extend_from_slice(&[0x81, 0x2B, 0x1F, 0xA4, 0xBE, 0xA3, 0x10, 0x19, 0x9D, 0x6E, 0x00, 0xDD, 0x01, 0x0F, 0x54, 0x02]);
    out.extend_from_slice(&[0, 0]); // version
    out.extend_from_slice(&[0x01, 0x90]); // Unicode, no rich text
    for s in [if name.is_empty() { address } else { name }, "SMTP", address] {
        out.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        out.extend_from_slice(&[0, 0]);
    }
    out
}

/// A FLATENTRYLIST of one-off entry ids (Reply-To).
fn flat_entry_list(people: &[(String, String)]) -> Vec<u8> {
    let mut entries = Vec::new();
    for (name, address) in people {
        let id = one_off_entry_id(name, address);
        entries.extend_from_slice(&(id.len() as u32).to_le_bytes());
        entries.extend_from_slice(&id);
        while entries.len() % 4 != 0 {
            entries.push(0);
        }
    }
    let mut out = Vec::with_capacity(entries.len() + 8);
    out.extend_from_slice(&(people.len() as u32).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&entries);
    out
}

/// Header bytes as text: UTF-8, else Windows-1252 (every byte kept as a character).
fn header_text(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1252.decode_without_bom_handling(raw).0.into_owned(),
    }
}

/// A header's value as written, unfolded (None when missing).
fn header(raw: &[u8], name: &str) -> Option<String> {
    let value = crate::parse::raw_header(raw, name)?;
    let text = header_text(&value).replace("\r\n", "").replace('\n', "");
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The subject without its "Re:" / "Fwd:" prefixes (Outlook's conversation topic).
fn topic(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(rest) = ["re:", "fw:", "fwd:", "aw:", "wg:", "tr:", "sv:", "vs:"].iter().find(|p| lower.starts_with(**p)).map(|p| &s[p.len()..]) else { break };
        s = rest.trim_start();
    }
    s.to_string()
}

/// Turns an original message (MIME) into Outlook's properties.
pub fn from_mime(raw: &[u8]) -> Item {
    from_mime_at(raw, 0)
}

fn from_mime_at(raw: &[u8], depth: usize) -> Item {
    let mut item = Item::default();
    let props = &mut item.props;
    let parsed = MessageParser::default().parse(raw);

    // the header block, exactly as received (Exchange keeps it the same way)
    let header_end = parsed.as_ref().map(|m| m.root_part().offset_body as usize).filter(|&n| n > 0).unwrap_or_else(|| {
        raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)).unwrap_or(raw.len())
    });
    props.0.push(text(TRANSPORT_HEADERS, &header_text(&raw[..header_end.min(raw.len())])));

    let Some(m) = parsed else {
        // (not readable as a message at all: the whole of it as the text, nothing dropped)
        props.0.push(text(MESSAGE_CLASS, "IPM.Note"));
        props.0.push(text(BODY, &header_text(raw)));
        return finish(item);
    };

    let subject = m.subject().map(crate::parse::clean_line).unwrap_or_default();
    let content_type = m.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("")).to_ascii_lowercase()).unwrap_or_default();
    let smime_signed = content_type == "multipart/signed";
    let smime_sealed = matches!(content_type.as_str(), "application/pkcs7-mime" | "application/x-pkcs7-mime");
    props.0.push(text(
        MESSAGE_CLASS,
        if smime_signed {
            "IPM.Note.SMIME.MultipartSigned"
        } else if smime_sealed {
            "IPM.Note.SMIME"
        } else {
            "IPM.Note"
        },
    ));
    props.0.push(text(SUBJECT, &subject));
    let topic = topic(&subject);
    props.0.push(text(CONVERSATION_TOPIC, &topic));
    props.0.push(text(NORMALIZED_SUBJECT, &topic));

    let date = m.date().map(|d| d.to_timestamp()).unwrap_or(0);
    if date > 0 {
        for id in [CLIENT_SUBMIT_TIME, DELIVERY_TIME, CREATION_TIME, LAST_MODIFICATION_TIME] {
            props.0.push(time(id, filetime(date)));
        }
    }

    let people = |a: Option<&mail_parser::Address>| -> Vec<(String, String)> {
        a.map(|a| {
            a.iter()
                .map(|p| (crate::parse::clean_line(p.name().unwrap_or_default()), p.address().unwrap_or_default().trim().to_string()))
                .filter(|(n, a)| !n.is_empty() || !a.is_empty())
                .collect()
        })
        .unwrap_or_default()
    };
    let from = people(m.from());
    let sender = people(m.sender());
    let reply_to = people(m.reply_to());
    let shown = |p: &[(String, String)]| p.iter().map(|(n, a)| if n.is_empty() { a.clone() } else { n.clone() }).collect::<Vec<_>>().join("; ");

    if let Some((name, address)) = from.first() {
        props.0.push(text(SENT_REPRESENTING_NAME, if name.is_empty() { address } else { name }));
        props.0.push(text(SENT_REPRESENTING_ADDRTYPE, "SMTP"));
        props.0.push(text(SENT_REPRESENTING_EMAIL, address));
        props.0.push(text(SENT_REPRESENTING_SMTP, address));
        props.0.push(bytes(SENT_REPRESENTING_ENTRYID, one_off_entry_id(name, address)));
    }
    if let Some((name, address)) = sender.first().or(from.first()) {
        props.0.push(text(SENDER_NAME, if name.is_empty() { address } else { name }));
        props.0.push(text(SENDER_ADDRTYPE, "SMTP"));
        props.0.push(text(SENDER_EMAIL, address));
        props.0.push(text(SENDER_SMTP, address));
        props.0.push(bytes(SENDER_ENTRYID, one_off_entry_id(name, address)));
    }
    if !reply_to.is_empty() {
        props.0.push(text(REPLY_RECIPIENT_NAMES, &shown(&reply_to)));
        props.0.push(bytes(REPLY_RECIPIENT_ENTRIES, flat_entry_list(&reply_to)));
    }

    let (to, cc, bcc) = (people(m.to()), people(m.cc()), people(m.bcc()));
    props.0.push(text(DISPLAY_TO, &shown(&to)));
    props.0.push(text(DISPLAY_CC, &shown(&cc)));
    props.0.push(text(DISPLAY_BCC, &shown(&bcc)));
    for (kind, list) in [(1, &to), (2, &cc), (3, &bcc)] {
        for (name, address) in list {
            let row = item.recipients.len() as i64;
            let shown_name = if name.is_empty() { address.as_str() } else { name.as_str() };
            let mut search_key = format!("SMTP:{}", address.to_uppercase()).into_bytes();
            search_key.push(0);
            item.recipients.push(Props(vec![
                int(ROWID, row),
                int(RECIPIENT_TYPE, kind),
                text(DISPLAY_NAME, shown_name),
                text(RECIPIENT_DISPLAY_NAME, shown_name),
                text(ADDRTYPE, "SMTP"),
                text(EMAIL_ADDRESS, address),
                text(SMTP_ADDRESS, address),
                bytes(ENTRYID, one_off_entry_id(name, address)),
                bytes(SEARCH_KEY, search_key),
                int(OBJECT_TYPE, 6),
                int(DISPLAY_TYPE, 0),
                int(RECIPIENT_FLAGS, 1),
            ]));
        }
    }
    let props = &mut item.props;

    for (id, name) in [(INTERNET_MESSAGE_ID, "Message-ID"), (IN_REPLY_TO, "In-Reply-To"), (INTERNET_REFERENCES, "References")] {
        if let Some(value) = header(raw, name) {
            props.0.push(text(id, &value));
        }
    }
    let priority = header(raw, "Importance").map(|v| v.to_ascii_lowercase()).or_else(|| header(raw, "X-Priority"));
    let importance = match priority.as_deref().map(|p| p.trim_start()) {
        Some(p) if p.starts_with("high") || p.starts_with('1') || p.starts_with('2') => 2,
        Some(p) if p.starts_with("low") || p.starts_with('4') || p.starts_with('5') => 0,
        _ => 1,
    };
    props.0.push(int(IMPORTANCE, importance));

    // the text, and the HTML when the message has one (not one made from the text)
    let html = m.html_body.iter().filter_map(|&i| m.parts.get(i as usize)).find(|p| p.is_text_html()).and_then(|p| p.text_contents()).map(str::to_string);
    let body = m.body_text(0).map(|t| t.into_owned()).unwrap_or_default();
    props.0.push(text(BODY, &body));
    if let Some(html) = &html {
        props.0.push(bytes(HTML, html.as_bytes().to_vec()));
        props.0.push(int(INTERNET_CPID, 65001));
    }
    props.0.push(int(NATIVE_BODY, if html.is_some() { 3 } else { 1 }));

    if smime_signed || smime_sealed {
        // Outlook keeps a signed or encrypted message whole, as one attachment: the original's
        // bytes unchanged, so the signature still checks
        item.attachments.push(Attachment {
            props: Props(vec![
                int(ATTACH_METHOD, 1),
                bytes(ATTACH_DATA, raw.to_vec()),
                text(ATTACH_FILENAME, "smime.p7m"),
                text(ATTACH_LONG_FILENAME, "smime.p7m"),
                text(DISPLAY_NAME, "smime.p7m"),
                text(ATTACH_EXTENSION, ".p7m"),
                text(ATTACH_MIME_TAG, &content_type),
                int(RENDERING_POSITION, -1),
            ]),
            embedded: None,
        });
        return finish(item);
    }

    for (n, &part_id) in m.attachments.iter().enumerate() {
        let Some(part) = m.parts.get(part_id as usize) else { continue };
        let mime = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".into())
            .to_ascii_lowercase();
        let contents = part.contents();
        let mut a = Props(vec![int(RENDERING_POSITION, -1), int(ATTACH_NUM, n as i64), int(ATTACH_SIZE, contents.len() as i64)]);
        if part.is_message() && depth < MAX_DEPTH {
            // a forwarded message: a message inside this one, as Outlook keeps it
            let inner = from_mime_at(contents, depth + 1);
            let name = inner.props.text(SUBJECT).map(str::to_string).filter(|s| !s.is_empty()).unwrap_or_else(|| "Forwarded message".into());
            a.0.extend([int(ATTACH_METHOD, 5), text(DISPLAY_NAME, &name), text(ATTACH_MIME_TAG, "message/rfc822")]);
            item.attachments.push(Attachment { props: a, embedded: Some(Box::new(inner)) });
            continue;
        }
        let name = part
            .attachment_name()
            .map(crate::parse::clean_line)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| if part.is_message() { format!("message-{}.eml", n + 1) } else { format!("attachment-{}", n + 1) });
        let extension = name.rsplit_once('.').map(|(_, e)| format!(".{e}")).unwrap_or_default();
        a.0.extend([
            int(ATTACH_METHOD, 1),
            bytes(ATTACH_DATA, contents.to_vec()),
            text(ATTACH_FILENAME, &name),
            text(ATTACH_LONG_FILENAME, &name),
            text(DISPLAY_NAME, &name),
            text(ATTACH_EXTENSION, &extension),
            text(ATTACH_MIME_TAG, &mime),
        ]);
        if let Some(cid) = part.content_id().map(|c| c.trim().trim_matches(['<', '>']).to_string()).filter(|c| !c.is_empty()) {
            // a picture the HTML shows in place: hidden from the attachment list, as Outlook does
            let shown = html.as_deref().is_some_and(|h| h.contains(&format!("cid:{cid}")));
            a.0.push(text(ATTACH_CONTENT_ID, &cid));
            if shown {
                a.0.push(prop(ATTACHMENT_HIDDEN, 0x000B, Value::Bool(true)));
                a.0.push(int(ATTACH_FLAGS, 4));
            }
        }
        item.attachments.push(Attachment { props: a, embedded: None });
    }
    finish(item)
}

/// The flags every message gets: read, and whether it has attachments to show.
fn finish(mut item: Item) -> Item {
    let shown = item.attachments.iter().any(|a| !matches!(a.props.get(ATTACHMENT_HIDDEN), Some(Value::Bool(true))));
    item.props.0.push(int(MESSAGE_FLAGS, MSGFLAG_READ | if shown { MSGFLAG_HASATTACH } else { 0 }));
    item
}

// ------------------------------------------------------------------------------------- writing

/// What a value is written as: (its MSG property type, written inline in 8 bytes?).
fn kind_of(p: &Prop) -> Option<u16> {
    let base = |t: u16| if t & 0x8000 != 0 { 0 } else { t & 0x0FFF & !0x4000 };
    Some(match &p.value {
        Value::Int(_) => match base(p.ptype) {
            t @ (0x0002 | 0x0003 | 0x000A | 0x0006 | 0x0014) => t,
            _ => 0x0003,
        },
        Value::Float(_) => match base(p.ptype) {
            0x0004 => 0x0004,
            0x0007 => 0x0007,
            _ => 0x0005,
        },
        Value::Bool(_) => 0x000B,
        Value::Time(_) => 0x0040,
        Value::Guid(_) => 0x0048,
        Value::Text(_) => 0x001F,
        Value::Bytes(_) => 0x0102,
        Value::Multi(values) => match values.first() {
            None => return None,
            Some(Value::Text(_)) => 0x101F,
            Some(Value::Bytes(_)) => 0x1102,
            Some(Value::Int(_)) => match base(p.ptype) {
                t @ (0x0002 | 0x0003 | 0x0014) => 0x1000 | t,
                _ => 0x1003,
            },
            Some(Value::Float(_)) => 0x1005,
            Some(Value::Time(_)) => 0x1040,
            Some(Value::Guid(_)) => 0x1048,
            Some(_) => return None,
        },
    })
}

fn fixed_bytes(kind: u16, value: &Value) -> Vec<u8> {
    match (kind & 0x0FFF, value) {
        (0x0002, Value::Int(n)) => (*n as i16).to_le_bytes().to_vec(),
        (0x0003 | 0x000A, Value::Int(n)) => (*n as i32).to_le_bytes().to_vec(),
        (0x0004, Value::Float(f)) => (*f as f32).to_le_bytes().to_vec(),
        (_, Value::Float(f)) => f.to_le_bytes().to_vec(),
        (_, Value::Int(n)) | (_, Value::Time(n)) => n.to_le_bytes().to_vec(),
        (_, Value::Bool(b)) => u16::from(*b).to_le_bytes().to_vec(),
        (_, Value::Guid(g)) => g.to_vec(),
        _ => Vec::new(),
    }
}

fn utf16(s: &str) -> Vec<u8> {
    s.trim_end_matches('\0').encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Named properties across the whole message (its recipients, attachments, and messages inside
/// it share one map): (property set, name) → its id here (0x8000 on).
#[derive(Default)]
struct Names {
    ids: HashMap<([u8; 16], NameKey), u16>,
    order: Vec<([u8; 16], NameKey)>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum NameKey {
    Id(u32),
    Text(String),
}

impl Names {
    fn id(&mut self, guid: [u8; 16], name: &Name) -> u16 {
        let key = (guid, match name {
            Name::Id(n) => NameKey::Id(*n),
            Name::Text(t) => NameKey::Text(t.clone()),
        });
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        let id = 0x8000 + self.order.len() as u16;
        self.ids.insert(key.clone(), id);
        self.order.push(key);
        id
    }
}

const PS_MAPI: [u8; 16] = [0x28, 0x03, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
const PS_PUBLIC_STRINGS: [u8; 16] = [0x29, 0x03, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

type Cfb = cfb::CompoundFile<Cursor<Vec<u8>>>;

fn io(e: std::io::Error) -> Error {
    Error::Other(format!("writing the .msg file: {e}"))
}

fn stream(file: &mut Cfb, path: &str, data: &[u8]) -> Result<()> {
    let mut s = file.create_stream(path).map_err(io)?;
    s.write_all(data).map_err(io)?;
    Ok(())
}

/// Where an object's properties go, and how its header starts.
#[derive(Clone, Copy, PartialEq)]
enum Level {
    Top,
    Embedded,
    Part,
}

fn write_props(file: &mut Cfb, dir: &str, props: &Props, names: &mut Names, level: Level, counts: (u32, u32)) -> Result<()> {
    let mut table = Vec::new();
    match level {
        Level::Top | Level::Embedded => {
            table.extend_from_slice(&[0; 8]);
            for n in [counts.0, counts.1, counts.0, counts.1] {
                table.extend_from_slice(&n.to_le_bytes());
            }
            if level == Level::Top {
                table.extend_from_slice(&[0; 8]);
            }
        }
        Level::Part => table.extend_from_slice(&[0; 8]),
    }
    let mut seen = std::collections::HashSet::new();
    for p in &props.0 {
        let id = match &p.named {
            Some((guid, name)) => names.id(*guid, name),
            None => p.id,
        };
        let Some(kind) = kind_of(p) else { continue };
        // (a property given twice: the first)
        if !seen.insert(id) {
            continue;
        }
        let tag = (u32::from(id) << 16) | u32::from(kind);
        table.extend_from_slice(&tag.to_le_bytes());
        table.extend_from_slice(&6u32.to_le_bytes()); // readable, writable
        let name = format!("{dir}__substg1.0_{tag:08X}");
        match (&p.value, kind) {
            (Value::Text(t), _) => {
                let data = utf16(t);
                table.extend_from_slice(&(data.len() as u32 + 2).to_le_bytes());
                table.extend_from_slice(&[0; 4]);
                stream(file, &name, &data)?;
            }
            (Value::Bytes(b), _) => {
                table.extend_from_slice(&(b.len() as u32).to_le_bytes());
                table.extend_from_slice(&[0; 4]);
                stream(file, &name, b)?;
            }
            (Value::Guid(g), _) => {
                table.extend_from_slice(&16u32.to_le_bytes());
                table.extend_from_slice(&[0; 4]);
                stream(file, &name, g)?;
            }
            (Value::Multi(values), 0x101F | 0x1102) => {
                let mut lengths = Vec::new();
                for (i, v) in values.iter().enumerate() {
                    let data = match v {
                        Value::Text(t) => {
                            let mut d = utf16(t);
                            d.extend_from_slice(&[0, 0]);
                            d
                        }
                        Value::Bytes(b) => b.clone(),
                        _ => Vec::new(),
                    };
                    lengths.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    if kind == 0x1102 {
                        lengths.extend_from_slice(&[0; 4]);
                    }
                    stream(file, &format!("{name}-{i:08X}"), &data)?;
                }
                table.extend_from_slice(&(lengths.len() as u32).to_le_bytes());
                table.extend_from_slice(&[0; 4]);
                stream(file, &name, &lengths)?;
            }
            (Value::Multi(values), _) => {
                let data: Vec<u8> = values.iter().flat_map(|v| fixed_bytes(kind, v)).collect();
                table.extend_from_slice(&(data.len() as u32).to_le_bytes());
                table.extend_from_slice(&[0; 4]);
                stream(file, &name, &data)?;
            }
            (value, _) => {
                let mut eight = fixed_bytes(kind, value);
                eight.resize(8, 0);
                table.extend_from_slice(&eight);
            }
        }
    }
    stream(file, &format!("{dir}__properties_version1.0"), &table)
}

/// Writes one message (and what's inside it) under `dir` ("/" for the file itself).
fn write_message(file: &mut Cfb, dir: &str, item: &Item, names: &mut Names, level: Level) -> Result<()> {
    let mut props = Props(item.props.0.iter().filter(|p| p.named.is_some() || p.id != STORE_SUPPORT_MASK).cloned().collect());
    if level == Level::Top {
        props.0.insert(0, int(STORE_SUPPORT_MASK, STORE_UNICODE_OK));
    }
    write_props(file, dir, &props, names, level, (item.recipients.len() as u32, item.attachments.len() as u32))?;
    for (i, r) in item.recipients.iter().enumerate() {
        let sub = format!("{dir}__recip_version1.0_#{i:08X}");
        file.create_storage(&sub).map_err(io)?;
        write_props(file, &format!("{sub}/"), r, names, Level::Part, (0, 0))?;
    }
    for (i, a) in item.attachments.iter().enumerate() {
        let sub = format!("{dir}__attach_version1.0_#{i:08X}");
        file.create_storage(&sub).map_err(io)?;
        let mut props = Props(a.props.0.iter().filter(|p| !(p.named.is_none() && p.id == ATTACH_DATA && a.embedded.is_some())).cloned().collect());
        if let Some(inner) = &a.embedded {
            // the message inside: a storage of its own, named by PR_ATTACH_DATA_OBJ
            props.0.retain(|p| p.named.is_some() || p.id != ATTACH_METHOD);
            props.0.push(int(ATTACH_METHOD, 5));
            let obj = format!("{sub}/__substg1.0_3701000D");
            file.create_storage(&obj).map_err(io)?;
            write_message(file, &format!("{obj}/"), inner, names, Level::Embedded)?;
            write_props(file, &format!("{sub}/"), &props, names, Level::Part, (0, 0))?;
            // (the object's own entry: size 0xFFFFFFFF, its data being the storage)
            let path = format!("{sub}/__properties_version1.0");
            let mut table = Vec::new();
            {
                use std::io::Read;
                file.open_stream(&path).map_err(io)?.read_to_end(&mut table).map_err(io)?;
            }
            table.extend_from_slice(&0x3701_000Du32.to_le_bytes());
            table.extend_from_slice(&6u32.to_le_bytes());
            table.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            table.extend_from_slice(&[0; 4]);
            stream(file, &path, &table)?;
        } else {
            write_props(file, &format!("{sub}/"), &props, names, Level::Part, (0, 0))?;
        }
    }
    Ok(())
}

/// The named-property map every .msg has (`__nameid_version1.0`).
fn write_names(file: &mut Cfb, names: &Names) -> Result<()> {
    let dir = "/__nameid_version1.0";
    file.create_storage(dir).map_err(io)?;
    let mut guids: Vec<[u8; 16]> = Vec::new();
    let (mut entries, mut strings) = (Vec::new(), Vec::new());
    let mut buckets: HashMap<u32, Vec<u8>> = HashMap::new();
    for (i, (guid, key)) in names.order.iter().enumerate() {
        let guid_index: u32 = if *guid == PS_MAPI {
            1
        } else if *guid == PS_PUBLIC_STRINGS {
            2
        } else {
            let at = guids.iter().position(|g| g == guid).unwrap_or_else(|| {
                guids.push(*guid);
                guids.len() - 1
            });
            3 + at as u32
        };
        let (value, hash, kind) = match key {
            NameKey::Id(n) => (*n, *n, 0u32),
            NameKey::Text(t) => {
                let name = utf16(t);
                let offset = strings.len() as u32;
                strings.extend_from_slice(&(name.len() as u32).to_le_bytes());
                strings.extend_from_slice(&name);
                while strings.len() % 4 != 0 {
                    strings.push(0);
                }
                (offset, crc32(&name), 1)
            }
        };
        let info = ((i as u32) << 16) | (guid_index << 1) | kind;
        entries.extend_from_slice(&value.to_le_bytes());
        entries.extend_from_slice(&info.to_le_bytes());
        let bucket = 0x1000 + ((hash ^ ((guid_index << 1) | kind)) % 0x1F);
        let b = buckets.entry(bucket).or_default();
        b.extend_from_slice(&hash.to_le_bytes());
        b.extend_from_slice(&info.to_le_bytes());
    }
    stream(file, &format!("{dir}/__substg1.0_00020102"), &guids.concat())?;
    stream(file, &format!("{dir}/__substg1.0_00030102"), &entries)?;
    stream(file, &format!("{dir}/__substg1.0_00040102"), &strings)?;
    for (bucket, data) in buckets {
        stream(file, &format!("{dir}/__substg1.0_{bucket:04X}0102"), &data)?;
    }
    Ok(())
}

/// Fills in what Exchange works out for itself and leaves out of its export: the To, Cc, and Bcc
/// lines Outlook shows (from the recipients), here and in the messages inside.
fn completed(item: &Item) -> Item {
    let mut item = item.clone();
    for (id, kind) in [(DISPLAY_TO, 1), (DISPLAY_CC, 2), (DISPLAY_BCC, 3)] {
        if item.props.get(id).is_some() {
            continue;
        }
        let names: Vec<&str> = item
            .recipients
            .iter()
            .filter(|r| r.int(RECIPIENT_TYPE).map(|t| t & 0x0F) == Some(kind))
            .filter_map(|r| r.text(DISPLAY_NAME).or(r.text(SMTP_ADDRESS)).or(r.text(EMAIL_ADDRESS)))
            .collect();
        item.props.0.push(text(id, &names.join("; ")));
    }
    for a in &mut item.attachments {
        if let Some(inner) = &a.embedded {
            a.embedded = Some(Box::new(completed(inner)));
        }
    }
    item
}

/// A message as an Outlook .msg file.
pub fn write(item: &Item) -> Result<Vec<u8>> {
    let item = &completed(item);
    let mut file = cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new())).map_err(io)?;
    // (the CLSID Outlook gives a saved message)
    file.set_storage_clsid("/", uuid::Uuid::from_u128(0x0002_0D0B_0000_0000_C000_0000_0000_0046)).map_err(io)?;
    let mut names = Names::default();
    write_message(&mut file, "/", item, &mut names, Level::Top)?;
    write_names(&mut file, &names)?;
    file.flush().map_err(io)?;
    Ok(file.into_inner().into_inner())
}

// --------------------------------------------------------------------------------- the library

/// Which kind of file a message is written out as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveAs {
    /// Outlook's .msg.
    Msg,
    /// The original bytes exactly (.eml).
    Eml,
}

impl crate::library::Library {
    /// A message as an Outlook .msg: from Exchange's own export when Eudora kept one (exactly
    /// what Exchange holds), else converted from the original message.
    pub fn message_msg(&self, id: i64) -> Result<Vec<u8>> {
        if id > 0 {
            for copy in self.copies_of(id)? {
                if copy.format == "exchange-export"
                    && let Ok(item) = crate::fts::parse(&self.copy_bytes(copy.id)?)
                {
                    return write(&item);
                }
            }
        }
        write(&from_mime(&self.raw_message(id)?))
    }

    /// Writes messages into a folder as .msg or .eml files named by their subjects (marked as
    /// downloaded from the Internet, as attachments are); returns where each went.
    pub fn save_messages(&self, ids: &[i64], how: SaveAs, dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
        ids.iter().map(|&id| self.save_message(id, how, dir)).collect()
    }

    /// Writes one message into a folder (see `save_messages`).
    pub fn save_message(&self, id: i64, how: SaveAs, dir: &std::path::Path) -> Result<std::path::PathBuf> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
        {
            let raw = self.raw_message(id)?;
            let subject = MessageParser::default().parse_headers(&raw).and_then(|m| m.subject().map(crate::parse::clean_line)).filter(|s| !s.trim().is_empty());
            // (a subject ending in a dot or a space: trimmed, so the name isn't "décor..msg")
            let stem = subject.map(|s| s.chars().take(120).collect::<String>().trim_end_matches(['.', ' ']).to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "(no subject)".into());
            let (data, ext) = match how {
                // (the stored bytes themselves: nothing parsed or rewritten)
                SaveAs::Eml => (raw, "eml"),
                SaveAs::Msg => (self.message_msg(id)?, "msg"),
            };
            let name = crate::saving::safe_file_name(&format!("{stem}.{ext}"));
            let (path, there) = crate::saving::place_for(dir, &name, &data);
            if !there {
                std::fs::write(&path, &data).map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
            }
            crate::saving::mark_downloaded(&path);
            Ok(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const MAIL: &[u8] = b"Received: from mx.example.com by mail.example.org; Tue, 3 Nov 2026 10:00:00 +0000\r\n\
From: Ann Smith <ann@example.com>\r\n\
To: Bob Jones <bob@example.org>, carol@example.org\r\n\
Cc: Dan <dan@example.org>\r\n\
Subject: Re: The contract\r\n\
Date: Tue, 3 Nov 2026 10:00:00 +0000\r\n\
Message-ID: <abc123@example.com>\r\n\
In-Reply-To: <prev@example.org>\r\n\
Importance: high\r\n\
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

    fn read(file: &mut cfb::CompoundFile<Cursor<Vec<u8>>>, path: &str) -> Vec<u8> {
        let mut out = Vec::new();
        file.open_stream(path).unwrap().read_to_end(&mut out).unwrap();
        out
    }

    fn string(file: &mut cfb::CompoundFile<Cursor<Vec<u8>>>, path: &str) -> String {
        let b = read(file, path);
        String::from_utf16(&b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_message_becomes_an_outlook_msg() {
        let msg = write(&from_mime(MAIL)).unwrap();
        let mut f = cfb::CompoundFile::open(Cursor::new(msg)).unwrap();
        assert_eq!(string(&mut f, "/__substg1.0_001A001F"), "IPM.Note");
        assert_eq!(string(&mut f, "/__substg1.0_0037001F"), "Re: The contract");
        assert_eq!(string(&mut f, "/__substg1.0_0070001F"), "The contract");
        assert_eq!(string(&mut f, "/__substg1.0_0C1F001F"), "ann@example.com");
        assert_eq!(string(&mut f, "/__substg1.0_0E04001F"), "Bob Jones; carol@example.org");
        assert_eq!(string(&mut f, "/__substg1.0_1035001F"), "<abc123@example.com>");
        assert!(string(&mut f, "/__substg1.0_1000001F").contains("Signed copy attached. Café."));
        assert_eq!(read(&mut f, "/__substg1.0_10130102"), "<p>Signed copy attached. Café.</p>".as_bytes());
        // the header block exactly as it came
        let end = MAIL.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(string(&mut f, "/__substg1.0_007D001F").as_bytes(), &MAIL[..end]);
        // the top header: 3 recipients, 2 attachments
        let table = read(&mut f, "/__properties_version1.0");
        assert_eq!(u32::from_le_bytes(table[16..20].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(table[20..24].try_into().unwrap()), 2);
        // the string sizes count the terminator; importance high
        let entries: Vec<&[u8]> = table[32..].chunks_exact(16).collect();
        let subject = entries.iter().find(|e| e[..4] == 0x0037_001Fu32.to_le_bytes()).unwrap();
        assert_eq!(u32::from_le_bytes(subject[8..12].try_into().unwrap()), "Re: The contract".len() as u32 * 2 + 2);
        let importance = entries.iter().find(|e| e[..4] == 0x0017_0003u32.to_le_bytes()).unwrap();
        assert_eq!(importance[8], 2);
        // recipients: Cc is type 2
        assert_eq!(string(&mut f, "/__recip_version1.0_#00000002/__substg1.0_39FE001F"), "dan@example.org");
        let r = read(&mut f, "/__recip_version1.0_#00000002/__properties_version1.0");
        let ty = r[8..].chunks_exact(16).find(|e| e[..4] == 0x0C15_0003u32.to_le_bytes()).unwrap();
        assert_eq!(ty[8], 2);
        // the attachment's exact bytes
        assert_eq!(read(&mut f, "/__attach_version1.0_#00000000/__substg1.0_37010102"), b"%PDF-1.4\n%\xc7\xec\x8f\xa2\n");
        assert_eq!(string(&mut f, "/__attach_version1.0_#00000000/__substg1.0_3707001F"), "contract.pdf");
        // the forwarded message: a message inside, with its own 24-byte header
        assert_eq!(string(&mut f, "/__attach_version1.0_#00000001/__substg1.0_3701000D/__substg1.0_0037001F"), "The contract");
        assert_eq!(read(&mut f, "/__attach_version1.0_#00000001/__substg1.0_3701000D/__properties_version1.0").len() % 16, 8);
        assert!(f.is_storage("/__nameid_version1.0"));
    }

    #[test]
    fn a_signed_message_is_kept_whole() {
        let raw = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: Signed\r\nMIME-Version: 1.0\r\nContent-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256; boundary=\"s\"\r\n\r\n--s\r\nContent-Type: text/plain\r\n\r\nHello\r\n--s\r\nContent-Type: application/pkcs7-signature; name=smime.p7s\r\nContent-Transfer-Encoding: base64\r\n\r\nAAAA\r\n--s--\r\n";
        let mut f = cfb::CompoundFile::open(Cursor::new(write(&from_mime(raw)).unwrap())).unwrap();
        assert_eq!(string(&mut f, "/__substg1.0_001A001F"), "IPM.Note.SMIME.MultipartSigned");
        assert_eq!(read(&mut f, "/__attach_version1.0_#00000000/__substg1.0_37010102"), raw);
    }

    #[test]
    fn named_properties_are_mapped() {
        let mut item = from_mime(MAIL);
        item.props.0.push(Prop { id: 0x9001, ptype: 0x101F, named: Some((PS_PUBLIC_STRINGS, Name::Text("Keywords".into()))), value: Value::Multi(vec![Value::Text("Legal".into())]) });
        let mut f = cfb::CompoundFile::open(Cursor::new(write(&item).unwrap())).unwrap();
        let entries = read(&mut f, "/__nameid_version1.0/__substg1.0_00030102");
        assert_eq!(entries.len(), 8);
        assert_eq!(u32::from_le_bytes(entries[4..8].try_into().unwrap()), (2 << 1) | 1);
        assert_eq!(read(&mut f, "/__substg1.0_8000101F-00000000"), [utf16("Legal"), vec![0, 0]].concat());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
