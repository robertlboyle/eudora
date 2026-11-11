//! Outlook's search words (its "Advanced Query Syntax"), so a search typed the way Outlook
//! takes it works here too. They're turned into the index's own terms before searching:
//!
//! - `hasattachment:yes` / `:no` (also `hasattachments:`), `attachment:report` / `attachments:`
//! - `received:` / `sent:` / `date:` with `today`, `yesterday`, `this week`, `last week`,
//!   `this month`, `last month`, `this year`, `last year`, a year (`2021`), a day (`3/15/2024`,
//!   `2024-03-15`), a range (`1/1/2024..3/31/2024`), or `>`, `>=`, `<`, `<=` a day
//! - `size:` / `messagesize:` with Outlook's names (`tiny` … `enormous`), `>5MB`, `<100KB`,
//!   `1MB..2MB`
//! - `participants:bob` (From, To or Cc), `recipient:bob` / `recipients:bob` (To, Cc or Bcc),
//!   `sender:bob` (From, as Outlook means it: not the Sender header)
//! - `subject:`, `from:`, `to:`, `cc:`, `bcc:`, `body:` already are the index's own;
//!   `AND`, `OR`, `NOT`, `-word`, "phrases" too. `folder:` / `in:` are resolved by the library
//!   (they need its folder names). Any other `name:value` is a header (`header_index`), so
//!   `importance:high` finds the Importance header.

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};

