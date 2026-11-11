//! Turns a raw message into what the library stores and indexes, cleaning up what
//! thirty years of mail software got wrong. The raw bytes themselves are never changed:
//! fixes only affect the fields read from them.
//!
//! Handled here: headers with raw 8-bit text (decoded as Windows-1252 when not UTF-8),
//! text with no charset, Windows-1252 text labeled ISO-8859-1 (C1 control characters),
//! uuencoded attachments (decoded), BinHex attachments (kept, noted), missing or broken
//! dates (Received, mbox "From " line, file time), missing Message-IDs (made from the
//! content, so the same message from two sources gets the same one).

use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

use mail_parser::{Address, Encoding, HeaderValue, Message, MessageParser, MessagePart, MimeHeaders};

/// Body text kept for the index, per message (characters). Longer bodies are cut.
pub const INDEX_BODY_LIMIT: usize = 256 * 1024;
const SNIPPET_LEN: usize = 200;
/// Dates later than this far past "now" are treated as broken (bad clocks).
const FUTURE_SLACK_SECS: i64 = 2 * 86_400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addr {
    pub name: String,
    pub address: String,
}

impl Addr {
    /// "Name <address>", or whichever part exists.
    pub fn display(&self) -> String {
        match (self.name.is_empty(), self.address.is_empty()) {
            (false, false) => format!("{} <{}>", self.name, self.address),
            (false, true) => self.name.clone(),
            _ => self.address.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Mime,
    Uuencode,
    BinHex,
}

impl AttachmentKind {
    pub fn code(self) -> &'static str {
        match self {
            AttachmentKind::Mime => "mime",
            AttachmentKind::Uuencode => "uuencode",
            AttachmentKind::BinHex => "binhex",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AttachmentInfo {
    /// Where it is in the message: MIME part number, or "uu:<n>" / "binhex:<n>" inside the text.
    pub part: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// BLAKE3 of the decoded bytes (BinHex: of the encoded block, not decoded yet).
    pub hash: [u8; 32],
    pub kind: AttachmentKind,
    /// A picture the message's HTML shows itself (a logo, a signature's photo: `cid:`), not a
    /// file the sender attached: no paperclip in the list for it.
    pub inline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateSource {
    Header,
    Received,
    Envelope,
    File,
    Unknown,
}

impl DateSource {
    pub fn code(self) -> u8 {
        match self {
            DateSource::Header => 0,
            DateSource::Received => 1,
            DateSource::Envelope => 2,
            DateSource::File => 3,
            DateSource::Unknown => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseStatus {
    Ok,
    /// Parsed, but something had to be guessed or fixed (see `problems`).
    Fixed,
    /// The parser couldn't read it: stored raw, searchable as plain text.
    Failed,
}

impl ParseStatus {
    pub fn code(self) -> u8 {
        match self {
            ParseStatus::Ok => 0,
            ParseStatus::Fixed => 1,
            ParseStatus::Failed => 2,
        }
    }
}

/// Something that had to be fixed or guessed. `kind` is a short stable code.
#[derive(Clone, Debug)]
pub struct Problem {
    pub kind: &'static str,
    pub detail: String,
}

/// Where the date came from when the Date header was missing or broken.
#[derive(Clone, Copy, Debug, Default)]
pub struct DateHints {
    /// The mbox "From " line's date.
    pub envelope: Option<i64>,
    /// The file's modification time (EML files).
    pub file: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct ParsedMessage {
    pub hash: [u8; 32],
    pub raw_len: u64,
    pub message_id: String,
    pub message_id_made: bool,
    /// Unix seconds, UTC; 0 when unknown.
    pub date: i64,
    pub date_source: DateSource,
    pub from: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub subject: String,
    pub snippet: String,
    pub body_text: String,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub list_id: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
    pub status: ParseStatus,
    pub problems: Vec<Problem>,
}

impl ParsedMessage {
    /// The attachments the sender attached: not the pictures the message shows in its text
    /// (the list's paperclip and `hasattachment:` count these).
    pub fn attached(&self) -> usize {
        self.attachments.iter().filter(|a| !a.inline).count()
    }

    pub fn from_display(&self) -> String {
        self.from.first().map(Addr::display).unwrap_or_default()
    }

    fn note(&mut self, kind: &'static str, detail: impl Into<String>) {
        if !self.problems.iter().any(|p| p.kind == kind) {
            self.problems.push(Problem { kind, detail: detail.into() });
        }
    }
}

/// Parses a raw message. Never fails: what can't be read is noted and kept as plain text.
pub fn parse(raw: &[u8], hints: DateHints, now: i64) -> ParsedMessage {
    let hash = *blake3::hash(raw).as_bytes();
    let result = catch_unwind(AssertUnwindSafe(|| parse_inner(raw, hash, hints, now)));
    match result {
        Ok(Some(parsed)) => parsed,
        Ok(None) => fallback(raw, hash, hints, now, "the message structure couldn't be read"),
        Err(_) => fallback(raw, hash, hints, now, "the mail parser stopped on this message"),
    }
}

fn empty(raw: &[u8], hash: [u8; 32]) -> ParsedMessage {
    ParsedMessage {
        hash,
        raw_len: raw.len() as u64,
        message_id: String::new(),
        message_id_made: false,
        date: 0,
        date_source: DateSource::Unknown,
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: String::new(),
        snippet: String::new(),
        body_text: String::new(),
        in_reply_to: Vec::new(),
        references: Vec::new(),
        list_id: None,
        attachments: Vec::new(),
        status: ParseStatus::Ok,
        problems: Vec::new(),
    }
}

fn parse_inner(raw: &[u8], hash: [u8; 32], hints: DateHints, now: i64) -> Option<ParsedMessage> {
    let mut out = empty(raw, hash);
    let fixed = fix_header_block(raw);
    if matches!(fixed, Cow::Owned(_)) {
        out.note("header-8bit", "header text with no charset read as Windows-1252");
    }
    let message = MessageParser::default().parse(fixed.as_ref())?;
    if message.parts.is_empty() {
        return None;
    }

    out.subject = clean_line(&fix_c1(message.subject().unwrap_or_default(), &mut out));
    out.from = addresses(message.from(), &mut out);
    out.to = addresses(message.to(), &mut out);
    out.cc = addresses(message.cc(), &mut out);
    out.bcc = addresses(message.bcc(), &mut out);
    out.in_reply_to = ids(message.in_reply_to());
    out.references = ids(message.references());
    out.list_id = message.list_id().as_text().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let (date, source) = best_date(&message, hints, now, &mut out);
    out.date = date;
    out.date_source = source;

    let mut body = String::new();
    let mut uu_count = 0;
    for &part_id in &message.text_body {
        let Some(part) = message.parts.get(part_id as usize) else { continue };
        let text = part_text(fixed.as_ref(), part, &mut out);
        let text = if part.is_text_html() { Cow::Owned(mail_parser::decoders::html::html_to_text(&text)) } else { text };
        let text = extract_inline_attachments(&text, &mut out, &mut uu_count);
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&text);
    }
    // a plain-text version left empty beside the real HTML one (Walgreens' receipts, 2026-10-17:
    // no preview, nothing to search, no Receipt): the text comes from the HTML then
    if body.trim().is_empty() {
        for &part_id in &message.html_body {
            let Some(part) = message.parts.get(part_id as usize) else { continue };
            if !part.is_text_html() {
                continue;
            }
            let html = part_text(fixed.as_ref(), part, &mut out);
            let text = mail_parser::decoders::html::html_to_text(&html);
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&text);
        }
    }
    out.snippet = snippet(&body);
    out.body_text = cap(body, INDEX_BODY_LIMIT);

    // the Content-IDs the HTML shows (`cid:`): those pictures are part of the message
    let html: String = message.parts.iter().filter(|p| p.is_text_html()).filter_map(|p| p.text_contents()).collect();
    for (n, &part_id) in message.attachments.iter().enumerate() {
        let Some(part) = message.parts.get(part_id as usize) else { continue };
        let mut a = mime_attachment(part, part_id, n);
        a.inline = a.mime.starts_with("image/")
            && part.content_id().map(|cid| cid.trim().trim_matches(['<', '>'])).is_some_and(|cid| !cid.is_empty() && html.contains(&format!("cid:{cid}")));
        out.attachments.push(a);
    }

    match message.message_id().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => out.message_id = normalize_id(id),
        None => {
            out.message_id = made_message_id(&out);
            out.message_id_made = true;
            out.note("no-message-id", "no Message-ID: one was made from the content");
        }
    }
    if !out.problems.is_empty() {
        out.status = ParseStatus::Fixed;
    }
    Some(out)
}

/// A message ready to show: the same reading the index got (`parsed`: addresses, date, problems,
/// attachments), plus its bodies in full.
#[derive(Clone, Debug)]
pub struct DisplayMessage {
    pub parsed: ParsedMessage,
    /// The HTML body, with `cid:` pictures turned into `data:` URLs. Not cleaned: show it only
    /// in a sandbox that runs no scripts and loads nothing from the network.
    pub html: Option<String>,
    /// The text body (from the HTML when there's no text part), uuencoded blocks taken out.
    pub text: String,
    /// The header block as written.
    pub headers: String,
    /// The attachments the HTML shows itself (pictures it names by `cid:`): their part numbers.
    pub inline: Vec<String>,
}

/// Reads a message for display. Never fails, like [`parse`].
pub fn display(raw: &[u8], now: i64) -> DisplayMessage {
    let parsed = parse(raw, DateHints::default(), now);
    let end = header_end(raw);
    let headers = decode_8bit(&raw[..end]).trim_end().to_string();
    let bodies = catch_unwind(AssertUnwindSafe(|| display_bodies(raw))).ok().flatten();
    let (html, text, inline) = bodies.unwrap_or_else(|| (None, decode_8bit(&raw[end..]).trim_start().to_string(), Vec::new()));
    DisplayMessage { parsed, html, text, headers, inline }
}

fn display_bodies(raw: &[u8]) -> Option<(Option<String>, String, Vec<String>)> {
    let fixed = fix_header_block(raw);
    let message = MessageParser::default().parse(fixed.as_ref())?;
    let mut scratch = empty(raw, [0; 32]);
    let html = message
        .html_body
        .iter()
        .filter_map(|&id| message.parts.get(id as usize))
        .find(|p| p.is_text_html())
        .map(|p| inline_pictures(&message, part_text(fixed.as_ref(), p, &mut scratch).into_owned()));
    let (html, inline) = match html {
        Some((h, parts)) => (Some(h), parts),
        None => (None, Vec::new()),
    };
    let mut text = String::new();
    let mut uu_count = 0;
    for &part_id in &message.text_body {
        let Some(part) = message.parts.get(part_id as usize) else { continue };
        let body = part_text(fixed.as_ref(), part, &mut scratch);
        let body = if part.is_text_html() { Cow::Owned(mail_parser::decoders::html::html_to_text(&body)) } else { body };
        let body = extract_inline_attachments(&body, &mut scratch, &mut uu_count);
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&body);
    }
    Some((html, text, inline))
}

/// Every text part of a message as text: the plain parts and the HTML ones (made into text),
/// each once. For reading facts out of a message (amounts), where the HTML part often holds
/// what the plain one leaves out. Never fails: an unreadable message gives "".
pub fn all_text(raw: &[u8]) -> String {
    let Some(message) = catch_unwind(AssertUnwindSafe(|| MessageParser::default().parse(raw))).ok().flatten() else { return String::new() };
    let mut out = String::new();
    for part in message.parts.iter() {
        let text = match &part.body {
            mail_parser::PartType::Text(t) => t.to_string(),
            mail_parser::PartType::Html(h) => mail_parser::decoders::html::html_to_text(h),
            _ => continue,
        };
        out.push_str(&text);
        out.push('\n');
        if out.len() > 200_000 {
            break;
        }
    }
    out
}

/// Points `cid:` references at the pictures themselves (`data:` URLs); with the part numbers
/// of the pictures it showed that way.
fn inline_pictures(message: &Message, mut html: String) -> (String, Vec<String>) {
    use base64::Engine as _;
    let mut shown = Vec::new();
    if !html.contains("cid:") {
        return (html, shown);
    }
    for (n, part) in message.parts.iter().enumerate() {
        let (Some(cid), Some(ct)) = (part.content_id(), part.content_type()) else { continue };
        if !ct.ctype().eq_ignore_ascii_case("image") {
            continue;
        }
        let mime = format!("image/{}", ct.subtype().unwrap_or("png")).to_ascii_lowercase();
        let url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(part.contents()));
        let reference = format!("cid:{}", cid.trim().trim_matches(['<', '>']));
        if html.contains(&reference) {
            html = html.replace(&reference, &url);
            shown.push(n.to_string());
        }
    }
    (html, shown)
}

/// What's kept when the parser can't read a message: the header lines we can find and the
/// whole thing as text, so it can still be found.
fn fallback(raw: &[u8], hash: [u8; 32], hints: DateHints, now: i64, why: &str) -> ParsedMessage {
    let mut out = empty(raw, hash);
    out.status = ParseStatus::Failed;
    out.note("unreadable", why);
    let text = decode_8bit(raw);
    out.subject = raw_header(raw, "subject").map(|s| clean_line(&decode_8bit(&s))).unwrap_or_default();
    out.snippet = snippet(&text);
    out.body_text = cap(text.into_owned(), INDEX_BODY_LIMIT);
    let (date, source) = hint_date(hints, now);
    out.date = date;
    out.date_source = source;
    out.message_id = made_message_id(&out);
    out.message_id_made = true;
    out
}

fn hint_date(hints: DateHints, now: i64) -> (i64, DateSource) {
    let plausible = |t: &i64| (0..=now + FUTURE_SLACK_SECS).contains(t);
    if let Some(t) = hints.envelope.filter(plausible) {
        (t, DateSource::Envelope)
    } else if let Some(t) = hints.file.filter(plausible) {
        (t, DateSource::File)
    } else {
        (0, DateSource::Unknown)
    }
}

fn best_date(message: &Message, hints: DateHints, now: i64, out: &mut ParsedMessage) -> (i64, DateSource) {
    let plausible = |t: i64| (0..=now + FUTURE_SLACK_SECS).contains(&t);
    if let Some(dt) = message.date().filter(|d| d.is_valid()) {
        let t = dt.to_timestamp();
        if plausible(t) {
            return (t, DateSource::Header);
        }
        out.note("bad-date", format!("Date header out of range ({})", dt.to_rfc3339()));
    } else if let Some(text) = message.header_raw("Date") {
        match lenient_date(text).filter(|t| plausible(*t)) {
            Some(t) => {
                out.note("loose-date", format!("Date header in an old format read loosely ({})", text.trim()));
                return (t, DateSource::Header);
            }
            None => out.note("bad-date", format!("Date header couldn't be read ({})", text.trim())),
        }
    } else {
        out.note("no-date", "no Date header");
    }
    // The newest Received line is the first one; the oldest (last) is closest to sending.
    let received = message
        .received_all()
        .filter_map(|r| r.date())
        .filter(|d| d.is_valid())
        .map(|d| d.to_timestamp())
        .filter(|t| plausible(*t))
        .last();
    if let Some(t) = received {
        return (t, DateSource::Received);
    }
    hint_date(hints, now)
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// Zone names old mail used, in minutes east of UTC.
fn zone_minutes(name: &str) -> Option<i64> {
    Some(match name.to_ascii_uppercase().as_str() {
        "GMT" | "UT" | "UTC" | "Z" | "WET" => 0,
        "BST" | "CET" | "MET" | "MEZ" | "WEST" => 60,
        "CEST" | "MEST" | "MESZ" | "EET" => 120,
        "EEST" => 180,
        "IST" => 330,
        "JST" | "KST" => 540,
        "AEST" | "EAST" => 600,
        "AST" | "EDT" => -240,
        "EST" => -300,
        "CDT" => -300,
        "CST" => -360,
        "MDT" => -360,
        "MST" => -420,
        "PDT" => -420,
        "PST" => -480,
        "AKST" => -540,
        "HST" => -600,
        _ => return None,
    })
}

/// Reads the dates old mail software wrote that strict RFC 822 parsing refuses: no seconds,
/// no weekday, two-digit years ("95" = 1995, "03" = 2003), three-digit Y2K-bug years ("100" =
/// 2000), zone names, "1995-02-01 09:00", words in any order. Unix seconds UTC.
pub fn lenient_date(text: &str) -> Option<i64> {
    // drop comments: "(EST)"
    let mut cleaned = String::with_capacity(text.len());
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => cleaned.push(if c == ',' { ' ' } else { c }),
            _ => {}
        }
    }
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let (mut year, mut month, mut day) = (None::<i64>, None::<u32>, None::<u32>);
    let (mut h, mut m, mut s) = (0u32, 0u32, 0u32);
    let mut offset = 0i64;
    let mut numbers: Vec<i64> = Vec::new();
    for token in &tokens {
        let lower = token.to_ascii_lowercase();
        if let Some((y, mo, d)) = iso_date(token) {
            (year, month, day) = (Some(y), Some(mo), Some(d));
        } else if lower.len() >= 3 && MONTHS.contains(&&lower[..3]) && lower.chars().all(|c| c.is_ascii_alphabetic() || c == '.') {
            month = MONTHS.iter().position(|x| *x == &lower[..3]).map(|i| i as u32 + 1);
        } else if token.contains(':') {
            let mut parts = token.split(':').map(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<u32>());
            h = parts.next()?.ok()?;
            m = parts.next().and_then(|p| p.ok()).unwrap_or(0);
            s = parts.next().and_then(|p| p.ok()).unwrap_or(0);
        } else if (token.starts_with('+') || token.starts_with('-')) && token.len() == 5 && token[1..].bytes().all(|b| b.is_ascii_digit()) {
            let v: i64 = token[1..].parse().ok()?;
            offset = (v / 100 * 60 + v % 100) * if token.starts_with('-') { -1 } else { 1 };
        } else if let Some(z) = zone_minutes(token) {
            offset = z;
        } else if let Ok(n) = token.parse::<i64>() {
            numbers.push(n);
        }
    }
    // Numbers left: the day and the year, in either order.
    for n in numbers {
        if day.is_none() && (1..=31).contains(&n) {
            day = Some(n as u32);
        } else if year.is_none() {
            year = Some(match n {
                0..=49 => 2000 + n,
                50..=99 => 1900 + n,
                100..=199 => 1900 + n, // the Y2K bug: years counted from 1900
                _ => n,
            });
        }
    }
    let date = chrono::NaiveDate::from_ymd_opt(year? as i32, month?, day?)?.and_hms_opt(h.min(23), m.min(59), s.min(59))?;
    Some(date.and_utc().timestamp() - offset * 60)
}

fn iso_date(token: &str) -> Option<(i64, u32, u32)> {
    let parts: Vec<&str> = token.split(['-', '/']).collect();
    if parts.len() != 3 || parts[0].len() != 4 {
        return None;
    }
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].get(..2).unwrap_or(parts[2]).parse().ok()?))
}

