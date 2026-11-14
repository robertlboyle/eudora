//! Reservations from the Travel view onto the calendar.
//!
//! Only Travel messages received in the last year are read (each once), and only reservations
//! still ahead are kept. Flights go on the calendar by themselves when no event there already
//! has them; hotel stays are offered, and go on from 7 PM of the check-in day to 10 AM of the
//! check-out day. Read from the text (airlines rarely carry schema.org data) and from schema.org
//! FlightReservation / LodgingReservation when a message has it.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::library::{Library, now};

/// Daylight saving time: none, the US / Canada rule, or the EU rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dst {
    None,
    Us,
    Eu,
}

/// Airports: code, city, standard UTC offset in minutes, daylight saving rule. The big ones of
/// North America and Europe and the busiest elsewhere; a code not here isn't taken for an
/// airport (three capital letters are common in mail).
const AIRPORTS: &str = "\
ATL Atlanta -300 U|BOS Boston -300 U|BWI Baltimore -300 U|CLT Charlotte -300 U|DCA Washington-National -300 U|IAD Washington-Dulles -300 U|\
DTW Detroit -300 U|EWR Newark -300 U|JFK New_York-JFK -300 U|LGA New_York-LaGuardia -300 U|FLL Fort_Lauderdale -300 U|MIA Miami -300 U|\
MCO Orlando -300 U|TPA Tampa -300 U|PBI West_Palm_Beach -300 U|RSW Fort_Myers -300 U|JAX Jacksonville -300 U|PHL Philadelphia -300 U|\
PIT Pittsburgh -300 U|CLE Cleveland -300 U|CMH Columbus -300 U|CVG Cincinnati -300 U|IND Indianapolis -300 U|RDU Raleigh-Durham -300 U|\
RIC Richmond -300 U|ORF Norfolk -300 U|BDL Hartford -300 U|PVD Providence -300 U|ALB Albany -300 U|BUF Buffalo -300 U|ROC Rochester -300 U|\
SYR Syracuse -300 U|HPN White_Plains -300 U|ISP Islip -300 U|SWF Newburgh -300 U|ABE Allentown -300 U|AVP Scranton -300 U|ACY Atlantic_City -300 U|\
TTN Trenton -300 U|MDT Harrisburg -300 U|SAV Savannah -300 U|CHS Charleston -300 U|MYR Myrtle_Beach -300 U|GSP Greenville -300 U|\
SRQ Sarasota -300 U|PIE St_Petersburg -300 U|SFB Sanford -300 U|DAB Daytona_Beach -300 U|EYW Key_West -300 U|PNS Pensacola -360 U|\
GRR Grand_Rapids -300 U|PWM Portland_ME -300 U|BTV Burlington -300 U|MHT Manchester -300 U|ROA Roanoke -300 U|CHO Charlottesville -300 U|\
LYH Lynchburg -300 U|GSO Greensboro -300 U|AVL Asheville -300 U|ILM Wilmington -300 U|TYS Knoxville -300 U|SDF Louisville -300 U|\
LEX Lexington -300 U|ORD Chicago-O'Hare -360 U|MDW Chicago-Midway -360 U|DFW Dallas-Fort_Worth -360 U|DAL Dallas-Love -360 U|\
IAH Houston -360 U|HOU Houston-Hobby -360 U|AUS Austin -360 U|SAT San_Antonio -360 U|MSY New_Orleans -360 U|MSP Minneapolis -360 U|\
STL St_Louis -360 U|MCI Kansas_City -360 U|MKE Milwaukee -360 U|BNA Nashville -360 U|MEM Memphis -360 U|OKC Oklahoma_City -360 U|\
TUL Tulsa -360 U|OMA Omaha -360 U|DSM Des_Moines -360 U|BHM Birmingham -360 U|HSV Huntsville -360 U|MOB Mobile -360 U|JAN Jackson -360 U|\
LIT Little_Rock -360 U|XNA Bentonville -360 U|COU Columbia_MO -360 U|SGF Springfield -360 U|ICT Wichita -360 U|MSN Madison -360 U|\
CID Cedar_Rapids -360 U|ELP El_Paso -420 U|HRL Harlingen -360 U|BRO Brownsville -360 U|MFE McAllen -360 U|CRP Corpus_Christi -360 U|\
LBB Lubbock -360 U|AMA Amarillo -360 U|DEN Denver -420 U|SLC Salt_Lake_City -420 U|ABQ Albuquerque -420 U|BOI Boise -420 U|\
COS Colorado_Springs -420 U|PHX Phoenix -420 N|TUS Tucson -420 N|LAS Las_Vegas -480 U|LAX Los_Angeles -480 U|SFO San_Francisco -480 U|\
SJC San_Jose -480 U|OAK Oakland -480 U|SAN San_Diego -480 U|SNA Santa_Ana -480 U|BUR Burbank -480 U|LGB Long_Beach -480 U|ONT Ontario -480 U|\
PSP Palm_Springs -480 U|SMF Sacramento -480 U|SEA Seattle -480 U|PDX Portland -480 U|RNO Reno -480 U|GEG Spokane -480 U|ANC Anchorage -540 U|\
FAI Fairbanks -540 U|HNL Honolulu -600 N|OGG Maui -600 N|KOA Kona -600 N|LIH Lihue -600 N|SJU San_Juan -240 N|STT St_Thomas -240 N|\
STX St_Croix -240 N|YYZ Toronto -300 U|YUL Montreal -300 U|YOW Ottawa -300 U|YVR Vancouver -480 U|YYC Calgary -420 U|YEG Edmonton -420 U|\
YWG Winnipeg -360 U|YHZ Halifax -240 U|NAS Nassau -300 U|MBJ Montego_Bay -300 N|KIN Kingston -300 N|PUJ Punta_Cana -240 N|SDQ Santo_Domingo -240 N|\
AUA Aruba -240 N|CUR Curacao -240 N|SXM St_Maarten -240 N|BGI Barbados -240 N|GCM Grand_Cayman -300 N|CUN Cancun -300 N|MEX Mexico_City -360 N|\
GDL Guadalajara -360 N|SJD Los_Cabos -420 N|PVR Puerto_Vallarta -360 N|SJO San_Jose_CR -360 N|LIR Liberia_CR -360 N|PTY Panama_City -300 N|\
BOG Bogota -300 N|LIM Lima -300 N|GRU Sao_Paulo -180 N|GIG Rio_de_Janeiro -180 N|EZE Buenos_Aires -180 N|SCL Santiago -240 N|\
LHR London-Heathrow 0 E|LGW London-Gatwick 0 E|STN London-Stansted 0 E|LTN London-Luton 0 E|MAN Manchester_UK 0 E|EDI Edinburgh 0 E|\
GLA Glasgow 0 E|DUB Dublin 0 E|SNN Shannon 0 E|LIS Lisbon 0 E|OPO Porto 0 E|KEF Reykjavik 0 N|CDG Paris-CDG 60 E|ORY Paris-Orly 60 E|\
NCE Nice 60 E|AMS Amsterdam 60 E|BRU Brussels 60 E|FRA Frankfurt 60 E|MUC Munich 60 E|BER Berlin 60 E|DUS Dusseldorf 60 E|HAM Hamburg 60 E|\
ZRH Zurich 60 E|GVA Geneva 60 E|VIE Vienna 60 E|PRG Prague 60 E|WAW Warsaw 60 E|BUD Budapest 60 E|CPH Copenhagen 60 E|ARN Stockholm 60 E|\
OSL Oslo 60 E|MAD Madrid 60 E|BCN Barcelona 60 E|AGP Malaga 60 E|PMI Palma 60 E|FCO Rome 60 E|CIA Rome-Ciampino 60 E|MXP Milan 60 E|\
LIN Milan-Linate 60 E|BGY Bergamo 60 E|VCE Venice 60 E|FLR Florence 60 E|PSA Pisa 60 E|BLQ Bologna 60 E|NAP Naples 60 E|PMO Palermo 60 E|\
CTA Catania 60 E|BRI Bari 60 E|TRN Turin 60 E|GOA Genoa 60 E|CAG Cagliari 60 E|OLB Olbia 60 E|HEL Helsinki 120 E|ATH Athens 120 E|\
IST Istanbul 180 N|TLV Tel_Aviv 120 N|DXB Dubai 240 N|AUH Abu_Dhabi 240 N|DOH Doha 180 N|DEL Delhi 330 N|BOM Mumbai 330 N|\
SIN Singapore 480 N|HKG Hong_Kong 480 N|PEK Beijing 480 N|PVG Shanghai 480 N|TPE Taipei 480 N|ICN Seoul 540 N|NRT Tokyo-Narita 540 N|\
HND Tokyo-Haneda 540 N|BKK Bangkok 420 N|MNL Manila 480 N|SYD Sydney 600 N|MEL Melbourne 600 N|AKL Auckland 720 N|JNB Johannesburg 120 N|\
CPT Cape_Town 120 N|CAI Cairo 120 N";

struct Airport {
    city: String,
    offset: i32,
    dst: Dst,
}

fn airports() -> &'static HashMap<String, Airport> {
    static TABLE: std::sync::OnceLock<HashMap<String, Airport>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        AIRPORTS
            .split('|')
            .filter_map(|row| {
                let mut f = row.split_whitespace();
                let (code, city, offset, dst) = (f.next()?, f.next()?, f.next()?.parse().ok()?, f.next()?);
                let dst = match dst {
                    "U" => Dst::Us,
                    "E" => Dst::Eu,
                    _ => Dst::None,
                };
                Some((code.to_string(), Airport { city: city.replace('_', " "), offset, dst }))
            })
            .collect()
    })
}

/// The `n`th `weekday` of a month (n from 1), or the last when `n` is 0.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, n: u32) -> NaiveDate {
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
    d + chrono::Duration::weeks(n as i64 - 1)
}

/// A local time at an airport as Unix seconds (its daylight saving time counted).
fn airport_utc(code: &str, local: NaiveDateTime) -> Option<i64> {
    let a = airports().get(code)?;
    let standard = local.and_utc().timestamp() - a.offset as i64 * 60;
    let year = local.year();
    let summer = match a.dst {
        Dst::None => false,
        // second Sunday of March 2:00 to first Sunday of November 2:00, local time
        Dst::Us => {
            let on = nth_weekday(year, 3, Weekday::Sun, 2).and_hms_opt(2, 0, 0).unwrap();
            let off = nth_weekday(year, 11, Weekday::Sun, 1).and_hms_opt(2, 0, 0).unwrap();
            local >= on && local < off
        }
        // last Sunday of March 1:00 UTC to last Sunday of October 1:00 UTC
        Dst::Eu => {
            let on = nth_weekday(year, 3, Weekday::Sun, 0).and_hms_opt(1, 0, 0).unwrap().and_utc().timestamp();
            let off = nth_weekday(year, 10, Weekday::Sun, 0).and_hms_opt(1, 0, 0).unwrap().and_utc().timestamp();
            standard >= on && standard < off
        }
    };
    Some(standard - if summer { 3600 } else { 0 })
}

/// Airlines: two-letter code and the names their mail goes by.
const AIRLINES: [(&str, &str); 34] = [
    ("UA", "united"), ("AA", "american airlines"), ("DL", "delta"), ("B6", "jetblue"), ("WN", "southwest"), ("AS", "alaska"),
    ("NK", "spirit"), ("F9", "frontier"), ("G4", "allegiant"), ("SY", "sun country"), ("HA", "hawaiian"), ("AC", "air canada"),
    ("WS", "westjet"), ("BA", "british airways"), ("VS", "virgin atlantic"), ("LH", "lufthansa"), ("AF", "air france"), ("KL", "klm"),
    ("IB", "iberia"), ("AZ", "ita airways"), ("LX", "swiss"), ("EI", "aer lingus"), ("TP", "tap"), ("EK", "emirates"),
    ("QR", "qatar"), ("TK", "turkish"), ("SK", "sas"), ("AY", "finnair"), ("AM", "aeromexico"), ("CM", "copa"),
    ("AV", "avianca"), ("MX", "breeze"), ("XP", "avelo"), ("FI", "icelandair"),
];

/// What the reader picked out of the text, by position.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    Flight(String, String),
    Airport(String),
    Time(NaiveTime),
    Date(NaiveDate),
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn month_of(word: &str) -> Option<u32> {
    let w = word.to_ascii_lowercase();
    if w.len() < 3 {
        return None;
    }
    let full = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    MONTHS.iter().position(|m| w == *m || (w.starts_with(m) && full.iter().any(|f| f.starts_with(&w))) || (w == "sept")).map(|i| i as u32 + 1)
}

/// A year for a date given without one: the first such date from a month before the
/// message on (a confirmation is for a trip ahead).
fn infer_year(month: u32, day: u32, sent: NaiveDate) -> Option<NaiveDate> {
    for year in [sent.year(), sent.year() + 1] {
        if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
            if d >= sent - chrono::Duration::days(30) {
                return Some(d);
            }
        }
    }
    None
}

