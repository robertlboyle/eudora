//! The full-text index (Tantivy, inside the process). Holds only what search needs; the
//! results are message ids that the database turns into rows.
//!
//! Each commit records the highest message id it covers (the commit payload). Messages
//! stored after that (a crash between the database commit and the index commit) are
//! added again when the library opens for writing.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tantivy::collector::{Count, TopDocs};
use tantivy::query::QueryParser;
use tantivy::schema::{
    DateOptions, DateTimePrecision, FAST, Field, INDEXED, IndexRecordOption, NumericOptions, STORED, Schema,
    TextFieldIndexing, TextOptions,
};
use tantivy::{DateTime, Index, IndexReader, IndexWriter, Order, ReloadPolicy, TantivyDocument, Term};

use crate::error::{Error, Result};

/// Index format version: bump when the schema changes (the index is then rebuilt).
pub const INDEX_VERSION: u32 = 1;

#[derive(Clone, Copy)]
pub struct Fields {
    pub id: Field,
    pub from: Field,
    pub to: Field,
    pub cc: Field,
    pub bcc: Field,
    pub subject: Field,
    pub body: Field,
    pub attachment: Field,
    pub date: Field,
    pub size: Field,
    pub attachments: Field,
    pub folder: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let words = |tokenizer: &str, positions: bool| {
        let options = if positions { IndexRecordOption::WithFreqsAndPositions } else { IndexRecordOption::WithFreqs };
        TextOptions::default().set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer).set_index_option(options))
    };
    let fields = Fields {
        id: b.add_u64_field("id", NumericOptions::default().set_indexed().set_fast().set_stored()),
        from: b.add_text_field("from", words("default", true)),
        to: b.add_text_field("to", words("default", true)),
        cc: b.add_text_field("cc", words("default", true)),
        bcc: b.add_text_field("bcc", words("default", true)),
        subject: b.add_text_field("subject", words("en_stem", true)),
        body: b.add_text_field("body", words("en_stem", true)),
        attachment: b.add_text_field("attachment", words("default", true)),
        date: b.add_date_field("date", DateOptions::from(INDEXED).set_fast().set_precision(DateTimePrecision::Seconds)),
        size: b.add_u64_field("size", INDEXED | FAST),
        attachments: b.add_u64_field("attachments", INDEXED | FAST),
        folder: b.add_u64_field("folder", INDEXED | FAST | STORED),
    };
    (b.build(), fields)
}

/// What goes into the index for one message.
pub struct IndexDoc<'a> {
    pub id: u64,
    pub from: &'a str,
    pub to: &'a str,
    pub cc: &'a str,
    pub bcc: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub attachment_names: &'a str,
    pub date: i64,
    pub size: u64,
    pub attachments: u64,
    pub folders: &'a [i64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Newest,
    Oldest,
    Relevance,
}

/// What a search keeps besides its words: only messages in `only` (when given) and none in
/// `not` (status, labels, priority, and account: search_marks.rs).
#[derive(Clone, Debug, Default)]
pub struct Narrow {
    pub only: Option<IdSet>,
    pub not: Option<IdSet>,
}

impl Narrow {
    pub fn keeps(&self, id: u64) -> bool {
        self.only.as_ref().is_none_or(|s| s.contains(id)) && self.not.as_ref().is_none_or(|s| !s.contains(id))
    }
}

/// A set of message ids as a bitmap (the messages a search leaves out).
#[derive(Clone, Debug, Default)]
pub struct IdSet {
    bits: Vec<u64>,
    len: usize,
}

impl IdSet {
    pub fn new(ids: impl IntoIterator<Item = u64>) -> IdSet {
        let mut set = IdSet::default();
        for id in ids {
            let (word, bit) = ((id / 64) as usize, id % 64);
            if word >= set.bits.len() {
                set.bits.resize(word + 1, 0);
            }
            if set.bits[word] & (1 << bit) == 0 {
                set.bits[word] |= 1 << bit;
                set.len += 1;
            }
        }
        set
    }