fn addresses(address: Option<&Address>, out: &mut ParsedMessage) -> Vec<Addr> {
    let Some(address) = address else { return Vec::new() };
    address
        .iter()
        .map(|a| Addr {
            name: clean_line(&fix_c1(a.name().unwrap_or_default(), out)),
            address: a.address().unwrap_or_default().trim().to_string(),
        })
        .filter(|a| !a.name.is_empty() || !a.address.is_empty())
        .collect()
}

fn ids(value: &HeaderValue) -> Vec<String> {
    let list: Vec<&str> = match value {
        HeaderValue::Text(t) => vec![t.as_ref()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.as_ref()).collect(),
        _ => Vec::new(),
    };
    list.into_iter().map(normalize_id).filter(|s| !s.is_empty()).collect()
}

/// Message-IDs without angle brackets and spaces (case kept: the left part is case-sensitive).
pub fn normalize_id(id: &str) -> String {
    id.trim().trim_start_matches('<').trim_end_matches('>').chars().filter(|c| !c.is_whitespace()).collect()
}

/// A Message-ID made from what doesn't change between copies of a message (not the raw
/// bytes: another copy may have more Received lines).
fn made_message_id(m: &ParsedMessage) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&m.date.to_le_bytes());
    for a in &m.from {
        h.update(a.address.to_lowercase().as_bytes());
    }
    h.update(b"\0");
    h.update(m.subject.as_bytes());
    h.update(b"\0");
    let body_start: String = m.body_text.chars().take(4096).collect();
    h.update(body_start.as_bytes());
    let hex = h.finalize().to_hex();
    format!("{}@made.eudora", &hex[..32])
}