/// Everything the reader recognizes in a text, with where it is.
fn scan(text: &str, sent: NaiveDate, airline: Option<&str>) -> Vec<(usize, Item)> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    let at = |i: usize| chars.get(i).map(|c| c.1).unwrap_or(' ');
    let mut i = 0;
    while i < n {
        let (pos, c) = chars[i];
        // a word of letters / digits
        if c.is_ascii_alphanumeric() {
            let start = i;
            let mut j = i;
            while j < n && (at(j).is_ascii_alphanumeric()) {
                j += 1;
            }
            let word: String = chars[start..j].iter().map(|c| c.1).collect();
            let prev_letter = start > 0 && at(start - 1).is_alphabetic();
            // airport codes: three capitals (digits may touch them: "LGA9:45")
            let letters: String = word.chars().take_while(|c| c.is_ascii_uppercase()).collect();
            if !prev_letter && letters.len() == 3 && airports().contains_key(&letters) && word[3..].chars().all(|c| c.is_ascii_digit()) {
                out.push((pos, Item::Airport(letters.clone())));
                if word.len() > 3 {
                    // the time right after the code: let the digits be read on their own
                    i = start + 3;
                    continue;
                }
            }
            // flight numbers: a known airline code and 1-4 digits ("UA 1893", "DL1376")
            if !prev_letter {
                let up = word.to_ascii_uppercase();
                for (code, _) in AIRLINES {
                    let digits = if up == code && word.len() == 2 {
                        // the number in the next word
                        let mut k = j;
                        while k < n && at(k) == ' ' {
                            k += 1;
                        }
                        let mut m = k;
                        while m < n && at(m).is_ascii_digit() {
                            m += 1;
                        }
                        (m > k && m - k <= 4 && !at(m).is_ascii_alphanumeric() && at(m) != ':' && at(m) != '.').then(|| chars[k..m].iter().map(|c| c.1).collect::<String>())
                    } else if word.starts_with(code) && word.len() > 2 && word.len() <= 6 && word[2..].chars().all(|c| c.is_ascii_digit()) {
                        Some(word[2..].to_string())
                    } else {
                        None
                    };
                    if let Some(d) = digits.filter(|d| d.parse::<u32>().is_ok_and(|x| x > 0)) {
                        if code.chars().any(|c| c.is_ascii_alphabetic()) && word.chars().take(2).all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                            out.push((pos, Item::Flight(code.to_string(), d.trim_start_matches('0').to_string())));
                        }
                    }
                }
                // "Flight 576" / "Flight #576" with the sender's airline
                if word.eq_ignore_ascii_case("flight") {
                    if let Some(code) = airline {
                        let mut k = j;
                        while k < n && (at(k) == ' ' || at(k) == '#' || at(k) == ':') {
                            k += 1;
                        }
                        let mut m = k;
                        while m < n && at(m).is_ascii_digit() {
                            m += 1;
                        }
                        if m > k && m - k <= 4 && !at(m).is_ascii_alphanumeric() {
                            out.push((pos, Item::Flight(code.to_string(), chars[k..m].iter().map(|c| c.1).collect::<String>().trim_start_matches('0').to_string())));
                        }
                    }
                }
            }
            // dates: "Oct 30, 2026", "Sun, Aug 11", "18 Dec 2019", "October 30"
            if let Some(month) = month_of(&word).filter(|_| word.chars().all(|c| c.is_ascii_alphabetic())) {
                let rest: String = chars[j..(j + 14).min(n)].iter().map(|c| c.1).collect();
                let mut parts = rest.trim_start_matches(['.', ' ']).split(|c: char| c == ' ' || c == ',');
                if let Some(day) = parts.next().and_then(|d| d.trim_end_matches(|c: char| c.is_alphabetic()).parse::<u32>().ok()).filter(|d| (1..=31).contains(d)) {
                    let year = parts.find(|p| !p.is_empty()).and_then(|y| y.parse::<i32>().ok()).filter(|y| (1990..2100).contains(y));
                    let date = match year {
                        Some(y) => NaiveDate::from_ymd_opt(y, month, day),
                        None => infer_year(month, day, sent),
                    };
                    if let Some(d) = date {
                        out.push((pos, Item::Date(d)));
                    }
                } else {
                    // "18 Dec 2019": the day before the month
                    let before: String = chars[start.saturating_sub(4)..start].iter().map(|c| c.1).collect();
                    let day = before.trim().rsplit(' ').next().and_then(|d| d.parse::<u32>().ok()).filter(|d| (1..=31).contains(d));
                    let after = rest.trim_start().split(|c: char| !c.is_ascii_digit()).next().and_then(|y| y.parse::<i32>().ok()).filter(|y| (1990..2100).contains(y));
                    if let (Some(day), Some(y)) = (day, after) {
                        if let Some(d) = NaiveDate::from_ymd_opt(y, month, day) {
                            out.push((pos, Item::Date(d)));
                        }
                    }
                }
            }
            // numeric dates (US order): "10/8/26", "10/08/2026"
            if word.chars().all(|c| c.is_ascii_digit()) && word.len() <= 2 && at(j) == '/' {
                let rest: String = chars[j..(j + 9).min(n)].iter().map(|c| c.1).collect();
                let mut parts = rest[1..].split('/');
                let day = parts.next().and_then(|d| d.parse::<u32>().ok());
                let year = parts.next().map(|y| y.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).and_then(|y| match y.len() {
                    2 => y.parse::<i32>().ok().map(|y| 2000 + y),
                    4 => y.parse::<i32>().ok(),
                    _ => None,
                });
                if let (Ok(month), Some(day), Some(year)) = (word.parse::<u32>(), day, year) {
                    if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
                        out.push((pos, Item::Date(d)));
                    }
                }
            }
            // times: "7:00 PM", "5:34pm", "14:05"
            if word.chars().all(|c| c.is_ascii_digit()) && word.len() <= 2 && at(j) == ':' {
                let mut m = j + 1;
                while m < n && at(m).is_ascii_digit() {
                    m += 1;
                }
                if m - j - 1 == 2 {
                    let h: u32 = word.parse().unwrap_or(99);
                    let min: u32 = chars[j + 1..m].iter().map(|c| c.1).collect::<String>().parse().unwrap_or(99);
                    let mut k = m;
                    while k < n && at(k) == ' ' {
                        k += 1;
                    }
                    let ampm: String = chars[k..(k + 4).min(n)].iter().map(|c| c.1).collect::<String>().to_ascii_lowercase().replace('.', "");
                    let hour = if ampm.starts_with("pm") || ampm.starts_with("p ") { (h % 12) + 12 } else if ampm.starts_with("am") || ampm.starts_with("a ") { h % 12 } else { h };
                    if let Some(t) = NaiveTime::from_hms_opt(hour, min, 0) {
                        out.push((pos, Item::Time(t)));
                    }
                    i = m;
                    continue;
                }
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    out
}

/// A reservation found in a message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Found {
    /// flight, stay
    pub kind: String,
    /// The same flight or stay in several messages is one: "flight:UA1893:2026-10-29".
    pub key: String,
    pub start: i64,
    pub end: i64,
    pub title: String,
    pub location: String,
    pub details: String,
    /// The arrival time wasn't in the mail (the end is a guess).
    pub end_guessed: bool,
    /// Whole days (a conference): `start` and `end` are UTC midnights, as the calendar keeps
    /// all-day events.
    #[serde(default)]
    pub all_day: bool,
}

/// The airline a message comes from (by its sender).
fn airline_of(from: &str) -> Option<&'static str> {
    let f = from.to_ascii_lowercase();
    AIRLINES.iter().find(|(_, name)| f.contains(&name.replace(' ', "")) || f.contains(name)).map(|(c, _)| *c)
}

/// Up to `len` bytes of `s` from `at`, kept to whole characters.
fn cut(s: &str, at: usize, len: usize) -> &str {
    let mut start = at.min(s.len());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + len).min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[start..end]
}

/// The booking's confirmation code, when the mail names one.
fn confirmation(text: &str, subject: &str) -> Option<String> {
    let hay = format!("{subject}\n{text}");
    let lower = hay.to_ascii_lowercase();
    // after a bare "Confirmation:" a code has a digit ("Reservation Confirmation: PAT" isn't one)
    for (label, digit) in [("confirmation number", false), ("confirmation code", false), ("confirmation #", false), ("record locator", false), ("booking reference", false), ("confirmation:", true), ("confirmation", true)] {
        let mut from = 0;
        while let Some(p) = lower[from..].find(label) {
            let at = from + p + label.len();
            let rest = cut(&hay, at, 30);
            let code: String = rest.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if (5..=10).contains(&code.len()) && (!digit || code.chars().any(|c| c.is_ascii_digit())) && code.chars().any(|c| c.is_ascii_digit() || c.is_ascii_uppercase()) && code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                return Some(code);
            }
            from = at;
        }
    }
    None
}

/// Flights in a message's text: each flight number with the airports, times and dates nearest
/// it; a connection's missing origin is the leg before's destination.
pub fn flights_in(text: &str, subject: &str, from: &str, sent: i64) -> Vec<Found> {
    let sent_day = chrono::DateTime::from_timestamp(sent, 0).unwrap_or_default().date_naive();
    let items = scan(text, sent_day, airline_of(from));
    let flights: Vec<(usize, String, String)> = items.iter().filter_map(|(p, it)| if let Item::Flight(c, n) = it { Some((*p, c.clone(), n.clone())) } else { None }).collect();
    if flights.is_empty() {
        return Vec::new();
    }
    // each airport / time / date to the flight number after it (airlines put a leg's details
    // before its number: United, JetBlue, Delta), else the one before it; within 600 characters
    let mut parts: Vec<(Vec<String>, Vec<NaiveTime>, Vec<NaiveDate>)> = vec![(Vec::new(), Vec::new(), Vec::new()); flights.len()];
    for (p, it) in &items {
        if matches!(it, Item::Flight(..)) {
            continue;
        }
        let next = flights.iter().position(|f| f.0 > *p).filter(|&i| flights[i].0 - p <= 600);
        let prev = flights.iter().rposition(|f| f.0 < *p).filter(|&i| p - flights[i].0 <= 600);
        let Some(best) = next.or(prev) else { continue };
        match it {
            Item::Airport(a) => {
                if !parts[best].0.contains(a) {
                    parts[best].0.push(a.clone());
                }
            }
            Item::Time(t) => parts[best].1.push(*t),
            Item::Date(d) => parts[best].2.push(*d),
            Item::Flight(..) => {}
        }
    }
    let code = confirmation(text, subject);
    let mut out: Vec<Found> = Vec::new();
    let mut last_arrival: Option<String> = None;
    let mut last_date: Option<NaiveDate> = None;
    for (i, (_, airline, number)) in flights.iter().enumerate() {
        let (airports_, times, dates) = &parts[i];
        // one airport only: a connection's other end is the next leg's origin (Delta lists each
        // stop once), else the last leg's destination
        let next_origin = parts.get(i + 1).and_then(|p| p.0.first()).cloned();
        let (origin, dest) = match airports_.as_slice() {
            [a, b, ..] => (a.clone(), b.clone()),
            [a] => match (&next_origin, &last_arrival) {
                (Some(n), _) if n != a => (a.clone(), n.clone()),
                (_, Some(l)) if l != a => (l.clone(), a.clone()),
                _ => continue,
            },
            [] => continue,
        };
        let Some(date) = dates.first().copied().or(last_date) else { continue };
        let Some(departs) = times.first() else { continue };
        let Some(start) = airport_utc(&origin, date.and_time(*departs)) else { continue };
        let (end, guessed) = match times.get(1).and_then(|t| airport_utc(&dest, date.and_time(*t))) {
            Some(mut e) => {
                while e <= start {
                    e += 86_400; // arrives the next day
                }
                if e - start > 20 * 3600 { (start + 2 * 3600, true) } else { (e, false) }
            }
            None => (start + 2 * 3600, true),
        };
        last_arrival = Some(dest.clone());
        last_date = Some(date);
        let key = format!("flight:{airline}{number}:{date}");
        if out.iter().any(|f| f.key == key) {
            continue;
        }
        let city = |c: &str| airports().get(c).map(|a| format!("{} ({c})", a.city)).unwrap_or_else(|| c.to_string());
        let local = |t: i64, code: &str| {
            let a = airports().get(code).map(|a| a.offset).unwrap_or(0);
            // (shown in the airport's own time, as the airline does)
            let shift = airport_utc(code, chrono::DateTime::from_timestamp(t, 0).unwrap().naive_utc()).map(|u| t - u).unwrap_or(a as i64 * 60);
            chrono::DateTime::from_timestamp(t + shift, 0).map(|d| d.format("%a %b %-d, %-I:%M %p").to_string()).unwrap_or_default()
        };
        let mut details = vec![format!("{airline} {number}: {} → {}", city(&origin), city(&dest)), format!("Departs {}, {} (local time)", city(&origin), local(start, &origin))];
        details.push(if guessed { "Arrival time: not in the email".to_string() } else { format!("Arrives {}, {} (local time)", city(&dest), local(end, &dest)) });
        if let Some(c) = &code {
            details.push(format!("Confirmation: {c}"));
        }
        out.push(Found {
            kind: "flight".into(),
            key,
            start,
            end,
            title: format!("✈ {airline} {number} {origin} → {dest}"),
            location: city(&origin),
            details: details.join("\n"),
            end_guessed: guessed,
            all_day: false,
        });
    }
    out
}

