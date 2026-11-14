//! Helping with a search that isn't quite right: an unfinished quote or parenthesis, `from:`
//! with nothing after it, a misspelled word like `form:` or `recieved:`, a date or size Eudora
//! can't read. What can safely be repaired is (the search runs on the repaired text and says so
//! in `notes`); for the rest, `suggestions` are corrected searches the user can pick, each with
//! why ("Did you mean from:bob?").

use chrono::NaiveDate;
use serde::Serialize;

use crate::aqs::{SECOND_WORDS, date_range, size_range, terms};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Suggestion {
    /// The whole search, corrected (what to run when it's picked).
    pub query: String,
    /// What it changes, for the user ("Did you mean from:bob?").
    pub why: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Help {
    /// The search to run (repaired; the same text when nothing needed repairing).
    pub fixed: String,
    /// What was repaired or left out.
    pub notes: Vec<String>,
    pub suggestions: Vec<Suggestion>,
}

/// The words before a colon that mean something to Eudora (fields, Outlook's and Gmail's words).
const KEYWORDS: [&str; 32] = [
    "from", "to", "cc", "bcc", "subject", "body", "attachment", "attachments", "hasattachment", "hasattachments", "received", "sent", "date", "size",
    "messagesize", "participants", "recipient", "recipients", "sender", "folder", "in", "has", "title", "filename",
    // the library's own (search_marks.rs)
    "is", "isread", "isflagged", "priority", "label", "category", "account", "personality",
];

/// Headers often searched, so `name:` isn't taken for a misspelled keyword.
const HEADERS: [&str; 16] = [
    "list-id", "reply-to", "return-path", "message-id", "importance", "x-priority", "user-agent", "delivered-to", "content-type", "in-reply-to",
    "references", "organization", "thread-topic", "authentication-results", "mailer", "precedence",
];

const DATE_HELP: &str = "today, yesterday, this week, last week, this month, last month, this year, a year (2024), a day (3/15/2024 or 2024-03-15), a range (1/1/2024..3/31/2024), or >, < a day";
const SIZE_HELP: &str = "tiny, small, medium, large, very large, huge, enormous, or a size like >5MB, <100KB, 1MB..2MB";

/// Edits between two words, a swap of neighboring letters counting as one (`form` → `from`).
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// The keyword `key` most likely stands for, when it's a slip of the fingers (`form`, `recieved`).
fn meant(key: &str) -> Option<&'static str> {
    if KEYWORDS.contains(&key) || HEADERS.contains(&key) || key.starts_with("x-") || key.len() < 3 {
        return None;
    }
    let allowed = if key.len() <= 4 { 1 } else { 2 };
    KEYWORDS.iter().filter(|k| k.len() >= 3).map(|k| (distance(key, k), *k)).filter(|(d, _)| *d <= allowed).min().map(|(_, k)| k)
}