fn mime_attachment(part: &MessagePart, part_id: u32, n: usize) -> AttachmentInfo {
    let contents = part.contents();
    let mime = part
        .content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
        .to_ascii_lowercase();
    let name = part
        .attachment_name()
        .map(|s| clean_line(s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if part.is_message() { format!("message-{}.eml", n + 1) } else { format!("attachment-{}", n + 1) });
    AttachmentInfo {
        part: part_id.to_string(),
        name,
        mime,
        size: contents.len() as u64,
        hash: *blake3::hash(contents).as_bytes(),
        kind: AttachmentKind::Mime,
        inline: false,
    }
}

/// A text part's text, re-read from its bytes when it had no charset and wasn't UTF-8.
fn part_text<'a>(raw: &[u8], part: &'a MessagePart, out: &mut ParsedMessage) -> Cow<'a, str> {
    let text = part.text_contents().unwrap_or_default();
    let charset = part.content_type().and_then(|c| c.attribute("charset")).map(|c| c.to_ascii_lowercase());
    let unlabeled = matches!(charset.as_deref(), None | Some("us-ascii") | Some("ascii"));
    if text.contains('\u{FFFD}') && unlabeled && part.encoding == Encoding::None {
        let start = part.raw_body_offset() as usize;
        let end = (part.raw_end_offset() as usize).min(raw.len());
        if let Some(bytes) = raw.get(start..end) {
            if std::str::from_utf8(bytes).is_err() {
                out.note("charset-guessed", "text with no charset read as Windows-1252");
                return Cow::Owned(decode_8bit(bytes).into_owned());
            }
        }
    }
    match fix_c1(text, out) {
        Cow::Owned(s) => Cow::Owned(s),
        Cow::Borrowed(_) => Cow::Borrowed(text),
    }
}