/// A hotel stay in a message: check-in and check-out dates by their labels (or schema.org's
/// LodgingReservation), the hotel's name from the data, the subject or the sender. On the
/// calendar from 7 PM of the check-in day to 10 AM of the check-out day (the computer's time).
pub fn stay_in(text: &str, html: &str, subject: &str, from: &str, sent: i64) -> Option<Found> {
    // after a stay ("Thank you for staying", "How was your stay"): nothing ahead in it
    let s = subject.to_lowercase();
    if ["thank you for staying", "thanks for staying", "enjoyed your stay", "how was your stay", "recent stay", "rate your stay", "review your stay", "your folio", "your receipt"].iter().any(|w| s.contains(w)) {
        return None;
    }
    let sent_day = chrono::DateTime::from_timestamp(sent, 0).unwrap_or_default().date_naive();
    let (mut checkin, mut checkout, mut name, mut address) = (None, None, None::<String>, None::<String>);
    // schema.org in the HTML (Airbnb, some hotel chains)
    for block in html.split("application/ld+json").skip(1) {
        let Some(start) = block.find('>') else { continue };
        let body = &block[start + 1..];
        let Some(end) = body.find("</script") else { continue };
        let Ok(v) = serde_json::from_str::<Value>(body[..end].trim()) else { continue };
        let list = if let Some(a) = v.as_array() { a.clone() } else { vec![v] };
        for r in list {
            if r["@type"].as_str() == Some("LodgingReservation") {
                let day = |k: &str| r[k].as_str().and_then(|s| NaiveDate::parse_from_str(&s[..s.len().min(10)], "%Y-%m-%d").ok());
                checkin = day("checkinDate").or(day("checkinTime"));
                checkout = day("checkoutDate").or(day("checkoutTime"));
                name = r["reservationFor"]["name"].as_str().map(str::to_string);
                let a = &r["reservationFor"]["address"];
                // (Airbnb's street address already has the city and state: not said twice)
                let mut parts: Vec<&str> = Vec::new();
                for k in ["streetAddress", "addressLocality", "addressRegion"] {
                    if let Some(v) = a[k].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                        if !parts.iter().any(|p| p.to_lowercase().contains(&v.to_lowercase())) {
                            parts.push(v);
                        }
                    }
                }
                address = (!parts.is_empty()).then(|| parts.join(", "));
            }
        }
    }
    if checkin.is_none() {
        let items = |label_set: &[&str]| -> Option<NaiveDate> {
            let lower = text.to_ascii_lowercase();
            for label in label_set {
                let mut from = 0;
                while let Some(p) = lower[from..].find(label) {
                    let at = from + p + label.len();
                    let window = cut(text, at, 60);
                    if let Some(d) = scan(window, sent_day, None).into_iter().find_map(|(_, it)| if let Item::Date(d) = it { Some(d) } else { None }) {
                        return Some(d);
                    }
                    from = at;
                }
            }
            None
        };
        checkin = items(&["check-in:", "check in:", "check-in date", "check in date", "checkin:", "arrival date", "check-in", "check in"]);
        checkout = items(&["check-out:", "check out:", "check-out date", "check out date", "checkout:", "departure date", "check-out", "check out"]);
    }
    let (Some(checkin), Some(checkout)) = (checkin, checkout) else { return None };
    if checkout <= checkin || (checkout - checkin).num_days() > 60 {
        return None;
    }
    let name = name
        .or_else(|| subject.split(" at ").nth(1).map(|s| s.trim().trim_end_matches('.').to_string()))
        .or_else(|| subject.strip_prefix("RE: Reservation for ").or(subject.strip_prefix("Reservation for ")).map(|s| s.split(',').next().unwrap_or(s).trim().to_string()))
        .unwrap_or_else(|| from.split('<').next().unwrap_or(from).trim().trim_matches('"').to_string());
    let local = |d: NaiveDate, h: u32| chrono::Local.from_local_datetime(&d.and_hms_opt(h, 0, 0).unwrap()).earliest().map(|t| t.timestamp());
    let (start, end) = (local(checkin, 19)?, local(checkout, 10)?);
    let mut details = vec![format!("Check in {} · check out {}", checkin.format("%a %b %-d"), checkout.format("%a %b %-d, %Y"))];
    if let Some(a) = &address {
        details.push(a.clone());
    }
    if let Some(c) = confirmation(text, subject) {
        details.push(format!("Confirmation: {c}"));
    }
    Some(Found {
        kind: "stay".into(),
        key: format!("stay:{}:{checkin}", name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).take(30).collect::<String>()),
        start,
        end,
        title: format!("🛏 {name}"),
        location: address.unwrap_or_else(|| name.clone()),
        details: details.join("\n"),
        end_guessed: false,
        all_day: false,
    })
}

/// A date and time in this computer's time zone as Unix seconds (trains, cars, rides: their
/// mail gives local times without a zone; most are near home).
fn local_ts(t: NaiveDateTime) -> Option<i64> {
    chrono::Local.from_local_datetime(&t).earliest().map(|x| x.timestamp())
}

/// The dates-with-times written after any of these labels (within `within` bytes of each).
fn labeled_times(text: &str, labels: &[&str], sent: NaiveDate, within: usize) -> Vec<(usize, NaiveDateTime)> {
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    for label in labels {
        let mut from = 0;
        while let Some(p) = lower[from..].find(label) {
            let at = from + p + label.len();
            let items = scan(cut(text, at, within), sent, None);
            let date = items.iter().find_map(|(_, it)| if let Item::Date(d) = it { Some(*d) } else { None });
            let time = items.iter().find_map(|(_, it)| if let Item::Time(t) = it { Some(*t) } else { None });
            if let (Some(d), Some(t)) = (date, time) {
                if !out.iter().any(|(_, x)| *x == d.and_time(t)) {
                    out.push((at, d.and_time(t)));
                }
            }
            from = at;
        }
    }
    out
}

/// The first line of text after a label ("Pick Up Location\nCharlotte Douglas Intl Airport,CLT").
fn line_after(text: &str, labels: &[&str]) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for label in labels {
        if let Some(p) = lower.find(label) {
            let rest = cut(text, p + label.len(), 200);
            let line = rest.trim_start_matches([':', ' ', '\t']).lines().map(str::trim).find(|l| !l.is_empty())?;
            let line: String = line.chars().take(80).collect();
            if !line.chars().any(|c| c.is_ascii_digit()) || line.len() > 6 {
                return Some(line.trim_end_matches(',').to_string());
            }
        }
    }
    None
}

/// Car rentals and car sharing, by the sender.
const RENTALS: [(&str, &str); 14] = [
    ("hertz", "Hertz"), ("avis", "Avis"), ("budget", "Budget"), ("enterprise", "Enterprise"), ("nationalcar", "National"), ("alamo", "Alamo"), ("sixt", "Sixt"),
    ("thrifty", "Thrifty"), ("dollar.com", "Dollar"), ("turo", "Turo"), ("zipcar", "Zipcar"), ("getaround", "Getaround"), ("foxrentacar", "Fox"), ("paylesscar", "Payless"),
];

/// A car rental (Hertz, Avis, Turo…): picked up and returned when and where the mail says; on
/// the calendar from pick-up to return, shown as free.
pub fn rental_in(text: &str, subject: &str, from: &str, sent: i64) -> Option<Found> {
    let f = from.to_ascii_lowercase();
    let company = RENTALS.iter().find(|(k, _)| f.contains(k))?.1;
    let s = subject.to_lowercase();
    if ["rate your", "how was", "receipt", "thanks for renting", "thank you for renting", "survey", "review"].iter().any(|w| s.contains(w)) {
        return None;
    }
    let sent_day = chrono::DateTime::from_timestamp(sent, 0).unwrap_or_default().date_naive();
    let picks = labeled_times(text, &["pick up date", "pickup date", "pick-up date", "pick up location", "pickup location", "pick-up location", "trip start", "pick up:", "pickup:", "pick-up:"], sent_day, 220);
    let returns = labeled_times(text, &["return date", "drop-off date", "drop off date", "return location", "drop-off location", "trip end", "return:", "drop-off:", "drop off:"], sent_day, 220);
    let mut all: Vec<NaiveDateTime> = picks.iter().chain(returns.iter()).map(|(_, t)| *t).collect();
    all.sort();
    let start = picks.first().map(|x| x.1).or_else(|| all.first().copied())?;
    let end = returns.iter().map(|x| x.1).chain(all.iter().copied()).filter(|t| *t > start).max();
    let place = line_after(text, &["pick up location", "pickup location", "pick-up location"]).or_else(|| subject.split(" at ").nth(1).map(|x| x.trim().to_string()));
    let (start_ts, end_ts) = (local_ts(start)?, end.and_then(local_ts));
    let mut details = vec![format!("Pick up {}{}", start.format("%a %b %-d, %-I:%M %p"), place.as_ref().map(|p| format!(" at {p}")).unwrap_or_default())];
    details.push(match end {
        Some(e) => format!("Return {}", e.format("%a %b %-d, %-I:%M %p")),
        None => "Return: not in the email".into(),
    });
    if let Some(c) = subject.split('#').nth(1).map(|x| x.split_whitespace().next().unwrap_or("").to_string()).filter(|x| x.len() >= 5).or_else(|| confirmation(text, subject)) {
        details.push(format!("Confirmation: {c}"));
    }
    Some(Found {
        kind: "car".into(),
        key: format!("car:{}:{}", company.to_lowercase(), start.format("%Y-%m-%dT%H%M")),
        start: start_ts,
        end: end_ts.unwrap_or(start_ts + 3600),
        title: format!("🚗 {company} rental{}", place.as_ref().map(|p| format!(" · {p}")).unwrap_or_default()),
        location: place.unwrap_or_default(),
        details: details.join("\n"),
        end_guessed: end_ts.is_none(),
        all_day: false,
    })
}

/// A ride booked ahead (Uber Reserve, Lyft's scheduled rides): never the receipts of rides taken.
pub fn ride_in(text: &str, subject: &str, from: &str, sent: i64) -> Option<Found> {
    let f = from.to_ascii_lowercase();
    let company = if f.contains("uber") { "Uber" } else if f.contains("lyft") { "Lyft" } else { return None };
    let s = subject.to_lowercase();
    if !["reserve", "scheduled", "upcoming ride", "ride is booked", "ride booked", "reservation"].iter().any(|w| s.contains(w)) || s.contains("receipt") {
        return None;
    }
    let sent_day = chrono::DateTime::from_timestamp(sent, 0).unwrap_or_default().date_naive();
    let at = labeled_times(text, &["pickup", "pick-up", "pick up", "scheduled for", "reserved for"], sent_day, 160).first().map(|x| x.1)?;
    let place = line_after(text, &["pickup location", "pick-up location", "pickup address", "from:"]);
    let start = local_ts(at)?;
    Some(Found {
        kind: "ride".into(),
        key: format!("ride:{}:{}", company.to_lowercase(), at.format("%Y-%m-%dT%H%M")),
        start,
        end: start + 1800,
        title: format!("🚕 {company} ride{}", place.as_ref().map(|p| format!(" from {p}")).unwrap_or_default()),
        location: place.unwrap_or_default(),
        details: format!("Pickup {}", at.format("%a %b %-d, %-I:%M %p")),
        end_guessed: true,
        all_day: false,
    })
}

/// Events with tickets or a registration, by the sender: (who, name). Shows and concerts,
/// Broadway, movies, games, and conferences, seminars and trade shows.
pub const EVENT_SELLERS: [(&str, &str); 40] = [
    ("ticketmaster", "Ticketmaster"), ("livenation", "Live Nation"), ("axs.com", "AXS"), ("eventbrite", "Eventbrite"), ("seatgeek", "SeatGeek"), ("stubhub", "StubHub"),
    ("vividseats", "Vivid Seats"), ("telecharge", "Telecharge"), ("ticketweb", "TicketWeb"), ("etix", "Etix"), ("dice.fm", "DICE"), ("todaytix", "TodayTix"),
    ("broadway.com", "Broadway.com"), ("ovationtix", "OvationTix"), ("showclix", "ShowClix"), ("tixr", "Tixr"), ("seetickets", "See Tickets"), ("eventim", "Eventim"),
    ("frontgatetickets", "Front Gate Tickets"), ("gametime", "Gametime"), ("ticketleap", "TicketLeap"), ("prekindle", "Prekindle"), ("universe.com", "Universe"), ("fevo", "Fevo"),
    // movies
    ("fandango", "Fandango"), ("amctheatres", "AMC"), ("regmovies", "Regal"), ("cinemark", "Cinemark"), ("atomtickets", "Atom Tickets"), ("alamodrafthouse", "Alamo Drafthouse"),
    // conferences, seminars, trade shows (registration services)
    ("cvent", "Cvent"), ("swapcard", "Swapcard"), ("whova", "Whova"), ("bizzabo", "Bizzabo"), ("mapyourshow", "Map Your Show"), ("a2zinc", "a2z"),
    ("eventscribe", "eventScribe"), ("regfox", "RegFox"), ("splashthat", "Splash"), ("zoomevents", "Zoom Events"),
];

