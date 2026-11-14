//! Search words for what isn't in the search index but in the library's own tables: a
//! message's status, priority, labels, and account (classic Eudora's Find Messages had
//! Status, Priority, Label, and Personality criteria; the Advanced Search window builds them):
//!
//! - `is:unread`, `is:read`, `is:replied` (`is:answered`), `is:forwarded`, `is:redirected`,
//!   `is:flagged` (`is:starred`), `is:unflagged`; Outlook's `isread:yes|no`, `isflagged:yes|no`
//! - `priority:highest|high|normal|low|lowest` (or 1 to 5) and `priority:none` (the priority set
//!   by hand, as the Priority column shows it; a header such as `importance:high` is a header search)
//! - `label:Name` (Outlook's `category:Name`), `label:any`, `label:none`
//! - `account:` an account's address, its name, or the name the user gave it
//!
//! Each is worked out as a set of message ids. Used plainly (every term must match, the usual
//! case) it narrows the search as a bitmap checked per match, costing nothing per message; inside
//! parentheses or beside OR it becomes `(id: IN [...])` in the query itself, so
//! `is:unread OR label:Important` means what it says.

use crate::aqs::terms;

/// One of these search words, as read from the query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `flags & bit != 0` (`set`) or `== 0`.
    Flag { bit: i64, set: bool },
    /// The priority set by hand; 0 = none.
    Priority(u8),
    /// A label by name (lowercase); "" = any label.
    Label(String),
    /// An account by address or name (lowercase).
    Account(String),
    /// A header search (`list-id:announce`, `x-mailer:*`) inside parentheses or beside OR: the
    /// header index's matches (a plain one is searched there with the rest, as before).
    Header { name: String, words: Vec<String> },
}

/// A piece of a query: text passed on, or one of these words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    /// `neg` = `-is:unread`; `top` = a plain required term (narrows by bitmap); `open` / `close`
    /// = the parentheses written around it, kept when it's put back into the query.
    Mark { mark: Mark, neg: bool, top: bool, open: usize, close: usize },
}

/// The status words `is:` takes.
pub const STATUS_WORDS: [&str; 7] = ["unread", "read", "replied", "forwarded", "redirected", "flagged", "unflagged"];

/// The search word `key:value` as a mark (and whether it's negated by its value, as
/// `is:unflagged` or `label:none`), or None when it isn't one.
pub fn mark_of(key: &str, value: &str) -> Option<(Mark, bool)> {
    use crate::marks::{FLAGGED, FORWARDED, REDIRECTED, REPLIED, UNREAD};
    let key = key.to_ascii_lowercase();
    let v = value.trim_matches('"').trim().to_lowercase();
    let yes = |v: &str| !matches!(v, "no" | "false" | "0");
    Some(match key.as_str() {
        "is" => match v.as_str() {
            "unread" | "new" => (Mark::Flag { bit: UNREAD, set: true }, false),
            "read" | "seen" => (Mark::Flag { bit: UNREAD, set: false }, false),
            "replied" | "answered" => (Mark::Flag { bit: REPLIED, set: true }, false),
            "forwarded" => (Mark::Flag { bit: FORWARDED, set: true }, false),
            "redirected" => (Mark::Flag { bit: REDIRECTED, set: true }, false),
            "flagged" | "starred" | "todo" => (Mark::Flag { bit: FLAGGED, set: true }, false),
            "unflagged" | "unstarred" => (Mark::Flag { bit: FLAGGED, set: false }, false),
            _ => return None,
        },
        "isread" => (Mark::Flag { bit: UNREAD, set: !yes(&v) }, false),
        "isflagged" => (Mark::Flag { bit: FLAGGED, set: yes(&v) }, false),
        "priority" => (
            Mark::Priority(match v.as_str() {
                "highest" | "1" => 1,
                "high" | "2" => 2,
                "normal" | "3" => 3,
                "low" | "4" => 4,
                "lowest" | "5" => 5,
                "none" | "0" => 0,
                _ => return None,
            }),
            false,
        ),
        "label" | "labels" | "category" | "categories" if !v.is_empty() => match v.as_str() {
            "any" | "*" => (Mark::Label(String::new()), false),
            "none" => (Mark::Label(String::new()), true),
            _ => (Mark::Label(v), false),
        },
        "account" | "personality" if !v.is_empty() => (Mark::Account(v), false),
        _ => return None,
    })
}

