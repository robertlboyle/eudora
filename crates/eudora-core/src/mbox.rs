//! Reads mbox files of every common flavor: mboxo and mboxrd ("From " escaping), and
//! mboxcl / mboxcl2 (Content-Length says where a message ends, so body lines starting with
//! "From " don't split it). Eudora's .mbx files are mbox too.
//!
//! A message starts at a "From " line (at the start of the file or after a newline) that
//! looks like an envelope line: it has a time and a four-digit year. The blank line mbox
//! writers put between messages is removed, and ">From " lines are unescaped one level.

use std::borrow::Cow;
use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use crate::error::{IoContext, Result};

pub struct MboxFile {
    map: Option<Mmap>,
}

/// One message's place in the file.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    /// Start of its "From " line.
    pub from_line: usize,
    /// First byte of the message itself.
    pub start: usize,
    /// End of the message (before the separator blank line).
    pub end: usize,
    /// Where the next message's "From " line starts (or the end of the file).
    pub next: usize,
    /// Content-Length decided the end: such files don't escape "From " lines.
    pub by_length: bool,
}

pub struct MboxMessage<'a> {
    pub raw: Cow<'a, [u8]>,
    pub envelope_date: Option<i64>,
    pub span: Span,
}

impl MboxFile {
    pub fn open(path: &Path) -> Result<MboxFile> {
        let file = File::open(path).context(|| format!("opening {}", path.display()))?;
        let len = file.metadata().context(|| format!("reading {}", path.display()))?.len();
        // An empty file can't be mapped on every platform.
        let map = if len == 0 {
            None
        } else {
            // SAFETY: the file is only read; if another program truncates it while we read,
            // the OS raises an error for the access, as with any memory-mapped reader.
            Some(unsafe { Mmap::map(&file) }.context(|| format!("reading {}", path.display()))?)
        };
        Ok(MboxFile { map })
    }

    pub fn data(&self) -> &[u8] {
        self.map.as_deref().unwrap_or(&[])
    }

    /// Whether this looks like an mbox file at all (starts with an envelope line).
    pub fn looks_like_mbox(&self) -> bool {
        let data = self.data();
        let line_end = memchr::memchr(b'\n', data).unwrap_or(data.len());
        is_envelope_line(&data[..line_end])
    }

    /// Message spans starting at `offset` (0, or a `Span::next` from an earlier pass).
    pub fn spans(&self, offset: usize) -> Spans<'_> {
        let data = self.data();
        let pos = if offset == 0 { first_envelope(data) } else { Some(offset) };
        Spans { data, pos }
    }

    pub fn count(&self) -> u64 {
        self.spans(0).count() as u64
    }

    pub fn message(&self, span: Span) -> MboxMessage<'_> {
        let data = self.data();
        let envelope = &data[span.from_line..span.start.saturating_sub(1).max(span.from_line)];
        let body = &data[span.start..span.end];
        let raw = if span.by_length { Cow::Borrowed(body) } else { unescape_from(body) };
        MboxMessage { raw, envelope_date: envelope_date(envelope), span }
    }
}

pub struct Spans<'a> {
    data: &'a [u8],
    pos: Option<usize>,
}

impl Iterator for Spans<'_> {
    type Item = Span;

    fn next(&mut self) -> Option<Span> {
        let data = self.data;
        let from_line = self.pos?;
        if from_line >= data.len() {
            self.pos = None;
            return None;
        }
        let start = memchr::memchr(b'\n', &data[from_line..]).map_or(data.len(), |n| from_line + n + 1);
        let (end, next, by_length) = match length_end(data, start) {
            Some(next) => (strip_separator(data, start, next), next, true),
            None => {
                let next = next_envelope(data, start).unwrap_or(data.len());
                (strip_separator(data, start, next), next, false)
            }
        };
        self.pos = (next < data.len()).then_some(next);
        Some(Span { from_line, start, end, next, by_length })
    }
}

fn first_envelope(data: &[u8]) -> Option<usize> {
    let first_line = &data[..memchr::memchr(b'\n', data).unwrap_or(data.len())];
    if is_envelope_line(first_line) { Some(0) } else { next_envelope(data, 0) }
}

/// Next "From " envelope line at or after `from` that starts a line.
fn next_envelope(data: &[u8], from: usize) -> Option<usize> {
    let finder = memchr::memmem::Finder::new(b"\nFrom ");
    let mut search = from.saturating_sub(1);
    while let Some(n) = finder.find(&data[search..]) {
        let line_start = search + n + 1;
        let line_end = memchr::memchr(b'\n', &data[line_start..]).map_or(data.len(), |e| line_start + e);
        if line_start >= from && is_envelope_line(&data[line_start..line_end]) {
            return Some(line_start);
        }
        search = line_start;
    }
    None
}