/// An event with tickets or a registration: from a ticket seller's or registration service's
/// order, ticket or registration email (never its promotions), its name, when and where; with
/// a time, on the calendar for three hours (the end isn't in the mail); with only dates (a
/// conference: "November 18 – 20, 2026"), all day over each of them. Its tickets go onto the
/// event.
pub fn event_in(text: &str, subject: &str, from: &str, sent: i64) -> Option<Found> {
    let f = from.to_ascii_lowercase();
    // a ticket seller's email, else any email that holds a ticket order (a venue's own box office, a forwarded
    // order): seats, a row or a section, and tickets or an order; never a travel company's (an airline's seat
    // is a flight)
    let known = EVENT_SELLERS.iter().find(|(k, _)| f.contains(k)).map(|x| x.1);
    let lower_all = text.to_lowercase();
    let seated = ["seat(s)", "seats:", "seat:", "seats "].iter().any(|w| lower_all.contains(w)) || (lower_all.contains("row") && (lower_all.contains("section") || lower_all.contains("seat")));
    let order_in_text = seated
        && (lower_all.contains("ticket") || lower_all.contains("order"))
        && !crate::views::TRAVEL_DOMAINS.iter().any(|d| f.contains(d));
    if known.is_none() && !order_in_text {
        return None;
    }
    let seller = known.unwrap_or("the email");
    let s = subject.to_lowercase();
    // an order, the tickets or a registration: words that say they're yours ("Get tickets for…",
    // "$16 Tickets to…" are ads); not a sale, a presale or a "rate your experience"
    let ordered = [
        "your tickets", "you got tickets", "got your tickets", "your ticket", "tickets are ready", "your order", "order confirm", "order #", "confirmation", "e-ticket", "eticket",
        "mobile tickets", "ticket transfer", "you're going", "you are going", "registration", "you're registered", "you are registered", "registered for",
        "your badge", "booking confirmed", "purchase confirmed", "sent you ticket",
    ]
    .iter()
    .any(|w| s.contains(w));
    let selling = [
        "presale", "on sale", "% off", "just announced", "don't miss", "recommended", "rate your", "how was", "survey", "review", "sale ends", "last chance", "new dates", "added a show",
        "fans also", "refund", "cancel", "early bird", "register now", "registration is open", "registration opens", "save the date", "call for", "exhibit", "sponsor",
    ]
    .iter()
    .any(|w| s.contains(w));
    // (from anyone, the subject must say it's an order: an alert or an ad naming shows with seats
    // and tickets in its text isn't one; 2026-10-26, a concert alert's "Tomorrow: … at Riverside Hall")
    if !ordered || selling {
        return None;
    }
    let sent_day = chrono::DateTime::from_timestamp(sent, 0).unwrap_or_default().date_naive();
    let name_keys = [
        "tickets to ", "tickets for ", "ticket to ", "ticket for ", "order for ", "confirmation for ", "you're going to ", "you are going to ", "registration for ",
        "registered for ", "registration confirmation: ", "registration confirmation - ", "ticket(s) for ", "ticket(s) to ",
    ];
    let lower_text = text.to_lowercase();
    // the subject's, else the email's ("accept your tickets for THE PAPER LANTERNS with Juniper Row."), else the subject
    let name = name_keys
        .iter()
        .find_map(|k| s.find(k).map(|p| subject[p + k.len()..].to_string()))
        .or_else(|| {
            ["tickets for ", "tickets to ", "seeing ", "ticket(s) for "].iter().find_map(|k| {
                let p = lower_text.find(k)? + k.len();
                let rest = text.get(p..)?;
                let end = rest.find(['.', '\n', '!', ',']).unwrap_or(rest.len()).min(80);
                Some(rest[..end].to_string()).filter(|n| n.trim().len() >= 3)
            })
        })
        .unwrap_or_else(|| subject.to_string());
    let name = name.trim().trim_end_matches(['!', '.']).trim_matches(['"', '“', '”', ' ']).to_string();
    let name: String = if name.is_empty() { format!("{seller} event") } else { name.chars().take(90).collect() };
    let venue = line_after(text, &["venue:", "venue\n", "location:", "where:", "location\n", "theatre:", "theater:"]);
    // the time: after a label (the show's before the doors'), else the first date with a time
    let labeled = labeled_times(text, &["date and time", "date & time", "event date", "show date", "showtime", "show time", "date:", "when:", "when ", "starts", "doors"], sent_day, 200);
    // (an event is after its tickets are sent: never a time before the email or within half an
    // hour of it, a header or "sent at" line; 2026-10-23, SeatGeek's email with no show date)
    let not_the_email = |t: &NaiveDateTime| local_ts(*t).is_some_and(|ts| ts - sent >= 1800);
    let mut date_at: Option<usize> = labeled.iter().find(|(_, t)| not_the_email(t)).map(|(p, _)| *p);
    let timed = labeled.iter().map(|(_, t)| *t).find(not_the_email).or_else(|| {
        let items = scan(text, sent_day, None);
        items.iter().enumerate().find_map(|(i, (p, it))| {
            let Item::Date(d) = it else { return None };
            let t = items[i + 1..].iter().take_while(|(q, _)| *q < p + 80).find_map(|(_, x)| if let Item::Time(t) = x { Some(d.and_time(*t)) } else { None })?;
            if !not_the_email(&t) {
                return None;
            }
            date_at = Some(*p);
            Some(t)
        })
    });
    // the venue: after a label, else the line after the date's ("Sat, Nov 7 @ 7:30 PM\nBayside
    // Music Hall, Port Ellis, FL")
    let venue = venue.or_else(|| {
        let after = text.get(date_at?..)?;
        let line = after.lines().skip(1).map(str::trim).find(|l| !l.is_empty())?;
        let place = |l: &str| l.len() <= 90 && l.contains(',') && l.chars().filter(|c| c.is_alphabetic()).count() >= 6 && !l.to_ascii_lowercase().starts_with("section");
        // (a short name of a hall on its own line, before the date: "Harbor Hall")
        let hall = |l: &str| (4..=50).contains(&l.len()) && l.chars().next().is_some_and(char::is_uppercase) && !l.chars().any(|c| c.is_ascii_digit()) && l.split_whitespace().count() <= 6 && l != l.to_uppercase();
        if place(line) {
            return Some(line.to_string());
        }
        // (or the line before it: SeatGeek writes the place, then the date)
        let head = text.get(..date_at?)?;
        let before = head.rsplit_once('\n').map(|(b, _)| b).unwrap_or("").lines().map(str::trim).rfind(|l| !l.is_empty())?;
        (place(before) || hall(before)).then(|| before.to_string())
    });
    let key_name: String = name.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
    let mut details = Vec::new();
    let found = match timed {
        Some(at) => {
            let start = local_ts(at)?;
            details.push(format!("{name}: {}", at.format("%a %b %-d, %Y, %-I:%M %p")));
            Found {
                kind: "event".into(),
                key: format!("event:{key_name}:{}", at.format("%Y-%m-%dT%H%M")),
                start,
                end: start + 3 * 3600,
                title: format!("🎟 {name}"),
                location: venue.clone().unwrap_or_default(),
                details: String::new(),
                end_guessed: true,
                all_day: false,
            }
        }
        None => {
            // only dates (a conference): the first ahead of the email, and the last of a range
            // written with it ("November 18 – 20, 2026")
            let items = scan(cut(text, 0, 6_000), sent_day, None);
            let dates: Vec<(usize, NaiveDate)> = items.iter().filter_map(|(p, it)| if let Item::Date(d) = it { Some((*p, *d)) } else { None }).collect();
            let (p1, first) = *dates.iter().find(|(_, d)| *d >= sent_day)?;
            // the range's end: a whole date ("Nov 30 – Dec 2"), or a day of the same month after a
            // dash ("November 18 - 20, 2026")
            let same_month = || -> Option<NaiveDate> {
                let after = cut(text, p1, 40);
                let dash = after.find(['-', '–', '—'])?;
                let rest = after[dash..].trim_start_matches(['-', '–', '—', ' ']);
                let day: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if day.is_empty() || day.len() > 2 || rest[day.len()..].starts_with(':') {
                    return None;
                }
                first.with_day(day.parse().ok()?).filter(|d| *d > first)
            };
            let last = dates
                .iter()
                .find(|(p, d)| *p > p1 && *p < p1 + 60 && *d > first && (*d - first).num_days() <= 14)
                .map(|x| x.1)
                .or_else(same_month)
                .unwrap_or(first);
            let midnight = |d: NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp());
            details.push(if last > first { format!("{name}: {} – {}", first.format("%a %b %-d"), last.format("%a %b %-d, %Y")) } else { format!("{name}: {}", first.format("%a %b %-d, %Y")) });
            Found {
                kind: "event".into(),
                key: format!("event:{key_name}:{}", first.format("%Y-%m-%d")),
                start: midnight(first)?,
                end: midnight(last + chrono::Duration::days(1))?,
                title: format!("🎟 {name}"),
                location: venue.clone().unwrap_or_default(),
                details: String::new(),
                end_guessed: false,
                all_day: true,
            }
        }
    };
    if let Some(v) = &venue {
        details.push(format!("Venue: {v}"));
    }
    if let Some(c) = confirmation(text, subject) {
        details.push(format!("Order: {c}"));
    }
    details.push(format!("From {seller}"));
    Some(Found { details: details.join("\n"), ..found })
}

/// Trains, buses and ferries, by the sender: (who, kind).
const CARRIERS: [(&str, &str, &str); 18] = [
    ("gobrightline", "Brightline", "train"), ("amtrak", "Amtrak", "train"), ("njtransit", "NJ Transit", "train"), ("viarail", "VIA Rail", "train"), ("eurostar", "Eurostar", "train"),
    ("trenitalia", "Trenitalia", "train"), ("italotreno", "Italo", "train"), ("sncf", "SNCF", "train"), ("greyhound", "Greyhound", "bus"), ("flixbus", "FlixBus", "bus"),
    ("megabus", "Megabus", "bus"), ("peterpanbus", "Peter Pan", "bus"), ("ourbus", "OurBus", "bus"), ("seastreak", "Seastreak", "ferry"), ("nywaterway", "NY Waterway", "ferry"),
    ("cmlf", "Cape May–Lewes Ferry", "ferry"), ("steamshipauthority", "Steamship Authority", "ferry"), ("bcferries", "BC Ferries", "ferry"),
];

/// Trains, buses and ferries: each leg written as "<station> Departs <time> … <station>
/// Arrives <time>" (Brightline and others), under the nearest date before it.
pub fn transit_in(text: &str, subject: &str, from: &str, sent: i64) -> Vec<Found> {
    let f = from.to_ascii_lowercase();
    let Some(&(_, carrier, kind)) = CARRIERS.iter().find(|(k, _, _)| f.contains(k)) else { return Vec::new() };
    let sent_day = chrono::DateTime::from_timestamp(sent, 0).unwrap_or_default().date_naive();
    let station_before = |p: usize| -> String {
        let line = text[..p].rsplit('\n').next().unwrap_or("");
        line.rsplit("  ").next().unwrap_or(line).trim().trim_end_matches(':').chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<String>().trim().to_string()
    };
    let first_time = |p: usize, len: usize| scan(cut(text, p, len), sent_day, None).into_iter().find_map(|(_, it)| if let Item::Time(t) = it { Some(t) } else { None });
    let code = confirmation(text, subject).or_else(|| {
        let l = text.to_ascii_lowercase();
        l.find("ticket number:").map(|p| cut(text, p + 14, 20).split_whitespace().next().unwrap_or("").to_string()).filter(|c| c.len() >= 5)
    });
    let mut out: Vec<Found> = Vec::new();
    let mut from_pos = 0;
    while let Some(p) = text[from_pos..].find("Departs").map(|x| x + from_pos) {
        from_pos = p + 7;
        let Some(arr) = text[p..].find("Arrives").map(|x| x + p).filter(|a| a - p < 400) else { continue };
        let (origin, dest) = (station_before(p), station_before(arr));
        let (Some(dep_t), Some(arr_t)) = (first_time(p + 7, 40), first_time(arr + 7, 40)) else { continue };
        let date = scan(cut(text, p.saturating_sub(800), p.min(800)), sent_day, None).into_iter().filter_map(|(_, it)| if let Item::Date(d) = it { Some(d) } else { None }).last();
        let Some(date) = date else { continue };
        let lower = cut(text, arr, 300).to_ascii_lowercase();
        let number = ["train number:", "train #", "train no.", "bus number:", "bus #", "trip number:", "vessel:"].iter().find_map(|l| {
            lower.find(l).map(|q| lower[q + l.len()..].trim_start().chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>()).filter(|n| !n.is_empty())
        });
        let (Some(start), Some(mut end)) = (local_ts(date.and_time(dep_t)), local_ts(date.and_time(arr_t))) else { continue };
        if end <= start {
            end += 86_400;
        }
        let key = format!("{kind}:{}:{}:{}", carrier.to_lowercase(), number.clone().unwrap_or_else(|| origin.to_lowercase()), date.and_time(dep_t).format("%Y-%m-%dT%H%M"));
        if out.iter().any(|x| x.key == key) || origin.is_empty() || dest.is_empty() {
            continue;
        }
        let icon = match kind {
            "bus" => "🚌",
            "ferry" => "⛴",
            _ => "🚆",
        };
        let mut details = vec![format!("{carrier}{}: {origin} → {dest}", number.as_ref().map(|n| format!(" {kind} {n}")).unwrap_or_default())];
        details.push(format!("Departs {origin}, {}", date.and_time(dep_t).format("%a %b %-d, %-I:%M %p")));
        details.push(format!("Arrives {dest}, {}", arr_t.format("%-I:%M %p")));
        if let Some(c) = &code {
            details.push(format!("Confirmation: {c}"));
        }
        out.push(Found {
            kind: kind.into(),
            key,
            start,
            end,
            title: format!("{icon} {carrier}{} {origin} → {dest}", number.map(|n| format!(" {n}")).unwrap_or_default()),
            location: origin,
            details: details.join("\n"),
            end_guessed: false,
            all_day: false,
        });
    }
    out
}

