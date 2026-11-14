//! Writing mail: what a new message, a reply or a forward starts with (Eudora's quoting: "At
//! 05:29 PM 12/31/1999, Ann wrote:" and "> " lines), and the MIME that's sent.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fts::{Part, bytes_part, disposition, display_name, encode_words, multipart, text_part};
use crate::parse::{self, Addr};

/// A message being written (as the compose window holds it).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    /// The address it's sent from (one of the user's identities), with its name.
    pub from: String,
    pub from_name: String,
    /// As typed: addresses separated by commas or semicolons ("Ann <ann@x.com>, bob@y.com").
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    /// The body as written (HTML) and as plain text.
    pub html: String,
    pub text: String,
    /// Written as plain text: sent as text/plain only (the HTML is left out).
    pub plain: bool,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Files added (base64).
    pub attachments: Vec<NewAttachment>,
    /// Attachments carried over from a message (a forward): taken from it when sent.
    pub forwarded: Vec<ForwardedPart>,
    /// Redirect: who the message is from ("Ann <ann@x.com>"): shown "Ann (by way of …)", and
    /// replies go to them (Reply-To).
    #[serde(default)]
    pub redirect_of: Option<String>,
    /// 1 (highest) … 5 (lowest); 3 or None = normal.
    pub priority: Option<u8>,
    /// Sent on behalf of `from` by this address (Exchange's Send on Behalf: "Sender:").
    pub sender: Option<String>,
    /// The message this answers (reply), passes on (forward) or redirects: its status changes
    /// once this is sent (Replied, Forwarded, Redirected).
    #[serde(default)]
    pub origin: Option<Origin>,
    /// An answer to an invitation (iCalendar METHOD:REPLY): sent beside the text as Outlook and
    /// Google send theirs (multipart/alternative, text/calendar; method=REPLY).
    #[serde(default)]
    pub calendar: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Origin {
    pub message: i64,
    /// "reply", "forward" or "redirect".
    pub kind: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NewAttachment {
    pub name: String,
    pub mime: String,
    pub data: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ForwardedPart {
    pub message: i64,
    /// The MIME part's number in that message.
    pub part: String,
    pub name: String,
    pub size: u64,
}

/// Splits an address field as typed into its addresses: commas, semicolons and spaces (not
/// inside quotes or angle brackets; a space ends an entry only after a whole bare address, so
/// "Ann Smith <ann@x.com>" stays one).
pub fn split_addresses(field: &str) -> Vec<String> {
    let (mut out, mut current, mut quoted, mut angle) = (Vec::new(), String::new(), false, false);
    let chars: Vec<char> = field.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            c if c.is_whitespace() && !quoted && !angle => {
                let so_far = current.trim();
                let bare = so_far.contains('@') && !so_far.contains(['<', '"', ' ']);
                let whole = so_far.ends_with('>') && so_far.contains('@'); // "Ann <ann@x.com>"
                // ("ann@x.com <…>" would be odd, but keep it together)
                let next = chars[i..].iter().find(|c| !c.is_whitespace());
                if (bare && next != Some(&'<')) || whole {
                    out.push(std::mem::take(&mut current));
                    continue;
                }
            }
            _ => {}
        }
        current.push(c);
    }
    out.push(current);
    out.into_iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect()
}

/// The address in "Name <addr>" or "addr".
pub fn address_of(entry: &str) -> String {
    match (entry.rfind('<'), entry.rfind('>')) {
        (Some(a), Some(b)) if b > a => entry[a + 1..b].trim().to_string(),
        _ => entry.trim().trim_matches('"').to_string(),
    }
}

/// An address field for the header: each entry checked (it must have an address with an @)
/// and its name encoded when it isn't plain ASCII.
fn header_addresses(field: &str, what: &str) -> Result<Option<String>> {
    let mut out = Vec::new();
    for entry in split_addresses(field) {
        let address = address_of(&entry);
        if !address.contains('@') || address.contains(' ') || address.starts_with('@') || address.ends_with('@') {
            return Err(Error::Other(format!("“{entry}” in {what} isn't an email address")));
        }
        let name = match (entry.rfind('<'), entry.rfind('>')) {
            (Some(a), Some(_)) => entry[..a].trim().trim_matches('"').trim().to_string(),
            _ => String::new(),
        };
        out.push(if name.is_empty() { address } else { format!("{} <{address}>", display_name(&name)) });
    }
    Ok((!out.is_empty()).then(|| out.join(",\r\n ")))
}

/// Every address the draft goes to (To, Cc, Bcc).
pub fn recipients(draft: &Draft) -> Vec<String> {
    [&draft.to, &draft.cc, &draft.bcc].iter().flat_map(|f| split_addresses(f)).map(|e| address_of(&e)).collect()
}