/// Bytes as UTF-8 when they are, else as Windows-1252 (what old mail without a charset
/// almost always was).
pub fn decode_8bit(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes).0,
    }
}

/// C1 control characters (U+0080-U+009F) never occur in real text: they're Windows-1252
/// bytes (curly quotes, dashes, €) decoded as ISO-8859-1.
fn fix_c1<'a>(text: &'a str, out: &mut ParsedMessage) -> Cow<'a, str> {
    if !text.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)) {
        return Cow::Borrowed(text);
    }
    out.note("c1-fixed", "Windows-1252 characters labeled as ISO-8859-1");
    let fixed = text
        .chars()
        .map(|c| {
            if ('\u{80}'..='\u{9f}').contains(&c) {
                let byte = [c as u32 as u8];
                encoding_rs::WINDOWS_1252.decode_without_bom_handling(&byte).0.chars().next().unwrap_or(c)
            } else {
                c
            }
        })
        .collect();
    Cow::Owned(fixed)
}

/// End of the header block (index of the blank line's first byte), or the whole message.
fn header_end(raw: &[u8]) -> usize {
    let mut i = 0;
    while i < raw.len() {
        match memchr::memchr(b'\n', &raw[i..]) {
            Some(n) => {
                let line_start = i + n + 1;
                if raw.get(line_start) == Some(&b'\n') {
                    return line_start;
                }
                if raw.get(line_start..line_start + 2) == Some(b"\r\n") {
                    return line_start;
                }
                i = line_start;
            }
            None => break,
        }
    }
    raw.len()
}