/// Everything a message tells about trips ahead or past: flights, stays, rentals, rides,
/// trains / buses / ferries. Its text: the plain-text part, else the HTML read as text (Breeze
/// sends its HTML as the "plain text").
pub fn found_in(raw: &[u8], sent: i64) -> (Vec<Found>, String) {
    let d = crate::parse::display(raw, now());
    let mut text = if d.text.trim().is_empty() { d.parsed.body_text.clone() } else { d.text.clone() };
    let head = text.trim_start().get(..200).unwrap_or(text.trim_start()).to_ascii_lowercase();
    if head.starts_with('<') && (head.contains("<html") || head.contains("<!doctype") || head.contains("<table")) {
        text = mail_parser::decoders::html::html_to_text(&text);
    }
    let html = d.html.as_deref().unwrap_or("");
    if text.trim().is_empty() && !html.is_empty() {
        text = mail_parser::decoders::html::html_to_text(html);
    }
    // no-break and narrow spaces read as spaces ("7:30\u{202f}PM": SeatGeek, 2026-10-23)
    let text = text.replace(['\u{a0}', '\u{202f}', '\u{2009}'], " ");
    let subject = d.parsed.subject.clone();
    let from = d.parsed.from_display();
    let mut found = flights_in(&text, &subject, &from, sent);
    found.extend(stay_in(&text, html, &subject, &from, sent));
    found.extend(rental_in(&text, &subject, &from, sent));
    found.extend(ride_in(&text, &subject, &from, sent));
    found.extend(transit_in(&text, &subject, &from, sent));
    // (an event: a ticket seller's, or any ticket order when nothing else was found in it)
    if found.is_empty() || EVENT_SELLERS.iter().any(|(k, _)| from.to_ascii_lowercase().contains(k)) {
        found.extend(event_in(&text, &subject, &from, sent));
    }
    (found, subject)
}

/// The kinds that go on the calendar by themselves (getting somewhere at a set time); stays and
/// rentals are offered.
pub fn adds_itself(kind: &str) -> bool {
    matches!(kind, "flight" | "train" | "bus" | "ferry" | "ride" | "event")
}

/// A reservation, as kept and shown.
#[derive(Clone, Debug, Serialize)]
pub struct Reservation {
    pub key: String,
    pub kind: String,
    pub start: i64,
    pub end: i64,
    pub title: String,
    pub location: String,
    pub details: String,
    /// added (on the calendar by Eudora), exists (already there), suggested (to ask), declined
    pub status: String,
    pub message: i64,
    pub subject: String,
    /// The email's tickets, boarding passes, receipts… (its attachments that aren't pictures).
    pub files: Vec<TripFile>,
}

/// A file attached to a reservation's email, and what it is to the traveler.
#[derive(Clone, Debug, Serialize)]
pub struct TripFile {
    pub part: String,
    pub name: String,
    /// "Boarding pass", "Ticket", "Receipt", "Invoice", "Wallet pass", "Itinerary"…
    pub label: String,
}

/// What an attachment of travel mail is, by its name (Brightline: "QPW8RN_P1_….pdf" is
/// passenger 1's boarding pass, "…_INV_…" the invoice, "…_rcp" the receipt).
fn file_label(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    let passenger = n.split(['_', '-', ' ']).find_map(|w| w.strip_prefix('p').filter(|d| !d.is_empty() && d.len() <= 2 && d.chars().all(|c| c.is_ascii_digit())).map(str::to_string));
    if n.ends_with(".pkpass") {
        "Wallet pass".into()
    } else if n.contains("boarding") || n.contains("bpass") {
        "Boarding pass".into()
    } else if let Some(p) = passenger {
        format!("Boarding pass (passenger {p})")
    } else if n.contains("inv") {
        "Invoice".into()
    } else if n.contains("rcp") || n.contains("receipt") {
        "Receipt".into()
    } else if n.contains("itin") {
        "Itinerary".into()
    } else if n.ends_with(".ics") {
        "Calendar file".into()
    } else {
        "Ticket".into()
    }
}

/// The legs worth keeping from one email: all of them when any is still ahead (an outbound
/// train already gone and its return to come are one trip), else none (only trips ahead).
fn ahead(found: Vec<Found>) -> Vec<Found> {
    if found.iter().any(|f| f.end > now()) { found } else { Vec::new() }
}

/// What a scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TravelScan {
    pub messages: u64,
    pub flights_added: u64,
    pub stays_suggested: u64,
    pub already_there: u64,
}

/// How far back the first scan reads Travel messages (bookings are made up to a year ahead).
const LOOK_BACK: i64 = 365 * 86_400;

impl Library {
    /// Reads the Travel messages not read yet (the first time: those of the last year), keeps
    /// the reservations still ahead, adds flights to the calendar when nothing there has them,
    /// and lists hotel stays to offer. The writer only (the download). `limit` messages a call.
    pub fn scan_travel(&self, limit: usize) -> Result<TravelScan> {
        let mut done = TravelScan::default();
        // (a mark of its own: the scan was widened 2026-10-19 to travel companies' mail outside
        // the Travel view, so the last year is read once more)
        let mark: i64 = self.setting("travel-mark-6")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let rows = self.travel_messages(Some(mark), Some(limit))?;
        let Some(&(last, _)) = rows.last() else { return Ok(done) };
        for (id, date) in &rows {
            done.messages += 1;
            let Ok(raw) = self.raw_message(*id) else { continue };
            let (found, subject) = found_in(&raw, *date);
            let found = self.without_ads(*id, found)?;
            for f in ahead(found) {
                match self.take_reservation(*id, &subject, &f)? {
                    Some("added") => done.flights_added += 1,
                    Some("suggested") => done.stays_suggested += 1,
                    Some("exists") => done.already_there += 1,
                    _ => {}
                }
            }
        }
        self.set_setting("travel-mark-6", &last.to_string())?;
        Ok(done)
    }