/// The message as sent: `forwarded` are the forwarded attachments' bytes (name, mime, bytes).
/// Returns the MIME and its Message-ID.
pub fn build_mime(draft: &Draft, forwarded: &[(String, String, Vec<u8>)], now: chrono::DateTime<chrono::Local>) -> Result<(Vec<u8>, String)> {
    if recipients(draft).is_empty() {
        return Err(Error::Other("there's no one to send it to (To, Cc or Bcc)".into()));
    }
    if !draft.from.contains('@') {
        return Err(Error::Other("choose who it's from".into()));
    }
    let domain = draft.from.rsplit('@').next().unwrap_or("eudora.invalid");
    let mut random = [0u8; 12];
    getrandom::fill(&mut random).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    let message_id = format!("{}.{}@{domain}", now.timestamp(), base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random));

    let mut h = String::new();
    let mut from = if draft.from_name.trim().is_empty() { draft.from.clone() } else { format!("{} <{}>", display_name(draft.from_name.trim()), draft.from) };
    // Redirect (classic Eudora's "by way of"): sent from the user's own address (servers refuse
    // anyone else's), named after the original sender, whom replies go to
    let redirect_of = draft.redirect_of.as_deref().and_then(|o| {
        let header = format!("From: {o}\r\n\r\n");
        let m = mail_parser::MessageParser::default().parse(header.as_bytes())?;
        m.from().and_then(|a| a.first()).map(|a| (a.name().unwrap_or_default().to_string(), a.address().unwrap_or_default().to_string()))
    });
    if let Some((name, address)) = redirect_of.as_ref().filter(|(_, a)| a.contains('@')) {
        let me = if draft.from_name.trim().is_empty() { draft.from.clone() } else { draft.from_name.trim().to_string() };
        let shown = format!("{} (by way of {me})", if name.is_empty() { address } else { name });
        from = format!("{} <{}>", display_name(&shown), draft.from);
    }
    h.push_str(&format!("Date: {}\r\nFrom: {from}\r\n", now.to_rfc2822()));
    if let Some((_, address)) = redirect_of.as_ref().filter(|(_, a)| a.contains('@')) {
        h.push_str(&format!("Reply-To: {address}\r\nResent-From: {}\r\n", draft.from));
    }
    if let Some(sender) = draft.sender.as_deref().filter(|s| s.contains('@')) {
        h.push_str(&format!("Sender: {sender}\r\n"));
    }
    for (name, field) in [("To", &draft.to), ("Cc", &draft.cc), ("Bcc", &draft.bcc)] {
        if let Some(v) = header_addresses(field, name)? {
            h.push_str(&format!("{name}: {v}\r\n"));
        }
    }
    h.push_str(&format!("Subject: {}\r\nMessage-ID: <{message_id}>\r\n", encode_words(draft.subject.trim())));
    if let Some(id) = draft.in_reply_to.as_deref().filter(|i| !i.is_empty()) {
        h.push_str(&format!("In-Reply-To: <{}>\r\n", id.trim_matches(['<', '>'])));
    }
    if !draft.references.is_empty() {
        let refs: Vec<String> = draft.references.iter().map(|r| format!("<{}>", r.trim_matches(['<', '>']))).collect();
        h.push_str(&format!("References: {}\r\n", refs.join("\r\n ")));
    }
    match draft.priority {
        Some(p @ (1 | 2)) => h.push_str(&format!("X-Priority: {p}\r\nImportance: High\r\n")),
        Some(p @ (4 | 5)) => h.push_str(&format!("X-Priority: {p}\r\nImportance: Low\r\n")),
        _ => {}
    }
    h.push_str("MIME-Version: 1.0\r\nX-Mailer: Eudora\r\n");

    let mut counter = 0;
    let text = if draft.text.trim().is_empty() { mail_parser::decoders::html::html_to_text(&draft.html) } else { draft.text.clone() };
    let body = if let Some(ics) = &draft.calendar {
        multipart("alternative", vec![text_part("text/plain", &text), text_part("text/calendar; method=REPLY", ics)], &mut counter)
    } else if draft.plain || draft.html.trim().is_empty() {
        text_part("text/plain", &text)
    } else {
        // pictures pasted into the text go as parts of their own (cid:), as mail programs show
        // them; many refuse data: pictures
        let (html, pictures) = inline_pictures(&draft.html, &message_id);
        let html = format!("<!doctype html><html><head><meta charset=\"utf-8\"></head><body>{html}</body></html>");
        let alternative = multipart("alternative", vec![text_part("text/plain", &text), text_part("text/html", &html)], &mut counter);
        if pictures.is_empty() {
            alternative
        } else {
            let mut parts = vec![alternative];
            parts.extend(pictures);
            let mut related = multipart("related", parts, &mut counter);
            if let Some(h) = related.headers.iter_mut().find(|h| h.starts_with("Content-Type: multipart/related")) {
                h.push_str("; type=\"multipart/alternative\"");
            }
            related
        }
    };
    let mut files: Vec<Part> = Vec::new();
    for a in &draft.attachments {
        let bytes = base64::engine::general_purpose::STANDARD.decode(a.data.as_bytes()).map_err(|_| Error::Other(format!("the attachment {} couldn't be read", a.name)))?;
        files.push(attachment(&a.name, &a.mime, &bytes));
    }
    for (name, mime, bytes) in forwarded {
        files.push(attachment(name, mime, bytes));
    }
    let top = if files.is_empty() {
        body
    } else {
        let mut parts = vec![body];
        parts.extend(files);
        multipart("mixed", parts, &mut counter)
    };
    for line in &top.headers {
        h.push_str(line);
        h.push_str("\r\n");
    }
    h.push_str("\r\n");
    h.push_str(&top.body);
    Ok((h.into_bytes(), message_id))
}

