//! Calendar files (.ics, iCalendar, RFC 5545) opened in Eudora: each event's title, times,
//! place, people, and notes, to show and to add to one of the user's calendars.
//!
//! Times: UTC ("…Z"), all day (VALUE=DATE), or local to a time zone. A file normally describes
//! its zones (VTIMEZONE: standard and daylight offsets with their yearly rules), which are read
//! here; a zone only named (TZID=America/New_York, "Eastern Standard Time") is looked up in a
//! short table of common ones; anything else, and times with no zone, are this computer's local
//! time (said in the event's `note`). No time-zone library: the rules are a few lines.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use serde::Serialize;

/// An event read from a calendar file.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct IcsEvent {
    pub uid: String,
    pub summary: String,
    /// Unix seconds (all day: midnight UTC of the first day, and after the last).
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub location: String,
    pub description: String,
    /// (name, address)
    pub organizer: Option<(String, String)>,
    pub attendees: Vec<(String, String)>,
    /// It repeats (the rule as written): only the first is added.
    pub repeats: Option<String>,
    /// Something to tell the user about how it was read (a time zone guessed).
    pub note: Option<String>,
    /// Which occurrence of a repeating event this one is (RECURRENCE-ID, as written).
    pub recurrence_id: Option<String>,
    /// STATUS (CONFIRMED, TENTATIVE, CANCELLED), upper case.
    pub status: Option<String>,
    /// TRANSP:TRANSPARENT: shown as free.
    pub free: bool,
    /// CLASS:PRIVATE or CONFIDENTIAL.
    pub private: bool,
    /// Each attendee's answer (PARTSTAT, upper case; "" when none), in the order of `attendees`.
    pub answers: Vec<String>,
    /// SEQUENCE.
    pub sequence: i64,
}

/// What the file is for (METHOD: PUBLISH, REQUEST = an invitation, CANCEL…), and its events.
#[derive(Clone, Debug, Default, Serialize)]
pub struct IcsFile {
    pub method: Option<String>,
    pub events: Vec<IcsEvent>,
}

/// The largest calendar file read (a bigger one isn't a calendar file).
pub const ICS_MAX: u64 = 20 * 1024 * 1024;

/// One line: its name, parameters (upper-case names), and value.
#[derive(Debug)]
struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Line {
    fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Lines unfolded (a line starting with a space or tab goes on the one before) and split.
fn lines(text: &str) -> Vec<Line> {
    let mut joined: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match raw.chars().next() {
            Some(' ' | '\t') if !joined.is_empty() => joined.last_mut().unwrap().push_str(&raw[1..]),
            _ => joined.push(raw.to_string()),
        }
    }
    joined
        .into_iter()
        .filter_map(|l| {
            // the value starts at the first ':' outside quotes
            let mut quoted = false;
            let colon = l.char_indices().find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                c == ':' && !quoted
            })?.0;
            let (head, value) = (&l[..colon], &l[colon + 1..]);
            let mut parts = head.split(';');
            let name = parts.next()?.trim().to_ascii_uppercase();
            let params = parts
                .filter_map(|p| p.split_once('=').map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().trim_matches('"').to_string())))
                .collect();
            Some(Line { name, params, value: value.to_string() })
        })
        .collect()
}

/// A text value with its escapes undone (\n, \, \; \\).
fn text_of(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    crate::text_repair::clean(out.trim(), 100_000)
}

/// A person (ORGANIZER / ATTENDEE): their name (CN) and address (mailto:).
fn person(l: &Line) -> (String, String) {
    let address = l.value.trim();
    let address = address.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map(|_| &address[7..]).unwrap_or(address);
    (crate::text_repair::clean(l.param("CN").unwrap_or(""), 200), crate::text_repair::clean(address, 320))
}

/// A yearly change of a zone's offset: in `month`, the `nth` `weekday` (0 = last), at `time`
/// local (before the change), to `to` minutes from UTC; `from` before it.
#[derive(Clone, Debug, PartialEq)]
struct Change {
    month: u32,
    nth: u32,
    weekday: Weekday,
    time: NaiveTime,
    from: i32,
    to: i32,
}

/// A time zone: its standard offset, and when it has one its daylight change and back.
#[derive(Clone, Debug, PartialEq)]
struct Zone {
    standard: i32,
    summer: Option<(Change, Change)>,
}

impl Zone {
    /// A local time in the zone as Unix seconds.
    fn utc(&self, local: NaiveDateTime) -> i64 {
        let at = |off: i32| local.and_utc().timestamp() - off as i64 * 60;
        let Some((on, off)) = &self.summer else { return at(self.standard) };
        let when = |c: &Change| nth_weekday(local.year(), c.month, c.weekday, c.nth).and_time(c.time).and_utc().timestamp() - c.from as i64 * 60;
        let (starts, ends) = (when(on), when(off));
        let summer_utc = at(on.to);
        let summer = if starts < ends { summer_utc >= starts && summer_utc < ends } else { summer_utc >= starts || summer_utc < ends };
        if summer { summer_utc } else { at(self.standard) }
    }
}

/// The `n`th `weekday` of a month (n from 1), or the last when `n` is 0.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, n: u32) -> NaiveDate {
    let month = month.clamp(1, 12);
    if n == 0 {
        let mut d = NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap_or_else(|| NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()).pred_opt().unwrap();
        while d.weekday() != weekday {
            d = d.pred_opt().unwrap();
        }
        return d;
    }
    let mut d = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    while d.weekday() != weekday {
        d = d.succ_opt().unwrap();
    }
    let d2 = d + Duration::weeks(n.min(5) as i64 - 1);
    if d2.month() == month { d2 } else { d2 - Duration::weeks(1) }
}

fn weekday_of(code: &str) -> Option<Weekday> {
    Some(match code {
        "SU" => Weekday::Sun,
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        _ => return None,
    })
}

