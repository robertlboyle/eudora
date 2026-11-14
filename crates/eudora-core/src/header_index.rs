//! Searching by any header ("x-mailer:outlook", "list-id:announce", "return-path:bob@x.com",
//! "x-spam-flag:*"). A second Tantivy index beside the main one (`<library>/header-index`):
//! the main index's schema can't take new fields without being rebuilt, which for millions
//! of messages would hold up everything else for hours. This one fills in the background in
//! the process that writes the library (the download): new mail first, then the rest from the
//! newest down, a batch at a time, so it's useful long before it's complete.
//!
//! Each header becomes words of its own: "X-Mailer: Microsoft Outlook 16" →
//! `x-mailer:*  x-mailer:microsoft  x-mailer:outlook  x-mailer:16`. A value is split into
//! words at everything that isn't a letter or digit, and a search value the same way, so
//! `return-path:bob@x.com` is the phrase `bob x com` within Return-Path.
//!
//! What's covered is the ids from `low` to `high` (the commit payload).

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    DateOptions, DateTimePrecision, Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, WhitespaceTokenizer};
use tantivy::{DateTime, Index, IndexReader, IndexWriter, Order, ReloadPolicy, TantivyDocument, Term};

use crate::error::{Error, Result};

const TOKENIZER: &str = "headers";
/// Headers that are long signatures or blobs: nothing anyone searches for.
const SKIPPED: [&str; 6] = ["dkim-signature", "arc-seal", "arc-message-signature", "x-google-dkim-signature", "x-ms-exchange-antispam-messagedata-0", "x-ms-exchange-antispam-messagedata-chunkcount"];
/// Words kept per header (long Received chains and the like are cut).
const WORDS_PER_HEADER: usize = 60;
/// Make what's been added searchable at least this often.
const COMMIT_EVERY: Duration = Duration::from_secs(30);

/// Every header of a raw message: (name lowercase, value unfolded and decoded), in order.
pub fn read_headers(raw: &[u8]) -> Vec<(String, String)> {
    let end = header_end(raw);
    let block = crate::parse::decode_8bit(&raw[..end]);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in block.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty() && !name.contains(' ') {
                headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
            }
        }
    }
    for h in &mut headers {
        h.1 = decode_words(&h.1);
    }
    headers
}

fn header_end(raw: &[u8]) -> usize {
    raw.windows(4).position(|w| w == b"\r\n\r\n").or_else(|| raw.windows(2).position(|w| w == b"\n\n")).unwrap_or(raw.len())
}

/// RFC 2047 encoded words (=?utf-8?B?…?=) decoded.
pub fn decode_words(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_string();
    }
    let fake = format!("Subject: {value}\r\n\r\n");
    mail_parser::MessageParser::default()
        .parse(fake.as_bytes())
        .and_then(|m| m.subject().map(str::to_string))
        .unwrap_or_else(|| value.to_string())
}

/// A value's words, as indexed and as searched.
pub fn words(value: &str) -> Vec<String> {
    value.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// The indexed text for a message's headers.
fn header_text(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in headers {
        if SKIPPED.contains(&name.as_str()) || name.chars().any(char::is_whitespace) {
            continue;
        }
        out.push_str(name);
        out.push_str(":* ");
        for w in words(value).into_iter().take(WORDS_PER_HEADER) {
            out.push_str(name);
            out.push(':');
            out.push_str(&w);
            out.push(' ');
        }
    }
    out
}

/// One `name:value` part of a search.
#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    /// Lowercase header name.
    pub name: String,
    /// The value's words (empty = "has this header": `name:*`).
    pub words: Vec<String>,
    /// `-name:value`: messages without it.
    pub negate: bool,
}

/// Whether a search (as typed) has header parts, after Outlook's words are turned into the
/// index's own (`hasattachment:`, `received:`… aren't headers): its results then depend on how
/// far the header index has come.
pub fn uses_headers(query: &str) -> bool {
    !split_query(&crate::aqs::translate(query, chrono::Local::now().date_naive())).1.is_empty()
}