/// Terms of a search: whitespace-separated, a quoted part counting as one.
pub(crate) fn terms(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut quoted = false;
        while i < chars.len() && (quoted || !chars[i].is_whitespace()) {
            if chars[i] == '"' {
                quoted = !quoted;
            }
            i += 1;
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

/// Two-word date and size values Outlook takes without quotes ("received:last week").
pub(crate) const SECOND_WORDS: [&str; 5] = ["week", "month", "year", "large", "days"];

/// Rewrites Outlook's search words into the index's terms; `today` is the local date.
pub fn translate(query: &str, today: NaiveDate) -> String {
    let words = terms(query);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let term = &words[i];
        i += 1;
        let (neg, body) = match term.strip_prefix('-') {
            Some(b) if b.contains(':') => ("-", b),
            _ => ("", term.as_str()),
        };
        let Some((key, value)) = body.split_once(':') else {
            out.push(term.clone());
            continue;
        };
        let key = key.to_ascii_lowercase();
        let mut value = value.trim_matches('"').to_string();
        // "received:last week", "size:very large": the next word belongs to it
        let two_words = matches!(key.as_str(), "received" | "sent" | "date" | "size" | "messagesize")
            && matches!(value.to_ascii_lowercase().as_str(), "this" | "last" | "very")
            && words.get(i).is_some_and(|w| SECOND_WORDS.contains(&w.to_ascii_lowercase().as_str()));
        if two_words {
            value = format!("{value} {}", words[i]);
            i += 1;
        }
        let rewritten = match key.as_str() {
            // (Gmail's has:attachment too)
            "hasattachment" | "hasattachments" | "has" if key != "has" || value.to_ascii_lowercase().starts_with("attachment") => {
                let yes = key == "has" || !matches!(value.to_ascii_lowercase().as_str(), "no" | "false" | "0");
                let with = yes == neg.is_empty();
                Some(format!("{}attachments:[1 TO 1000000]", if with { "" } else { "-" }))
            }
            "attachment" | "attachments" | "attachmentname" | "filename" => Some(format!("{neg}attachment:{}", quote(&value))),
            "received" | "sent" | "date" => date_range(&value, today).map(|(a, b)| format!("{neg}date:[{a} TO {b}}}")),
            "size" | "messagesize" => size_range(&value).map(|(a, b)| format!("{neg}size:[{a} TO {b}]")),
            "participants" => Some(format!("{neg}(from:{v} OR to:{v} OR cc:{v})", v = quote(&value))),
            "recipients" | "recipient" => Some(format!("{neg}(to:{v} OR cc:{v} OR bcc:{v})", v = quote(&value))),
            // (Outlook's sender: is who it's from; the Sender header is `header:` territory)
            "sender" | "author" => Some(format!("{neg}from:{}", quote(&value))),
            "title" => Some(format!("{neg}subject:{}", quote(&value))),
            _ => None,
        };
        out.push(rewritten.unwrap_or_else(|| term.clone()));
    }
    // Outlook's AND / NOT: every word is required already, and NOT x is -x here
    let mut joined: Vec<String> = Vec::new();
    let mut not = false;
    for term in out {
        match term.as_str() {
            "AND" => continue,
            "NOT" => not = !not,
            _ if not => {
                not = false;
                joined.push(match term.strip_prefix('-') {
                    Some(positive) => positive.to_string(),
                    None => format!("-{term}"),
                });
            }
            _ => joined.push(term),
        }
    }
    joined.join(" ")
}

fn quote(value: &str) -> String {
    if value.contains(char::is_whitespace) { format!("\"{value}\"") } else { value.to_string() }
}

/// A local day's start as the index's dates (UTC, RFC 3339).
fn day_start(d: NaiveDate) -> String {
    let local = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().unwrap_or_else(|| Local.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()));
    local.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_day(text: &str) -> Option<NaiveDate> {
    let t = text.trim();
    for f in ["%Y-%m-%d", "%m/%d/%Y", "%m/%d/%y", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(t, f) {
            return Some(d);
        }
    }
    None
}

/// A date value as [from, to) local days, in the index's terms.
pub(crate) fn date_range(value: &str, today: NaiveDate) -> Option<(String, String)> {
    let v = value.trim().to_ascii_lowercase();
    let week_start = today - Duration::days(today.weekday().num_days_from_sunday() as i64);
    let month_start = today.with_day(1)?;
    let (a, b) = match v.as_str() {
        "today" => (today, today + Duration::days(1)),
        "yesterday" => (today - Duration::days(1), today),
        "this week" => (week_start, week_start + Duration::days(7)),
        "last week" => (week_start - Duration::days(7), week_start),
        "this month" => (month_start, add_months(month_start, 1)?),
        "last month" => (add_months(month_start, -1)?, month_start),
        "this year" => (NaiveDate::from_ymd_opt(today.year(), 1, 1)?, NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)?),
        "last year" => (NaiveDate::from_ymd_opt(today.year() - 1, 1, 1)?, NaiveDate::from_ymd_opt(today.year(), 1, 1)?),
        _ => {
            if let Some((x, y)) = v.split_once("..") {
                (parse_day(x)?, parse_day(y)? + Duration::days(1))
            } else if let Some(x) = v.strip_prefix(">=") {
                (parse_day(x)?, NaiveDate::from_ymd_opt(2200, 1, 1)?)
            } else if let Some(x) = v.strip_prefix('>') {
                (parse_day(x)? + Duration::days(1), NaiveDate::from_ymd_opt(2200, 1, 1)?)
            } else if let Some(x) = v.strip_prefix("<=") {
                (NaiveDate::from_ymd_opt(1970, 1, 1)?, parse_day(x)? + Duration::days(1))
            } else if let Some(x) = v.strip_prefix('<') {
                (NaiveDate::from_ymd_opt(1970, 1, 1)?, parse_day(x)?)
            } else if v.len() == 4 && v.chars().all(|c| c.is_ascii_digit()) {
                let y: i32 = v.parse().ok()?;
                (NaiveDate::from_ymd_opt(y, 1, 1)?, NaiveDate::from_ymd_opt(y + 1, 1, 1)?)
            } else {
                let d = parse_day(&v)?;
                (d, d + Duration::days(1))
            }
        }
    };
    Some((day_start(a), day_start(b)))
}

fn add_months(d: NaiveDate, months: i32) -> Option<NaiveDate> {
    let total = d.year() * 12 + d.month0() as i32 + months;
    NaiveDate::from_ymd_opt(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1)
}

fn bytes(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase().replace(' ', "");
    let (number, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len()));
    let n: f64 = number.parse().ok()?;
    let scale = match unit {
        "" | "b" | "bytes" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * scale) as u64)
}