/// "+0100", "-0500", "-04:00" as minutes.
fn offset_of(value: &str) -> Option<i32> {
    let v: String = value.trim().chars().filter(|c| *c != ':').collect();
    let (sign, digits) = match v.chars().next()? {
        '-' => (-1, &v[1..]),
        '+' => (1, &v[1..]),
        _ => (1, v.as_str()),
    };
    let h: i32 = digits.get(..2)?.parse().ok()?;
    let m: i32 = digits.get(2..4).and_then(|m| m.parse().ok()).unwrap_or(0);
    Some(sign * (h * 60 + m))
}

/// A zone's STANDARD or DAYLIGHT part as a yearly change (its RRULE, else its own date).
fn change_of(part: &[Line]) -> Option<Change> {
    let get = |n: &str| part.iter().find(|l| l.name == n).map(|l| l.value.trim().to_string());
    let from = offset_of(&get("TZOFFSETFROM")?)?;
    let to = offset_of(&get("TZOFFSETTO")?)?;
    let start = get("DTSTART").and_then(|v| NaiveDateTime::parse_from_str(&v, "%Y%m%dT%H%M%S").ok())?;
    let (mut month, mut nth, mut weekday) = (start.month(), (start.day() - 1) / 7 + 1, start.weekday());
    if let Some(rule) = get("RRULE") {
        for item in rule.split(';') {
            match item.split_once('=') {
                Some(("BYMONTH", m)) => month = m.parse().unwrap_or(month),
                Some(("BYDAY", d)) => {
                    let d = d.split(',').next().unwrap_or(d);
                    let (n, code) = d.split_at(d.len().saturating_sub(2));
                    if let Some(w) = weekday_of(code) {
                        weekday = w;
                        nth = match n.parse::<i32>() {
                            Ok(n) if n < 0 => 0,
                            Ok(n) => n as u32,
                            Err(_) => nth,
                        };
                    }
                }
                _ => {}
            }
        }
    }
    Some(Change { month, nth, weekday, time: start.time(), from, to })
}

/// The zones a file describes (VTIMEZONE), by TZID.
fn zones_in(all: &[Line]) -> Vec<(String, Zone)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < all.len() {
        if all[i].name == "BEGIN" && all[i].value.eq_ignore_ascii_case("VTIMEZONE") {
            let end = all[i..].iter().position(|l| l.name == "END" && l.value.eq_ignore_ascii_case("VTIMEZONE")).map(|p| i + p).unwrap_or(all.len());
            let block = &all[i..end];
            let id = block.iter().find(|l| l.name == "TZID").map(|l| l.value.trim().to_string()).unwrap_or_default();
            let part = |kind: &str| -> Option<Change> {
                let s = block.iter().position(|l| l.name == "BEGIN" && l.value.eq_ignore_ascii_case(kind))?;
                let e = block[s..].iter().position(|l| l.name == "END" && l.value.eq_ignore_ascii_case(kind)).map(|p| s + p)?;
                change_of(&block[s + 1..e])
            };
            let standard = part("STANDARD");
            let daylight = part("DAYLIGHT");
            let zone = match (standard, daylight) {
                (Some(s), Some(d)) => Some(Zone { standard: s.to, summer: Some((d, s)) }),
                (Some(s), None) => Some(Zone { standard: s.to, summer: None }),
                (None, Some(d)) => Some(Zone { standard: d.to, summer: None }),
                (None, None) => None,
            };
            if let Some(z) = zone {
                out.push((id, z));
            }
            i = end;
        }
        i += 1;
    }
    out
}

/// Common zones by name (IANA and Windows' names): standard offset and daylight rule
/// ('U' the US's, 'E' Europe's, 'A' southeast Australia's, ' ' none).
const NAMED: &[(&str, i32, char)] = &[
    ("America/New_York", -300, 'U'), ("US/Eastern", -300, 'U'), ("Eastern Standard Time", -300, 'U'), ("America/Detroit", -300, 'U'), ("America/Toronto", -300, 'U'),
    ("America/Chicago", -360, 'U'), ("US/Central", -360, 'U'), ("Central Standard Time", -360, 'U'),
    ("America/Denver", -420, 'U'), ("US/Mountain", -420, 'U'), ("Mountain Standard Time", -420, 'U'),
    ("America/Phoenix", -420, ' '), ("US Mountain Standard Time", -420, ' '),
    ("America/Los_Angeles", -480, 'U'), ("US/Pacific", -480, 'U'), ("Pacific Standard Time", -480, 'U'), ("America/Vancouver", -480, 'U'),
    ("America/Anchorage", -540, 'U'), ("Alaskan Standard Time", -540, 'U'), ("Pacific/Honolulu", -600, ' '), ("Hawaiian Standard Time", -600, ' '),
    ("America/Puerto_Rico", -240, ' '), ("America/Halifax", -240, 'U'),
    ("UTC", 0, ' '), ("Etc/UTC", 0, ' '), ("GMT", 0, ' '), ("Europe/London", 0, 'E'), ("GMT Standard Time", 0, 'E'), ("Europe/Dublin", 0, 'E'), ("Europe/Lisbon", 0, 'E'),
    ("Europe/Paris", 60, 'E'), ("Europe/Berlin", 60, 'E'), ("Europe/Rome", 60, 'E'), ("Europe/Madrid", 60, 'E'), ("Europe/Amsterdam", 60, 'E'), ("Europe/Brussels", 60, 'E'),
    ("Europe/Zurich", 60, 'E'), ("Europe/Vienna", 60, 'E'), ("Europe/Stockholm", 60, 'E'), ("Europe/Copenhagen", 60, 'E'), ("Europe/Oslo", 60, 'E'), ("Europe/Warsaw", 60, 'E'),
    ("W. Europe Standard Time", 60, 'E'), ("Romance Standard Time", 60, 'E'), ("Central Europe Standard Time", 60, 'E'),
    ("Europe/Athens", 120, 'E'), ("Europe/Helsinki", 120, 'E'), ("GTB Standard Time", 120, 'E'), ("FLE Standard Time", 120, 'E'),
    ("Asia/Jerusalem", 120, ' '), ("Asia/Dubai", 240, ' '), ("Asia/Kolkata", 330, ' '), ("India Standard Time", 330, ' '), ("Asia/Singapore", 480, ' '),
    ("Asia/Shanghai", 480, ' '), ("Asia/Hong_Kong", 480, ' '), ("Asia/Tokyo", 540, ' '), ("Tokyo Standard Time", 540, ' '),
    ("Australia/Sydney", 600, 'A'), ("Australia/Melbourne", 600, 'A'), ("AUS Eastern Standard Time", 600, 'A'), ("Australia/Brisbane", 600, ' '),
];