/// Splits a query (after Outlook's words are translated) into text and these words.
pub fn split(query: &str) -> Vec<Piece> {
    let words = terms(query);
    let mut out = Vec::new();
    let mut depth: i64 = 0;
    for (i, term) in words.iter().enumerate() {
        let open = term.chars().take_while(|c| *c == '(').count();
        let inner = &term[open..];
        // trailing ')' outside quotes
        let close = if inner.matches('"').count() % 2 == 0 { inner.chars().rev().take_while(|c| *c == ')').count() } else { 0 };
        let core = &inner[..inner.len() - close];
        let (neg, body) = match core.strip_prefix('-') {
            Some(b) => (true, b),
            None => (false, core),
        };
        let beside_or = (i > 0 && words[i - 1] == "OR") || words.get(i + 1).is_some_and(|w| w == "OR");
        let top = depth == 0 && open == 0 && close == 0 && !beside_or;
        let found = body.split_once(':').and_then(|(k, v)| mark_of(k, v)).or_else(|| {
            (!top)
                .then(|| crate::header_index::header_clause(body))
                .flatten()
                .map(|c| (Mark::Header { name: c.name, words: c.words }, false))
        });
        match found {
            Some((mark, inverted)) => out.push(Piece::Mark { mark, neg: neg != inverted, top, open, close }),
            None => out.push(Piece::Text(term.clone())),
        }
        depth = (depth + open as i64 - close as i64).max(0);
    }
    out
}

/// Whether a query uses any of these words (so the library needs to work them out).
pub fn uses_marks(query: &str) -> bool {
    split(query).iter().any(|p| matches!(p, Piece::Mark { .. }))
}