/// The HTML with each `data:image/…;base64,…` picture replaced by a `cid:` reference, and the
/// pictures as inline parts. A picture that can't be decoded is left as it was.
fn inline_pictures(html: &str, message_id: &str) -> (String, Vec<Part>) {
    let (mut out, mut parts, mut rest) = (String::new(), Vec::new(), html);
    while let Some(at) = rest.find("data:image/") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail.find(['"', '\'', ')', ' ', '>']).unwrap_or(tail.len());
        let url = &tail[..end];
        let decoded = url.split_once(";base64,").and_then(|(head, data)| {
            let mime = head.trim_start_matches("data:");
            let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim().as_bytes()).ok()?;
            (mime.is_ascii() && !mime.is_empty()).then(|| (mime.to_string(), bytes))
        });
        match decoded {
            Some((mime, bytes)) => {
                let n = parts.len() + 1;
                let cid = format!("picture{n}.{message_id}");
                let ext = mime.split('/').nth(1).unwrap_or("png").replace("jpeg", "jpg");
                let extra = vec![format!("Content-ID: <{cid}>"), disposition("inline", Some(&format!("picture{n}.{ext}")))];
                parts.push(bytes_part(&mime, None, &bytes, Some(extra)));
                out.push_str(&format!("cid:{cid}"));
            }
            None => out.push_str(url),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    (out, parts)
}

fn attachment(name: &str, mime: &str, bytes: &[u8]) -> Part {
    let mime = if mime.contains('/') && mime.is_ascii() { mime } else { "application/octet-stream" };
    if mime == "message/rfc822" && bytes.is_ascii() {
        // a whole message (forwarded as an attachment) goes as it is: RFC 2046 wants 7bit here
        let mut body = String::from_utf8_lossy(bytes).replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n");
        if !body.ends_with("\r\n") {
            body.push_str("\r\n");
        }
        return Part { headers: vec!["Content-Type: message/rfc822".into(), "Content-Transfer-Encoding: 7bit".into(), disposition("attachment", Some(name))], body };
    }
    let mut part = bytes_part(mime, None, bytes, Some(vec![disposition("attachment", Some(name))]));
    if mime.starts_with("text/") {
        // attachments travel byte for byte: base64, never re-encoded as text
        part.headers.retain(|h| !h.starts_with("Content-Transfer-Encoding"));
        part.headers.push("Content-Transfer-Encoding: base64".into());
        part.body = base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .as_bytes()
            .chunks(76)
            .map(|c| format!("{}\r\n", std::str::from_utf8(c).unwrap()))
            .collect();
    }
    part
}

/// What a reply or forward starts with.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Seed {
    /// Which of the user's addresses it was sent to (the From to answer with), if one.
    pub to_me: Option<String>,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub html: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub forwarded: Vec<ForwardedPart>,
    /// Redirect: the original sender (see `Draft::redirect_of`).
    pub redirect_of: Option<String>,
    /// Start as plain text (a reply to a message written in plain text).
    pub plain: bool,
    /// Written below the original (classic Eudora's quoting: "> " lines, the reply under them),
    /// so the message window puts the cursor, and the signature, at the end.
    pub bottom: bool,
}

/// How replies quote the original (Settings ▸ Mail, setting `reply-quoting`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quoting {
    /// The original in a quote block (a line down its side) below, the reply written above it.
    #[default]
    Modern,
    /// Classic Eudora's: plain text, "> " before each line of the original, the reply below it.
    Classic,
}

impl Quoting {
    pub const SETTING: &'static str = "reply-quoting";

    /// From the setting's value ("classic" / "modern", JSON-quoted or not).
    pub fn from_setting(value: Option<&str>) -> Quoting {
        match value.map(|v| v.trim().trim_matches('"')) {
            Some("classic") => Quoting::Classic,
            _ => Quoting::Modern,
        }
    }

    /// What a reply asks for ("classic", "modern", or "swap": the other way than `setting`; none:
    /// as `setting` says).
    pub fn resolve(asked: Option<&str>, setting: Option<&str>) -> Quoting {
        let usual = Quoting::from_setting(setting);
        match asked.map(|a| a.trim().trim_matches('"')) {
            Some("swap") => usual.other(),
            Some(a @ ("classic" | "modern")) => Quoting::from_setting(Some(a)),
            _ => usual,
        }
    }

    pub fn other(self) -> Quoting {
        match self {
            Quoting::Modern => Quoting::Classic,
            Quoting::Classic => Quoting::Modern,
        }
    }
}