/// A size value as an inclusive byte range (Outlook's names: tiny < 10 KB, small 10-25 KB,
/// medium 25-100 KB, large 100-500 KB, very large 500 KB-1 MB, huge 1-5 MB, enormous > 5 MB).
pub(crate) fn size_range(value: &str) -> Option<(u64, u64)> {
    const K: u64 = 1024;
    const MAX: u64 = 1 << 40;
    let v = value.trim().to_ascii_lowercase();
    Some(match v.as_str() {
        "tiny" => (0, 10 * K - 1),
        "small" => (10 * K, 25 * K - 1),
        "medium" => (25 * K, 100 * K - 1),
        "large" => (100 * K, 500 * K - 1),
        "very large" | "verylarge" => (500 * K, K * K - 1),
        "huge" => (K * K, 5 * K * K - 1),
        "enormous" => (5 * K * K, MAX),
        _ => {
            if let Some((a, b)) = v.split_once("..") {
                (bytes(a)?, bytes(b)?)
            } else if let Some(x) = v.strip_prefix(">=") {
                (bytes(x)?, MAX)
            } else if let Some(x) = v.strip_prefix('>') {
                (bytes(x)? + 1, MAX)
            } else if let Some(x) = v.strip_prefix("<=") {
                (0, bytes(x)?)
            } else if let Some(x) = v.strip_prefix('<') {
                (0, bytes(x)?.saturating_sub(1))
            } else {
                return None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(q: &str) -> String {
        translate(q, NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()) // a Friday
    }

    #[test]
    fn outlook_words_become_index_terms() {
        assert_eq!(t("invoice hasattachment:yes"), "invoice attachments:[1 TO 1000000]");
        assert_eq!(t("hasattachments:no"), "-attachments:[1 TO 1000000]");
        assert_eq!(t("subject:\"budget 2026\" from:bob"), "subject:\"budget 2026\" from:bob");
        assert_eq!(t("attachments:report"), "attachment:report");
        assert_eq!(t("participants:ann"), "(from:ann OR to:ann OR cc:ann)");
        assert_eq!(t("recipient:bob@x.com"), "(to:bob@x.com OR cc:bob@x.com OR bcc:bob@x.com)");
        assert_eq!(t("sender:\"Ann Smith\" -sender:spam"), "from:\"Ann Smith\" -from:spam");
        assert_eq!(t("from:ann to:bob"), "from:ann to:bob", "the index's own fields stay");
        assert_eq!(t("recipients:\"Ann Smith\""), "(to:\"Ann Smith\" OR cc:\"Ann Smith\" OR bcc:\"Ann Smith\")");
        assert_eq!(t("size:large"), "size:[102400 TO 511999]");
        assert_eq!(t("messagesize:>5MB"), format!("size:[5242881 TO {}]", 1u64 << 40));
        assert_eq!(t("size:very large"), "size:[512000 TO 1048575]");
        assert_eq!(t("importance:high OR spam"), "importance:high OR spam");
        assert_eq!(t("budget AND NOT draft"), "budget -draft");
        assert_eq!(t("NOT hasattachment:yes"), "attachments:[1 TO 1000000]".replace("attachments", "-attachments"));
    }

    #[test]
    fn dates_are_local_days() {
        let day = |d: &str| day_start(NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap());
        assert_eq!(t("received:today"), format!("date:[{} TO {}}}", day("2026-10-09"), day("2026-10-10")));
        assert_eq!(t("received:last week"), format!("date:[{} TO {}}}", day("2026-09-27"), day("2026-10-04")));
        assert_eq!(t("sent:this month"), format!("date:[{} TO {}}}", day("2026-10-01"), day("2026-11-01")));
        assert_eq!(t("received:2021"), format!("date:[{} TO {}}}", day("2021-01-01"), day("2022-01-01")));
        assert_eq!(t("received:3/15/2024..3/20/2024"), format!("date:[{} TO {}}}", day("2024-03-15"), day("2024-03-21")));
        assert_eq!(t("received:>=2024-01-01"), format!("date:[{} TO {}}}", day("2024-01-01"), day("2200-01-01")));
        assert_eq!(t("received:someday"), "received:someday", "not a date: left alone");
    }
}