/// The ids in both sorted lists.
pub(crate) fn intersect(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// A search word that matches no message, different each time: an empty set inside an OR
/// group can't be `id: IN [0]`, as Tantivy (0.26) takes an OR of two identical clauses that
/// match nothing, numeric ones, as matching every message.
pub(crate) fn nothing() -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    format!("body:zqxnothing{}zqx", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// `(id: IN [...])` for ids; negated, every message but those, in a group of its own (Tantivy
/// reads `a OR -b` as "a and not b").
pub(crate) fn inline(ids: &[u64], neg: bool) -> String {
    const EVERY: &str = "date:[1970-01-01T00:00:00Z TO 2200-01-01T00:00:00Z]";
    if ids.is_empty() {
        return if neg { format!("({EVERY})") } else { format!("({})", nothing()) };
    }
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" ");
    if neg {
        format!("({EVERY} -id: IN [{list}])")
    } else {
        format!("(id: IN [{list}])")
    }
}

impl crate::library::Library {
    /// The message ids a mark takes in (sorted).
    pub(crate) fn mark_ids(&self, mark: &Mark) -> crate::Result<Vec<u64>> {
        let ids = |sql: &str, value: Option<&str>| -> crate::Result<Vec<u64>> {
            let mut stmt = self.conn.prepare_cached(sql)?;
            let rows = match value {
                Some(v) => stmt.query_map(rusqlite::params![v], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?,
                None => stmt.query_map([], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?,
            };
            let mut out: Vec<u64> = rows.into_iter().map(|i| i as u64).collect();
            out.sort_unstable();
            out.dedup();
            Ok(out)
        };
        match mark {
            Mark::Flag { bit, set } => ids(&format!("SELECT id FROM messages WHERE (flags & {bit}) {} 0", if *set { "!=" } else { "=" }), None),
            Mark::Priority(p) => ids(&format!("SELECT id FROM messages WHERE ((flags >> 8) & 7) = {p}"), None),
            Mark::Label(name) if name.is_empty() => ids("SELECT DISTINCT message FROM categories", None),
            Mark::Label(name) => ids("SELECT message FROM categories WHERE lower(label) = ?1", Some(name)),
            Mark::Header { name, words } => {
                let hx = self.headers.as_ref().ok_or_else(|| crate::Error::Other("searching by header isn't ready yet: the download prepares it (it starts the next time the download runs)".into()))?;
                hx.reload()?;
                let mut out = hx.matching_ids(&[crate::header_index::Clause { name: name.clone(), words: words.clone(), negate: false }])?;
                out.sort_unstable();
                out.dedup();
                Ok(out)
            }
            Mark::Account(who) => ids(
                "SELECT DISTINCT l.message FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
                 WHERE lower(a.address) = ?1 OR lower(a.name) = ?1 OR lower(COALESCE(a.label, '')) = ?1",
                Some(who),
            ),
        }
    }

    /// The query without these words, and how they narrow it (None: they weren't used plainly).
    /// Those inside parentheses or beside OR are put back as `(id: IN [...])`.
    pub(crate) fn resolve_marks(&self, query: &str) -> crate::Result<(String, Option<crate::index::Narrow>)> {
        if !uses_marks(query) {
            return Ok((query.to_string(), None));
        }
        let mut text = Vec::new();
        let mut only: Option<Vec<u64>> = None;
        let mut not: Vec<u64> = Vec::new();
        let mut narrowed = false;
        for piece in split(query) {
            match piece {
                Piece::Text(t) => text.push(t),
                Piece::Mark { mark, neg, top: true, .. } => {
                    narrowed = true;
                    let ids = self.mark_ids(&mark)?;
                    if neg {
                        not.extend(ids);
                    } else {
                        only = Some(match only {
                            Some(have) => intersect(&have, &ids),
                            None => ids,
                        });
                    }
                }
                Piece::Mark { mark, neg, open, close, .. } => {
                    let ids = self.mark_ids(&mark)?;
                    text.push(format!("{}{}{}", "(".repeat(open), inline(&ids, neg), ")".repeat(close)));
                }
            }
        }
        let narrow = narrowed.then(|| crate::index::Narrow {
            only: only.map(crate::index::IdSet::new),
            not: (!not.is_empty()).then(|| crate::index::IdSet::new(not)),
        });
        Ok((text.join(" "), narrow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::{FLAGGED, UNREAD};

    #[test]
    fn the_words_are_read() {
        assert_eq!(mark_of("is", "unread"), Some((Mark::Flag { bit: UNREAD, set: true }, false)));
        assert_eq!(mark_of("IS", "Flagged"), Some((Mark::Flag { bit: FLAGGED, set: true }, false)));
        assert_eq!(mark_of("isread", "no"), Some((Mark::Flag { bit: UNREAD, set: true }, false)));
        assert_eq!(mark_of("priority", "high"), Some((Mark::Priority(2), false)));
        assert_eq!(mark_of("label", "\"To Do!\""), Some((Mark::Label("to do!".into()), false)));
        assert_eq!(mark_of("label", "none"), Some((Mark::Label(String::new()), true)));
        assert_eq!(mark_of("account", "Bob@X.com"), Some((Mark::Account("bob@x.com".into()), false)));
        assert_eq!(mark_of("is", "purple"), None);
        assert_eq!(mark_of("from", "bob"), None);
    }

    #[test]
    fn plain_terms_narrow_and_grouped_ones_go_inline() {
        let p = split("budget is:unread -label:Personal");
        assert_eq!(p[0], Piece::Text("budget".into()));
        assert!(matches!(&p[1], Piece::Mark { top: true, neg: false, .. }));
        assert!(matches!(&p[2], Piece::Mark { top: true, neg: true, mark: Mark::Label(l), .. } if l == "personal"));
        let p = split("from:bob OR is:flagged");
        assert!(matches!(&p[2], Piece::Mark { top: false, .. }));
        let p = split("from:bob OR x-mailer:outlook");
        assert!(matches!(&p[2], Piece::Mark { mark: Mark::Header { name, .. }, top: false, .. } if name == "x-mailer"));
        assert_eq!(split("x-mailer:outlook budget")[0], Piece::Text("x-mailer:outlook".into()), "a plain one stays for the header search");
        let p = split("(is:unread from:ann) budget");
        assert!(matches!(&p[0], Piece::Mark { top: false, open: 1, close: 0, .. }));
        assert_eq!(p[1], Piece::Text("from:ann)".into()));
        assert!(matches!(&p[2], Piece::Text(t) if t == "budget"));
        let p = split("(from:ann OR to:ann) is:replied");
        assert!(matches!(&p[3], Piece::Mark { top: true, .. }), "after the group closes it's plain again");
        assert!(!uses_marks("from:bob importance:high"));
    }

    #[test]
    fn sets_are_intersected_and_written() {
        assert_eq!(intersect(&[1, 3, 5, 9], &[3, 4, 9]), vec![3, 9]);
        assert_eq!(inline(&[4, 7], true), "(date:[1970-01-01T00:00:00Z TO 2200-01-01T00:00:00Z] -id: IN [4 7])");
        assert!(inline(&[], false).starts_with("(body:zqxnothing"));
        assert_ne!(inline(&[], false), inline(&[], false), "each one different");
        assert_eq!(inline(&[], true), "(date:[1970-01-01T00:00:00Z TO 2200-01-01T00:00:00Z])");
    }
}