/// When the header block holds raw 8-bit text that isn't UTF-8, a copy with the headers
/// converted from Windows-1252 to UTF-8 (the body untouched) for the parser to read.
fn fix_header_block(raw: &[u8]) -> Cow<'_, [u8]> {
    let end = header_end(raw);
    let headers = &raw[..end];
    if headers.is_ascii() || std::str::from_utf8(headers).is_ok() {
        return Cow::Borrowed(raw);
    }
    let decoded = encoding_rs::WINDOWS_1252.decode_without_bom_handling(headers).0;
    let mut fixed = Vec::with_capacity(raw.len() + 64);
    fixed.extend_from_slice(decoded.as_bytes());
    fixed.extend_from_slice(&raw[end..]);
    Cow::Owned(fixed)
}

/// A header's raw value (folded lines joined), for messages the parser couldn't read.
pub fn raw_header(raw: &[u8], name: &str) -> Option<Vec<u8>> {
    let end = header_end(raw);
    let mut value: Option<Vec<u8>> = None;
    for line in raw[..end].split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(v) = value.as_mut() {
            if line.first().is_some_and(|b| *b == b' ' || *b == b'\t') {
                v.push(b' ');
                v.extend_from_slice(line.trim_ascii());
                continue;
            }
            return value;
        }
        if let Some(colon) = line.iter().position(|b| *b == b':') {
            if line[..colon].trim_ascii().eq_ignore_ascii_case(name.as_bytes()) {
                value = Some(line[colon + 1..].trim_ascii().to_vec());
            }
        }
    }
    value
}