    /// Puts an id in (`on`) or takes it out.
    pub fn set(&mut self, id: u64, on: bool) {
        let (word, bit) = ((id / 64) as usize, id % 64);
        if word >= self.bits.len() {
            if !on {
                return;
            }
            self.bits.resize(word + 1, 0);
        }
        let was = self.bits[word] & (1 << bit) != 0;
        if on && !was {
            self.bits[word] |= 1 << bit;
            self.len += 1;
        } else if !on && was {
            self.bits[word] &= !(1 << bit);
            self.len -= 1;
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.bits.get((id / 64) as usize).is_some_and(|w| w & (1 << (id % 64)) != 0)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub struct SearchResult {
    /// Every match, not just the ones returned.
    pub total: usize,
    pub ids: Vec<u64>,
    pub took: Duration,
    /// What was repaired or left out of the search, for the user.
    pub notes: Vec<String>,
    /// Corrected searches to offer (`search_help`).
    pub suggestions: Vec<crate::search_help::Suggestion>,
    /// Matches left out because they're only in mailboxes left out of search (search_scope.rs).
    pub hidden: usize,
}

/// Tantivy's parser, as every search uses it.
fn parser(index: &Index, f: &Fields) -> QueryParser {
    let mut parser = QueryParser::for_index(index, vec![f.subject, f.body, f.from, f.to, f.cc, f.attachment]);
    parser.set_conjunction_by_default();
    parser.set_field_boost(f.subject, 2.0);
    parser
}

/// A word searched as the start of words: the last word typed, or any word ending
/// in `*`.
#[derive(Debug, PartialEq)]
struct Prefix {
    /// `-word*`: left out.
    not: bool,
    /// `from:word*`: only that field.
    field: Option<String>,
    word: String,
    /// The last word typed: it matches whole too (as stemmed: "running" finds "run").
    typed: bool,
}

/// Fields a prefix may name (the text fields).
const PREFIX_FIELDS: [&str; 7] = ["from", "to", "cc", "bcc", "subject", "body", "attachment"];

/// Splits the prefix words out of a search: what's left (for Tantivy's parser) and the prefixes.
/// Only plain words at the top level count: never inside quotes, parentheses or a range, nor in
/// a search using OR (where taking a word out would change what the rest means).
fn split_prefixes(query: &str) -> (String, Vec<Prefix>) {
    // the search's top-level pieces, separated by spaces
    let (mut pieces, mut current, mut quoted, mut depth) = (Vec::new(), String::new(), false, 0i32);
    for c in query.chars() {
        match c {
            '"' => quoted = !quoted,
            '(' | '[' | '{' if !quoted => depth += 1,
            ')' | ']' | '}' if !quoted => depth -= 1,
            c if c.is_whitespace() && !quoted && depth <= 0 => {
                if !current.is_empty() {
                    pieces.push(std::mem::take(&mut current));
                }
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    if pieces.iter().any(|p| p == "OR" || p == "||") {
        return (query.to_string(), Vec::new());
    }
    let read = |piece: &str, typed: bool| -> Option<Prefix> {
        let (not, rest) = match piece.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, piece),
        };
        let (field, word) = match rest.split_once(':') {
            Some((f, w)) if PREFIX_FIELDS.contains(&f.to_ascii_lowercase().as_str()) => (Some(f.to_ascii_lowercase()), w),
            Some(_) => return None,
            None => (None, rest),
        };
        let (word, starred) = match word.strip_suffix('*') {
            Some(w) => (w, true),
            None => (word, false),
        };
        let letters = word.chars().count();
        let plain = !word.is_empty() && word.chars().all(char::is_alphanumeric);
        if !plain || (!starred && !typed) || ["AND", "NOT"].contains(&word) {
            return None;
        }
        // a typed word only from three letters (fewer would match half the library), a starred one from two
        if letters < if starred { 2 } else { 3 } || (typed && !starred && not) {
            return None;
        }
        Some(Prefix { not, field, word: word.to_lowercase(), typed: typed && !starred })
    };
    let last = pieces.len().saturating_sub(1);
    let (mut kept, mut prefixes) = (Vec::new(), Vec::new());
    for (i, piece) in pieces.into_iter().enumerate() {
        match read(&piece, i == last) {
            Some(p) => prefixes.push(p),
            None => kept.push(piece),
        }
    }
    (kept.join(" "), prefixes)
}

/// A search's top-level pieces, separated by spaces (outside quotes, parentheses, and ranges).
fn top_pieces(query: &str) -> Vec<String> {
    let (mut pieces, mut current, mut quoted, mut depth) = (Vec::new(), String::new(), false, 0i32);
    for c in query.chars() {
        match c {
            '"' => quoted = !quoted,
            '(' | '[' | '{' if !quoted => depth += 1,
            ')' | ']' | '}' if !quoted => depth -= 1,
            c if c.is_whitespace() && !quoted && depth <= 0 => {
                if !current.is_empty() {
                    pieces.push(std::mem::take(&mut current));
                }
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// `word*`, `-word*`, `field:word*` (a text field, a word of two letters or more): (left out,
/// the field, the word in lowercase).
fn star_word(token: &str) -> Option<(bool, Option<String>, String)> {
    let (not, rest) = match token.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, token),
    };
    let rest = rest.strip_suffix('*')?;
    let (field, word) = match rest.split_once(':') {
        Some((f, w)) if PREFIX_FIELDS.contains(&f.to_ascii_lowercase().as_str()) => (Some(f.to_ascii_lowercase()), w),
        Some(_) => return None,
        None => (None, rest),
    };
    (word.chars().count() >= 2 && word.chars().all(char::is_alphanumeric)).then(|| (not, field, word.to_lowercase()))
}

/// Reads a search, the prefix words included (see `split_prefixes`).
fn read_query(parser: &QueryParser, fields: &Fields, query: &str) -> Result<(Box<dyn tantivy::query::Query>, Option<String>)> {
    use tantivy::query::{AllQuery, BooleanQuery, FuzzyTermQuery, Occur, Query};
    let (rest, prefixes) = split_prefixes(query);
    if prefixes.is_empty() {
        return read_words(parser, query);
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut note = None;
    if !rest.trim().is_empty() {
        let (q, n) = read_words(parser, &rest)?;
        clauses.push((Occur::Must, q));
        note = n;
    }
    for p in &prefixes {
        let searched: Vec<Field> = match p.field.as_deref() {
            Some("from") => vec![fields.from],
            Some("to") => vec![fields.to],
            Some("cc") => vec![fields.cc],
            Some("bcc") => vec![fields.bcc],
            Some("subject") => vec![fields.subject],
            Some("body") => vec![fields.body],
            Some("attachment") => vec![fields.attachment],
            _ => vec![fields.subject, fields.body, fields.from, fields.to, fields.cc, fields.attachment],
        };
        // a distance of 0 with "prefix" = words starting with it
        let mut either: Vec<(Occur, Box<dyn Query>)> =
            searched.into_iter().map(|f| (Occur::Should, Box::new(FuzzyTermQuery::new_prefix(Term::from_field_text(f, &p.word), 0, false)) as Box<dyn Query>)).collect();
        if p.typed {
            let whole = match &p.field {
                Some(f) => format!("{f}:{}", p.word),
                None => p.word.clone(),
            };
            either.push((Occur::Should, read_words(parser, &whole)?.0));
        }
        clauses.push((if p.not { Occur::MustNot } else { Occur::Must }, Box::new(BooleanQuery::new(either))));
    }
    if !clauses.iter().any(|(o, _)| *o == Occur::Must) {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }
    Ok((Box::new(BooleanQuery::new(clauses)), note))
}

/// Reads a search; one with a syntax error (often a search still being typed) is read as far
/// as it can be, and says what was left out.
fn read_words(parser: &QueryParser, query: &str) -> Result<(Box<dyn tantivy::query::Query>, Option<String>)> {
    match parser.parse_query(query) {
        Ok(q) => Ok((q, None)),
        Err(tantivy::query::QueryParserError::SyntaxError(_)) => {
            let (q, _) = parser.parse_query_lenient(query);
            Ok((q, Some("Part of the search couldn't be read, so only the rest was searched.".into())))
        }
        Err(e) => Err(e.into()),
    }
}

pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    pub fields: Fields,
    writer: Mutex<Option<IndexWriter>>,
}

impl SearchIndex {
    pub fn open(dir: &Path) -> Result<SearchIndex> {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io { context: format!("creating {}", dir.display()), source })?;
        let (schema, fields) = schema();
        let index = if dir.join("meta.json").exists() {
            Index::open_in_dir(dir)?
        } else {
            Index::create_in_dir(dir, schema.clone())?
        };
        if index.schema() != schema {
            return Err(Error::Other("the search index was made by another version: rebuild it (eudora reindex)".into()));
        }
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(SearchIndex { index, reader, fields, writer: Mutex::new(None) })
    }

    /// Opens the writer: `threads` indexing threads sharing `memory` bytes.
    pub fn start_writing(&self, threads: usize, memory: usize) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        if writer.is_none() {
            let threads = threads.clamp(1, 8);
            let memory = memory.max(threads * 20 * 1024 * 1024);
            *writer = Some(self.index.writer_with_num_threads(threads, memory)?);
        }
        Ok(())
    }

    fn document(&self, d: &IndexDoc) -> TantivyDocument {
        let f = &self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_u64(f.id, d.id);
        for (field, text) in [(f.from, d.from), (f.to, d.to), (f.cc, d.cc), (f.bcc, d.bcc), (f.subject, d.subject), (f.body, d.body), (f.attachment, d.attachment_names)] {
            if !text.is_empty() {
                doc.add_text(field, text);
            }
        }
        doc.add_date(f.date, DateTime::from_timestamp_secs(d.date));
        doc.add_u64(f.size, d.size);
        doc.add_u64(f.attachments, d.attachments);
        for folder in d.folders {
            doc.add_u64(f.folder, *folder as u64);
        }
        doc
    }

    pub fn add(&self, d: &IndexDoc) -> Result<()> {
        let doc = self.document(d);
        let writer = self.writer.lock().unwrap();
        let writer = writer.as_ref().ok_or_else(|| Error::Other("the index isn't open for writing".into()))?;
        writer.add_document(doc)?;
        Ok(())
    }

    /// Replaces a message's entry (e.g. it's now in one more folder).
    pub fn replace(&self, d: &IndexDoc) -> Result<()> {
        let doc = self.document(d);
        let writer = self.writer.lock().unwrap();
        let writer = writer.as_ref().ok_or_else(|| Error::Other("the index isn't open for writing".into()))?;
        writer.delete_term(Term::from_field_u64(self.fields.id, d.id));
        writer.add_document(doc)?;
        Ok(())
    }

    /// Removes a message's entry.
    pub fn delete(&self, id: u64) -> Result<()> {
        let writer = self.writer.lock().unwrap();
        let writer = writer.as_ref().ok_or_else(|| Error::Other("the index isn't open for writing".into()))?;
        writer.delete_term(Term::from_field_u64(self.fields.id, id));
        Ok(())
    }

    pub fn delete_all(&self) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let writer = writer.as_mut().ok_or_else(|| Error::Other("the index isn't open for writing".into()))?;
        writer.delete_all_documents()?;
        Ok(())
    }

    /// Makes everything added so far searchable, recording the highest id it covers.
    pub fn commit(&self, max_id: i64) -> Result<()> {
        {
            let mut guard = self.writer.lock().unwrap();
            let writer = guard.as_mut().ok_or_else(|| Error::Other("the index isn't open for writing".into()))?;
            let mut prepared = writer.prepare_commit()?;
            prepared.set_payload(&serde_json::json!({ "max_id": max_id, "version": INDEX_VERSION }).to_string());
            prepared.commit()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    /// Merges every segment into one (Optimize Library): searches get a little quicker and the
    /// space of deleted entries comes back. Returns how many segments there were.
    pub fn merge_all(&self) -> Result<usize> {
        let ids = self.index.searchable_segment_ids()?;
        {
            let mut guard = self.writer.lock().unwrap();
            let writer = guard.as_mut().ok_or_else(|| Error::Other("the index isn't open for writing".into()))?;
            if ids.len() > 1 {
                writer.merge(&ids).wait()?;
            }
            writer.garbage_collect_files().wait()?;
        }
        self.reader.reload()?;
        Ok(ids.len())
    }

    /// Closes the writer, letting background merges finish.
    pub fn finish_writing(&self) -> Result<()> {
        if let Some(writer) = self.writer.lock().unwrap().take() {
            writer.wait_merging_threads()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    /// The highest message id the last commit covered (0 = none).
    pub fn committed_max_id(&self) -> Result<i64> {
        let metas = self.index.load_metas()?;
        Ok(metas
            .payload
            .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
            .and_then(|v| v["max_id"].as_i64())
            .unwrap_or(0))
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    /// Runs a query: AND between words by default, `OR`, `NOT`/`-`, "phrases", `field:word`,
    /// `date:[2001-01-01T00:00:00Z TO 2002-01-01T00:00:00Z]`.
    pub fn search(&self, query: &str, limit: usize, order: SortOrder) -> Result<SearchResult> {
        self.search_filtered(query, limit, order, None, None, None)
    }

    /// `search`, keeping only messages among `ids` (what a header search found).
    pub fn search_within(&self, query: &str, limit: usize, order: SortOrder, ids: &[u64]) -> Result<SearchResult> {
        self.search_filtered(query, limit, order, Some(ids), None, None)
    }

    /// `search`, keeping only messages among `within` (when given) and leaving out those in
    /// `without` (the ones only in mailboxes left out of search: search_scope.rs); `hidden` then
    /// says how many matches were left out. `without` is checked per match against the id column
    /// (a bitmap: a million messages left out cost nothing extra), as is `narrow` (matches it
    /// doesn't keep aren't matches at all, so they aren't counted as left out).
    pub fn search_filtered(&self, query: &str, limit: usize, order: SortOrder, within: Option<&[u64]>, without: Option<std::sync::Arc<IdSet>>, narrow: Option<std::sync::Arc<Narrow>>) -> Result<SearchResult> {
        use tantivy::collector::FilterCollector;
        use tantivy::query::{BooleanQuery, Occur, Query, TermSetQuery};
        let started = Instant::now();
        let f = &self.fields;
        // `word*` inside parentheses or beside OR (the advanced search's "starts with" under
        // Match any): the words the index has that start so
        let query = &self.expand_grouped_stars(query)?;
        let (words, note) = read_query(&parser(&self.index, f), f, query)?;
        let query: Box<dyn Query> = match within {
            Some(ids) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, words),
                (Occur::Must, Box::new(TermSetQuery::new(ids.iter().map(|id| Term::from_field_u64(f.id, *id)))) as Box<dyn Query>),
            ])),
            None => words,
        };
        let searcher = self.reader.searcher();
        let limit = limit.max(1);
        // (every match, then those kept: the difference was left out)
        let keep = move |id: u64| without.as_ref().is_none_or(|set| !set.contains(id));
        let narrowed = move |id: u64| narrow.as_ref().is_none_or(|n| n.keeps(id));
        let (total, all, addresses) = match order {
            SortOrder::Relevance => {
                let (all, (total, top)) = searcher.search(&query, &FilterCollector::new("id".to_string(), narrowed.clone(), (Count, FilterCollector::new("id".to_string(), keep.clone(), (Count, TopDocs::with_limit(limit).order_by_score())))))?;
                (total, all, top.into_iter().map(|(_, a)| a).collect::<Vec<_>>())
            }
            SortOrder::Newest | SortOrder::Oldest => {
                let direction = if order == SortOrder::Newest { Order::Desc } else { Order::Asc };
                let collector = TopDocs::with_limit(limit).order_by_fast_field::<DateTime>("date", direction);
                let (all, (total, top)) = searcher.search(&query, &FilterCollector::new("id".to_string(), narrowed.clone(), (Count, FilterCollector::new("id".to_string(), keep.clone(), (Count, collector)))))?;
                (total, all, top.into_iter().map(|(_, a)| a).collect::<Vec<_>>())
            }
        };
        let mut ids = Vec::with_capacity(addresses.len());
        for address in addresses {
            let column = searcher.segment_reader(address.segment_ord).fast_fields().u64("id")?;
            if let Some(id) = column.first(address.doc_id) {
                ids.push(id);
            }
        }
        Ok(SearchResult { total, ids, took: started.elapsed(), notes: note.into_iter().collect(), suggestions: Vec::new(), hidden: all.saturating_sub(total) })
    }

    /// Rewrites each `word*` / `field:word*` that `split_prefixes` leaves in the query (inside
    /// parentheses, or in a search using OR) as `(field:word1 OR field:word2 …)`: the index's
    /// words starting so, at most `STAR_WORDS` of them a field (Tantivy reads only phrases of two
    /// words or more with a `*`).
    fn expand_grouped_stars(&self, query: &str) -> Result<String> {
        const STAR_WORDS: usize = 200;
        if !query.contains('*') {
            return Ok(query.to_string());
        }
        let top_or = top_pieces(query).iter().any(|p| p == "OR" || p == "||");
        let f = &self.fields;
        let searcher = self.reader.searcher();
        let mut out = String::with_capacity(query.len());
        let (mut quoted, mut depth, mut token) = (false, 0i32, String::new());
        let flush = |token: &mut String, out: &mut String, depth: i32| -> Result<()> {
            let t = std::mem::take(token);
            match (depth > 0 || top_or).then(|| star_word(&t)).flatten() {
                Some((not, field, word)) => {
                    let fields: Vec<(&str, Field)> = match field.as_deref() {
                        Some("from") => vec![("from", f.from)],
                        Some("to") => vec![("to", f.to)],
                        Some("cc") => vec![("cc", f.cc)],
                        Some("bcc") => vec![("bcc", f.bcc)],
                        Some("subject") => vec![("subject", f.subject)],
                        Some("body") => vec![("body", f.body)],
                        Some("attachment") => vec![("attachment", f.attachment)],
                        _ => vec![("subject", f.subject), ("body", f.body), ("from", f.from), ("to", f.to), ("cc", f.cc), ("attachment", f.attachment)],
                    };
                    let mut either = Vec::new();
                    for (name, field) in fields {
                        let mut found = std::collections::BTreeSet::new();
                        for segment in searcher.segment_readers() {
                            let inverted = segment.inverted_index(field)?;
                            let mut stream = inverted.terms().range().ge(word.as_bytes()).into_stream().map_err(|e| Error::Other(format!("reading the search index: {e}")))?;
                            while found.len() < STAR_WORDS && stream.advance() {
                                if !stream.key().starts_with(word.as_bytes()) {
                                    break;
                                }
                                if let Ok(w) = std::str::from_utf8(stream.key()) {
                                    found.insert(w.to_string());
                                }
                            }
                        }
                        either.extend(found.into_iter().filter(|w| w.chars().all(char::is_alphanumeric)).map(|w| format!("{name}:{w}")));
                    }
                    let group = if either.is_empty() { format!("({})", crate::search_marks::nothing()) } else { format!("({})", either.join(" OR ")) };
                    out.push_str(if not { "-" } else { "" });
                    out.push_str(&group);
                }
                None => out.push_str(&t),
            }
            Ok(())
        };
        for c in query.chars() {
            let boundary = !quoted && (c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}'));
            if boundary {
                flush(&mut token, &mut out, depth)?;
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    _ => {}
                }
                out.push(c);
            } else {
                if c == '"' {
                    quoted = !quoted;
                }
                token.push(c);
            }
        }
        flush(&mut token, &mut out, depth)?;
        Ok(out)
    }

    pub fn disk_size(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc<'a>(id: u64, from: &'a str, subject: &'a str, body: &'a str) -> IndexDoc<'a> {
        IndexDoc { id, from, to: "", cc: "", bcc: "", subject, body, attachment_names: "", date: 1_700_000_000 + id as i64, size: 1, attachments: 0, folders: &[] }
    }

    #[test]
    fn the_last_word_and_starred_words_match_the_start_of_words() {
        let (rest, p) = split_prefixes("budget walkerfi");
        assert_eq!(rest, "budget");
        assert_eq!(p, vec![Prefix { not: false, field: None, word: "walkerfi".into(), typed: true }]);
        let (rest, p) = split_prefixes("from:weiss* \"big plan\" -draft* report");
        assert_eq!(rest, "\"big plan\"");
        assert_eq!(p.len(), 3);
        assert!(p[0].field.as_deref() == Some("from") && p[1].not && p[2].typed);
        // left alone: OR, a range, short words, a header, a typed exclusion
        assert!(split_prefixes("apple OR pear").1.is_empty());
        assert!(split_prefixes("date:[2001-01-01T00:00:00Z TO 2002-01-01T00:00:00Z]").1.is_empty());
        assert!(split_prefixes("budget ab").1.is_empty());
        assert!(split_prefixes("importance:high").1.is_empty());
        assert!(split_prefixes("budget -spam").1.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        index.start_writing(1, 50_000_000).unwrap();
        index.add(&doc(1, "Morgan Walker <morgan@walkerfield.example>", "Budget", "the numbers")).unwrap();
        index.add(&doc(2, "Ann <ann@x.com>", "Running late", "see you")).unwrap();
        index.add(&doc(3, "Bob <bob@y.com>", "Lunch", "paul said hi")).unwrap();
        index.commit(3).unwrap();
        index.reload().unwrap();
        let found = |q: &str| {
            let mut ids = index.search(q, 10, SortOrder::Newest).unwrap().ids;
            ids.sort();
            ids
        };
        assert_eq!(found("walkerfi"), vec![1]);
        assert_eq!(found("budget walkerfi"), vec![1]);
        assert_eq!(found("lunch walkerfi"), Vec::<u64>::new());
        assert_eq!(found("from:walkerf*"), vec![1]);
        assert_eq!(found("pau* -from:walkerf*"), vec![3]);
        // the typed word still matches whole, stemmed: "running" is kept as "run"
        assert_eq!(found("running"), vec![2]);
    }
}