/// Removes the blank line mbox writers put before the next "From " line.
fn strip_separator(data: &[u8], start: usize, next: usize) -> usize {
    let body = &data[start..next];
    if body.ends_with(b"\r\n\r\n") {
        next - 2
    } else if body.ends_with(b"\n\n") {
        next - 1
    } else {
        next
    }
}

/// mboxcl / mboxcl2: when the headers carry a Content-Length that lands exactly on the next
/// envelope line (or the end of the file), trust it.
fn length_end(data: &[u8], start: usize) -> Option<usize> {
    let rest = &data[start..];
    let header_end = header_block_end(rest)?;
    let length = header_value(&rest[..header_end], b"content-length")?;
    let length: usize = std::str::from_utf8(length).ok()?.trim().parse().ok()?;
    let body_start = start + header_end;
    let end = body_start.checked_add(length)?;
    if end > data.len() {
        return None;
    }
    // The separator: zero, one or two newlines, then an envelope line or the end.
    let mut next = end;
    for _ in 0..2 {
        if data.get(next) == Some(&b'\r') && data.get(next + 1) == Some(&b'\n') {
            next += 2;
        } else if data.get(next) == Some(&b'\n') {
            next += 1;
        }
        if next >= data.len() {
            return Some(data.len());
        }
        let line_end = memchr::memchr(b'\n', &data[next..]).map_or(data.len(), |e| next + e);
        if is_envelope_line(&data[next..line_end]) {
            return Some(next);
        }
    }
    None
}

/// Offset just past the blank line that ends the header block.
fn header_block_end(rest: &[u8]) -> Option<usize> {
    let mut i = 0;
    loop {
        let n = memchr::memchr(b'\n', &rest[i..])?;
        let line = &rest[i..i + n];
        if line.is_empty() || line == b"\r" {
            return Some(i + n + 1);
        }
        i += n + 1;
        if i >= rest.len() {
            return None;
        }
    }
}

fn header_value<'a>(headers: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for line in headers.split(|b| *b == b'\n') {
        if let Some(colon) = line.iter().position(|b| *b == b':') {
            if line[..colon].eq_ignore_ascii_case(name) {
                return Some(line[colon + 1..].trim_ascii());
            }
        }
    }
    None
}

/// mboxrd unescaping: ">From ", ">>From " … lose one ">" (mboxo files only ever have one).
fn unescape_from(body: &[u8]) -> Cow<'_, [u8]> {
    let needs = body.starts_with(b">") && is_escaped_from(body) || memchr::memmem::find(body, b"\n>").is_some_and(|_| {
        memchr::memmem::find_iter(body, b"\n>").any(|n| is_escaped_from(&body[n + 1..]))
    });
    if !needs {
        return Cow::Borrowed(body);
    }
    let mut out = Vec::with_capacity(body.len());
    let mut line_start = true;
    let mut i = 0;
    while i < body.len() {
        if line_start && body[i] == b'>' && is_escaped_from(&body[i..]) {
            i += 1; // drop one '>'
        }
        let b = body[i];
        out.push(b);
        line_start = b == b'\n';
        i += 1;
    }
    Cow::Owned(out)
}

fn is_escaped_from(s: &[u8]) -> bool {
    let quotes = s.iter().take_while(|b| **b == b'>').count();
    quotes > 0 && s[quotes..].starts_with(b"From ")
}

/// "From sender Sat Jan  3 01:05:34 1996": a "From " line with a time and a 4-digit year.
pub fn is_envelope_line(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"From ") else { return false };
    let rest = rest.strip_suffix(b"\r").unwrap_or(rest);
    let text = String::from_utf8_lossy(rest);
    let mut has_time = false;
    let mut has_year = false;
    for token in text.split_whitespace().skip(1) {
        if is_time(token) {
            has_time = true;
        } else if token.len() == 4 && token.bytes().all(|b| b.is_ascii_digit()) && (token.starts_with("19") || token.starts_with("20")) {
            has_year = true;
        }
    }
    has_time && has_year
}