/// Finds uuencoded and BinHex blocks inside text, records them as attachments and
/// returns the text without them (encoded data would only clutter the index).
fn extract_inline_attachments<'a>(text: &'a str, out: &mut ParsedMessage, count: &mut usize) -> Cow<'a, str> {
    if !text.contains("begin ") && !text.contains("BinHex") {
        return Cow::Borrowed(text);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut kept = String::with_capacity(text.len());
    let mut i = 0;
    let mut changed = false;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        if let Some(name) = uu_begin(line) {
            if let Some((data, next)) = uudecode(&lines, i + 1) {
                *count += 1;
                out.attachments.push(AttachmentInfo {
                    part: format!("uu:{count}"),
                    name: name.to_string(),
                    mime: "application/octet-stream".into(),
                    size: data.len() as u64,
                    hash: *blake3::hash(&data).as_bytes(),
                    kind: AttachmentKind::Uuencode,
                    inline: false,
                });
                kept.push_str(&format!("[uuencoded attachment: {name}]\n"));
                i = next;
                changed = true;
                continue;
            }
        }
        if line.contains("(This file must be converted with BinHex") {
            if let Some((block, next)) = binhex_block(&lines, i + 1) {
                *count += 1;
                out.attachments.push(AttachmentInfo {
                    part: format!("binhex:{count}"),
                    name: format!("binhex-{count}.hqx"),
                    mime: "application/mac-binhex40".into(),
                    size: block.len() as u64,
                    hash: *blake3::hash(block.as_bytes()).as_bytes(),
                    kind: AttachmentKind::BinHex,
                    inline: false,
                });
                out.note("binhex", "BinHex attachment kept as it was (not decoded yet)");
                kept.push_str("[BinHex attachment]\n");
                i = next;
                changed = true;
                continue;
            }
        }
        kept.push_str(lines[i]);
        if i + 1 < lines.len() {
            kept.push('\n');
        }
        i += 1;
    }
    if changed { Cow::Owned(kept) } else { Cow::Borrowed(text) }
}

/// "begin 644 report.doc" → "report.doc".
fn uu_begin(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("begin ")?;
    let (mode, name) = rest.split_once(' ')?;
    (mode.len() >= 3 && mode.len() <= 4 && mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) && !name.trim().is_empty())
        .then(|| name.trim())
}

/// Decodes uuencoded lines from `start` up to "end"; returns the bytes and the line after "end".
fn uudecode(lines: &[&str], start: usize) -> Option<(Vec<u8>, usize)> {
    let mut data = Vec::new();
    for (i, line) in lines.iter().enumerate().skip(start) {
        let line = line.trim_end_matches('\r');
        if line == "end" {
            return Some((data, i + 1));
        }
        let bytes = line.as_bytes();
        let Some(&len_char) = bytes.first() else { continue };
        let n = (len_char.wrapping_sub(b' ') & 0x3F) as usize;
        if n == 0 {
            continue;
        }
        let quad = |b: u8| (b.wrapping_sub(b' ')) & 0x3F;
        let mut out = Vec::with_capacity(n + 3);
        for chunk in bytes[1..].chunks(4) {
            let mut c = [0u8; 4];
            for (k, b) in chunk.iter().enumerate() {
                c[k] = quad(*b);
            }
            out.push((c[0] << 2) | (c[1] >> 4));
            out.push((c[1] << 4) | (c[2] >> 2));
            out.push((c[2] << 6) | c[3]);
        }
        if out.len() < n {
            return None;
        }
        data.extend_from_slice(&out[..n]);
        if i - start > 2_000_000 {
            return None;
        }
    }
    None
}

/// A BinHex 4.0 block: from the line starting with ':' to the line ending with ':'.
fn binhex_block(lines: &[&str], start: usize) -> Option<(String, usize)> {
    let mut i = start;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if !lines.get(i)?.trim_start().starts_with(':') {
        return None;
    }
    let mut block = String::new();
    for (j, line) in lines.iter().enumerate().skip(i) {
        let line = line.trim_end_matches('\r');
        block.push_str(line);
        block.push('\n');
        if j > i && line.ends_with(':') || (j == i && line.len() > 1 && line.ends_with(':')) {
            return Some((block, j + 1));
        }
    }
    None
}