fn named_zone(id: &str) -> Option<Zone> {
    let (_, standard, rule) = NAMED.iter().find(|(n, ..)| n.eq_ignore_ascii_case(id.trim()))?;
    let two = NaiveTime::from_hms_opt(2, 0, 0).unwrap();
    let change = |month, nth, time, from, to| Change { month, nth, weekday: Weekday::Sun, time, from, to };
    let summer = match rule {
        'U' => Some((change(3, 2, two, *standard, standard + 60), change(11, 1, two, standard + 60, *standard))),
        // (1:00 UTC: local times differ by zone)
        'E' => {
            let at = |off: i32| NaiveTime::from_hms_opt(((60 + off).rem_euclid(1440) / 60) as u32, 0, 0).unwrap();
            Some((change(3, 0, at(*standard), *standard, standard + 60), change(10, 0, at(standard + 60), standard + 60, *standard)))
        }
        'A' => Some((change(10, 1, two, *standard, standard + 60), change(4, 1, NaiveTime::from_hms_opt(3, 0, 0).unwrap(), standard + 60, *standard))),
        _ => None,
    };
    Some(Zone { standard: *standard, summer })
}

/// A DTSTART / DTEND as (Unix seconds, all day, a note when the zone was a guess).
fn time_of(l: &Line, zones: &[(String, Zone)]) -> Option<(i64, bool, Option<String>)> {
    let v = l.value.trim();
    if l.param("VALUE").is_some_and(|x| x.eq_ignore_ascii_case("DATE")) || (v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit())) {
        let d = NaiveDate::parse_from_str(v.get(..8)?, "%Y%m%d").ok()?;
        return Some((d.and_hms_opt(0, 0, 0)?.and_utc().timestamp(), true, None));
    }
    let local = NaiveDateTime::parse_from_str(v.trim_end_matches(['Z', 'z']), "%Y%m%dT%H%M%S").ok()?;
    if v.ends_with(['Z', 'z']) {
        return Some((local.and_utc().timestamp(), false, None));
    }
    let here = || chrono::Local.from_local_datetime(&local).earliest().map(|t| t.timestamp()).unwrap_or_else(|| local.and_utc().timestamp());
    match l.param("TZID") {
        Some(id) => {
            let id = id.trim_start_matches('/');
            if let Some((_, z)) = zones.iter().find(|(n, _)| n == id) {
                Some((z.utc(local), false, None))
            } else if let Some(z) = named_zone(id) {
                Some((z.utc(local), false, None))
            } else {
                Some((here(), false, Some(format!("Its time zone ({id}) isn't one Eudora knows: the times are read as this computer's."))))
            }
        }
        None => Some((here(), false, None)),
    }
}

/// A duration (DURATION:PT1H30M, P1D, P2W) in seconds.
fn duration_of(value: &str) -> Option<i64> {
    let v = value.trim();
    let (sign, v) = match v.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, v.trim_start_matches('+')),
    };
    let v = v.strip_prefix('P')?;
    let (mut total, mut number, mut in_time) = (0i64, String::new(), false);
    for c in v.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => number.push(c),
            _ => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += n * match (c, in_time) {
                    ('W', _) => 604_800,
                    ('D', _) => 86_400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(sign * total)
}