fn is_time(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    (parts.len() == 2 || parts.len() == 3) && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// The envelope line's date (asctime, local time of the writer: read as UTC).
pub fn envelope_date(line: &[u8]) -> Option<i64> {
    let text = String::from_utf8_lossy(line.strip_prefix(b"From ")?);
    let tokens: Vec<&str> = text.split_whitespace().skip(1).collect();
    let month_at = tokens.iter().position(|t| t.len() >= 3 && MONTHS.contains(&t[..3].to_ascii_lowercase().as_str()))?;
    let month = MONTHS.iter().position(|m| *m == tokens[month_at][..3].to_ascii_lowercase())? as u32 + 1;
    let day: u32 = tokens.get(month_at + 1)?.parse().ok()?;
    let time = tokens.iter().skip(month_at).find(|t| is_time(t))?;
    let year: i32 = tokens.iter().skip(month_at).find(|t| t.len() == 4 && t.parse::<i32>().is_ok())?.parse().ok()?;
    let mut hms = time.split(':').map(|p| p.parse::<u32>().unwrap_or(0));
    let (h, m, s) = (hms.next()?, hms.next()?, hms.next().unwrap_or(0));
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(h, m, s)?;
    Some(date.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(data: &[u8]) -> Vec<(Vec<u8>, Option<i64>)> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mbox");
        std::fs::write(&path, data).unwrap();
        let mbox = MboxFile::open(&path).unwrap();
        mbox.spans(0).map(|s| {
            let m = mbox.message(s);
            (m.raw.into_owned(), m.envelope_date)
        }).collect()
    }

    #[test]
    fn splits_and_unescapes() {
        let data = b"From alice@example.com Sat Jan  3 01:05:34 1996\nSubject: one\n\nFrom what I hear, fine.\n>From the top\n>>From deeper\n\n\
From bob@example.com Mon Feb  5 10:00:00 2001\nSubject: two\n\nsecond\n";
        let m = messages(data);
        assert_eq!(m.len(), 2);
        // "From what I hear" isn't an envelope line (no time, no year).
        assert_eq!(m[0].0, b"Subject: one\n\nFrom what I hear, fine.\nFrom the top\n>From deeper\n".to_vec());
        assert_eq!(m[0].1, Some(820_631_134));
        assert_eq!(m[1].0, b"Subject: two\n\nsecond\n".to_vec());
    }

    #[test]
    fn crlf_and_leading_garbage() {
        let data = b"junk before\r\nFrom x@y Tue Mar  4 05:06:07 1997\r\nSubject: a\r\n\r\nbody\r\n\r\nFrom x@y Tue Mar  4 05:06:08 1997\r\nSubject: b\r\n\r\nbody2\r\n";
        let m = messages(data);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].0, b"Subject: a\r\n\r\nbody\r\n".to_vec());
        assert_eq!(m[1].0, b"Subject: b\r\n\r\nbody2\r\n".to_vec());
    }

    #[test]
    fn content_length_keeps_unescaped_from_lines() {
        let body = b"line one\nFrom joe@x Sat Jan  3 01:05:34 1996 looks like an envelope\nend\n";
        let data = [
            b"From a@b Sat Jan  3 01:05:34 1996\nContent-Length: ".as_slice(),
            body.len().to_string().as_bytes(),
            b"\nSubject: cl\n\n",
            body,
            b"\nFrom c@d Sun Jan  4 01:05:34 1996\nSubject: next\n\nok\n",
        ]
        .concat();
        let m = messages(&data);
        assert_eq!(m.len(), 2, "{:?}", m.iter().map(|x| String::from_utf8_lossy(&x.0).into_owned()).collect::<Vec<_>>());
        assert!(m[0].0.ends_with(body));
        assert_eq!(m[1].0, b"Subject: next\n\nok\n".to_vec());
    }

    #[test]
    fn resume_from_next_offset() {
        let data = b"From a@b Sat Jan  3 01:05:34 1996\nSubject: 1\n\nx\n\nFrom a@b Sat Jan  3 01:05:35 1996\nSubject: 2\n\ny\n\nFrom a@b Sat Jan  3 01:05:36 1996\nSubject: 3\n\nz\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.mbox");
        std::fs::write(&path, data).unwrap();
        let mbox = MboxFile::open(&path).unwrap();
        let first = mbox.spans(0).next().unwrap();
        let rest: Vec<_> = mbox.spans(first.next).map(|s| mbox.message(s).raw.into_owned()).collect();
        assert_eq!(rest, vec![b"Subject: 2\n\ny\n".to_vec(), b"Subject: 3\n\nz\n".to_vec()]);
        assert_eq!(mbox.count(), 3);
    }

    #[test]
    fn empty_file() {
        assert!(messages(b"").is_empty());
    }
}