/// One line, whitespace collapsed.
pub fn clean_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn snippet(body: &str) -> String {
    let mut out = String::with_capacity(SNIPPET_LEN + 8);
    for word in body.split_whitespace() {
        if out.len() + word.len() + 1 > SNIPPET_LEN {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn cap(mut s: String, limit: usize) -> String {
    if s.len() > limit {
        let mut cut = limit;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000; // 2026-09

    fn p(raw: &[u8]) -> ParsedMessage {
        parse(raw, DateHints::default(), NOW)
    }

    fn kinds(m: &ParsedMessage) -> Vec<&'static str> {
        m.problems.iter().map(|p| p.kind).collect()
    }

    #[test]
    fn ordinary_mime_message() {
        let m = p(b"From: \"Ann Lee\" <ann@example.com>\r\nTo: bob@example.com, Carl <carl@example.org>\r\n\
Subject: =?utf-8?q?Caf=C3=A9_plans?=\r\nDate: Tue, 3 Mar 1998 10:00:00 -0500\r\nMessage-ID: <abc@example.com>\r\n\
In-Reply-To: <parent@example.com>\r\nReferences: <root@example.com> <parent@example.com>\r\nMIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\nSee you at the caf\xc3\xa9.\r\n");
        assert_eq!(m.status, ParseStatus::Ok, "{:?}", m.problems);
        assert_eq!(m.subject, "Café plans");
        assert_eq!(m.from[0].display(), "Ann Lee <ann@example.com>");
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.message_id, "abc@example.com");
        assert_eq!(m.in_reply_to, vec!["parent@example.com"]);
        assert_eq!(m.references, vec!["root@example.com", "parent@example.com"]);
        assert_eq!(m.date, 888_937_200);
        assert_eq!(m.date_source, DateSource::Header);
        assert!(m.body_text.contains("café"));
    }

    #[test]
    fn raw_8bit_headers_and_body_without_charset() {
        let m = p(b"From: Jos\xe9 P\xe9rez <jose@example.es>\nSubject: Ma\xf1ana\nDate: 1 Feb 95 09:00 GMT\n\nHasta ma\xf1ana \x93amigo\x94\n");
        assert_eq!(m.from[0].name, "José Pérez");
        assert_eq!(m.subject, "Mañana");
        assert!(m.body_text.contains("Hasta mañana \u{201c}amigo\u{201d}"), "{}", m.body_text);
        assert!(kinds(&m).contains(&"header-8bit"));
        assert!(kinds(&m).contains(&"charset-guessed"));
        assert_eq!(m.status, ParseStatus::Fixed);
        // Two-digit year.
        assert_eq!(m.date, 791_629_200);
    }

    #[test]
    fn windows_1252_labeled_latin1() {
        let m = p(b"From: a@b.com\nSubject: =?iso-8859-1?q?=93Quoted=94?=\nMIME-Version: 1.0\n\
Content-Type: text/plain; charset=iso-8859-1\nContent-Transfer-Encoding: 8bit\n\nIt\x92s \x96 fine\n");
        assert_eq!(m.subject, "\u{201c}Quoted\u{201d}");
        assert!(m.body_text.contains("It\u{2019}s \u{2013} fine"), "{}", m.body_text);
    }

    #[test]
    fn c1_controls_become_windows_1252_characters() {
        // (mail-parser already reads ISO-8859-1 the Windows way; this catches other decoders)
        let mut m = p(b"From: a@b.com\n\nx\n");
        assert_eq!(fix_c1("\u{93}Hi\u{94} \u{80}5", &mut m), "\u{201c}Hi\u{201d} \u{20ac}5");
        assert!(kinds(&m).contains(&"c1-fixed"));
        assert!(matches!(fix_c1("plain", &mut m), Cow::Borrowed(_)));
    }

    #[test]
    fn old_date_formats() {
        let t = |s: &str| lenient_date(s);
        assert_eq!(t("1 Feb 95 09:00 GMT"), Some(791_629_200));
        assert_eq!(t("Wed, 1 Feb 1995 09:00 GMT"), Some(791_629_200));
        assert_eq!(t("Feb 1 95 04:00 EST"), Some(791_629_200));
        assert_eq!(t("1 Feb 1995 10:00:00 +0100 (CET)"), Some(791_629_200));
        assert_eq!(t("1995-02-01 09:00"), Some(791_629_200));
        assert_eq!(t("Sat, 1 Jan 100 00:00:00 GMT"), Some(946_684_800)); // Y2K bug: 100 = 2000
        assert_eq!(t("1 Jan 03 00:00"), Some(1_041_379_200)); // 03 = 2003
        assert_eq!(t("tomorrow"), None);
    }

    #[test]
    fn missing_message_id_is_made_from_content_not_bytes() {
        let a = p(b"From: a@b.com\nSubject: Hi\nDate: Mon, 1 Jan 1996 00:00:00 +0000\n\nSame body\n");
        let b = p(b"Received: from x by y; Mon, 1 Jan 1996 00:00:05 +0000\nFrom: a@b.com\nSubject: Hi\nDate: Mon, 1 Jan 1996 00:00:00 +0000\n\nSame body\n");
        assert!(a.message_id_made);
        assert_eq!(a.message_id, b.message_id);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn date_fallbacks() {
        let m = p(b"Received: from a by b; Wed, 5 Jun 1996 12:00:00 +0000\nFrom: a@b.com\nSubject: x\n\nbody\n");
        assert_eq!(m.date_source, DateSource::Received);
        assert_eq!(m.date, 833_976_000);
        let m = parse(b"From: a@b.com\nSubject: x\nDate: garbage\n\nbody\n", DateHints { envelope: Some(800_000_000), file: None }, NOW);
        assert_eq!((m.date, m.date_source), (800_000_000, DateSource::Envelope));
        assert!(kinds(&m).contains(&"bad-date"));
        let m = parse(b"From: a@b.com\nDate: Thu, 1 Jan 2099 00:00:00 +0000\n\nx\n", DateHints { envelope: None, file: Some(900_000_000) }, NOW);
        assert_eq!((m.date, m.date_source), (900_000_000, DateSource::File));
        let m = p(b"From: a@b.com\n\nx\n");
        assert_eq!((m.date, m.date_source), (0, DateSource::Unknown));
    }

    #[test]
    fn uuencoded_attachment_is_decoded_and_removed_from_text() {
        let m = p(b"From: a@b.com\nSubject: file\n\nHere it is:\n\nbegin 644 cat.txt\n#0V%T\n`\nend\n\nBye\n");
        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!((a.name.as_str(), a.size, a.kind), ("cat.txt", 3, AttachmentKind::Uuencode));
        assert_eq!(a.hash, *blake3::hash(b"Cat").as_bytes());
        assert!(!m.body_text.contains("#0V%T"));
        assert!(m.body_text.contains("Bye"));
    }

    #[test]
    fn binhex_is_noted() {
        let m = p(b"From: a@b.com\nSubject: mac file\n\n(This file must be converted with BinHex 4.0)\n:$f*TEQKPH#jdCA0d,R0TG!\"6594%8dP8)3#3\"!&m!*!%EMa6593K!!%!!!&mFNa\n!!!!:\n\nThanks\n");
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].kind, AttachmentKind::BinHex);
        assert!(kinds(&m).contains(&"binhex"));
        assert!(m.body_text.contains("Thanks"));
    }

    #[test]
    fn an_empty_plain_text_version_takes_the_text_from_the_html() {
        // Walgreens' e-receipts (2026-10-17): text/plain empty, the receipt in text/html
        let raw = "From: Walgreens <walgreens@mktx.walgreens.com>\r\nSubject: Thank you for shopping at Walgreens.\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n \r\n--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>Your purchase receipt</p><p>Purchase summary Total: $6.70</p></body></html>\r\n--b--\r\n";
        let p = parse(raw.as_bytes(), DateHints::default(), 0);
        assert!(p.snippet.contains("Your purchase receipt"), "{:?}", p.snippet);
        assert!(p.body_text.contains("Total: $6.70"), "{:?}", p.body_text);
    }

    #[test]
    fn mime_attachments_and_html_body() {
        let m = p(b"From: a@b.com\nSubject: pics\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=XX\n\n--XX\n\
Content-Type: text/html; charset=utf-8\n\n<p>Hello <b>there</b></p>\n--XX\nContent-Type: application/pdf; name=report.pdf\n\
Content-Disposition: attachment; filename=report.pdf\nContent-Transfer-Encoding: base64\n\nJVBERi0xLjQK\n--XX--\n");
        assert!(m.body_text.contains("Hello there"), "{}", m.body_text);
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].name, "report.pdf");
        assert_eq!(m.attachments[0].mime, "application/pdf");
        assert_eq!(m.attachments[0].hash, *blake3::hash(b"%PDF-1.4\n").as_bytes());
    }

    #[test]
    fn unreadable_input_is_kept_as_text() {
        let m = p(b"");
        assert!(matches!(m.status, ParseStatus::Failed | ParseStatus::Ok | ParseStatus::Fixed));
        let m = p(b"\x00\x01\x02 just bytes, no headers at all");
        assert!(!m.message_id.is_empty());
    }

    #[test]
    fn display_keeps_bodies_and_inlines_pictures() {
        let raw = b"From: A <a@example.com>\r\nSubject: Pic\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi <img src=\"cid:p1@x\"></p>\r\n\
--b\r\nContent-Type: image/png\r\nContent-ID: <p1@x>\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0K\r\n--b--\r\n";
        let d = display(raw, 0);
        assert_eq!(d.parsed.subject, "Pic");
        let html = d.html.unwrap();
        assert!(html.contains("src=\"data:image/png;base64,iVBORw0K\""), "{html}");
        assert!(d.text.contains("Hi"));
        assert!(d.headers.starts_with("From: A"));
        // the picture is shown in the message: the attachment list leaves it out there
        let picture = d.parsed.attachments.iter().find(|a| a.mime == "image/png").expect("the picture is an attachment");
        assert_eq!(d.inline, vec![picture.part.clone()]);
        assert!(picture.inline, "a picture the HTML shows isn't a paperclip");

        let old = b"From: b@example.com\nSubject: caf\xe9\n\nNa\xefve text\n";
        let d = display(old, 0);
        assert_eq!(d.parsed.subject, "caf\u{e9}");
        assert!(d.html.is_none());
        assert_eq!(d.text.trim(), "Na\u{ef}ve text");
    }

    #[test]
    fn raw_header_unfolds() {
        let raw = b"Subject: one\r\n two\r\nFrom: x\r\n\r\nbody";
        assert_eq!(raw_header(raw, "subject").unwrap(), b"one two");
        assert_eq!(raw_header(raw, "date"), None);
    }
}