/// Reads a calendar file. Events with no start are left out; a file with none gives none.
pub fn read(text: &str) -> IcsFile {
    let all = lines(text);
    let zones = zones_in(&all);
    let method = all.iter().find(|l| l.name == "METHOD").map(|l| l.value.trim().to_ascii_uppercase());
    let mut events = Vec::new();
    let mut i = 0;
    while i < all.len() {
        if all[i].name == "BEGIN" && all[i].value.eq_ignore_ascii_case("VEVENT") {
            // (an alarm or other part inside the event is skipped)
            let mut depth = 0;
            let mut end = i + 1;
            while end < all.len() {
                let l = &all[end];
                if l.name == "BEGIN" {
                    depth += 1;
                } else if l.name == "END" {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                end += 1;
            }
            let mut own: Vec<&Line> = Vec::new();
            let mut nested = 0;
            for l in &all[i + 1..end] {
                match l.name.as_str() {
                    "BEGIN" => nested += 1,
                    "END" => nested -= 1,
                    _ if nested == 0 => own.push(l),
                    _ => {}
                }
            }
            if let Some(e) = event_of(&own, &zones) {
                events.push(e);
            }
            i = end;
        }
        i += 1;
    }
    IcsFile { method, events }
}

fn event_of(own: &[&Line], zones: &[(String, Zone)]) -> Option<IcsEvent> {
    let get = |n: &str| own.iter().find(|l| l.name == n).copied();
    let (start, all_day, mut note) = time_of(get("DTSTART")?, zones)?;
    let end = match get("DTEND").and_then(|l| time_of(l, zones)) {
        Some((end, _, n)) => {
            note = note.or(n);
            end
        }
        None => match get("DURATION").and_then(|l| duration_of(&l.value)) {
            Some(d) => start + d,
            None if all_day => start + 86_400,
            None => start + 3600,
        },
    };
    // (an end before the start: the start's length is an hour, as calendars show it)
    let end = if end <= start { start + if all_day { 86_400 } else { 3600 } } else { end };
    let summary = get("SUMMARY").map(|l| text_of(&l.value)).filter(|s| !s.is_empty()).unwrap_or_else(|| "(no title)".into());
    Some(IcsEvent {
        uid: get("UID").map(|l| text_of(&l.value)).unwrap_or_default(),
        summary: summary.chars().take(500).collect(),
        start,
        end,
        all_day,
        location: get("LOCATION").map(|l| text_of(&l.value)).unwrap_or_default(),
        description: get("DESCRIPTION").map(|l| text_of(&l.value)).unwrap_or_default(),
        organizer: get("ORGANIZER").map(|l| person(l)),
        attendees: own.iter().filter(|l| l.name == "ATTENDEE").map(|l| person(l)).take(500).collect(),
        repeats: get("RRULE").map(|l| l.value.trim().chars().take(300).collect()),
        note,
        recurrence_id: get("RECURRENCE-ID").map(|l| l.value.trim().to_string()),
        status: get("STATUS").map(|l| l.value.trim().to_ascii_uppercase()),
        free: get("TRANSP").is_some_and(|l| l.value.trim().eq_ignore_ascii_case("TRANSPARENT")),
        private: get("CLASS").is_some_and(|l| matches!(l.value.trim().to_ascii_uppercase().as_str(), "PRIVATE" | "CONFIDENTIAL")),
        answers: own.iter().filter(|l| l.name == "ATTENDEE").take(500).map(|l| l.param("PARTSTAT").unwrap_or("").to_ascii_uppercase()).collect(),
        sequence: get("SEQUENCE").and_then(|l| l.value.trim().parse().ok()).unwrap_or(0),
    })
}

/// A text value escaped for a calendar file (\ ; , and new lines).
/// The most occurrences one series makes (a rule with no end, read far ahead, stops here).
const MAX_OCCURRENCES: usize = 5000;

/// A file's events between `from` and `to` (Unix seconds), each occurrence of a repeating event
/// on its own (RECURRENCE-ID = its original start, UTC "…Z", or the date for all-day ones), as a
/// server does when asked to expand them (RFC 4791 `expand`). For servers that send the rule
/// instead (cPanel's): DAILY, WEEKLY, MONTHLY, and YEARLY with INTERVAL, COUNT, UNTIL, BYDAY
/// (weekdays, and "2TU" / "-1FR" in a month or year), BYMONTHDAY, BYMONTH, and BYSETPOS; EXDATE
/// removes one; a changed occurrence (its own RECURRENCE-ID) takes its place. Occurrences are
/// made in the event's own time zone, so a meeting stays at 9 AM across daylight saving.
pub fn expand(text: &str, from: i64, to: i64) -> Vec<IcsEvent> {
    let all = lines(text);
    let zones = zones_in(&all);
    // each event's own lines (an alarm inside skipped)
    let mut blocks: Vec<Vec<&Line>> = Vec::new();
    let mut i = 0;
    while i < all.len() {
        if all[i].name == "BEGIN" && all[i].value.eq_ignore_ascii_case("VEVENT") {
            let (mut own, mut nested) = (Vec::new(), 0);
            i += 1;
            while i < all.len() {
                let l = &all[i];
                match l.name.as_str() {
                    "END" if nested == 0 => break,
                    "BEGIN" => nested += 1,
                    "END" => nested -= 1,
                    _ if nested == 0 => own.push(l),
                    _ => {}
                }
                i += 1;
            }
            blocks.push(own);
        }
        i += 1;
    }
    fn get<'a>(own: &[&'a Line], n: &str) -> Option<&'a Line> {
        own.iter().find(|l| l.name == n).copied()
    }
    let overlaps = |e: &IcsEvent| e.end > from && e.start < to;
    // changed occurrences, by the time of the occurrence they replace
    let replaced: Vec<i64> = blocks.iter().filter_map(|b| get(b, "RECURRENCE-ID").and_then(|l| time_of(l, &zones)).map(|t| t.0)).collect();
    let mut out = Vec::new();
    for own in &blocks {
        let Some(event) = event_of(own, &zones) else { continue };
        if event.recurrence_id.is_some() || event.repeats.is_none() {
            if overlaps(&event) {
                out.push(event);
            }
            continue;
        }
        let Some(start_line) = get(own, "DTSTART") else { continue };
        let length = event.end - event.start;
        // the first start in its own zone's wall time, and the way back to Unix seconds
        let v = start_line.value.trim();
        let (local, utc_kind) = if event.all_day {
            (NaiveDate::parse_from_str(v.get(..8).unwrap_or(""), "%Y%m%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)), true)
        } else {
            (NaiveDateTime::parse_from_str(v.trim_end_matches(['Z', 'z']), "%Y%m%dT%H%M%S").ok(), v.ends_with(['Z', 'z']))
        };
        let Some(local) = local else { continue };
        let zone = start_line.param("TZID").map(|id| id.trim_start_matches('/')).and_then(|id| zones.iter().find(|(n, _)| n == id).map(|(_, z)| z.clone()).or_else(|| named_zone(id)));
        let to_utc = |t: NaiveDateTime| -> i64 {
            if utc_kind {
                t.and_utc().timestamp()
            } else if let Some(z) = &zone {
                z.utc(t)
            } else {
                chrono::Local.from_local_datetime(&t).earliest().map(|x| x.timestamp()).unwrap_or_else(|| t.and_utc().timestamp())
            }
        };
        let removed: Vec<i64> = own
            .iter()
            .filter(|l| l.name == "EXDATE")
            .flat_map(|l| {
                l.value.split(',').filter_map(|v| time_of(&Line { name: "EXDATE".into(), params: l.params.clone(), value: v.trim().to_string() }, &zones).map(|t| t.0)).collect::<Vec<_>>()
            })
            .collect();
        let rule = Rule::of(event.repeats.as_deref().unwrap_or(""), local, &to_utc);
        for at in rule.starts(local, to) {
            let start = to_utc(at);
            if removed.contains(&start) || replaced.contains(&start) {
                continue;
            }
            let mut e = event.clone();
            e.start = start;
            e.end = start + length;
            e.recurrence_id = Some(if event.all_day { at.format("%Y%m%d").to_string() } else { chrono::DateTime::from_timestamp(start, 0).map(|t| t.format("%Y%m%dT%H%M%SZ").to_string()).unwrap_or_default() });
            if overlaps(&e) {
                out.push(e);
            }
        }
    }
    out.sort_by_key(|e| e.start);
    out
}

/// A repeat rule (RRULE), the parts Eudora works out.
struct Rule {
    freq: char,
    interval: i64,
    count: Option<usize>,
    /// UNTIL in the event's own wall time (compared with each local start).
    until: Option<NaiveDateTime>,
    /// (nth, weekday): nth 0 = every one.
    by_day: Vec<(i32, Weekday)>,
    by_month_day: Vec<i32>,
    by_month: Vec<u32>,
    by_set_pos: Vec<i32>,
}

impl Rule {
    fn of(text: &str, first: NaiveDateTime, to_utc: &dyn Fn(NaiveDateTime) -> i64) -> Rule {
        let mut r = Rule { freq: 'W', interval: 1, count: None, until: None, by_day: Vec::new(), by_month_day: Vec::new(), by_month: Vec::new(), by_set_pos: Vec::new() };
        let ints = |v: &str| v.split(',').filter_map(|x| x.trim().parse::<i32>().ok()).collect::<Vec<_>>();
        for part in text.split(';') {
            let Some((k, v)) = part.split_once('=') else { continue };
            match k.trim().to_ascii_uppercase().as_str() {
                "FREQ" => r.freq = v.trim().to_ascii_uppercase().chars().next().unwrap_or('W'),
                "INTERVAL" => r.interval = v.trim().parse().unwrap_or(1).max(1),
                "COUNT" => r.count = v.trim().parse().ok(),
                "UNTIL" => {
                    let v = v.trim();
                    r.until = if v.len() == 8 {
                        NaiveDate::parse_from_str(v, "%Y%m%d").ok().and_then(|d| d.and_hms_opt(23, 59, 59))
                    } else if let Ok(t) = NaiveDateTime::parse_from_str(v.trim_end_matches(['Z', 'z']), "%Y%m%dT%H%M%S") {
                        if v.ends_with(['Z', 'z']) {
                            // (a UTC end, back to the event's wall time by the first start's offset)
                            let offset = to_utc(first) - first.and_utc().timestamp();
                            Some(t + Duration::seconds(-offset))
                        } else {
                            Some(t)
                        }
                    } else {
                        None
                    };
                }
                "BYDAY" => {
                    for d in v.split(',') {
                        let d = d.trim().to_ascii_uppercase();
                        if d.len() < 2 {
                            continue;
                        }
                        let (n, code) = d.split_at(d.len() - 2);
                        if let Some(w) = weekday_of(code) {
                            r.by_day.push((n.parse().unwrap_or(0), w));
                        }
                    }
                }
                "BYMONTHDAY" => r.by_month_day = ints(v),
                "BYMONTH" => r.by_month = ints(v).into_iter().filter(|m| (1..=12).contains(m)).map(|m| m as u32).collect(),
                "BYSETPOS" => r.by_set_pos = ints(v),
                _ => {}
            }
        }
        r
    }

    /// The starts (wall time) up to `to` (Unix seconds, compared roughly: a day's slack).
    fn starts(&self, first: NaiveDateTime, to: i64) -> Vec<NaiveDateTime> {
        let mut out = vec![first];
        let time = first.time();
        let date = first.date();
        let past = |t: NaiveDateTime| t.and_utc().timestamp() > to + 2 * 86_400 || self.until.is_some_and(|u| t > u);
        let full = |out: &Vec<NaiveDateTime>| self.count.is_some_and(|c| out.len() >= c) || out.len() >= MAX_OCCURRENCES;
        // one period after another, each period's dates in order
        for period in 0i64.. {
            if full(&out) {
                break;
            }
            let step = period * self.interval;
            let mut dates: Vec<NaiveDate> = match self.freq {
                'D' => {
                    let d = date + Duration::days(step);
                    if self.month_ok(d) && self.weekday_ok(d) { vec![d] } else { vec![] }
                }
                'W' => {
                    // the week (from the first's weekday) shifted by whole weeks
                    let start = date + Duration::weeks(step);
                    let days: Vec<NaiveDate> = (0..7).map(|k| start + Duration::days(k)).collect();
                    if self.by_day.is_empty() { vec![start] } else { days.into_iter().filter(|d| self.by_day.iter().any(|(_, w)| *w == d.weekday())).collect() }
                }
                'M' => {
                    let months = date.year() as i64 * 12 + date.month0() as i64 + step;
                    let (y, m) = ((months / 12) as i32, (months % 12) as u32 + 1);
                    self.in_month(y, m, date.day())
                }
                'Y' => {
                    let y = date.year() + step as i32;
                    let months: Vec<u32> = if self.by_month.is_empty() { vec![date.month()] } else { self.by_month.clone() };
                    let mut all = Vec::new();
                    for m in months {
                        if self.by_day.is_empty() && self.by_month_day.is_empty() {
                            all.extend(NaiveDate::from_ymd_opt(y, m, date.day()));
                        } else {
                            all.extend(self.in_month(y, m, date.day()));
                        }
                    }
                    all
                }
                _ => return out,
            };
            dates.sort();
            if !self.by_set_pos.is_empty() {
                let n = dates.len() as i32;
                let picked: Vec<NaiveDate> = self.by_set_pos.iter().filter_map(|p| if *p > 0 { dates.get(*p as usize - 1).copied() } else if *p < 0 && -p <= n { dates.get((n + p) as usize).copied() } else { None }).collect();
                dates = picked;
            }
            // (a period entirely after the end: done; the first period's own start is already in)
            let period_start = match self.freq {
                'D' => date + Duration::days(step),
                'W' => date + Duration::weeks(step),
                _ => dates.first().copied().unwrap_or(date + Duration::days(step * 28)),
            };
            if past(period_start.and_time(time)) && !dates.iter().any(|d| d.and_time(time) <= first) {
                break;
            }
            for d in dates {
                let t = d.and_time(time);
                if t <= first {
                    continue;
                }
                if past(t) || full(&out) {
                    return out;
                }
                out.push(t);
            }
            // (a monthly or yearly rule that never matches: stop after 1,000 years of looking)
            if period > 12_000 {
                break;
            }
        }
        out
    }

    fn month_ok(&self, d: NaiveDate) -> bool {
        self.by_month.is_empty() || self.by_month.contains(&d.month())
    }

    fn weekday_ok(&self, d: NaiveDate) -> bool {
        self.by_day.is_empty() || self.by_day.iter().any(|(_, w)| *w == d.weekday())
    }

    /// A month's dates the rule names (BYMONTHDAY, BYDAY with or without "2TU" / "-1FR"),
    /// else the first's own day of the month (when that month has it).
    fn in_month(&self, y: i32, m: u32, day: u32) -> Vec<NaiveDate> {
        let Some(first) = NaiveDate::from_ymd_opt(y, m, 1) else { return vec![] };
        let last = NaiveDate::from_ymd_opt(if m == 12 { y + 1 } else { y }, if m == 12 { 1 } else { m + 1 }, 1).and_then(|d| d.pred_opt()).unwrap_or(first);
        let days: Vec<NaiveDate> = first.iter_days().take_while(|d| *d <= last).collect();
        if self.by_day.is_empty() && self.by_month_day.is_empty() {
            return NaiveDate::from_ymd_opt(y, m, day).into_iter().collect();
        }
        let mut out: Vec<NaiveDate> = Vec::new();
        for (n, w) in &self.by_day {
            let of: Vec<NaiveDate> = days.iter().copied().filter(|d| d.weekday() == *w).collect();
            match *n {
                0 => out.extend(of),
                n if n > 0 => out.extend(of.get(n as usize - 1)),
                n => out.extend(of.len().checked_sub((-n) as usize).and_then(|k| of.get(k))),
            }
        }
        if !self.by_month_day.is_empty() {
            let named: Vec<NaiveDate> = self.by_month_day.iter().filter_map(|&n| if n > 0 { days.get(n as usize - 1).copied() } else { days.len().checked_sub((-n) as usize).and_then(|k| days.get(k).copied()) }).collect();
            out = if self.by_day.is_empty() { named } else { out.into_iter().filter(|d| named.contains(d)).collect() };
        }
        out.sort();
        out.dedup();
        out
    }
}

pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// A line of a calendar file: at most 75 octets a line, continued with a space (RFC 5545
/// §3.1), never inside a character; ends with CRLF.
pub fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut width = 0;
    for c in line.chars() {
        if width + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
    out
}

/// An answer to an invitation, as calendars send each other (iTIP, RFC 5546: METHOD:REPLY):
/// the request's UID, SEQUENCE, RECURRENCE-ID and ORGANIZER as they were, the user as the one
/// ATTENDEE with their answer, and a COMMENT when there's a note. `answer`: accept, tentative or
/// decline. The organizer's calendar program reads it and records the answer.
pub fn reply(request: &str, me: &str, my_name: &str, answer: &str, comment: Option<&str>, stamp: i64) -> Option<String> {
    let all = lines(request);
    let start = all.iter().position(|l| l.name == "BEGIN" && l.value.eq_ignore_ascii_case("VEVENT"))?;
    let end = all[start..].iter().position(|l| l.name == "END" && l.value.eq_ignore_ascii_case("VEVENT")).map(|p| start + p)?;
    let event = &all[start + 1..end];
    let get = |n: &str| event.iter().find(|l| l.name == n);
    let uid = get("UID")?.value.trim().to_string();
    let as_line = |l: &Line| {
        let params: String = l.params.iter().map(|(k, v)| if v.contains([':', ';', ',']) { format!(";{k}=\"{v}\"") } else { format!(";{k}={v}") }).collect();
        format!("{}{params}:{}", l.name, l.value.trim())
    };
    let partstat = match answer {
        "accept" => "ACCEPTED",
        "tentative" => "TENTATIVE",
        _ => "DECLINED",
    };
    let stamp = chrono::DateTime::from_timestamp(stamp, 0).unwrap_or_default().format("%Y%m%dT%H%M%SZ");
    let name = my_name.replace(['"', '\r', '\n'], "");
    let mut out = String::new();
    for l in ["BEGIN:VCALENDAR", "PRODID:-//Planet Networks//Eudora//EN", "VERSION:2.0", "METHOD:REPLY", "BEGIN:VEVENT"] {
        out.push_str(&fold(l));
    }
    out.push_str(&fold(&format!("UID:{uid}")));
    out.push_str(&fold(&format!("DTSTAMP:{stamp}")));
    out.push_str(&fold(&format!("SEQUENCE:{}", get("SEQUENCE").map(|l| l.value.trim().to_string()).unwrap_or_else(|| "0".into()))));
    for n in ["RECURRENCE-ID", "ORGANIZER", "DTSTART", "DTEND"] {
        if let Some(l) = get(n) {
            out.push_str(&fold(&as_line(l)));
        }
    }
    if let Some(s) = get("SUMMARY") {
        out.push_str(&fold(&format!("SUMMARY:{}", s.value.trim())));
    }
    let cn = if name.is_empty() { String::new() } else { format!(";CN=\"{name}\"") };
    out.push_str(&fold(&format!("ATTENDEE;PARTSTAT={partstat}{cn}:mailto:{me}")));
    if let Some(c) = comment.map(str::trim).filter(|c| !c.is_empty()) {
        out.push_str(&fold(&format!("COMMENT:{}", escape(c))));
    }
    // (times named by a zone need the zone: the request's own, as it described it)
    let zones: Vec<&str> = event.iter().filter(|l| l.name == "DTSTART" || l.name == "DTEND").filter_map(|l| l.param("TZID")).collect();
    out.push_str(&fold("END:VEVENT"));
    let mut tz = String::new();
    if !zones.is_empty() {
        let mut i = 0;
        while i < all.len() {
            if all[i].name == "BEGIN" && all[i].value.eq_ignore_ascii_case("VTIMEZONE") {
                let stop = all[i..].iter().position(|l| l.name == "END" && l.value.eq_ignore_ascii_case("VTIMEZONE")).map(|p| i + p).unwrap_or(all.len() - 1);
                let id = all[i..stop].iter().find(|l| l.name == "TZID").map(|l| l.value.trim()).unwrap_or("");
                if zones.contains(&id) {
                    for l in &all[i..=stop] {
                        tz.push_str(&fold(&as_line(l)));
                    }
                }
                i = stop;
            }
            i += 1;
        }
    }
    // (the zones go before the event: VCALENDAR, its zones, then the event)
    let head_end = out.find("BEGIN:VEVENT").unwrap_or(out.len());
    out.insert_str(head_end, &tz);
    out.push_str(&fold("END:VCALENDAR"));
    Some(out)
}

/// The organizer's address in a calendar request (ORGANIZER:mailto:…), and the attendee
/// addresses, lowercase.
pub fn organizer_and_attendees(request: &str) -> (Option<String>, Vec<String>) {
    let all = lines(request);
    let address = |l: &Line| {
        let (_, a) = person(l);
        Some(a.to_lowercase()).filter(|a| a.contains('@'))
    };
    let organizer = all.iter().find(|l| l.name == "ORGANIZER").and_then(address);
    let attendees = all.iter().filter(|l| l.name == "ATTENDEE").filter_map(address).collect();
    (organizer, attendees)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    /// The occurrences of a one-event file between two times, as "YYYY-MM-DD HH:MM" UTC.
    fn occurrences(event: &str, from: &str, to: &str) -> Vec<String> {
        let text = format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:S\r\n{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", event.replace('\n', "\r\n"));
        expand(&text, at(from), at(to)).iter().map(|e| chrono::DateTime::from_timestamp(e.start, 0).unwrap().format("%Y-%m-%d %H:%M").to_string()).collect()
    }

    #[test]
    fn repeating_events_are_worked_out_when_the_server_sends_the_rule() {
        let (from, to) = ("2026-01-01T00:00:00Z", "2028-01-01T00:00:00Z");
        // weekly, four times
        assert_eq!(occurrences("DTSTART:20261123T140000Z\nDTEND:20261123T143000Z\nRRULE:FREQ=WEEKLY;COUNT=4", from, to), ["2026-11-23 14:00", "2026-11-30 14:00", "2026-12-07 14:00", "2026-12-14 14:00"]);
        // Monday, Wednesday, and Friday until a date; one removed
        assert_eq!(
            occurrences("DTSTART:20261102T090000Z\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR;UNTIL=20261109T235959Z\nEXDATE:20261104T090000Z", from, to),
            ["2026-11-02 09:00", "2026-11-06 09:00", "2026-11-09 09:00"]
        );
        // every other week on Tuesday and Thursday
        assert_eq!(
            occurrences("DTSTART:20261103T100000Z\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH;COUNT=4", from, to),
            ["2026-11-03 10:00", "2026-11-05 10:00", "2026-11-17 10:00", "2026-11-19 10:00"]
        );
        // the second Tuesday and the last Friday of each month
        assert_eq!(occurrences("DTSTART:20261110T150000Z\nRRULE:FREQ=MONTHLY;BYDAY=2TU;COUNT=3", from, to), ["2026-11-10 15:00", "2026-12-08 15:00", "2027-01-12 15:00"]);
        assert_eq!(occurrences("DTSTART:20261127T150000Z\nRRULE:FREQ=MONTHLY;BYDAY=-1FR;COUNT=3", from, to), ["2026-11-27 15:00", "2026-12-25 15:00", "2027-01-29 15:00"]);
        // the 31st: only months that have one; the last weekday of the month (BYSETPOS)
        assert_eq!(occurrences("DTSTART:20261231T120000Z\nRRULE:FREQ=MONTHLY;BYMONTHDAY=31;COUNT=3", from, to), ["2026-12-31 12:00", "2027-01-31 12:00", "2027-03-31 12:00"]);
        assert_eq!(occurrences("DTSTART:20261130T120000Z\nRRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=3", from, to), ["2026-11-30 12:00", "2026-12-31 12:00", "2027-01-29 12:00"]);
        // every other day; yearly; all day
        assert_eq!(occurrences("DTSTART:20261201T080000Z\nRRULE:FREQ=DAILY;INTERVAL=2;COUNT=3", from, to), ["2026-12-01 08:00", "2026-12-03 08:00", "2026-12-05 08:00"]);
        assert_eq!(occurrences("DTSTART;VALUE=DATE:20260704\nRRULE:FREQ=YEARLY", from, to), ["2026-07-04 00:00", "2027-07-04 00:00"]);
        // a 9 AM meeting in New York stays at 9 AM when daylight saving ends (1 November 2026)
        assert_eq!(occurrences("DTSTART;TZID=America/New_York:20261026T090000\nRRULE:FREQ=WEEKLY;COUNT=3", from, to), ["2026-10-26 13:00", "2026-11-02 14:00", "2026-11-09 14:00"]);
        // only those in the range asked for; no end: as far as asked
        assert_eq!(occurrences("DTSTART:20260105T090000Z\nRRULE:FREQ=WEEKLY", "2026-03-01T00:00:00Z", "2026-03-20T00:00:00Z"), ["2026-03-02 09:00", "2026-03-09 09:00", "2026-03-16 09:00"]);
    }

    #[test]
    fn a_changed_occurrence_takes_the_place_of_the_one_it_replaces() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:Weekly\r\nDTSTART:20261123T140000Z\r\nDTEND:20261123T143000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:x\r\nRECURRENCE-ID:20261130T140000Z\r\nSUMMARY:Moved\r\nDTSTART:20261130T170000Z\r\nDTEND:20261130T173000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = expand(text, at("2026-11-01T00:00:00Z"), at("2027-01-01T00:00:00Z"));
        let shown: Vec<(String, Option<String>)> = events.iter().map(|e| (e.summary.clone(), e.recurrence_id.clone())).collect();
        assert_eq!(
            shown,
            [
                ("Weekly".to_string(), Some("20261123T140000Z".to_string())),
                ("Moved".to_string(), Some("20261130T140000Z".to_string())),
                ("Weekly".to_string(), Some("20261207T140000Z".to_string())),
            ]
        );
    }

    #[test]
    fn utc_all_day_and_escaped_text() {
        let f = read(
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:abc@x\r\nSUMMARY:Board meeting\\, quarterly\r\nDTSTART:20261103T150000Z\r\nDTEND:20261103T160000Z\r\n\
LOCATION:Room 4\r\nDESCRIPTION:Line one\\nLine two that is folded \r\n onto the next line\r\nORGANIZER;CN=\"Ann Smith\":mailto:ann@x.com\r\nATTENDEE;CN=Bob:MAILTO:bob@y.com\r\n\
BEGIN:VALARM\r\nTRIGGER:-PT15M\r\nDESCRIPTION:Reminder\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261126\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(f.method.as_deref(), Some("REQUEST"));
        assert_eq!(f.events.len(), 2);
        let e = &f.events[0];
        assert_eq!((e.summary.as_str(), e.start, e.end), ("Board meeting, quarterly", at("2026-11-03T15:00:00Z"), at("2026-11-03T16:00:00Z")));
        assert_eq!(e.description, "Line one\nLine two that is folded onto the next line", "folded, the alarm's text not taken");
        assert_eq!(e.organizer, Some(("Ann Smith".into(), "ann@x.com".into())));
        assert_eq!(e.attendees, vec![("Bob".into(), "bob@y.com".into())]);
        let h = &f.events[1];
        assert!(h.all_day);
        assert_eq!((h.start, h.end), (at("2026-11-26T00:00:00Z"), at("2026-11-27T00:00:00Z")));
    }

    #[test]
    fn times_in_a_described_zone_keep_daylight_saving() {
        let zone = "BEGIN:VTIMEZONE\r\nTZID:Eastern\r\nBEGIN:STANDARD\r\nDTSTART:16011104T020000\r\nRRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\n\
BEGIN:DAYLIGHT\r\nDTSTART:16010311T020000\r\nRRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3\r\nTZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n";
        let f = read(&format!(
            "BEGIN:VCALENDAR\r\n{zone}BEGIN:VEVENT\r\nSUMMARY:Summer\r\nDTSTART;TZID=Eastern:20260715T090000\r\nDURATION:PT1H30M\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nSUMMARY:Winter\r\nDTSTART;TZID=Eastern:20261215T090000\r\nDTEND;TZID=Eastern:20261215T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        ));
        assert_eq!((f.events[0].start, f.events[0].end), (at("2026-07-15T13:00:00Z"), at("2026-07-15T14:30:00Z")));
        assert_eq!(f.events[1].start, at("2026-12-15T14:00:00Z"));
    }

    #[test]
    fn named_zones_are_known_and_others_said() {
        let f = read("BEGIN:VEVENT\r\nSUMMARY:Rome\r\nDTSTART;TZID=Europe/Rome:20260715T090000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nSUMMARY:LA\r\nDTSTART;TZID=Pacific Standard Time:20260115T090000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nSUMMARY:Elsewhere\r\nDTSTART;TZID=Mars/Olympus:20260115T090000\r\nEND:VEVENT\r\n");
        assert_eq!(f.events[0].start, at("2026-07-15T07:00:00Z"), "Rome in summer: UTC+2");
        assert_eq!((f.events[0].end - f.events[0].start), 3600, "no end: an hour");
        assert_eq!(f.events[1].start, at("2026-01-15T17:00:00Z"), "Los Angeles in winter: UTC-8");
        assert!(f.events[2].note.as_deref().is_some_and(|n| n.contains("Mars/Olympus")));
    }

    #[test]
    fn an_answer_carries_what_the_organizer_s_calendar_needs() {
        let request = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Eastern Standard Time\r\nBEGIN:STANDARD\r\nDTSTART:16011104T020000\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:040000008200E00074C5B7101A82E008\r\nSEQUENCE:2\r\nSUMMARY:Budget review\r\nDTSTART;TZID=Eastern Standard Time:20261105T140000\r\nDTEND;TZID=Eastern Standard Time:20261105T150000\r\n\
ORGANIZER;CN=Ann Smith:mailto:ann@x.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:Pat@Example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (organizer, attendees) = organizer_and_attendees(request);
        assert_eq!(organizer.as_deref(), Some("ann@x.com"));
        assert_eq!(attendees, vec!["pat@example.com".to_string()]);
        let r = reply(request, "pat@example.com", "Pat Morgan", "tentative", Some("Running late, may join at 2:15"), 1_793_000_000).unwrap();
        for want in [
            "METHOD:REPLY", "UID:040000008200E00074C5B7101A82E008", "SEQUENCE:2", "ORGANIZER;CN=Ann Smith:mailto:ann@x.com",
            "ATTENDEE;PARTSTAT=TENTATIVE;CN=\"Pat Morgan\":mailto:pat@example.com", "COMMENT:Running late\\, may join at 2:15", "TZID:Eastern Standard Time",
        ] {
            assert!(r.replace("\r\n ", "").contains(want), "{want} in:\n{r}");
        }
        assert!(r.find("BEGIN:VTIMEZONE").unwrap() < r.find("BEGIN:VEVENT").unwrap(), "the zone before the event");
        assert!(r.split("\r\n").all(|l| l.len() <= 75), "folded");
        // and it reads back as the same meeting
        let back = read(&r);
        assert_eq!((back.method.as_deref(), back.events[0].summary.as_str()), (Some("REPLY"), "Budget review"));
        assert!(reply("no calendar here", "a@b.c", "", "accept", None, 0).is_none());
    }

    #[test]
    fn nothing_breaks_on_rubbish() {
        assert!(read("").events.is_empty());
        assert!(read("not a calendar\nBEGIN:VEVENT\nSUMMARY:no start\nEND:VEVENT").events.is_empty());
        assert_eq!(duration_of("P1W2DT3H"), Some(604_800 + 2 * 86_400 + 3 * 3600));
    }
}