/// Field names the main index searches itself (anything else before a colon is a header).
const MAIN_FIELDS: [&str; 11] = ["from", "to", "cc", "bcc", "subject", "body", "attachment", "date", "size", "attachments", "folder"];

/// Splits a search into its header parts and the rest (for the main index). A header part is
/// `name:word`, `name:"some words"`, `name:*`, with `-` before it for "without".
pub fn split_query(query: &str) -> (String, Vec<Clause>) {
    let mut rest = String::new();
    let mut clauses = Vec::new();
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;
    let mut in_range = 0usize;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            rest.push(chars[i]);
            i += 1;
            continue;
        }
        // one term: up to whitespace, a quoted part counting as one
        let start = i;
        let mut quoted = false;
        while i < chars.len() && (quoted || !chars[i].is_whitespace()) {
            if chars[i] == '"' {
                quoted = !quoted;
            }
            i += 1;
        }
        let term: String = chars[start..i].iter().collect();
        // inside a range ("date:[2001-01-01T00:00:00Z TO 2002-…]") nothing is a header
        let opens = term.matches(['[', '{']).count();
        let closes = term.matches([']', '}']).count();
        let clause = if in_range == 0 && opens == 0 { header_clause(&term) } else { None };
        in_range = (in_range + opens).saturating_sub(closes);
        match clause {
            Some(c) => clauses.push(c),
            None => rest.push_str(&term),
        }
    }
    (rest.split_whitespace().collect::<Vec<_>>().join(" "), clauses)
}