    /// The messages the trip reader reads: the last year's in the Travel view, and the last
    /// year's from travel companies (airlines, trains, buses, ferries, rentals, rides) wherever
    /// they're filed; after `mark` (a message id), oldest id first.
    fn travel_messages(&self, mark: Option<i64>, limit: Option<usize>) -> Result<Vec<(i64, i64)>> {
        // (ticket sellers too: shows and events with tickets, 2026-10-23)
        let senders: Vec<String> = crate::views::TRAVEL_DOMAINS
            .iter()
            .copied()
            .chain(EVENT_SELLERS.iter().map(|(k, _)| *k))
            .map(|d| format!("m.from_display LIKE '%{}%'", d.replace('\'', "")))
            .collect();
        let sql = format!(
            "SELECT m.id, m.date FROM messages m WHERE m.id > ?1 AND m.date > ?2
               AND (EXISTS (SELECT 1 FROM tags t WHERE t.message = m.id AND t.tag = 'travel') OR {}
                    OR lower(m.subject) LIKE '%ticket%' OR lower(m.snippet) LIKE '%ticket%' OR lower(m.snippet) LIKE '%your order%' OR lower(m.snippet) LIKE '%order details%' OR lower(m.snippet) LIKE '%seat%')
             ORDER BY m.id LIMIT ?3",
            senders.join(" OR ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![mark.unwrap_or(0), now() - LOOK_BACK, limit.map(|l| l as i64).unwrap_or(i64::MAX)], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A dry run: the reservations ahead in the last year's travel mail, each with the message it
    /// came from and whether the calendar has it already. Changes nothing.
    pub fn travel_preview(&self) -> Result<Vec<(Found, i64, String, bool)>> {
        let rows = self.travel_messages(None, None)?;
        let mut out: Vec<(Found, i64, String, bool)> = Vec::new();
        for (id, date) in rows {
            let Ok(raw) = self.raw_message(id) else { continue };
            let (found, subject) = found_in(&raw, date);
            let found = self.without_ads(id, found)?;
            for f in ahead(found) {
                if out.iter().any(|(o, ..)| o.key == f.key) {
                    continue;
                }
                let has = self.calendar_has(&f)?;
                out.push((f, id, subject.clone(), has));
            }
        }
        out.sort_by_key(|(f, ..)| f.start);
        Ok(out)
    }

    /// One reservation found: kept once (by its key); a flight put on the calendar when nothing
    /// there has it, a stay kept to offer. Returns what became of it (None: known already).
    fn take_reservation(&self, message: i64, subject: &str, f: &Found) -> Result<Option<&'static str>> {
        let known: Option<String> = self.conn.query_row("SELECT status FROM reservations WHERE key = ?1", params![f.key], |r| r.get(0)).optional()?;
        match known.as_deref() {
            // "already on the calendar" checked again (a wrong match taken back: 2026-10-20)
            Some("exists") if self.calendar_match(f)?.is_none() => {
                self.conn.execute("DELETE FROM reservations WHERE key = ?1", params![f.key])?;
            }
            Some(_) => {
                // a later email of a trip already known (the check-in email with the boarding
                // pass): its tickets go onto the same event
                self.add_ticket_source(&f.key, message)?;
                return Ok(None);
            }
            None => {}
        }
        // a changed booking: a newer email of the same booking (confirmation, route, day) with
        // another train, flight or time replaces the older reservation (its event too, if
        // Eudora made one)
        if self.supersede(message, f)? {
            self.conn.execute(
                "INSERT INTO reservations (key, kind, start, end, title, location, details, status, message, subject, found) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'hidden', ?8, ?9, ?10)",
                params![f.key, f.kind, f.start, f.end, f.title, f.location, f.details, message, subject, now()],
            )?;
            return Ok(Some("hidden"));
        }
        let (status, event) = match self.calendar_match(f)? {
            Some(e) => ("exists", Some(e)),
            None if adds_itself(&f.kind) => match self.add_reservation_event(message, f)? {
                Some(e) => ("added", Some(e)),
                None => ("suggested", None), // no Microsoft calendar to put it on: offered later
            },
            None => ("suggested", None),
        };
        self.conn.execute(
            "INSERT INTO reservations (key, kind, start, end, title, location, details, status, message, subject, found, event) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![f.key, f.kind, f.start, f.end, f.title, f.location, f.details, status, message, subject, now(), event],
        )?;
        Ok(Some(status))
    }

    /// Older reservations this one replaces: the same kind and confirmation code, the same route
    /// on the same day, from an older email. They leave Trips; an event Eudora made for one goes.
    fn supersede(&self, message: i64, f: &Found) -> Result<bool> {
        let code = |details: &str| details.lines().find_map(|l| l.strip_prefix("Confirmation: ")).map(str::to_string);
        // "✈ UA 1893 BOS → RSW" / "🚆 Brightline 5217 West Palm Beach → Aventura": the route
        let route = |title: &str| title.split_whitespace().skip(3).collect::<Vec<_>>().join(" ");
        let (Some(my_code), my_route) = (code(&f.details), route(&f.title)) else { return Ok(false) };
        let mut newer = false;
        let day = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(&chrono::Local).date_naive());
        let my_date: i64 = self.conn.query_row("SELECT date FROM messages WHERE id = ?1", params![message], |r| r.get(0)).unwrap_or(0);
        let rows: Vec<(String, String, String, i64, i64, String, Option<i64>)> = {
            let mut stmt = self.conn.prepare(
                "SELECT r.key, r.title, r.details, r.start, COALESCE(m.date, 0), r.status, r.event FROM reservations r LEFT JOIN messages m ON m.id = r.message
                 WHERE r.kind = ?1 AND r.key <> ?2 AND r.status <> 'hidden'",
            )?;
            stmt.query_map(params![f.kind, f.key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (key, title, details, start, sent, status, event) in rows {
            if code(&details).as_deref() != Some(my_code.as_str()) || route(&title) != my_route || day(start) != day(f.start) {
                continue;
            }
            if sent > my_date {
                newer = true; // (this email is the older one: the newer stands)
                continue;
            }
            if status == "added" {
                if let Some(e) = event.filter(|e| self.conn.query_row("SELECT 1 FROM events WHERE id = ?1", params![e], |_| Ok(())).is_ok()) {
                    self.event_delete(e)?;
                }
            }
            self.conn.execute("UPDATE reservations SET status = 'hidden', event = NULL WHERE key = ?1", params![key])?;
        }
        Ok(newer)
    }

    /// Whether the calendar has this already.
    fn calendar_has(&self, f: &Found) -> Result<bool> {
        Ok(self.calendar_match(f)?.is_some())
    }

    /// The calendar's event for it, if it has one: an event near the time naming the flight
    /// number (or both airports), the train's number (or both stations), the rental or ride
    /// company, or, for a stay, an event over its first evening naming the place.
    fn calendar_match(&self, f: &Found) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT subject, location, event, id FROM events WHERE start < ?2 AND end > ?1 AND start > ?1 - 400 * 86400")?;
        let (from, to) = if f.kind == "stay" { (f.start - 86_400, f.start + 86_400) } else { (f.start - 6 * 3600, f.start + 6 * 3600) };
        let rows: Vec<(String, String, String, i64)> = stmt.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let norm = |s: &str| s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
        Ok(rows.iter().find(|(subject, location, event, _)| {
            let hay = norm(&format!("{subject} {location} {event}"));
            if f.kind == "flight" {
                let number = f.key.split(':').nth(1).unwrap_or("");
                let digits: String = number.chars().skip(2).collect();
                let airports_: Vec<String> = f.title.split_whitespace().filter(|w| w.len() == 3 && w.chars().all(|c| c.is_ascii_uppercase())).map(|w| w.to_ascii_lowercase()).collect();
                hay.contains(&norm(number)) || (!digits.is_empty() && hay.contains(&format!("flight{digits}"))) || (airports_.len() == 2 && airports_.iter().all(|a| hay.contains(a)))
            } else if matches!(f.kind.as_str(), "train" | "bus" | "ferry") {
                // its number, or its carrier and both stations: never the cities alone (a flight
                // between the same two cities isn't this train: 2026-10-20, Brightline vs Breeze)
                let number = f.key.split(':').nth(2).unwrap_or("");
                let carrier = norm(f.title.split_whitespace().nth(1).unwrap_or(""));
                let ends: Vec<String> = f.title.split(" → ").map(|s| norm(s.rsplit(' ').next().unwrap_or(""))).collect();
                (number.chars().all(|c| c.is_ascii_digit()) && number.len() >= 3 && hay.contains(number) && hay.contains(&carrier))
                    || (carrier.len() >= 3 && hay.contains(&carrier) && ends.len() == 2 && ends.iter().all(|e| e.len() >= 4 && hay.contains(e)))
            } else if matches!(f.kind.as_str(), "car" | "ride") {
                // the company's name near the time ("Avis", "Uber")
                let company = norm(f.title.split_whitespace().nth(1).unwrap_or(""));
                company.len() >= 3 && hay.contains(&company)
            } else {
                let name = norm(f.title.trim_start_matches("🛏 "));
                let first: String = name.chars().take(10).collect();
                // (an event either way round: "Paper Lanterns" on the calendar is "THE PAPER LANTERNS with Juniper Row"'s
                // tickets too)
                let theirs = norm(subject);
                (first.len() >= 4 && hay.contains(&first)) || (f.kind == "event" && theirs.len() >= 4 && name.contains(&theirs))
            }
        }).map(|r| r.3))
    }

    /// Puts a reservation on the calendar: the default calendar of the Microsoft account the
    /// message came to (else the first one's), here at once and to the server through the queue.
    fn add_reservation_event(&self, message: i64, f: &Found) -> Result<Option<i64>> {
        let calendar: Option<i64> = self
            .conn
            .query_row(
                "SELECT c.id FROM calendars c JOIN accounts a ON a.id = c.account
                 WHERE c.is_default AND c.can_edit
                 ORDER BY (a.id IN (SELECT fo.account FROM locations l JOIN folders fo ON fo.id = l.folder WHERE l.message = ?1)) DESC, a.position, a.id LIMIT 1",
                params![message],
                |r| r.get(0),
            )
            .optional()?;
        let Some(calendar) = calendar else { return Ok(None) };
        let iso = |t: i64| Utc.timestamp_opt(t, 0).single().unwrap_or_default().format("%Y-%m-%dT%H:%M:%S").to_string();
        let body = format!("{}\n\nAdded by Eudora from your email.", f.details);
        let graph = json!({
            "subject": f.title,
            "start": { "dateTime": iso(f.start), "timeZone": "UTC" },
            "end": { "dateTime": iso(f.end), "timeZone": "UTC" },
            "location": { "displayName": f.location },
            "body": { "contentType": "text", "content": body },
            // a conference over whole days: all day (UTC midnights, as the calendar keeps them)
            "isAllDay": f.all_day,
            // a stay and a rental span days: free; getting somewhere: busy, with a reminder
            "showAs": if matches!(f.kind.as_str(), "stay" | "car") { "free" } else { "busy" },
            "isReminderOn": adds_itself(&f.kind) && !f.all_day,
            "reminderMinutesBeforeStart": match f.kind.as_str() { "flight" => 180, "event" => 120, "ride" => 30, k if adds_itself(k) => 60, _ => 0 },
            "categories": ["Travel"],
        });
        self.create_event(calendar, &graph).map(Some)
    }

    /// The reservations ahead (and the legs already gone of a trip still going: its email's
    /// others), with each email's tickets and passes; those removed from Trips left out.
    pub fn reservations(&self) -> Result<Vec<Reservation>> {
        // the last weeks' too: a leg already gone belongs while its trip goes on (the next leg
        // within three days, as the Trips view groups them; 2026-10-20: a changed outbound train
        // in an email of its own)
        let mut stmt = self.conn.prepare(
            "SELECT key, kind, start, end, title, location, details, status, message, subject FROM reservations
             WHERE status <> 'hidden' AND end > ?1 ORDER BY start",
        )?;
        let rows = stmt.query_map(params![now() - 30 * 86_400], |r| {
            Ok(Reservation {
                key: r.get(0)?,
                kind: r.get(1)?,
                start: r.get(2)?,
                end: r.get(3)?,
                title: r.get(4)?,
                location: r.get(5)?,
                details: r.get(6)?,
                status: r.get(7)?,
                message: r.get(8)?,
                subject: r.get(9)?,
                files: Vec::new(),
            })
        })?;
        let all: Vec<Reservation> = rows.collect::<rusqlite::Result<_>>()?;
        // into trips (each leg within three days of the last); keep the trips with a leg ahead
        let mut list: Vec<Reservation> = Vec::new();
        let mut trip: Vec<Reservation> = Vec::new();
        let mut trip_end = i64::MIN;
        let flush = |trip: &mut Vec<Reservation>, list: &mut Vec<Reservation>| {
            if trip.iter().any(|r| r.end > now()) {
                list.append(trip);
            }
            trip.clear();
        };
        for r in all {
            if !trip.is_empty() && r.start > trip_end + 3 * 86_400 {
                flush(&mut trip, &mut list);
                trip_end = i64::MIN;
            }
            trip_end = trip_end.max(r.end);
            trip.push(r);
        }
        flush(&mut trip, &mut list);
        let mut files = self.conn.prepare_cached(
            "SELECT r.part, r.name FROM attachment_refs r JOIN attachments a ON a.id = r.attachment
             WHERE r.message = ?1 AND a.mime NOT LIKE 'image/%' ORDER BY r.part",
        )?;
        for r in &mut list {
            r.files = files
                .query_map(params![r.message], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|(part, name)| TripFile { label: file_label(&name), part, name })
                .collect();
        }
        Ok(list)
    }

    /// Takes a reservation's event off the calendar (Eudora's own, or the one found there; a
    /// meeting someone else organized is declined, as Delete does). It stays in Trips, not on
    /// the calendar, and isn't added again. Returns what was done.
    pub fn remove_reservation_event(&self, key: &str) -> Result<String> {
        let row: Option<(i64, String, i64, i64, String, String, String, Option<i64>)> = self
            .conn
            .query_row("SELECT message, kind, start, end, title, location, details, event FROM reservations WHERE key = ?1", params![key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
            })
            .optional()?;
        let Some((_, kind, start, end, title, location, details, event)) = row else { return Err(Error::Other(format!("no reservation {key}"))) };
        let all_day = kind == "event" && start % 86_400 == 0 && (end - start) % 86_400 == 0;
        let f = Found { kind, key: key.to_string(), start, end, title, location, details, end_guessed: false, all_day };
        // its own event (still there), else the calendar's match
        let still: Option<i64> = match event {
            Some(e) => self.conn.query_row("SELECT id FROM events WHERE id = ?1", params![e], |r| r.get(0)).optional()?,
            None => None,
        };
        let said = match still.map(Some).unwrap_or(self.calendar_match(&f)?) {
            Some(e) => self.event_delete(e)?.to_string(),
            None => "already gone".to_string(),
        };
        self.conn.execute("UPDATE reservations SET status = 'declined', event = NULL WHERE key = ?1", params![key])?;
        Ok(said)
    }

    /// Removes a reservation from Trips (its calendar event, if any, stays): not shown or
    /// offered again.
    pub fn hide_reservation(&self, key: &str) -> Result<()> {
        self.conn.execute("UPDATE reservations SET status = 'hidden' WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Mail the spam filter judged marketing, a mailing list, or spam is an ad, never a booking
    /// or an order (2026-10-26: Bandsintown alerts, "Get tickets for…", and "LAST CALL" offers on
    /// the calendar; a festival's list mail offered as a hotel stay "10 AM ET"). Real bookings
    /// come as notifications or personal mail.
    fn without_ads(&self, id: i64, found: Vec<Found>) -> Result<Vec<Found>> {
        if found.is_empty() {
            return Ok(found);
        }
        let category: Option<String> = self.conn.query_row("SELECT category FROM triage WHERE message = ?1", params![id], |r| r.get(0)).optional()?;
        let ad = matches!(category.as_deref(), Some("bulk" | "list" | "spam" | "blocked"));
        Ok(if ad { Vec::new() } else { found })
    }

    /// Add to Trips (the user, on an email Eudora didn't take for travel): its reservations
    /// read now, whatever view it's in; flights, trains… go on the calendar as when found by
    /// themselves. Returns how many were found (0: none in it).
    pub fn trips_from_message(&self, id: i64) -> Result<usize> {
        let raw = self.raw_message(id)?;
        let date: i64 = self.conn.query_row("SELECT date FROM messages WHERE id = ?1", params![id], |r| r.get(0))?;
        let (found, subject) = found_in(&raw, date);
        let n = found.len();
        for f in &found {
            if self.take_reservation(id, &subject, f)?.is_none() {
                // known already (perhaps removed from Trips): back in Trips
                self.conn.execute("UPDATE reservations SET status = 'suggested' WHERE key = ?1 AND status = 'hidden'", params![f.key])?;
            }
        }
        Ok(n)
    }

    /// Answers the question about offered reservations: those in `add` go on the calendar, the
    /// rest of `keys` are declined (not asked about again). Returns how many were added.
    pub fn answer_reservations(&self, add: &[String], decline: &[String]) -> Result<u64> {
        let mut added = 0;
        for key in add {
            let row: Option<(i64, String, i64, i64, String, String, String)> = self
                .conn
                .query_row("SELECT message, kind, start, end, title, location, details FROM reservations WHERE key = ?1 AND status IN ('suggested', 'declined')", params![key], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                })
                .optional()?;
            let Some((message, kind, start, end, title, location, details)) = row else { continue };
            let all_day = kind == "event" && start % 86_400 == 0 && (end - start) % 86_400 == 0;
            let f = Found { kind, key: key.clone(), start, end, title, location, details, end_guessed: false, all_day };
            if let Some(event) = self.add_reservation_event(message, &f)? {
                self.conn.execute("UPDATE reservations SET status = 'added', event = ?2 WHERE key = ?1", params![key, event])?;
                added += 1;
            }
        }
        for key in decline {
            self.conn.execute("UPDATE reservations SET status = 'declined' WHERE key = ?1 AND status = 'suggested'", params![key])?;
        }
        Ok(added)
    }

    /// A reservation email's tickets, for its calendar event: its PDFs and Wallet passes, and
    /// the pictures that are a boarding pass, a barcode or a QR code (by their name, alt text or
    /// address); each as {name, contentType, message, part} (read when sent), {name,
    /// contentType, contentBytes} (a picture in the email itself) or {name, url} (fetched when
    /// sent).
    pub fn ticket_files(&self, message: i64) -> Result<Vec<Value>> {
        use base64::Engine;
        let mut out: Vec<Value> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let name_once = |name: &str, names: &mut Vec<String>| -> String {
            let mut n = name.to_string();
            let mut i = 2;
            while names.contains(&n) {
                n = format!("{i} {name}");
                i += 1;
            }
            names.push(n.clone());
            n
        };
        // the attachments: tickets and passes, and pictures named as one
        let mut stmt = self.conn.prepare(
            "SELECT r.part, r.name, a.mime, a.size FROM attachment_refs r JOIN attachments a ON a.id = r.attachment WHERE r.message = ?1 ORDER BY r.part",
        )?;
        let parts: Vec<(String, String, String, i64)> = stmt.query_map(params![message], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        for (part, name, mime, size) in parts {
            let n = name.to_ascii_lowercase();
            let ticket = n.ends_with(".pdf") || n.ends_with(".pkpass") || mime == "application/pdf" || mime == "application/vnd.apple.pkpass";
            let picture = mime.starts_with("image/") && ticket_picture(&n) && !icon(&n) && size >= TICKET_MIN as i64;
            if (ticket || picture) && !n.ends_with(".ics") && (size as usize) <= crate::server_changes::TICKET_MAX {
                out.push(json!({ "name": name_once(&name, &mut names), "contentType": mime, "message": message, "part": part }));
            }
        }
        // the pictures the email shows: a barcode / QR / boarding pass by its alt text, name or address
        let raw = self.raw_message(message)?;
        let d = crate::parse::display(&raw, now());
        if let Some(html) = &d.html {
            let lower = html.to_ascii_lowercase();
            let mut from = 0;
            while let Some(i) = lower[from..].find("<img") {
                let start = from + i;
                let end = lower[start..].find('>').map(|e| start + e + 1).unwrap_or(lower.len());
                from = end;
                let tag = &lower[start..end];
                if !ticket_picture(tag) || icon(tag) {
                    continue;
                }
                let Some(src) = attribute(&html[start..end], "src") else { continue };
                let label = if tag.contains("qr") { "QR code" } else if tag.contains("boarding") { "Boarding pass" } else { "Barcode" };
                if let Some(rest) = src.strip_prefix("data:") {
                    let Some((mime, b64)) = rest.split_once(";base64,") else { continue };
                    let ext = mime.rsplit('/').next().unwrap_or("png").replace("jpeg", "jpg");
                    let len = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map(|b| b.len()).unwrap_or(usize::MAX);
                    if (TICKET_MIN..=crate::server_changes::TICKET_MAX).contains(&len) {
                        out.push(json!({ "name": name_once(&format!("{label}.{ext}"), &mut names), "contentType": mime, "contentBytes": b64.trim() }));
                    }
                } else if src.starts_with("http") || src.starts_with("//") {
                    let ext = src.split(['?', '#']).next().and_then(|s| s.rsplit('.').next()).filter(|e| ["png", "gif", "jpg", "jpeg"].contains(e)).unwrap_or("png");
                    out.push(json!({ "name": name_once(&format!("{label}.{ext}"), &mut names), "url": src }));
                }
                if out.len() >= 12 {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The tickets' bytes, read now (those named by message and part); the rest as they are.
    pub fn ticket_bytes(&self, files: &Value) -> Result<Vec<Value>> {
        use base64::Engine;
        let mut out = Vec::new();
        for f in files.as_array().into_iter().flatten() {
            match (f["message"].as_i64(), f["part"].as_str()) {
                (Some(message), Some(part)) => {
                    let Ok((_, bytes)) = self.attachment_bytes(message, part) else { continue };
                    let mut f = f.clone();
                    f["contentBytes"] = base64::engine::general_purpose::STANDARD.encode(bytes).into();
                    out.push(f);
                }
                _ => out.push(f.clone()),
            }
        }
        Ok(out)
    }

    /// A later email of a trip already known (its check-in email, with the boarding pass): kept
    /// as another source of tickets for its event (setting "travel-ticket-sources": key → emails).
    fn add_ticket_source(&self, key: &str, message: i64) -> Result<()> {
        const SOURCES: &str = "travel-ticket-sources";
        let mut sources: HashMap<String, Vec<i64>> = self.setting(SOURCES)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let list = sources.entry(key.to_string()).or_default();
        if !list.contains(&message) {
            list.push(message);
            self.set_setting(SOURCES, &serde_json::to_string(&sources)?)?;
        }
        Ok(())
    }

    /// Puts each travel event's tickets on it (the download, after reading travel mail): from
    /// every email of the trip (the booking, then the check-in email with the boarding pass),
    /// each email once. The events Eudora added get the booking email's text too, so the
    /// confirmation is there on a phone; an event that was already on the calendar keeps its
    /// own text. Returns how many were queued.
    pub fn queue_tickets(&self) -> Result<u64> {
        const DONE: &str = "travel-tickets";
        let mut done: Vec<String> = self.setting(DONE)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let sources: HashMap<String, Vec<i64>> = self.setting("travel-ticket-sources")?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let mut stmt = self.conn.prepare(
            "SELECT r.key, r.status, r.message, r.details, r.event, c.account FROM reservations r JOIN events e ON e.id = r.event JOIN calendars c ON c.id = e.calendar
             WHERE r.status IN ('added', 'exists') AND r.end > ?1",
        )?;
        let rows: Vec<(String, String, i64, String, i64, i64)> =
            stmt.query_map(params![now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
        let before = done.len();
        let mut queued = 0;
        for (key, status, booking, details, event, account) in rows {
            if self.local_only_account(account, crate::local_only::Service::Calendar)? {
                continue; // (a calendar kept local: nothing goes to its server)
            }
            let emails = std::iter::once(booking).chain(sources.get(&key).into_iter().flatten().copied());
            for message in emails {
                let mark = format!("{key}:{message}");
                if done.contains(&mark) {
                    continue;
                }
                let files = self.ticket_files(message)?;
                let body = if status == "added" && message == booking {
                    let raw = self.raw_message(message)?;
                    let d = crate::parse::display(&raw, now());
                    let p = &d.parsed;
                    let from = p.from.first().map(|a| a.address.clone()).unwrap_or_default();
                    let mut text: String = d.text.chars().take(30_000).collect();
                    if d.text.chars().count() > 30_000 {
                        text.push_str("\n…");
                    }
                    let tickets = if files.is_empty() { String::new() } else { format!("\n\nYour tickets are attached to this event ({}).", files.len()) };
                    json!({ "contentType": "text", "content": format!("{details}{tickets}\n\nAdded by Eudora from your email.\n\n── The email: “{}” from {from} ──\n\n{text}", p.subject) })
                } else {
                    Value::Null
                };
                if !files.is_empty() || !body.is_null() {
                    self.conn.execute(
                        "INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'event-change', ?2)",
                        params![now(), json!({ "event": event, "account": account, "server_id": "", "action": "attach", "patch": { "files": files, "body": body } }).to_string()],
                    )?;
                    queued += 1;
                }
                // (an email's attachments never change: each is looked at once)
                done.push(mark);
            }
        }
        if done.len() != before {
            self.set_setting(DONE, &serde_json::to_string(&done)?)?;
        }
        Ok(queued)
    }
}

/// The smallest picture taken for a ticket (a QR code or barcode is bigger; an icon isn't).
pub const TICKET_MIN: usize = 2_000;

/// Whether a picture (its tag or name, lowercase) is an icon, not a ticket (2026-10-23: United's
/// booking emails show "Boarding-pass-24x24.png" beside a link): named so, sized under 100 in its
/// name, or drawn under 100 pixels wide or high.
fn icon(tag: &str) -> bool {
    if tag.contains("icon") {
        return true;
    }
    let small = |s: &str| s.parse::<u32>().is_ok_and(|n| n < 100);
    // "24x24" in a name
    let sized = tag.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.split_once('x').is_some_and(|(a, b)| small(a) && small(b)));
    let attr = |name: &str| attribute(tag, name).map(|v| v.trim_end_matches("px").to_string()).is_some_and(|v| small(&v));
    sized || attr("width") || attr("height")
}

/// Whether a picture (its tag or name, lowercase) is a ticket: a QR code, a barcode, a boarding
/// pass.
fn ticket_picture(text: &str) -> bool {
    let words = ["qrcode", "qr code", "qr-code", "qr_code", "barcode", "bar code", "bar-code", "bar_code", "boarding", "aztec", "pdf417", "mobile pass", "e-ticket", "eticket"];
    if words.iter().any(|w| text.contains(w)) {
        return true;
    }
    // "qr" as a word of its own ("qr.png", alt="QR")
    text.match_indices("qr").any(|(i, _)| {
        let before = text[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
        let after = text[i + 2..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric());
        before && after
    })
}

/// An HTML attribute's value in a tag (quoted or not).
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before = lower[..at].chars().next_back().is_some_and(|c| c.is_whitespace());
        let rest = lower[at + name.len()..].trim_start();
        if !before || !rest.starts_with('=') {
            continue;
        }
        let value_at = tag.len() - tag[at + name.len()..].trim_start().len() + 1;
        let v = tag[value_at..].trim_start();
        return Some(match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
            _ => v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("").to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_with_tickets_are_found_and_promotions_are_not() {
        let sent = chrono::Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap().timestamp();
        let tm = "Order # 12-34567
The Lantern Keepers (Touring)
Sat · Nov 14, 2026 · 7:30 PM
Venue: Lakeside Performing Arts Hall, Riverton, FL
Section 212, Row F";
        let f = event_in(tm, "You Got Tickets To The Lantern Keepers (Touring)", "Ticketmaster <customer_support@email.ticketmaster.com>", sent).expect("a Ticketmaster order");
        assert_eq!(f.kind, "event");
        assert_eq!(f.title, "🎟 The Lantern Keepers (Touring)");
        assert!(f.location.starts_with("Lakeside Performing Arts Hall"), "{}", f.location);
        assert_eq!(chrono::Local.timestamp_opt(f.start, 0).unwrap().format("%Y-%m-%d %H:%M").to_string(), "2026-11-14 19:30");
        assert!(adds_itself(&f.kind));
        let eb = "Date and time
Saturday, December 5, 2026 from 7:00 PM to 10:00 PM (EST)
Location
The Factory, 123 Main St";
        let f = event_in(eb, "Your tickets for Winter Jazz Night", "Eventbrite <noreply@order.eventbrite.com>", sent).expect("an Eventbrite order");
        assert_eq!(f.title, "🎟 Winter Jazz Night");
        assert_eq!(chrono::Local.timestamp_opt(f.start, 0).unwrap().format("%Y-%m-%d %H:%M").to_string(), "2026-12-05 19:00");
        assert!(event_in(tm, "Presale: The Lantern Keepers tickets on sale Friday", "Ticketmaster <x@email.ticketmaster.com>", sent).is_none(), "a promotion");
        // a venue's own box office (or a forwarded order): seats in it make it an order
        assert!(event_in(tm, "Your order", "Lakeside Hall <boxoffice@lakesidehall.example>", sent).is_some(), "a box office's order");
        assert!(
            event_in(tm, "Tomorrow: The Paper Lanterns at Riverside Hall.", "Bandsintown <notifications@updates.bandsintown.com>", sent).is_none(),
            "an alert about a show, with seats and tickets in its text, isn't an order"
        );
        for ad in ["Get tickets for The Velvet Teapot for only $26", "$16 Tickets to A Night in Marlowe Through Saturday!", "BrightCon attendees, win 2 tickets to The Game"] {
            assert!(event_in(tm, ad, "Lakeside Hall <news@lakesidehall.example>", sent).is_none(), "an ad: {ad}");
        }
        let store = "Order # 113-55
Car seat cover, black
Arriving Friday, Oct 23, 2026 by 8:00 PM";
        assert!(event_in(store, "Your Amazon.com order", "Amazon <auto-confirm@amazon.example>", sent).is_none(), "an order with no seats");
        // a conference: dates only, all day over each
        let cv = "Fiber Futures Forum 2026
December 8 - 10, 2026
Location: Harborview Convention Center, Port Ellis";
        let f = event_in(cv, "Registration Confirmation: Fiber Futures Forum 2026", "Cvent <noreply@cvent.com>", sent).expect("a conference registration");
        assert!(f.all_day, "{f:?}");
        assert_eq!(f.title, "🎟 Fiber Futures Forum 2026");
        assert_eq!((f.end - f.start) / 86_400, 3, "{f:?}");
        assert!(event_in(cv, "Registration is open: Fiber Futures Forum 2026", "Cvent <noreply@cvent.com>", sent).is_none(), "an invitation to register");
    }

    #[test]
    fn tickets_are_told_from_other_pictures() {
        assert!(ticket_picture("<img alt=\"your boarding pass\" src=\"x\">") && ticket_picture("<img src=\"https://a.example/qr.png\">"));
        assert!(ticket_picture("<img class=\"barcode\">") && ticket_picture("aztec code") && ticket_picture("pdf417.png"));
        assert!(!ticket_picture("<img src=\"logo.png\" alt=\"United\">") && !ticket_picture("<img src=\"aqrst.png\">"));
        assert_eq!(attribute("<img alt=\"QR\" src=\"cid:abc\">", "src").as_deref(), Some("cid:abc"));
        assert_eq!(attribute("<IMG SRC=https://x.example/b.png width=4>", "src").as_deref(), Some("https://x.example/b.png"));
        assert_eq!(attribute("<img data-src=\"a\" src=\"b\">", "src").as_deref(), Some("b"));
        assert!(icon("<img src=\"https://media.united.com/assets/m/x/original/boarding-pass-24x24.png\">"));
        assert!(icon("<img alt=\"qr\" width=\"32\" src=\"a.png\">") && !icon("<img alt=\"qr\" width=\"300\" src=\"a.png\">"));
    }

    fn at(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    #[test]
    fn united_confirmation() {
        let text = "Thanks ALEX for choosing United! United confirmation number : LMW4KP\nFlight to Fort Myers Oct 29, 2026 Nonstop\n\n6:45 PM 9:52 PM BOS   3h 7m           RSW Boston, MA, US Fort Myers, FL, US FLIGHT INFO Duration: 3h 7m\nUA 1893\nBoeing 737-900";
        let f = flights_in(text, "Your United Airlines booking confirmation – LMW4KP", "United Airlines <notifications@united.com>", at("2026-10-18T10:06:00Z"));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].key, "flight:UA1893:2026-10-29");
        assert_eq!(f[0].start, at("2026-10-29T22:45:00Z"), "6:45 PM Eastern daylight time");
        assert_eq!(f[0].end, at("2026-10-30T01:52:00Z"));
        assert!(f[0].details.contains("Confirmation: LMW4KP"), "{}", f[0].details);
        assert_eq!(f[0].title, "✈ UA 1893 BOS → RSW");
        // two legs, each leg's details before its number, times with "AM" (also Aeromexico's code)
        let two = "Flight to Fort Myers Oct 15, 2026 Nonstop\n6:50 PM 9:58 PM BOS   3h 8m           RSW Boston, MA, US Fort Myers, FL, US FLIGHT INFO Duration: 3h 8m\nUA 1893\nBoeing 737 MAX 9\nUnited Economy\nSnacks for Purchase\nFlight to Boston Oct 21, 2026 Nonstop\n6:15 AM 9:20 AM FLL   3h 5m           BOS Fort Lauderdale, FL, US Boston, MA, US FLIGHT INFO Duration: 3h 5m\nUA 2471\nBoeing 737 MAX 9";
        let f = flights_in(two, "Your United Airlines booking confirmation – LMW4KP", "United Airlines <notifications@united.com>", at("2026-10-04T03:43:00Z"));
        assert_eq!(f.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["flight:UA1893:2026-10-15", "flight:UA2471:2026-10-21"], "{f:?}");
        assert_eq!(f[1].start, at("2026-10-21T10:15:00Z"), "6:15 AM Eastern");
        assert_eq!(f[1].end, at("2026-10-21T13:20:00Z"));
    }

    #[test]
    fn jetblue_without_a_year_and_delta_with_a_connection() {
        let jb = "Flights AUS BOS Austin, TX\nTerminal: Main Boston, MA Date Sat, Jul 20 Departs 4:10pm Arrives 8:45pm Flight 1184 If your booking";
        let f = flights_in(jb, "JetBlue booking confirmation - PNXWEJ", "JetBlue Reservations <jetblueairways@email.jetblue.com>", at("2019-05-14T19:04:00Z"));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].key, "flight:B61184:2019-07-20");
        assert_eq!(f[0].start, at("2019-07-20T21:10:00Z"), "4:10 PM Central");
        assert_eq!(f[0].end, at("2019-07-21T00:45:00Z"), "8:45 PM Eastern");
        let delta = "FLIGHT CONFIRMATION #: GZLRTW DEPARTURE\n LGA9:45 AM Thu, Mar 30 DL1376\n ATL 1:30 PM DL1902 DESTINATION\n MEM1:41 PM Thu, Mar 30";
        let f = flights_in(delta, "Your LGA > MEM Trip Details", "Delta Air Lines <DeltaAirLines@t.delta.com>", at("2023-03-27T17:23:00Z"));
        assert_eq!(f.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), vec!["✈ DL 1376 LGA → ATL", "✈ DL 1902 ATL → MEM"], "{f:?}");
        assert_eq!(f[0].start, at("2023-03-30T13:45:00Z"));
        assert!(f[0].end_guessed);
        assert_eq!(f[1].start, at("2023-03-30T17:30:00Z"));
        assert_eq!(f[1].end, at("2023-03-30T18:41:00Z"), "1:41 PM Central");
        assert!(f[0].details.contains("GZLRTW"));
    }

    #[test]
    fn hotel_stays() {
        let ihg = "Holiday Inn Express & Suites Millbrook-Riverside\nCheck In:    Tue 10 Dec 2019 03:00 PM\nCheck Out:    Fri 13 Dec 2019 12:00 PM\n";
        let s = stay_in(ihg, "", "Your Reservation Confirmation # 73920418 at Holiday Inn Express & Suites.", "Holiday Inn Express Reservations <x@ihg.com>", at("2019-12-05T14:52:00Z")).unwrap();
        assert_eq!(s.title, "🛏 Holiday Inn Express & Suites");
        assert!(s.details.starts_with("Check in Tue Dec 10 · check out Fri Dec 13, 2019"), "{}", s.details);
        assert!(s.details.contains("73920418"));
        assert_eq!(s.end - s.start, 3 * 86_400 - 9 * 3600, "7 PM to 10 AM three nights later");
        let airbnb = r#"<script type="application/ld+json">{"@context":"http://schema.org","@type":"LodgingReservation","reservationNumber":"HM1","checkinDate":"2026-11-12T16:00:00-05:00","checkoutDate":"2026-11-14T11:00:00-05:00","reservationFor":{"@type":"LodgingBusiness","name":"Palm Breeze Cottage 2BR","address":{"@type":"PostalAddress","streetAddress":"42 Gulfview Lane","addressLocality":"Sarasota","addressRegion":"FL"}}}</script>"#;
        let s = stay_in("", airbnb, "Confirmed: Your November 12 – 14 trip", "Airbnb <automated@airbnb.com>", at("2026-11-01T16:09:00Z")).unwrap();
        assert_eq!((s.title.as_str(), s.location.as_str()), ("🛏 Palm Breeze Cottage 2BR", "42 Gulfview Lane, Sarasota, FL"));
        assert!(stay_in("Nothing about a stay here.", "", "Hello", "x", at("2026-10-09T16:09:00Z")).is_none());
    }

    #[test]
    fn trains_and_rentals() {
        let bl = "OUTBOUND TRIP DETAILS     Ticket Number: QPW8RN\nAlex Sample\nMANAGE TRIP     Boarding Closes 02:42 PM Saturday, October 10, 2026\nPassengers: 1\n  West Palm Beach Departs\n02:47 PM\nAventura Arrives\n03:31 PM\n  Train Number: 5217\n Duration:00:44\n RETURN TRIP DETAILS     Ticket Number: QPW8RN\nMANAGE TRIP     Boarding Closes 10:25 AM Wednesday, October 14, 2026\n  Aventura Departs\n10:30 AM\nWest Palm Beach Arrives\n11:14 AM\n  Train Number: 5244\n";
        let t = transit_in(bl, "Your Trip Confirmation: QPW8RN | Sat, Oct 10", "Brightline <email@mail.gobrightline.com>", at("2026-10-09T15:20:00Z"));
        assert_eq!(t.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), vec!["🚆 Brightline 5217 West Palm Beach → Aventura", "🚆 Brightline 5244 Aventura → West Palm Beach"], "{t:?}");
        assert_eq!(t[0].end - t[0].start, 44 * 60);
        assert!(t[1].details.contains("Wed Oct 14, 10:30 AM"), "{}", t[1].details);
        assert!(t[0].details.contains("QPW8RN"));
        let avis = "Automatic Transmission Itinerary\nPick Up Location\nCharlotte Douglas Intl Airport,CLT\nMon Sep 21, 2026 at 10:30 AM\n5501 Airport Access Rd ,\nReturn Location\nCharlotte Douglas Intl Airport,CLT\nThu Sep 24, 2026 at 9:00 AM\n";
        let r = rental_in(avis, "Reservation Confirmation: ALEX, your Avis reservation #58306127US2 at Charlotte Douglas Intl Airport, CLT is confirmed", "Avis <avis@e.avis.com>", at("2026-09-18T23:24:00Z")).unwrap();
        assert_eq!(r.title, "🚗 Avis rental · Charlotte Douglas Intl Airport,CLT");
        assert_eq!(r.end - r.start, 3 * 86_400 - 90 * 60);
        assert!(r.details.contains("58306127US2"), "{}", r.details);
        let turo = "Jordan’s Kia EV6 2025\nTrip start: 10/3/26 8:00 pm\nTrip end: 10/4/26 11:30 pm\nReturn: 10/5/26 12:00 am\n100 Sample Lane\nAnytown, VA";
        let r = rental_in(turo, "Trip booked with Jordan’s Kia EV6!", "Turo <noreply@turo.com>", at("2026-10-02T14:10:00Z")).unwrap();
        assert_eq!(r.key, "car:turo:2026-10-03T2000");
        assert!(!r.end_guessed);
        assert!(rental_in(avis, "How was your rental? Rate your experience", "Avis <avis@e.avis.com>", at("2026-09-26T00:00:00Z")).is_none(), "after the rental");
        assert!(ride_in("Your trip on Sunday", "[Personal] Your Sunday afternoon trip with Uber", "Uber Receipts <noreply@uber.com>", at("2026-10-19T00:00:00Z")).is_none(), "a receipt isn't a booking");
    }

    #[test]
    fn one_email_s_legs_stay_together_and_its_files_are_named() {
        let leg = |start: i64| Found { kind: "train".into(), key: format!("k{start}"), start, end: start + 3600, title: String::new(), location: String::new(), details: String::new(), end_guessed: false, all_day: false };
        assert_eq!(ahead(vec![leg(now() - 86_400), leg(now() + 86_400)]).len(), 2, "the outbound already gone stays with the return ahead");
        assert!(ahead(vec![leg(now() - 2 * 86_400)]).is_empty(), "only trips ahead");
        assert_eq!(file_label("QPW8RN_P1_261010144212345.pdf"), "Boarding pass (passenger 1)");
        assert_eq!(file_label("QPW8RN-2_INV_261010144215678.pdf"), "Invoice");
        assert_eq!(file_label("QPW8RN_261010144212301_rcp.pdf"), "Receipt");
        assert_eq!(file_label("QPW8RN_261010144212345.pdf"), "Ticket");
        assert_eq!(file_label("UA1893.pkpass"), "Wallet pass");
    }

    #[test]
    fn airports_keep_their_daylight_saving_time() {
        let local = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        assert_eq!(airport_utc("EWR", local("2026-01-15 09:00")), Some(at("2026-01-15T14:00:00Z")));
        assert_eq!(airport_utc("EWR", local("2026-07-15 09:00")), Some(at("2026-07-15T13:00:00Z")));
        assert_eq!(airport_utc("PHX", local("2026-07-15 09:00")), Some(at("2026-07-15T16:00:00Z")), "no daylight saving in Arizona");
        assert_eq!(airport_utc("FCO", local("2026-07-15 09:00")), Some(at("2026-07-15T07:00:00Z")));
        assert_eq!(airport_utc("FCO", local("2026-12-15 09:00")), Some(at("2026-12-15T08:00:00Z")));
        assert_eq!(airport_utc("XXX", local("2026-12-15 09:00")), None);
    }
}

#[cfg(test)]
mod try_real_mail {
    /// Reads the emails named in EUDORA_TRY (paths, ';' between) and prints what the trip and
    /// event readers find: for checking against real mail by hand (`cargo test try_real -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn try_real() {
        for path in std::env::var("EUDORA_TRY").unwrap_or_default().split(';').filter(|p| !p.is_empty()) {
            let raw = std::fs::read(path).unwrap();
            let p = crate::parse::parse(&raw, crate::parse::DateHints::default(), crate::library::now());
            let (found, subject) = super::found_in(&raw, p.date);
            let d = crate::parse::display(&raw, crate::library::now());
            let text = if d.text.trim().is_empty() { mail_parser::decoders::html::html_to_text(d.html.as_deref().unwrap_or("")) } else { d.text.clone() };
            println!("{path}: {subject}\n{found:#?}\n--- text ---\n{}", text.chars().take(40_000).collect::<String>());
        }
    }
}