fn replace_term(all: &[String], i: usize, with: &str) -> String {
    let mut out: Vec<String> = all.to_vec();
    out[i] = with.to_string();
    out.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Looks a search over; `today` is the local date (for checking dates).
pub fn check(query: &str, today: NaiveDate) -> Help {
    let mut help = Help::default();
    let mut text = query.trim().to_string();

    // 1. an unfinished quote: closed
    if text.matches('"').count() % 2 == 1 {
        let without = {
            let at = text.rfind('"').unwrap();
            format!("{}{}", &text[..at], &text[at + 1..])
        };
        text.push('"');
        help.notes.push("Your quote wasn't closed, so it was closed at the end of the search.".into());
        help.suggestions.push(Suggestion { query: without.split_whitespace().collect::<Vec<_>>().join(" "), why: "Search without the quote mark".into() });
    }
    // 2. parentheses: an extra ")" goes, a missing one is added (outside quotes)
    let (mut depth, mut quoted, mut cleaned) = (0i32, false, String::new());
    let mut dropped = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth == 0 => {
                dropped = true;
                continue;
            }
            ')' if !quoted => depth -= 1,
            _ => {}
        }
        cleaned.push(c);
    }
    if dropped {
        help.notes.push("A “)” with no “(” before it was left out.".into());
    }
    if depth > 0 {
        cleaned.push_str(&")".repeat(depth as usize));
        help.notes.push("A “(” wasn't closed, so it was closed at the end.".into());
    }
    text = cleaned;

    // 3. word by word
    let mut words = terms(&text);
    let mut i = 0;
    while i < words.len() {
        let term = words[i].clone();
        let bare = term.trim_start_matches('-');
        // an unfinished range ("date:[2024-01-01 TO"): left out
        if (bare.contains('[') || bare.contains('{')) && !words[i..].iter().any(|w| w.contains(']') || w.contains('}')) {
            let end = words.len();
            words.drain(i..end);
            help.notes.push(format!("The range starting at “{term}” wasn't finished, so it was left out. Dates are easiest as received:1/1/2024..3/31/2024."));
            break;
        }
        // AND / OR / NOT with nothing to join
        if matches!(term.as_str(), "AND" | "OR" | "NOT") && (i + 1 == words.len() || (i == 0 && term != "NOT")) {
            help.notes.push(format!("“{term}” had nothing {} it, so it was left out.", if i == 0 { "before" } else { "after" }));
            words.remove(i);
            continue;
        }
        let Some((key, value)) = bare.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.to_ascii_lowercase();
        if !key.starts_with(|c: char| c.is_ascii_alphabetic()) {
            i += 1;
            continue;
        }
        // "from:" with nothing after it
        if value.trim_matches('"').is_empty() {
            let example = match key.as_str() {
                "from" | "to" | "cc" | "bcc" | "sender" | "recipient" | "recipients" | "participants" => format!("{key}:name or {key}:someone@example.com"),
                "received" | "sent" | "date" => format!("{key}:last week"),
                "size" | "messagesize" => format!("{key}:large"),
                "hasattachment" | "hasattachments" => format!("{key}:yes"),
                _ => format!("{key}:something"),
            };
            help.notes.push(format!("“{key}:” had nothing after it, so it was left out. For example: {example}."));
            words.remove(i);
            continue;
        }
        // a slip of the fingers: form: → from:
        if let Some(right) = meant(&key) {
            let neg = if term.starts_with('-') { "-" } else { "" };
            let corrected = format!("{neg}{right}:{value}");
            help.suggestions.push(Suggestion { query: replace_term(&words, i, &corrected), why: format!("Did you mean {corrected}? (“{key}:” searches a header of that name)") });
        }
        let mut value = value.trim_matches('"').to_string();
        let two_words = words.get(i + 1).is_some_and(|w| SECOND_WORDS.contains(&w.to_ascii_lowercase().as_str())) && matches!(value.to_ascii_lowercase().as_str(), "this" | "last" | "very");
        if two_words {
            value = format!("{value} {}", words[i + 1]);
        }
        match key.as_str() {
            "received" | "sent" if date_range(&value, today).is_none() => {
                help.suggestions.push(Suggestion { query: replace_term(&words, i, &format!("{key}:last week")), why: format!("“{key}:” takes {DATE_HELP}") });
            }
            "size" | "messagesize" if size_range(&value).is_none() => {
                help.suggestions.push(Suggestion { query: replace_term(&words, i, &format!("{key}:large")), why: format!("“{key}:” takes {SIZE_HELP}") });
            }
            "hasattachment" | "hasattachments" if !matches!(value.to_ascii_lowercase().as_str(), "yes" | "no" | "true" | "false" | "1" | "0") => {
                help.suggestions.push(Suggestion { query: replace_term(&words, i, "hasattachment:yes"), why: "With attachments".into() });
                help.suggestions.push(Suggestion { query: replace_term(&words, i, "hasattachment:no"), why: "Without attachments".into() });
            }
            _ => {}
        }
        i += if two_words { 2 } else { 1 };
    }
    help.fixed = words.join(" ");
    help
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(q: &str) -> Help {
        check(q, NaiveDate::from_ymd_opt(2026, 10, 9).unwrap())
    }

    #[test]
    fn good_searches_are_left_alone() {
        for q in ["from:bob to:ann", "subject:\"budget 2026\" -spam", "received:last week hasattachment:yes", "x-mailer:outlook", "(a OR b) c", "date:[2001-01-01T00:00:00Z TO 2002-01-01T00:00:00Z]"] {
            let h = c(q);
            assert_eq!((h.fixed.as_str(), h.notes.len(), h.suggestions.len()), (q, 0, 0), "{q}");
        }
    }

    #[test]
    fn what_can_be_repaired_is() {
        let h = c("note \"hello");
        assert_eq!(h.fixed, "note \"hello\"");
        assert_eq!(h.suggestions[0].query, "note hello");
        assert_eq!(c("(budget OR plan").fixed, "(budget OR plan)");
        assert_eq!(c("budget)").fixed, "budget");
        let h = c("invoice from:");
        assert_eq!(h.fixed, "invoice");
        assert!(h.notes[0].contains("from:someone@example.com"));
        assert_eq!(c("budget AND").fixed, "budget");
        assert_eq!(c("date:[2024-01-01 TO").fixed, "");
    }

    #[test]
    fn slips_and_bad_values_get_suggestions() {
        let h = c("form:bob invoice");
        assert_eq!(h.suggestions[0].query, "from:bob invoice");
        assert!(h.suggestions[0].why.starts_with("Did you mean from:bob?"));
        assert_eq!(c("recieved:2024").suggestions[0].query, "received:2024");
        assert_eq!(c("-sendr:spam").suggestions[0].query, "-sender:spam");
        assert!(c("received:someday").suggestions[0].why.contains("last week"));
        assert_eq!(c("received:last week").suggestions.len(), 0);
        assert_eq!(c("size:big").suggestions[0].query, "size:large");
        assert_eq!(c("hasattachment:maybe").suggestions.len(), 2);
        // real headers and short words aren't "corrected"
        assert!(c("list-id:announce").suggestions.is_empty());
        assert!(c("re:hello").suggestions.is_empty());
    }
}