pub(crate) fn header_clause(term: &str) -> Option<Clause> {
    let (negate, term) = match term.strip_prefix('-') {
        Some(t) => (true, t),
        None => (false, term),
    };
    let (name, value) = term.split_once(':')?;
    let name = name.to_ascii_lowercase();
    let is_name = name.starts_with(|c: char| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !is_name || MAIN_FIELDS.contains(&name.as_str()) || value.is_empty() || value.starts_with('[') || value.starts_with('{') {
        return None;
    }
    let value = value.trim_matches('"');
    let words = if value == "*" { Vec::new() } else { words(value) };
    if value != "*" && words.is_empty() {
        return None;
    }
    Some(Clause { name, words, negate })
}

pub struct HeaderIndex {
    index: Index,
    reader: IndexReader,
    id: Field,
    date: Field,
    headers: Field,
    writer: Mutex<Option<IndexWriter>>,
    /// What's added but not yet committed: (high, low, when the last commit was).
    pending: Mutex<Option<(i64, i64, Instant)>>,
}

fn schema() -> (Schema, Field, Field, Field) {
    let mut b = Schema::builder();
    let id = b.add_u64_field("id", NumericOptions::default().set_indexed().set_fast().set_stored());
    let date = b.add_date_field("date", DateOptions::default().set_fast().set_precision(DateTimePrecision::Seconds));
    let text = TextOptions::default().set_indexing_options(TextFieldIndexing::default().set_tokenizer(TOKENIZER).set_index_option(IndexRecordOption::WithFreqsAndPositions));
    let headers = b.add_text_field("headers", text);
    (b.build(), id, date, headers)
}

impl HeaderIndex {
    /// Opens the header index; `create` makes it if it isn't there (only the library's writer
    /// does: None for a reader when it doesn't exist yet).
    pub fn open(dir: &Path, create: bool) -> Result<Option<HeaderIndex>> {
        let (schema, id, date, headers) = schema();
        let index = if dir.join("meta.json").exists() {
            Index::open_in_dir(dir)?
        } else if create {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io { context: format!("creating {}", dir.display()), source })?;
            Index::create_in_dir(dir, schema.clone())?
        } else {
            return Ok(None);
        };
        if index.schema() != schema {
            return Err(Error::Other("the header index was made by another version: delete the header-index folder and it's made again".into()));
        }
        index.tokenizers().register(TOKENIZER, TextAnalyzer::builder(WhitespaceTokenizer::default()).filter(LowerCaser).build());
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(Some(HeaderIndex { index, reader, id, date, headers, writer: Mutex::new(None), pending: Mutex::new(None) }))
    }

    pub fn start_writing(&self) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        if writer.is_none() {
            *writer = Some(self.index.writer_with_num_threads(2, 64 * 1024 * 1024)?);
        }
        Ok(())
    }

    /// The ids covered so far: (low, high); None before anything was committed.
    pub fn covered(&self) -> Result<Option<(i64, i64)>> {
        let metas = self.index.load_metas()?;
        Ok(metas.payload.and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok()).and_then(|v| Some((v["low"].as_i64()?, v["high"].as_i64()?))))
    }

    /// Adds (or replaces) messages: (id, date, raw headers).
    fn add(&self, docs: &[(i64, i64, Vec<(String, String)>)]) -> Result<()> {
        let writer = self.writer.lock().unwrap();
        let writer = writer.as_ref().ok_or_else(|| Error::Other("the header index isn't open for writing".into()))?;
        for (id, date, headers) in docs {
            let mut doc = TantivyDocument::default();
            doc.add_u64(self.id, *id as u64);
            doc.add_date(self.date, DateTime::from_timestamp_secs(*date));
            doc.add_text(self.headers, header_text(headers));
            writer.delete_term(Term::from_field_u64(self.id, *id as u64));
            writer.add_document(doc)?;
        }
        Ok(())
    }

    fn commit(&self, low: i64, high: i64) -> Result<()> {
        {
            let mut guard = self.writer.lock().unwrap();
            let writer = guard.as_mut().ok_or_else(|| Error::Other("the header index isn't open for writing".into()))?;
            let mut prepared = writer.prepare_commit()?;
            prepared.set_payload(&serde_json::json!({ "low": low, "high": high }).to_string());
            prepared.commit()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    fn query(&self, clauses: &[Clause]) -> Box<dyn Query> {
        let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for c in clauses {
            let term = |w: &str| Term::from_field_text(self.headers, &format!("{}:{w}", c.name));
            let q: Box<dyn Query> = match c.words.len() {
                0 => Box::new(TermQuery::new(term("*"), IndexRecordOption::Basic)),
                1 => Box::new(TermQuery::new(term(&c.words[0]), IndexRecordOption::Basic)),
                _ => Box::new(PhraseQuery::new(c.words.iter().map(|w| term(w)).collect())),
            };
            parts.push((if c.negate { Occur::MustNot } else { Occur::Must }, q));
        }
        if parts.iter().all(|(o, _)| *o == Occur::MustNot) {
            parts.push((Occur::Must, Box::new(AllQuery)));
        }
        Box::new(BooleanQuery::new(parts))
    }

    /// Every message matching the header parts (for combining with the rest of a search).
    pub fn matching_ids(&self, clauses: &[Clause]) -> Result<Vec<u64>> {
        let searcher = self.reader.searcher();
        let found = searcher.search(&*self.query(clauses), &DocSetCollector)?;
        let mut ids = Vec::with_capacity(found.len());
        for address in found {
            let column = searcher.segment_reader(address.segment_ord).fast_fields().u64("id")?;
            if let Some(id) = column.first(address.doc_id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// A search of only header parts: (every match, the `limit` newest or oldest ids).
    pub fn search(&self, clauses: &[Clause], limit: usize, newest: bool) -> Result<(usize, Vec<u64>)> {
        let searcher = self.reader.searcher();
        let direction = if newest { Order::Desc } else { Order::Asc };
        let collector = TopDocs::with_limit(limit.max(1)).order_by_fast_field::<DateTime>("date", direction);
        let (total, top) = searcher.search(&*self.query(clauses), &(Count, collector))?;
        let mut ids = Vec::with_capacity(top.len());
        for (_, address) in top {
            let column = searcher.segment_reader(address.segment_ord).fast_fields().u64("id")?;
            if let Some(id) = column.first(address.doc_id) {
                ids.push(id);
            }
        }
        Ok((total, ids))
    }
}

/// How far the header index has come, for the search's note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Coverage {
    pub covered: u64,
    pub total: u64,
}

impl crate::Library {
    /// One step of keeping the header index up to date (the library's writer calls it often):
    /// new messages first, then older ones from the newest down, about `batch` at a time.
    /// Returns how many were added.
    pub fn update_header_index(&self, batch: usize) -> Result<usize> {
        let Some(hx) = &self.headers else { return Ok(0) };
        if self.access != crate::library::Access::Write {
            return Ok(0);
        }
        let mut pending = hx.pending.lock().unwrap();
        let (low, high, last_commit) = match *pending {
            Some(p) => p,
            None => {
                let (low, high) = match hx.covered()? {
                    Some(c) => c,
                    None => {
                        // nothing yet: start at the newest message and work both ways
                        let max: i64 = self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?;
                        (max + 1, max)
                    }
                };
                (low, high, Instant::now())
            }
        };
        let newer: Vec<(i64, i64, crate::pack::Location)> = {
            let mut stmt = self.conn.prepare_cached("SELECT id, date, pack, pack_offset, pack_len FROM messages WHERE id > ?1 ORDER BY id LIMIT ?2")?;
            stmt.query_map(rusqlite::params![high, batch as i64], |r| Ok((r.get(0)?, r.get(1)?, crate::library::Library::location(r, 2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let (work, newer_side) = if !newer.is_empty() {
            (newer, true)
        } else {
            let mut stmt = self.conn.prepare_cached("SELECT id, date, pack, pack_offset, pack_len FROM messages WHERE id < ?1 ORDER BY id DESC LIMIT ?2")?;
            let older: Vec<(i64, i64, crate::pack::Location)> = stmt
                .query_map(rusqlite::params![low, batch as i64], |r| Ok((r.get(0)?, r.get(1)?, crate::library::Library::location(r, 2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            (older, false)
        };
        let (mut low, mut high) = (low, high);
        let added = work.len();
        if !work.is_empty() {
            use rayon::prelude::*;
            let packs = &self.packs;
            let docs: Vec<(i64, i64, Vec<(String, String)>)> = work
                .par_iter()
                .filter_map(|(id, date, location)| packs.read(*location).ok().map(|r| (*id, *date, read_headers(&r.raw))))
                .collect();
            hx.add(&docs)?;
            if newer_side {
                high = work.iter().map(|w| w.0).max().unwrap_or(high);
            } else {
                low = work.iter().map(|w| w.0).min().unwrap_or(low);
            }
        }
        // commit every half minute, and once caught up
        let changed = hx.covered()? != Some((low, high));
        if changed && (last_commit.elapsed() >= COMMIT_EVERY || work.is_empty()) {
            hx.commit(low, high)?;
            *pending = Some((low, high, Instant::now()));
        } else {
            *pending = Some((low, high, last_commit));
        }
        Ok(added)
    }

    /// Makes everything the header index has been given searchable (end of a download).
    pub fn commit_header_index(&self) -> Result<()> {
        let Some(hx) = &self.headers else { return Ok(()) };
        if let Some((low, high, _)) = *hx.pending.lock().unwrap() {
            if hx.covered()? != Some((low, high)) {
                hx.commit(low, high)?;
            }
        }
        Ok(())
    }

    /// How many messages header search covers so far.
    pub fn header_coverage(&self) -> Result<Coverage> {
        let total: i64 = self.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        let covered = match self.headers.as_ref().map(|h| h.covered()).transpose()?.flatten() {
            Some((low, high)) => self.conn.query_row("SELECT COUNT(*) FROM messages WHERE id >= ?1 AND id <= ?2", rusqlite::params![low, high], |r| r.get::<_, i64>(0))?,
            None => 0,
        };
        Ok(Coverage { covered: covered as u64, total: total as u64 })
    }

    /// Values of these headers for each message (in order; a header that appears more than
    /// once is joined with " · "), for the message list's header columns.
    pub fn header_values(&self, ids: &[i64], names: &[String]) -> Result<Vec<Vec<String>>> {
        use rayon::prelude::*;
        let wanted: Vec<String> = names.iter().map(|n| n.trim().trim_end_matches(':').to_ascii_lowercase()).collect();
        let mut stmt = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1")?;
        let mut locations = Vec::with_capacity(ids.len());
        for id in ids {
            use rusqlite::OptionalExtension;
            locations.push(stmt.query_row([id], |r| crate::library::Library::location(r, 0)).optional()?);
        }
        let packs = &self.packs;
        Ok(locations
            .par_iter()
            .map(|l| {
                let headers = l.and_then(|l| packs.read(l).ok()).map(|r| read_headers(&r.raw)).unwrap_or_default();
                wanted
                    .iter()
                    .map(|n| headers.iter().filter(|(h, _)| h == n).map(|(_, v)| v.as_str()).collect::<Vec<_>>().join(" · "))
                    .collect()
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_read_unfolded_and_decoded() {
        let raw = b"From: a@x\r\nX-Mailer: Microsoft\r\n Outlook 16\r\nSubject: =?utf-8?B?Q2Fmw6k=?=\r\n\r\nX-Body: no\r\n";
        let h = read_headers(raw);
        assert_eq!(h, vec![("from".into(), "a@x".into()), ("x-mailer".into(), "Microsoft Outlook 16".into()), ("subject".into(), "Café".into())]);
        assert_eq!(header_text(&h[1..2]), "x-mailer:* x-mailer:microsoft x-mailer:outlook x-mailer:16 ");
    }

    #[test]
    fn a_search_is_split_into_header_parts_and_the_rest() {
        let (rest, c) = split_query(r#"invoice from:bob X-Mailer:"Outlook 16" -list-id:* return-path:bob@x.com date:[2001 TO 2002] 10:30 date:[1970-01-01T00:00:00Z TO 9999-01-01T00:00:00Z}"#);
        assert_eq!(rest, "invoice from:bob date:[2001 TO 2002] 10:30 date:[1970-01-01T00:00:00Z TO 9999-01-01T00:00:00Z}");
        assert!(uses_headers("x-mailer:outlook") && !uses_headers("hasattachment:yes received:last week from:bob"));
        assert_eq!(c, vec![
            Clause { name: "x-mailer".into(), words: vec!["outlook".into(), "16".into()], negate: false },
            Clause { name: "list-id".into(), words: vec![], negate: true },
            Clause { name: "return-path".into(), words: vec!["bob".into(), "x".into(), "com".into()], negate: false },
        ]);
    }

    #[test]
    fn header_search_finds_messages_by_any_header() {
        let dir = tempfile::tempdir().unwrap();
        let hx = HeaderIndex::open(dir.path(), true).unwrap().unwrap();
        hx.start_writing().unwrap();
        let msg = |mailer: &str, list: Option<&str>| {
            let mut h = vec![("x-mailer".to_string(), mailer.to_string())];
            if let Some(l) = list {
                h.push(("list-id".to_string(), l.to_string()));
            }
            h
        };
        hx.add(&[(1, 100, msg("Microsoft Outlook 16", None)), (2, 200, msg("Thunderbird 115", Some("<news.example.com>"))), (3, 300, msg("Outlook Express 6", None))]).unwrap();
        hx.commit(1, 3).unwrap();
        let find = |q: &str| {
            let (_, c) = split_query(q);
            let mut ids = hx.matching_ids(&c).unwrap();
            ids.sort();
            ids
        };
        assert_eq!(find("x-mailer:outlook"), vec![1, 3]);
        assert_eq!(find(r#"x-mailer:"outlook 16""#), vec![1]);
        assert_eq!(find("list-id:*"), vec![2]);
        assert_eq!(find("-list-id:*"), vec![1, 3]);
        assert_eq!(find("list-id:news.example.com"), vec![2]);
        let (_, c) = split_query("x-mailer:outlook");
        assert_eq!(hx.search(&c, 10, true).unwrap(), (2, vec![3, 1]), "newest first");
        assert_eq!(hx.covered().unwrap(), Some((1, 3)));
    }
}