/// The original's text as classic Eudora quoted it: "> " before each line (">" before a line
/// already quoted, so ">> " nests), long lines wrapped at 76 characters first.
pub fn quote_lines(text: &str) -> String {
    const WIDTH: usize = 74;
    let mut out = String::new();
    for line in text.replace("\r\n", "\n").trim_end().lines() {
        let line = line.trim_end();
        if line.starts_with('>') {
            out.push('>');
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let mut current = String::new();
        for word in line.split(' ') {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > WIDTH {
                out.push_str("> ");
                out.push_str(&current);
                out.push('\n');
                current.clear();
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push_str(if current.is_empty() { ">" } else { "> " });
        out.push_str(&current);
        out.push('\n');
    }
    out
}

fn person(a: &Addr) -> String {
    if a.name.is_empty() || a.name == a.address { a.address.clone() } else { format!("{} <{}>", a.name.replace(['<', '>', ','], " ").trim(), a.address) }
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The part of an HTML document inside its body (the whole thing when there's no body tag).
fn inner_body(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<body").and_then(|b| lower[b..].find('>').map(|e| b + e + 1)).unwrap_or(0);
    let end = lower.rfind("</body>").filter(|&e| e >= start).unwrap_or(html.len());
    &html[start..end]
}

/// The original as HTML for quoting (its own HTML, or its text with line breaks kept).
fn original_html(d: &parse::DisplayMessage) -> String {
    match &d.html {
        Some(html) => inner_body(html).to_string(),
        None => format!("<div style=\"white-space:pre-wrap\">{}</div>", escape(&d.text)),
    }
}

fn when(date: i64) -> String {
    chrono::DateTime::from_timestamp(date, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%I:%M %p %-m/%-d/%Y").to_string())
        .unwrap_or_default()
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let lower = subject.trim().to_lowercase();
    if lower.starts_with(&prefix.to_lowercase()) { subject.trim().to_string() } else { format!("{prefix} {}", subject.trim()) }
}

/// A reply to `raw` (to everyone with `all`). `mine`: the user's addresses (left out of a
/// Reply to All; the one it was sent to answers it).
pub fn reply(raw: &[u8], all: bool, mine: &[String]) -> Seed {
    reply_as(raw, all, mine, Quoting::Modern)
}

/// A reply quoting the original as `quoting` says.
pub fn reply_as(raw: &[u8], all: bool, mine: &[String], quoting: Quoting) -> Seed {
    let d = parse::display(raw, crate::library::now());
    let p = &d.parsed;
    let is_mine = |a: &str| mine.iter().any(|m| m.eq_ignore_ascii_case(a));
    let reply_to: Vec<String> = mail_parser::MessageParser::default()
        .parse(raw)
        .and_then(|m| m.reply_to().map(|r| r.iter().filter_map(|a| a.address().map(|x| x.to_string())).collect()))
        .unwrap_or_default();
    let mut to: Vec<String> = if reply_to.is_empty() { p.from.iter().map(person).collect() } else { reply_to };
    let mut cc: Vec<String> = Vec::new();
    if all {
        to.extend(p.to.iter().filter(|a| !is_mine(&a.address)).map(person));
        cc.extend(p.cc.iter().filter(|a| !is_mine(&a.address)).map(person));
    }
    to.dedup();
    let to_me = p.to.iter().chain(&p.cc).find(|a| is_mine(&a.address)).map(|a| a.address.clone());
    let sender = p.from.first().map(|a| if a.name.is_empty() { a.address.clone() } else { a.name.clone() }).unwrap_or_default();
    let attribution = format!("At {}, {} wrote:", when(p.date), sender);
    let html = match quoting {
        Quoting::Modern => format!(
            "<div><br></div><div>{}</div><blockquote type=\"cite\" style=\"margin:0 0 0 .8ex;border-left:2px solid #1f5fae;padding-left:1ex\">{}</blockquote>",
            escape(&attribution),
            original_html(&d)
        ),
        // a line a <div>, as the message window writes plain text; the empty last one is where
        // the reply is written
        Quoting::Classic => {
            let mut html = format!("<div>{}</div>", escape(&attribution));
            for line in quote_lines(&d.text).lines() {
                html.push_str(&format!("<div>{}</div>", escape(line)));
            }
            html.push_str("<div><br></div><div class=\"eudora-reply-here\"><br></div>");
            html
        }
    };
    let mut references: Vec<String> = p.references.clone();
    if !p.message_id_made {
        references.push(p.message_id.clone());
    }
    Seed {
        to_me,
        to: to.join(", "),
        cc: cc.join(", "),
        subject: prefixed("Re:", &p.subject),
        html,
        in_reply_to: (!p.message_id_made).then(|| p.message_id.clone()),
        references,
        forwarded: Vec::new(),
        redirect_of: None,
        plain: d.html.is_none() || quoting == Quoting::Classic,
        bottom: quoting == Quoting::Classic,
    }
}

/// A forward of message `id` (`raw`): the original below a header block, its attachments along.
pub fn forward(id: i64, raw: &[u8], mine: &[String]) -> Seed {
    let d = parse::display(raw, crate::library::now());
    let p = &d.parsed;
    let list = |a: &[Addr]| a.iter().map(person).collect::<Vec<_>>().join(", ");
    let header = format!(
        "---------- Forwarded message ----------<br>From: {}<br>Date: {}<br>Subject: {}<br>To: {}{}",
        escape(&list(&p.from)),
        escape(&when(p.date)),
        escape(&p.subject),
        escape(&list(&p.to)),
        if p.cc.is_empty() { String::new() } else { format!("<br>Cc: {}", escape(&list(&p.cc))) }
    );
    let html = format!("<div><br></div><div>{header}</div>{}", original_html(&d));
    let is_mine = |a: &str| mine.iter().any(|m| m.eq_ignore_ascii_case(a));
    Seed {
        to_me: p.to.iter().chain(&p.cc).find(|a| is_mine(&a.address)).map(|a| a.address.clone()),
        subject: prefixed("Fwd:", &p.subject),
        html,
        forwarded: p
            .attachments
            .iter()
            .filter(|a| a.kind.code() == "mime")
            .map(|a| ForwardedPart { message: id, part: a.part.clone(), name: a.name.clone(), size: a.size })
            .collect(),
        ..Default::default()
    }
}

/// Redirect: the message passed on as it is (its subject, text and attachments), from the
/// user's address "by way of" them, replies going to the original sender.
pub fn redirect(id: i64, raw: &[u8], mine: &[String]) -> Seed {
    let d = parse::display(raw, crate::library::now());
    let p = &d.parsed;
    let is_mine = |a: &str| mine.iter().any(|m| m.eq_ignore_ascii_case(a));
    Seed {
        to_me: p.to.iter().chain(&p.cc).find(|a| is_mine(&a.address)).map(|a| a.address.clone()),
        subject: p.subject.clone(),
        html: original_html(&d),
        forwarded: attachments_of(id, p),
        redirect_of: p.from.first().map(person),
        ..Default::default()
    }
}

/// Send Again: a new message with the same recipients, subject, text and attachments (one the
/// user sent, or one that came back undelivered: sent again, changed or not).
pub fn again(id: i64, raw: &[u8], mine: &[String]) -> Seed {
    let d = parse::display(raw, crate::library::now());
    let p = &d.parsed;
    let list = |a: &[Addr]| a.iter().map(person).collect::<Vec<_>>().join(", ");
    let is_mine = |a: &str| mine.iter().any(|m| m.eq_ignore_ascii_case(a));
    Seed {
        to_me: p.from.iter().find(|a| is_mine(&a.address)).map(|a| a.address.clone()),
        to: list(&p.to),
        cc: list(&p.cc),
        subject: p.subject.clone(),
        html: original_html(&d),
        forwarded: attachments_of(id, p),
        ..Default::default()
    }
}

fn attachments_of(id: i64, p: &parse::ParsedMessage) -> Vec<ForwardedPart> {
    p.attachments
        .iter()
        .filter(|a| a.kind.code() == "mime")
        .map(|a| ForwardedPart { message: id, part: a.part.clone(), name: a.name.clone(), size: a.size })
        .collect()
}

/// Several messages forwarded together: each attached whole (an .eml file, the message as
/// stored), with a short list of them in the text. `messages`: (id, raw).
pub fn forward_many(messages: &[(i64, Vec<u8>)], mine: &[String]) -> Seed {
    let is_mine = |a: &str| mine.iter().any(|m| m.eq_ignore_ascii_case(a));
    let mut lines = Vec::new();
    let mut forwarded = Vec::new();
    let mut to_me = None;
    let mut first_subject = String::new();
    for (id, raw) in messages {
        let p = parse::parse(raw, Default::default(), crate::library::now());
        if first_subject.is_empty() {
            first_subject = p.subject.clone();
        }
        if to_me.is_none() {
            to_me = p.to.iter().chain(&p.cc).find(|a| is_mine(&a.address)).map(|a| a.address.clone());
        }
        let from = p.from.first().map(person).unwrap_or_default();
        lines.push(format!("<li>{} · {} · {}</li>", escape(if p.subject.is_empty() { "(no subject)" } else { &p.subject }), escape(&from), escape(&when(p.date))));
        let safe: String = p.subject.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).take(60).collect();
        let name = format!("{}.eml", if safe.trim().is_empty() { format!("message {id}") } else { safe.trim().to_string() });
        forwarded.push(ForwardedPart { message: *id, part: WHOLE_MESSAGE.into(), name, size: raw.len() as u64 });
    }
    let subject = match messages.len() {
        0 | 1 => prefixed("Fwd:", &first_subject),
        n => format!("{} (and {} more)", prefixed("Fwd:", &first_subject), n - 1),
    };
    let html = format!("<div><br></div><div>Forwarded {} messages (attached):</div><ul>{}</ul>", messages.len(), lines.join(""));
    Seed { to_me, subject, html, forwarded, ..Default::default() }
}

/// `ForwardedPart::part` for a message attached whole.
pub const WHOLE_MESSAGE: &str = "message";

/// A forwarded attachment's bytes, from the message it's in.
pub fn part_bytes(raw: &[u8], part: &str) -> Option<(String, Vec<u8>)> {
    use mail_parser::MimeHeaders;
    if part == WHOLE_MESSAGE {
        return Some(("message/rfc822".into(), raw.to_vec()));
    }
    let message = mail_parser::MessageParser::default().parse(raw)?;
    let part = message.parts.get(part.parse::<usize>().ok()?)?;
    let mime = part
        .content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into());
    Some((mime.to_ascii_lowercase(), part.contents().to_vec()))
}

/// What a `mailto:` link asks for (RFC 6068): its addresses and the To, Cc, Bcc, subject, and body
/// it names; the rest left out. Nothing is sent: a new message is started with these.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Mailto {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
}

/// Reads a `mailto:` link; None when it isn't one.
pub fn parse_mailto(link: &str) -> Option<Mailto> {
    let link = link.trim();
    let rest = link.get(..7).filter(|s| s.eq_ignore_ascii_case("mailto:")).map(|_| &link[7..])?;
    let (addresses, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut m = Mailto::default();
    let add = |field: &mut String, value: &str| {
        let value = value.trim();
        if !value.is_empty() {
            if !field.is_empty() {
                field.push_str(", ");
            }
            field.push_str(value);
        }
    };
    add(&mut m.to, &percent_decode(addresses));
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        // (a "+" is a plus in mailto:, not a space: RFC 6068)
        let value = percent_decode(value);
        match key.to_ascii_lowercase().as_str() {
            "to" => add(&mut m.to, &value),
            "cc" => add(&mut m.cc, &value),
            "bcc" => add(&mut m.bcc, &value),
            "subject" if m.subject.is_empty() => m.subject = value.lines().next().unwrap_or("").chars().take(500).collect(),
            "body" if m.body.is_empty() => m.body = value.replace("\r\n", "\n").chars().take(100_000).collect(),
            _ => {}
        }
    }
    let clean = |s: &mut String| *s = s.chars().filter(|c| !c.is_control() || *c == '\n').take(5_000).collect();
    clean(&mut m.to);
    clean(&mut m.cc);
    clean(&mut m.bcc);
    Some(m)
}

/// %XX decoded as UTF-8 (broken sequences kept as they were).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            // (read as bytes: what follows "%" may be any character)
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};

    fn local(t: i64) -> chrono::DateTime<chrono::Local> {
        chrono::DateTime::from_timestamp(t, 0).unwrap().with_timezone(&chrono::Local)
    }

    #[test]
    fn addresses_are_split_and_checked() {
        assert_eq!(split_addresses("\"Smith, Ann\" <ann@x.com>; bob@y.com,  "), vec!["\"Smith, Ann\" <ann@x.com>", "bob@y.com"]);
        // spaces separate bare addresses, but names with spaces stay whole
        assert_eq!(split_addresses("a@x.com b@y.com  Ann Smith <ann@x.com> c@z.com"), vec!["a@x.com", "b@y.com", "Ann Smith <ann@x.com>", "c@z.com"]);
        assert_eq!(address_of("\"Smith, Ann\" <ann@x.com>"), "ann@x.com");
        assert!(header_addresses("not an address", "To").is_err());
        assert_eq!(header_addresses("José <j@x.com>", "To").unwrap().unwrap(), "=?utf-8?B?Sm9zw6k=?= <j@x.com>");
    }

    #[test]
    fn several_messages_are_forwarded_as_attachments() {
        let one = b"From: Ann <ann@x.com>\r\nTo: pat@example.com\r\nSubject: Plans\r\nDate: Fri, 31 Dec 1999 20:38:46 -0500\r\n\r\nSee you.\r\n".to_vec();
        let two = b"From: bob@y.com\r\nSubject: Re: Plans\r\n\r\nOK.\r\n".to_vec();
        let seed = forward_many(&[(1, one.clone()), (2, two)], &["pat@example.com".into()]);
        assert_eq!(seed.subject, "Fwd: Plans (and 1 more)");
        assert_eq!(seed.to_me.as_deref(), Some("pat@example.com"));
        assert_eq!(seed.forwarded.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Plans.eml", "Re_ Plans.eml"]);
        assert!(seed.html.contains("Forwarded 2 messages"));
        // sent as message/rfc822, the original whole inside
        let (mime, bytes) = part_bytes(&one, WHOLE_MESSAGE).unwrap();
        let draft = Draft { from: "pat@example.com".into(), to: "c@z.com".into(), subject: seed.subject.clone(), html: seed.html.clone(), ..Default::default() };
        let (sent, _) = build_mime(&draft, &[("Plans.eml".into(), mime, bytes)], local(946_684_800)).unwrap();
        let text = String::from_utf8(sent).unwrap();
        assert!(text.contains("Content-Type: message/rfc822\r\nContent-Transfer-Encoding: 7bit"));
        assert!(text.contains("Subject: Plans\r\n"));
    }

    #[test]
    fn a_redirect_comes_by_way_of_the_user_and_replies_go_to_the_sender() {
        let draft = Draft {
            from: "pat@example.com".into(),
            from_name: "Pat Morgan".into(),
            to: "bob@y.com".into(),
            subject: "Quote".into(),
            html: "<p>Prices</p>".into(),
            redirect_of: Some("Ann Smith <ann@x.com>".into()),
            ..Default::default()
        };
        let (mime, _) = build_mime(&draft, &[], local(946_684_800)).unwrap();
        let m = MessageParser::default().parse(&mime).unwrap();
        let from = m.from().unwrap().first().unwrap();
        assert_eq!((from.name(), from.address()), (Some("Ann Smith (by way of Pat Morgan)"), Some("pat@example.com")));
        assert_eq!(m.reply_to().unwrap().first().unwrap().address(), Some("ann@x.com"));
        assert_eq!(m.subject(), Some("Quote"), "a redirect keeps the subject as it was");
    }

    #[test]
    fn a_draft_becomes_a_message_any_reader_can_read() {
        let draft = Draft {
            from: "pat@example.com".into(),
            from_name: "Pat Morgan".into(),
            to: "\"Smith, Ann\" <ann@x.com>".into(),
            cc: "bob@y.com".into(),
            bcc: "carol@z.com".into(),
            subject: "Grüße".into(),
            html: "<p>Hello <b>there</b></p>".into(),
            in_reply_to: Some("orig@x.com".into()),
            references: vec!["a@x".into(), "orig@x.com".into()],
            attachments: vec![NewAttachment { name: "notes.txt".into(), mime: "text/plain".into(), data: "aGVsbG8=".into() }],
            priority: Some(1),
            ..Default::default()
        };
        let (mime, id) = build_mime(&draft, &[("pic.png".into(), "image/png".into(), vec![1, 2, 3])], local(946_684_800)).unwrap();
        let m = MessageParser::default().parse(&mime).unwrap();
        assert_eq!(m.subject(), Some("Grüße"));
        assert_eq!(m.message_id(), Some(id.as_str()));
        assert!(id.ends_with("@example.com"));
        assert_eq!(m.from().unwrap().first().unwrap().name(), Some("Pat Morgan"));
        assert_eq!(m.to().unwrap().first().unwrap().name(), Some("Smith, Ann"));
        assert_eq!(m.bcc().unwrap().first().unwrap().address(), Some("carol@z.com"));
        assert_eq!(m.in_reply_to().as_text(), Some("orig@x.com"));
        assert_eq!(m.body_text(0).unwrap().trim(), "Hello there");
        assert!(m.body_html(0).unwrap().contains("<b>there</b>"));
        let names: Vec<_> = m.attachments().map(|a| (a.attachment_name().unwrap_or("").to_string(), a.contents().to_vec())).collect();
        assert_eq!(names, vec![("notes.txt".to_string(), b"hello".to_vec()), ("pic.png".to_string(), vec![1, 2, 3])]);
        assert!(String::from_utf8_lossy(&mime).contains("X-Priority: 1"));

        assert!(build_mime(&Draft { from: "a@b.c".into(), ..Default::default() }, &[], local(0)).is_err(), "no recipients");
    }

    #[test]
    fn classic_quoting_puts_marks_before_each_line_and_the_reply_below() {
        let raw = b"From: Ann Smith <ann@example.com>\r\nTo: pat@example.com\r\nSubject: Lunch\r\nDate: Fri, 31 Dec 1999 17:29:00 +0000\r\nMessage-ID: <a@b>\r\n\r\nShall we meet at noon?\r\n> earlier words\r\n";
        let r = reply_as(raw, false, &["pat@example.com".into()], Quoting::Classic);
        assert!(r.plain && r.bottom);
        assert!(r.html.contains("Ann Smith wrote:</div><div>&gt; Shall we meet at noon?</div><div>&gt;&gt; earlier words</div>"), "{}", r.html);
        assert!(r.html.ends_with("<div class=\"eudora-reply-here\"><br></div>"));
        let modern = reply_as(raw, false, &[], Quoting::Modern);
        assert!(!modern.bottom && modern.html.contains("<blockquote"));
        let long = quote_lines(&"word ".repeat(40));
        assert!(long.lines().all(|l| l.starts_with("> ") && l.chars().count() <= 76), "{long}");
        assert_eq!(Quoting::from_setting(Some("\"classic\"")), Quoting::Classic);
        assert_eq!(Quoting::from_setting(None), Quoting::Modern);
        assert_eq!(Quoting::resolve(Some("swap"), Some("classic")), Quoting::Modern);
        assert_eq!(Quoting::resolve(Some("swap"), None), Quoting::Classic);
        assert_eq!(Quoting::resolve(None, Some("classic")), Quoting::Classic);
        assert_eq!(Quoting::resolve(Some("modern"), Some("classic")), Quoting::Modern);
    }

    #[test]
    fn pasted_pictures_go_inline_and_plain_text_goes_alone() {
        let draft = Draft {
            from: "pat@example.com".into(),
            to: "ann@x.com".into(),
            html: "<p>See <img src=\"data:image/png;base64,AQID\"> here</p>".into(),
            ..Default::default()
        };
        let (mime, _) = build_mime(&draft, &[], local(946_684_800)).unwrap();
        let raw = String::from_utf8_lossy(&mime).to_string();
        assert!(raw.contains("multipart/related") && !raw.contains("data:image"));
        let m = MessageParser::default().parse(&mime).unwrap();
        let html = m.body_html(0).unwrap();
        let cid = html.split("cid:").nth(1).unwrap().split('"').next().unwrap().to_string();
        let pic = m.attachments().next().unwrap();
        assert_eq!((pic.content_id(), pic.contents()), (Some(cid.as_str()), &[1u8, 2, 3][..]));

        let plain = Draft { plain: true, text: "Just words".into(), ..draft.clone() };
        let (mime, _) = build_mime(&plain, &[], local(946_684_800)).unwrap();
        let raw = String::from_utf8_lossy(&mime).to_string();
        assert!(raw.contains("Content-Type: text/plain") && !raw.contains("text/html") && raw.contains("Just words"));

        // an answer to an invitation: the words and the calendar part side by side
        let answer = Draft { text: "Pat has accepted this invitation.".into(), html: String::new(), calendar: Some("BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n".into()), ..draft };
        let (mime, _) = build_mime(&answer, &[], local(946_684_800)).unwrap();
        let parsed = mail_parser::MessageParser::default().parse(&mime).unwrap();
        assert!(String::from_utf8_lossy(&mime).contains("multipart/alternative"));
        let cal = parsed.parts.iter().find(|p| p.content_type().is_some_and(|c| c.subtype() == Some("calendar"))).expect("a calendar part");
        assert_eq!(cal.content_type().and_then(|c| c.attribute("method")), Some("REPLY"));
        assert!(String::from_utf8_lossy(cal.contents()).contains("METHOD:REPLY"));
        assert_eq!(crate::invites::calendar_texts(&mime).len(), 1);
    }

    const ORIGINAL: &[u8] = b"From: Ann Smith <ann@x.com>\r\nTo: pat@example.com, bob@y.com\r\nCc: carol@z.com\r\nReply-To: list@x.com\r\nSubject: Plans\r\nDate: Fri, 31 Dec 1999 17:29:00 -0500\r\nMessage-ID: <orig@x.com>\r\nReferences: <first@x.com>\r\n\r\nLine one\r\nLine two\r\n";

    #[test]
    fn replies_quote_like_eudora_and_answer_the_right_people() {
        let mine = vec!["pat@example.com".to_string()];
        let r = reply(ORIGINAL, false, &mine);
        assert_eq!(r.to, "list@x.com", "Reply-To wins");
        assert_eq!(r.subject, "Re: Plans");
        assert_eq!(r.to_me.as_deref(), Some("pat@example.com"));
        assert_eq!(r.in_reply_to.as_deref(), Some("orig@x.com"));
        assert_eq!(r.references, vec!["first@x.com".to_string(), "orig@x.com".to_string()]);
        assert!(r.html.contains("Ann Smith wrote:"));
        assert!(r.html.contains("Line one"));

        let all = reply(ORIGINAL, true, &mine);
        assert_eq!(all.to, "list@x.com, bob@y.com", "everyone but me");
        assert_eq!(all.cc, "carol@z.com");
        assert_eq!(reply(b"Subject: Re: Plans\r\nFrom: a@b.c\r\n\r\nx", false, &mine).subject, "Re: Plans");
    }

    #[test]
    fn forwards_carry_the_original_and_its_attachments() {
        let raw = b"From: a@b.c\r\nTo: pat@example.com\r\nSubject: Report\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee attached\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=r.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n--b--\r\n";
        let f = forward(7, raw, &["pat@example.com".to_string()]);
        assert_eq!(f.subject, "Fwd: Report");
        assert!(f.html.contains("Forwarded message") && f.html.contains("See attached"));
        assert_eq!(f.forwarded.len(), 1);
        assert_eq!((f.forwarded[0].message, f.forwarded[0].name.as_str()), (7, "r.pdf"));
        let (mime, bytes) = part_bytes(raw, &f.forwarded[0].part).unwrap();
        assert_eq!((mime.as_str(), bytes.as_slice()), ("application/pdf", b"%PDF".as_slice()));
    }

    #[test]
    fn mailto_links_are_read() {
        let m = parse_mailto("MAILTO:ann%40x.com,bob@y.com?cc=c@z.com&bcc=d@z.com&subject=Hello%20there&body=Line%201%0D%0ALine%202&x=1").unwrap();
        assert_eq!(m.to, "ann@x.com,bob@y.com");
        assert_eq!((m.cc.as_str(), m.bcc.as_str()), ("c@z.com", "d@z.com"));
        assert_eq!(m.subject, "Hello there");
        assert_eq!(m.body, "Line 1\nLine 2");
        assert_eq!(parse_mailto("mailto:?to=a@b.com&to=c@d.com&subject=a+b").unwrap(), Mailto { to: "a@b.com, c@d.com".into(), subject: "a+b".into(), ..Default::default() });
        assert_eq!(parse_mailto("mailto:x@y.com?subject=%E2%9C%93%zz").unwrap().subject, "✓%zz");
        assert!(parse_mailto("https://x.com").is_none());
        assert!(!parse_mailto("mailto:a@b.com?subject=Hi%0ABcc:%20evil@x.com").unwrap().subject.contains('\n'), "a subject is one line");
        assert_eq!(parse_mailto("mailto:a@b.com?subject=%é9").unwrap().subject, "%é9");
    }
}
