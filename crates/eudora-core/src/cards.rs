//! Address books: contacts from every account, in one library.
//!
//! A `Card` is Eudora's reading of one contact as one account keeps it; the server's own form
//! (Graph's JSON, a vCard) is kept beside it byte for byte (`cards.original`), so nothing a
//! server holds is lost even where Eudora has no field for it. Microsoft 365 address books come
//! in through the download's change thread (`server_changes::Batch::contacts`): it fetches,
//! the download (the library's only writer) records here.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::library::{Library, now};

/// A value with its kind ("work", "home", "mobile", "other"…).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Labeled {
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Postal {
    pub kind: String,
    pub street: String,
    pub city: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
}

impl Postal {
    fn is_empty(&self) -> bool {
        [&self.street, &self.city, &self.region, &self.postal_code, &self.country].iter().all(|s| s.trim().is_empty())
    }
}

/// One contact.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Card {
    /// The name shown ("Ann Smith"); made from the parts when the server has none.
    pub name: String,
    pub given: String,
    pub middle: String,
    pub family: String,
    pub prefix: String,
    pub suffix: String,
    pub nickname: String,
    pub company: String,
    pub department: String,
    pub title: String,
    pub emails: Vec<Labeled>,
    pub phones: Vec<Labeled>,
    pub addresses: Vec<Postal>,
    pub websites: Vec<String>,
    /// YYYY-MM-DD (or --MM-DD without a year).
    pub birthday: String,
    pub notes: String,
    pub categories: Vec<String>,
}

impl Card {
    /// The name to show: its own, else made from the parts, else the company or first address.
    pub fn shown(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        let parts = [&self.prefix, &self.given, &self.middle, &self.family, &self.suffix]
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim())
            .collect::<Vec<_>>()
            .join(" ");
        if !parts.is_empty() {
            return parts;
        }
        if !self.company.trim().is_empty() {
            return self.company.trim().to_string();
        }
        self.emails.first().map(|e| e.value.clone()).unwrap_or_default()
    }
}

// ------------------------------------------------------------------ Microsoft Graph

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").trim().to_string()
}

/// A Graph contact read into a card.
pub fn from_graph(v: &Value) -> Card {
    let mut card = Card {
        name: s(&v["displayName"]),
        given: s(&v["givenName"]),
        middle: s(&v["middleName"]),
        family: s(&v["surname"]),
        prefix: s(&v["title"]),
        suffix: s(&v["generation"]),
        nickname: s(&v["nickName"]),
        company: s(&v["companyName"]),
        department: s(&v["department"]),
        title: s(&v["jobTitle"]),
        notes: s(&v["personalNotes"]),
        ..Default::default()
    };
    for e in v["emailAddresses"].as_array().into_iter().flatten() {
        let address = s(&e["address"]);
        if !address.is_empty() {
            card.emails.push(Labeled { kind: "other".into(), value: address });
        }
    }
    let mut phone = |kind: &str, value: &Value| {
        let n = s(value);
        if !n.is_empty() {
            card.phones.push(Labeled { kind: kind.into(), value: n });
        }
    };
    phone("mobile", &v["mobilePhone"]);
    for n in v["businessPhones"].as_array().into_iter().flatten() {
        phone("work", n);
    }
    for n in v["homePhones"].as_array().into_iter().flatten() {
        phone("home", n);
    }
    for (kind, key) in [("work", "businessAddress"), ("home", "homeAddress"), ("other", "otherAddress")] {
        let a = &v[key];
        let postal = Postal {
            kind: kind.into(),
            street: s(&a["street"]),
            city: s(&a["city"]),
            region: s(&a["state"]),
            postal_code: s(&a["postalCode"]),
            country: s(&a["countryOrRegion"]),
        };
        if !postal.is_empty() {
            card.addresses.push(postal);
        }
    }
    let home = s(&v["businessHomePage"]);
    if !home.is_empty() {
        card.websites.push(home);
    }
    if let Some(b) = v["birthday"].as_str().filter(|b| b.len() >= 10) {
        card.birthday = b[..10].to_string();
    }
    card.categories = v["categories"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(str::to_string)).collect();
    card
}

/// A card as Graph takes it (for making or changing a contact). Graph has room for three email
/// addresses, one mobile, and lists of work and home numbers; the rest of a card's numbers go
/// to the work list.
pub fn to_graph(card: &Card) -> Value {
    let phones = |kind: &str| -> Vec<String> { card.phones.iter().filter(|p| p.kind == kind).map(|p| p.value.clone()).collect() };
    let mut business = phones("work");
    business.extend(card.phones.iter().filter(|p| !matches!(p.kind.as_str(), "work" | "home" | "mobile")).map(|p| p.value.clone()));
    let address = |kind: &str| -> Value {
        card.addresses
            .iter()
            .find(|a| a.kind == kind)
            .map(|a| json!({ "street": a.street, "city": a.city, "state": a.region, "postalCode": a.postal_code, "countryOrRegion": a.country }))
            .unwrap_or_else(|| json!({}))
    };
    let mut v = json!({
        "displayName": card.shown(),
        "givenName": card.given,
        "middleName": card.middle,
        "surname": card.family,
        "title": card.prefix,
        "generation": card.suffix,
        "nickName": card.nickname,
        "companyName": card.company,
        "department": card.department,
        "jobTitle": card.title,
        "personalNotes": card.notes,
        "emailAddresses": card.emails.iter().take(3).map(|e| json!({ "address": e.value, "name": card.shown() })).collect::<Vec<_>>(),
        "mobilePhone": card.phones.iter().find(|p| p.kind == "mobile").map(|p| p.value.clone()).unwrap_or_default(),
        "businessPhones": business,
        "homePhones": phones("home"),
        "businessAddress": address("work"),
        "homeAddress": address("home"),
        "otherAddress": address("other"),
        "businessHomePage": card.websites.first().cloned().unwrap_or_default(),
        "categories": card.categories,
    });
    if card.birthday.len() == 10 && card.birthday.as_bytes()[4] == b'-' {
        v["birthday"] = json!(format!("{}T11:59:00Z", card.birthday));
    }
    v
}

// ------------------------------------------------------------------ vCard

fn vescape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace(',', "\\,").replace(';', "\\;")
}

fn vunescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A line folded at 75 octets (RFC 6350), never inside a UTF-8 character.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if count + n > 75 {
            out.push_str("\r\n ");
            count = 1;
        }
        out.push(c);
        count += n;
    }
    out.push_str("\r\n");
    out
}

/// A card as a vCard 3.0 (understood by Outlook, Apple, Google, Thunderbird and CardDAV servers).
pub fn to_vcard(card: &Card) -> String {
    let mut v = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    let mut put = |line: String| v.push_str(&fold(&line));
    put(format!("FN:{}", vescape(&card.shown())));
    put(format!("N:{};{};{};{};{}", vescape(&card.family), vescape(&card.given), vescape(&card.middle), vescape(&card.prefix), vescape(&card.suffix)));
    if !card.nickname.is_empty() {
        put(format!("NICKNAME:{}", vescape(&card.nickname)));
    }
    if !card.company.is_empty() || !card.department.is_empty() {
        put(format!("ORG:{};{}", vescape(&card.company), vescape(&card.department)));
    }
    if !card.title.is_empty() {
        put(format!("TITLE:{}", vescape(&card.title)));
    }
    for e in &card.emails {
        put(format!("EMAIL;TYPE=INTERNET{}:{}", if matches!(e.kind.as_str(), "work" | "home") { format!(",{}", e.kind.to_uppercase()) } else { String::new() }, e.value));
    }
    for p in &card.phones {
        let kind = match p.kind.as_str() {
            "mobile" => "CELL",
            "work" => "WORK",
            "home" => "HOME",
            "fax" => "FAX",
            _ => "VOICE",
        };
        put(format!("TEL;TYPE={kind}:{}", vescape(&p.value)));
    }
    for a in &card.addresses {
        put(format!(
            "ADR;TYPE={}:;;{};{};{};{};{}",
            if a.kind == "other" { "POSTAL".to_string() } else { a.kind.to_uppercase() },
            vescape(&a.street),
            vescape(&a.city),
            vescape(&a.region),
            vescape(&a.postal_code),
            vescape(&a.country)
        ));
    }
    for w in &card.websites {
        put(format!("URL:{w}"));
    }
    if !card.birthday.is_empty() {
        put(format!("BDAY:{}", card.birthday));
    }
    if !card.notes.is_empty() {
        put(format!("NOTE:{}", vescape(&card.notes)));
    }
    if !card.categories.is_empty() {
        put(format!("CATEGORIES:{}", card.categories.iter().map(|c| vescape(c)).collect::<Vec<_>>().join(",")));
    }
    v.push_str("END:VCARD\r\n");
    v
}

/// Splits a value at unescaped `;` (structured vCard values).
fn parts(value: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            out.last_mut().unwrap().push('\\');
            out.last_mut().unwrap().push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ';' {
            out.push(String::new());
        } else {
            out.last_mut().unwrap().push(c);
        }
    }
    out.into_iter().map(|p| vunescape(&p).trim().to_string()).collect()
}

/// Every card in a vCard file (versions 2.1, 3.0 and 4.0; folded lines; several cards).
pub fn from_vcard(text: &str) -> Vec<Card> {
    // unfold: a line starting with a space or tab continues the one before
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().unwrap().push_str(&raw[1..]);
        } else {
            lines.push(raw.to_string());
        }
    }
    let mut cards = Vec::new();
    let mut card: Option<Card> = None;
    for line in lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut head_parts = head.split(';');
        let name = head_parts.next().unwrap_or("").to_ascii_uppercase();
        // (a group prefix "item1.EMAIL")
        let name = name.rsplit('.').next().unwrap_or("").to_string();
        let params: String = head_parts.collect::<Vec<_>>().join(";").to_ascii_lowercase();
        let kind = if params.contains("cell") || params.contains("mobile") {
            "mobile"
        } else if params.contains("fax") {
            "fax"
        } else if params.contains("work") {
            "work"
        } else if params.contains("home") {
            "home"
        } else {
            "other"
        };
        match name.as_str() {
            "BEGIN" if value.trim().eq_ignore_ascii_case("VCARD") => card = Some(Card::default()),
            "END" if value.trim().eq_ignore_ascii_case("VCARD") => {
                if let Some(c) = card.take() {
                    cards.push(c);
                }
            }
            _ => {
                let Some(c) = card.as_mut() else { continue };
                match name.as_str() {
                    "FN" => c.name = vunescape(value).trim().to_string(),
                    "N" => {
                        let p = parts(value);
                        let get = |i: usize| p.get(i).cloned().unwrap_or_default();
                        (c.family, c.given, c.middle, c.prefix, c.suffix) = (get(0), get(1), get(2), get(3), get(4));
                    }
                    "NICKNAME" => c.nickname = vunescape(value).trim().to_string(),
                    "ORG" => {
                        let p = parts(value);
                        c.company = p.first().cloned().unwrap_or_default();
                        c.department = p.get(1).cloned().unwrap_or_default();
                    }
                    "TITLE" => c.title = vunescape(value).trim().to_string(),
                    "EMAIL" => {
                        let v = value.trim().trim_start_matches("mailto:").to_string();
                        if v.contains('@') {
                            c.emails.push(Labeled { kind: kind.into(), value: v });
                        }
                    }
                    "TEL" => {
                        let v = vunescape(value).trim().trim_start_matches("tel:").to_string();
                        if !v.is_empty() {
                            c.phones.push(Labeled { kind: kind.into(), value: v });
                        }
                    }
                    "ADR" => {
                        let p = parts(value);
                        let get = |i: usize| p.get(i).cloned().unwrap_or_default();
                        let street = [get(0), get(1), get(2)].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("\n");
                        let a = Postal { kind: if kind == "mobile" || kind == "fax" { "other".into() } else { kind.into() }, street, city: get(3), region: get(4), postal_code: get(5), country: get(6) };
                        if !a.is_empty() {
                            c.addresses.push(a);
                        }
                    }
                    "URL" => c.websites.push(value.trim().to_string()),
                    "BDAY" => {
                        let b = value.trim();
                        c.birthday = if b.len() == 8 && b.chars().all(|ch| ch.is_ascii_digit()) { format!("{}-{}-{}", &b[..4], &b[4..6], &b[6..]) } else { b.chars().take(10).collect() };
                    }
                    "NOTE" => c.notes = vunescape(value),
                    "CATEGORIES" => c.categories = value.split(',').map(|x| vunescape(x).trim().to_string()).filter(|x| !x.is_empty()).collect(),
                    _ => {}
                }
            }
        }
    }
    cards
}

// ------------------------------------------------------------------ CSV

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// Cards as CSV, with the column names Outlook and Google read on import.
pub fn to_csv(cards: &[Card]) -> String {
    let mut out = String::from(
        "First Name,Middle Name,Last Name,Display Name,Nickname,Company,Department,Job Title,E-mail Address,E-mail 2 Address,E-mail 3 Address,Mobile Phone,Business Phone,Home Phone,Business Street,Business City,Business State,Business Postal Code,Business Country/Region,Home Street,Home City,Home State,Home Postal Code,Home Country/Region,Web Page,Birthday,Notes,Categories\r\n",
    );
    for c in cards {
        let email = |i: usize| c.emails.get(i).map(|e| e.value.clone()).unwrap_or_default();
        let phone = |k: &str| c.phones.iter().find(|p| p.kind == k).map(|p| p.value.clone()).unwrap_or_default();
        let addr = |k: &str| c.addresses.iter().find(|a| a.kind == k).cloned().unwrap_or_default();
        let (w, h) = (addr("work"), addr("home"));
        let row = [
            c.given.clone(), c.middle.clone(), c.family.clone(), c.shown(), c.nickname.clone(), c.company.clone(), c.department.clone(), c.title.clone(),
            email(0), email(1), email(2), phone("mobile"), phone("work"), phone("home"),
            w.street, w.city, w.region, w.postal_code, w.country, h.street, h.city, h.region, h.postal_code, h.country,
            c.websites.first().cloned().unwrap_or_default(), c.birthday.clone(), c.notes.clone(), c.categories.join(";"),
        ];
        out.push_str(&row.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        out.push_str("\r\n");
    }
    out
}

// ------------------------------------------------------------------ the library

/// What the change thread fetches: one Microsoft account's address books.
#[derive(Clone, Debug)]
pub struct ContactsJob {
    pub account: i64,
    /// None = the signed-in user's own mailbox; Some = a shared one (later).
    pub mailbox: Option<String>,
}

/// What it brought back: (book id, book name, contacts as Graph gives them), or why not.
#[derive(Debug)]
pub struct ContactsFetched {
    pub account: i64,
    pub books: std::result::Result<Vec<(Option<String>, String, Vec<Value>)>, String>,
    /// Whose contacts they are (Microsoft's Graph, Google's People API, a CardDAV server).
    pub source: Source,
}

/// Where an account's contacts come from, and so how they read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Microsoft Graph: each contact's `id` and `changeKey`.
    Graph,
    /// Google's People API: `resourceName`, `etag` (one book, "Google Contacts").
    Google,
    /// A CardDAV server: {id: its href, etag, vcard} (`dav::contacts`).
    Dav,
}

/// What recording an account's address books did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CardsRecorded {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

// ------------------------------------------------------------------ duplicates

/// A phone number as digits only, the last ten (country code and punctuation don't matter);
/// None when too short to tell people apart.
fn phone_key(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 7 {
        return None;
    }
    Some(digits[digits.len().saturating_sub(10)..].to_string())
}

/// A name as compared: lowercase words, punctuation left out; None for one word alone or an
/// address (too many people share "John").
fn name_key(card: &Card) -> Option<String> {
    let shown = card.shown();
    if shown.contains('@') {
        return None;
    }
    let words: Vec<String> = shown
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    (words.len() >= 2).then(|| words.join(" "))
}

/// How much of a card is filled in (the fullest one leads a merge).
fn filled(card: &Card) -> usize {
    let scalars = [&card.name, &card.given, &card.middle, &card.family, &card.prefix, &card.suffix, &card.nickname, &card.company, &card.department, &card.title, &card.birthday, &card.notes];
    scalars.iter().filter(|s| !s.trim().is_empty()).count()
        + card.emails.len()
        + card.phones.len()
        + card.addresses.len()
        + card.websites.len()
        + card.categories.len()
}

/// The single-value fields (and notes) where the cards hold different values, by the card's
/// field name ("name", "company", "notes"…). The same text in other capitals or spacing, or a
/// value only some of them have, isn't a difference: it merges by itself. Lists (addresses,
/// numbers, websites, categories) never differ: every value is kept.
pub fn card_differences(cards: &[&Card]) -> Vec<String> {
    let same = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let fields: [(&str, fn(&Card) -> &str); 12] = [
        ("name", |c| &c.name),
        ("prefix", |c| &c.prefix),
        ("given", |c| &c.given),
        ("middle", |c| &c.middle),
        ("family", |c| &c.family),
        ("suffix", |c| &c.suffix),
        ("nickname", |c| &c.nickname),
        ("company", |c| &c.company),
        ("department", |c| &c.department),
        ("title", |c| &c.title),
        ("birthday", |c| &c.birthday),
        ("notes", |c| &c.notes),
    ];
    let mut out = Vec::new();
    for (field, get) in fields.iter() {
        let values: HashSet<String> = cards.iter().map(|c| same(get(c))).filter(|v| !v.is_empty()).collect();
        if values.len() > 1 {
            out.push(field.to_string());
        }
    }
    out
}

/// The cards as one: `first`'s values first; a field it lacks taken from the others; every
/// email, phone, address, website, and category any of them has, each once; their different
/// notes one after another. Nothing any of them holds is lost.
pub fn merge_of(first: &Card, others: &[&Card]) -> Card {
    let mut out = first.clone();
    for other in others {
        for (mine, theirs) in [
            (&mut out.name, &other.name),
            (&mut out.given, &other.given),
            (&mut out.middle, &other.middle),
            (&mut out.family, &other.family),
            (&mut out.prefix, &other.prefix),
            (&mut out.suffix, &other.suffix),
            (&mut out.nickname, &other.nickname),
            (&mut out.company, &other.company),
            (&mut out.department, &other.department),
            (&mut out.title, &other.title),
            (&mut out.birthday, &other.birthday),
        ] {
            if mine.trim().is_empty() && !theirs.trim().is_empty() {
                *mine = theirs.clone();
            }
        }
        for e in &other.emails {
            if !e.value.trim().is_empty() && !out.emails.iter().any(|m| m.value.trim().eq_ignore_ascii_case(e.value.trim())) {
                out.emails.push(e.clone());
            }
        }
        for p in &other.phones {
            let key = phone_key(&p.value).unwrap_or_else(|| p.value.trim().to_string());
            if !p.value.trim().is_empty() && !out.phones.iter().any(|m| phone_key(&m.value).unwrap_or_else(|| m.value.trim().to_string()) == key) {
                out.phones.push(p.clone());
            }
        }
        let place = |a: &Postal| {
            [&a.street, &a.city, &a.region, &a.postal_code, &a.country]
                .iter()
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase())
                .collect::<Vec<_>>()
        };
        for a in &other.addresses {
            if !a.is_empty() && !out.addresses.iter().any(|m| place(m) == place(a)) {
                out.addresses.push(a.clone());
            }
        }
        let site = |w: &str| w.trim().trim_end_matches('/').to_lowercase();
        for w in &other.websites {
            if !w.trim().is_empty() && !out.websites.iter().any(|m| site(m) == site(w)) {
                out.websites.push(w.clone());
            }
        }
        for c in &other.categories {
            if !c.trim().is_empty() && !out.categories.iter().any(|m| m.trim().eq_ignore_ascii_case(c.trim())) {
                out.categories.push(c.clone());
            }
        }
        let note = other.notes.trim();
        if !note.is_empty() && !out.notes.contains(note) {
            out.notes = if out.notes.trim().is_empty() { note.to_string() } else { format!("{}\n\n{note}", out.notes.trim_end()) };
        }
    }
    out
}

/// One card in a list, with where it lives.
#[derive(Clone, Debug, Serialize)]
pub struct CardRow {
    pub id: i64,
    pub account: i64,
    pub account_address: String,
    pub book: String,
    pub card: Card,
    pub shown: String,
}

/// The setting naming the account a new contact went into last (`new_contact_account`).
const NEW_CONTACT_SETTING: &str = "contacts-new-in";

/// An account a contact can be added to (`contact_targets`).
#[derive(Clone, Debug, Serialize)]
pub struct ContactTarget {
    pub account: i64,
    pub address: String,
    /// "Microsoft 365", "Google", or "CardDAV".
    pub service: String,
    /// The address book a contact goes into (its server id; "" = the account's default).
    pub book: String,
    pub book_name: String,
    /// How many contacts the account holds.
    pub count: u64,
}

/// What `copy_cards` did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CardsCopied {
    pub copied: u64,
    pub moved: u64,
    /// Already in that account (or in it to begin with).
    pub skipped: u64,
    /// (card, why)
    pub failed: Vec<(i64, String)>,
}

/// Contacts that look like one person (`contact_duplicates`): the cards, why they're taken as
/// one, and what merging them makes.
#[derive(Clone, Debug, Serialize)]
pub struct DuplicateGroup {
    pub cards: Vec<CardRow>,
    /// "the same email address ann@example.com", "the same phone number 813 555 0100", "the same
    /// name Ann Smith"
    pub why: Vec<String>,
    /// The card most filled in, whose name and fields come first in the merged one.
    pub keep: i64,
    pub merged: Card,
    /// Only the same name: maybe the same person, maybe not (never merged by Merge All).
    pub possible: bool,
    /// The fields holding different values (`card_differences`): the user chooses those.
    pub differences: Vec<String>,
}

/// What `merge_cards` did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CardsMerged {
    /// The merge's mark, for `undo_merge`.
    pub merge: String,
    /// Cards changed into the merged contact (one per account).
    pub updated: u64,
    /// Extra copies deleted (kept in the history).
    pub deleted: u64,
    /// (card, why): left as they were.
    pub failed: Vec<(i64, String)>,
}

/// A merge made lately, which `undo_merge` can take back.
#[derive(Clone, Debug, Serialize)]
pub struct MergeDone {
    pub merge: String,
    pub at: i64,
    /// The contact's name as merged.
    pub name: String,
    /// How many contacts went into it.
    pub cards: u64,
}

/// Where a card came from: its account and service, its address book, and how it stands with
/// its server.
#[derive(Clone, Debug, Serialize)]
pub struct CardOrigin {
    pub account: i64,
    pub address: String,
    /// "Microsoft 365", "Outlook.com"… (`db::kind_name`).
    pub service: String,
    pub book: String,
    pub on_server: bool,
    pub updated: i64,
    /// A change of the user's still on its way to the server.
    pub waiting: Option<String>,
    pub last_error: Option<String>,
    /// Changes go back to its server (Microsoft 365, Outlook.com, Google, a CardDAV server).
    pub editable: bool,
}

/// A change to a card going to its server (the download's change thread).
#[derive(Clone, Debug)]
pub struct CardJob {
    pub op: i64,
    pub card: Option<i64>,
    pub mailbox: Option<String>,
    pub what: CardChange,
}

#[derive(Clone, Debug)]
pub enum CardChange {
    Create { book: Option<String>, contact: Value },
    Update { server_id: String, contact: Value },
    Delete { server_id: String },
}

/// What happened: the contact as Graph keeps it now (made or changed), or why not.
#[derive(Debug)]
pub struct CardDone {
    pub op: i64,
    pub card: Option<i64>,
    pub result: std::result::Result<Option<Value>, String>,
}

impl Library {
    /// The Microsoft accounts whose address books the download reads with this sign-in.
    pub fn contacts_jobs(&self, signed_in: &str) -> Result<Vec<ContactsJob>> {
        let mut stmt = self.conn.prepare("SELECT id FROM accounts WHERE kind = 'graph' AND lower(address) = lower(?1)")?;
        let ids: Vec<i64> = stmt.query_map(params![signed_in], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids.into_iter().map(|account| ContactsJob { account, mailbox: None }).collect())
    }

    /// Stores an account's address books as fetched: new cards added, changed ones (by Graph's
    /// changeKey) read again, those gone from the server removed. The server's own form is kept.
    pub fn record_contacts(&self, account: i64, books: &[(Option<String>, String, Vec<Value>)]) -> Result<CardsRecorded> {
        self.record_cards_from(account, books, Source::Graph)
    }

    /// A Google account's contacts (People API), recorded the same way: one address book,
    /// "Google Contacts"; each person's `resourceName` its id, its `etag` its version.
    pub fn record_google_contacts(&self, account: i64, people: &[Value]) -> Result<CardsRecorded> {
        self.record_cards_from(account, &[(None, "Google Contacts".to_string(), people.to_vec())], Source::Google)
    }

    pub(crate) fn record_cards_from(&self, account: i64, books: &[(Option<String>, String, Vec<Value>)], source: Source) -> Result<CardsRecorded> {
        let mut done = CardsRecorded::default();
        let known: HashMap<String, (i64, Option<String>)> = {
            let mut stmt = self.conn.prepare("SELECT server_id, id, version FROM cards WHERE account = ?1 AND server_id IS NOT NULL")?;
            stmt.query_map(params![account], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?
        };
        // cards with a change of the user's still on its way to the server keep it (an edit isn't
        // overwritten by the server's older copy, a deletion isn't brought back)
        let waiting: HashSet<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.server_id FROM pending_ops p JOIN cards c ON c.id = json_extract(p.payload, '$.card')
                 WHERE p.kind IN ('card-update', 'card-create') AND c.account = ?1 AND c.server_id IS NOT NULL
                 UNION SELECT json_extract(payload, '$.server_id') FROM pending_ops WHERE kind = 'card-delete' AND json_extract(payload, '$.account') = ?1",
            )?;
            stmt.query_map(params![account], |r| r.get::<_, Option<String>>(0))?.filter_map(|r| r.ok().flatten()).collect()
        };
        // contacts kept local (`local_only`): only contacts new here come in (never one deleted
        // here); those here keep their own version, and nothing the server removed leaves
        let local = self.local_only_account(account, crate::local_only::Service::Contacts)?;
        let deleted_here = if local { self.local_deleted(account, "cards")? } else { HashSet::new() };
        let mut seen = HashSet::new();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<()> {
            for (book, book_name, contacts) in books {
                for v in contacts {
                    let (id, version, card) = match source {
                        Source::Google => (v["resourceName"].as_str(), v["etag"].as_str(), crate::google::card_from_person(v)),
                        Source::Dav => (v["id"].as_str(), v["etag"].as_str(), from_vcard(v["vcard"].as_str().unwrap_or("")).into_iter().next().unwrap_or_default()),
                        Source::Graph => (v["id"].as_str(), v["changeKey"].as_str(), from_graph(v)),
                    };
                    let Some(id) = id else { continue };
                    seen.insert(id.to_string());
                    if waiting.contains(id) || (local && (known.contains_key(id) || deleted_here.contains(id))) {
                        continue;
                    }
                    let version = version.map(str::to_string);
                    let card = serde_json::to_string(&card)?;
                    match known.get(id) {
                        Some((_, old)) if old == &version && version.is_some() => {}
                        Some((row, _)) => {
                            self.conn.execute(
                                "UPDATE cards SET book = ?2, book_name = ?3, version = ?4, card = ?5, original = ?6, updated = ?7 WHERE id = ?1",
                                params![row, book.clone().unwrap_or_default(), book_name, version, card, v.to_string(), now()],
                            )?;
                            done.changed += 1;
                        }
                        None => {
                            self.conn.execute(
                                "INSERT INTO cards (account, book, book_name, server_id, version, card, original, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                                params![account, book.clone().unwrap_or_default(), book_name, id, version, card, v.to_string(), now()],
                            )?;
                            done.added += 1;
                        }
                    }
                }
            }
            for (server_id, (row, _)) in &known {
                if !local && !seen.contains(server_id) && !waiting.contains(server_id) {
                    self.conn.execute("DELETE FROM cards WHERE id = ?1", params![row])?;
                    done.removed += 1;
                }
            }
            Ok(())
        })();
        self.conn.execute_batch(if outcome.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        outcome.map(|_| done)
    }

    /// The address book: every card of every account (`account` one only), those matching
    /// `query` (any word of the name, an address, a number, the company), by name.
    pub fn cards(&self, query: &str, account: Option<i64>) -> Result<Vec<CardRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.account, COALESCE(a.address, ''), c.book_name, c.card FROM cards c JOIN accounts a ON a.id = c.account
             WHERE (?1 IS NULL OR c.account = ?1)",
        )?;
        let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();
        let mut rows: Vec<CardRow> = stmt
            .query_map(params![account], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?
            .filter_map(|r| r.ok())
            .filter_map(|(id, account, address, book, json)| {
                let card: Card = serde_json::from_str(&json).ok()?;
                Some(CardRow { id, account, account_address: address, book, shown: card.shown(), card })
            })
            .filter(|row| {
                if words.is_empty() {
                    return true;
                }
                let c = &row.card;
                let hay = format!(
                    "{} {} {} {} {} {}",
                    row.shown,
                    c.nickname,
                    c.company,
                    c.emails.iter().map(|e| e.value.as_str()).collect::<Vec<_>>().join(" "),
                    c.phones.iter().map(|p| p.value.as_str()).collect::<Vec<_>>().join(" "),
                    c.notes
                )
                .to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        rows.sort_by(|a, b| a.shown.to_lowercase().cmp(&b.shown.to_lowercase()).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    /// Where a card came from and how it stands with its server, for the Address Book.
    pub fn card_origin(&self, id: i64) -> Result<CardOrigin> {
        let (account, address, kind, book, server_id, updated): (i64, String, String, String, Option<String>, i64) = self.conn.query_row(
            "SELECT a.id, COALESCE(a.address, ''), a.kind, c.book_name, c.server_id, c.updated FROM cards c JOIN accounts a ON a.id = c.account WHERE c.id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        let waiting: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT kind, last_error FROM pending_ops WHERE kind IN ('card-update', 'card-create') AND json_extract(payload, '$.card') = ?1 ORDER BY id DESC LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let dav = self.dav_config_of(account)?.is_some();
        Ok(CardOrigin {
            account,
            address,
            service: if dav { "CardDAV".to_string() } else { crate::db::kind_name(&kind).to_string() },
            book,
            on_server: server_id.is_some(),
            updated,
            waiting: waiting.as_ref().map(|(k, _)| if k == "card-create" { "new, waiting to be made on the server" } else { "changed here, waiting to go to the server" }.to_string()),
            last_error: waiting.and_then(|(_, e)| e),
            editable: kind.starts_with("graph") || kind == "gmail" || dav,
        })
    }

    /// A card whose changes can go back to its server (Microsoft's, Google's, or a CardDAV
    /// server), else why not.
    /// Whether a card's account keeps its contacts local (`local_only`).
    fn card_local_only(&self, id: i64) -> Result<bool> {
        let account: i64 = self.conn.query_row("SELECT account FROM cards WHERE id = ?1", params![id], |r| r.get(0))?;
        self.local_only_account(account, crate::local_only::Service::Contacts)
    }

    fn card_editable(&self, id: i64) -> Result<()> {
        let (kind, account): (String, i64) = self.conn.query_row("SELECT a.kind, a.id FROM cards c JOIN accounts a ON a.id = c.account WHERE c.id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if kind.starts_with("graph") || kind == "gmail" || self.dav_config_of(account)?.is_some() {
            Ok(())
        } else {
            Err(Error::Other("this contact's address book can't be changed from Eudora".into()))
        }
    }

    /// Changes a card: at once here, and on its server through the queue (kept as it was in
    /// `card_history` first).
    pub fn update_card(&self, id: i64, card: &Card) -> Result<()> {
        self.update_card_as(id, card, "edited")
    }

    fn update_card_as(&self, id: i64, card: &Card, reason: &str) -> Result<()> {
        self.card_editable(id)?;
        self.keep_card_history(id, reason)?;
        self.conn.execute("UPDATE cards SET card = ?2, updated = ?3 WHERE id = ?1", params![id, serde_json::to_string(card)?, now()])?;
        let already: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pending_ops WHERE kind IN ('card-update', 'card-create') AND json_extract(payload, '$.card') = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if !already && !self.card_local_only(id)? {
            self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'card-update', ?2)", params![now(), json!({ "card": id }).to_string()])?;
        }
        Ok(())
    }

    /// A new card in an account's address book (`book`: its server id, "" = the default one);
    /// made on the server through the queue. Returns its id.
    pub fn create_card(&self, account: i64, book: &str, card: &Card) -> Result<i64> {
        let book_name: String = self
            .conn
            .query_row("SELECT book_name FROM cards WHERE account = ?1 AND book = ?2 LIMIT 1", params![account, book], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| "Contacts".into());
        self.conn.execute(
            "INSERT INTO cards (account, book, book_name, server_id, version, card, original, updated) VALUES (?1, ?2, ?3, NULL, NULL, ?4, NULL, ?5)",
            params![account, book, book_name, serde_json::to_string(card)?, now()],
        )?;
        let id = self.conn.last_insert_rowid();
        if !self.local_only_account(account, crate::local_only::Service::Contacts)? {
            self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'card-create', ?2)", params![now(), json!({ "card": id }).to_string()])?;
        }
        Ok(id)
    }

    /// Deletes a card: here at once, on its server through the queue (Exchange keeps it in
    /// Deleted Items); kept as it was in `card_history`, so it can be brought back.
    pub fn delete_card(&self, id: i64) -> Result<()> {
        self.delete_card_as(id, "deleted")
    }

    fn delete_card_as(&self, id: i64, reason: &str) -> Result<()> {
        self.card_editable(id)?;
        self.keep_card_history(id, reason)?;
        let (account, server_id): (i64, Option<String>) = self.conn.query_row("SELECT account, server_id FROM cards WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.conn.execute("DELETE FROM pending_ops WHERE kind IN ('card-update', 'card-create') AND json_extract(payload, '$.card') = ?1", params![id])?;
        self.conn.execute("DELETE FROM cards WHERE id = ?1", params![id])?;
        let local = self.local_only_account(account, crate::local_only::Service::Contacts)?;
        if let Some(server_id) = server_id.as_deref().filter(|_| local) {
            // contacts kept local (`local_only`): remembered, so the server's copy isn't brought back
            self.remember_local_delete(account, "cards", server_id)?;
        } else if let Some(server_id) = server_id {
            self.conn.execute(
                "INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'card-delete', ?2)",
                params![now(), json!({ "account": account, "server_id": server_id }).to_string()],
            )?;
        }
        Ok(())
    }

    /// Every account a contact can be added to (its own address book on a server Eudora writes
    /// to: Microsoft 365 / Outlook.com, Gmail, CardDAV), empty ones included, with the address
    /// book a new contact goes into (the one holding most of its contacts) and how many it holds.
    pub fn contact_targets(&self) -> Result<Vec<ContactTarget>> {
        let mut out = Vec::new();
        let accounts: Vec<(i64, String, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, COALESCE(address, ''), kind FROM accounts ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let dav: Vec<crate::dav::DavConfig> = self.dav_accounts()?.into_iter().filter(|c| c.contacts.is_some()).collect();
        for (account, address, kind) in accounts {
            let is_dav = dav.iter().any(|c| c.address.eq_ignore_ascii_case(&address));
            let service = if kind == "graph" {
                "Microsoft 365"
            } else if kind == "gmail" {
                "Google"
            } else if is_dav {
                "CardDAV"
            } else {
                continue;
            };
            let (book, book_name, count): (String, String, i64) = self
                .conn
                .query_row(
                    "SELECT book, book_name, (SELECT COUNT(*) FROM cards WHERE account = ?1) FROM cards WHERE account = ?1 GROUP BY book ORDER BY COUNT(*) DESC LIMIT 1",
                    params![account],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .unwrap_or_else(|| (String::new(), "Contacts".into(), 0));
            out.push(ContactTarget { account, address, service: service.into(), book, book_name, count: count as u64 });
        }
        Ok(out)
    }

    /// The account a new contact starts in: the one chosen last time (setting `contacts-new-in`),
    /// else the first Microsoft 365 account (the main one), else the first that can take one.
    pub fn new_contact_account(&self) -> Result<Option<i64>> {
        let targets = self.contact_targets()?;
        let last: Option<i64> = self.setting(NEW_CONTACT_SETTING)?.and_then(|s| s.trim().parse().ok());
        Ok(last
            .filter(|a| targets.iter().any(|t| t.account == *a))
            .or_else(|| targets.iter().find(|t| t.service == "Microsoft 365").map(|t| t.account))
            .or_else(|| targets.first().map(|t| t.account)))
    }

    /// Remembers the account a new contact went into, for the next one.
    pub fn set_new_contact_account(&self, account: i64) -> Result<()> {
        self.set_setting(NEW_CONTACT_SETTING, &account.to_string())
    }

    /// Copies contacts into another account's address book (each made there through the queue),
    /// or moves them (copied, then deleted from where they were: kept in the history, so nothing
    /// is lost). `skip_existing`: a contact already in that account (an email address in common,
    /// else the same name with no address) is left out. One save for all of them.
    pub fn copy_cards(&self, ids: &[i64], account: i64, move_them: bool, skip_existing: bool) -> Result<CardsCopied> {
        let target = self
            .contact_targets()?
            .into_iter()
            .find(|t| t.account == account)
            .ok_or_else(|| Error::Other("that account has no address book Eudora can add contacts to".into()))?;
        let mut done = CardsCopied::default();
        let tx = self.conn.unchecked_transaction()?;
        // what the target holds already: its email addresses, and the names of cards with none
        let mut emails: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in self.cards("", Some(account))? {
            if row.card.emails.is_empty() {
                names.insert(row.shown.trim().to_lowercase());
            }
            emails.extend(row.card.emails.iter().map(|e| e.value.trim().to_lowercase()));
        }
        for &id in ids {
            let row = match self.card(id) {
                Ok(r) => r,
                Err(e) => {
                    done.failed.push((id, e.to_string()));
                    continue;
                }
            };
            if row.account == account {
                done.skipped += 1;
                continue;
            }
            let theirs: Vec<String> = row.card.emails.iter().map(|e| e.value.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
            let there = if theirs.is_empty() { names.contains(&row.shown.trim().to_lowercase()) } else { theirs.iter().any(|e| emails.contains(e)) };
            if skip_existing && there {
                done.skipped += 1;
                continue;
            }
            if move_them {
                if let Err(e) = self.card_editable(id) {
                    done.failed.push((id, format!("{}: {e}", row.shown)));
                    continue;
                }
            }
            self.create_card(account, &target.book, &row.card)?;
            emails.extend(theirs);
            if row.card.emails.is_empty() {
                names.insert(row.shown.trim().to_lowercase());
            }
            if move_them {
                self.delete_card(id)?;
                done.moved += 1;
            } else {
                done.copied += 1;
            }
        }
        tx.commit()?;
        Ok(done)
    }

    /// Contacts that are the same person, in one account or across accounts: the same email address
    /// or the same phone number (its last ten digits). The same name alone doesn't make one person:
    /// those sets come after, marked `possible`. Biggest groups first. `account`: only groups with
    /// a contact in it.
    pub fn contact_duplicates(&self, account: Option<i64>) -> Result<Vec<DuplicateGroup>> {
        let rows = self.cards("", None)?;
        let mut parent: Vec<usize> = (0..rows.len()).collect();
        fn root(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        // each key → the first card holding it; later cards holding it join that one's group
        let mut first: HashMap<String, usize> = HashMap::new();
        let mut why: HashMap<String, String> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            let c = &row.card;
            let mut keys: Vec<(String, String)> = Vec::new();
            for e in &c.emails {
                let v = e.value.trim().to_lowercase();
                if !v.is_empty() {
                    keys.push((format!("e:{v}"), format!("the same email address {}", e.value.trim())));
                }
            }
            for p in &c.phones {
                if let Some(k) = phone_key(&p.value) {
                    keys.push((format!("p:{k}"), format!("the same phone number {}", p.value.trim())));
                }
            }
            for (key, said) in keys {
                match first.get(&key) {
                    Some(&j) => {
                        let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                        if a != b {
                            parent[a] = b;
                        }
                        why.entry(key).or_insert(said);
                    }
                    None => {
                        first.insert(key, i);
                    }
                }
            }
        }
        let roots: Vec<usize> = (0..rows.len()).map(|i| root(&mut parent, i)).collect();
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, r) in roots.iter().enumerate() {
            groups.entry(*r).or_default().push(i);
        }
        // the same name, not already one person by address or number: maybe the same
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            if let Some(k) = name_key(&row.card) {
                by_name.entry(k).or_default().push(i);
            }
        }
        let mut sets: Vec<(Vec<usize>, bool)> = groups.into_values().filter(|m| m.len() > 1).map(|m| (m, false)).collect();
        sets.extend(by_name.into_values().filter(|m| m.iter().map(|&i| roots[i]).collect::<HashSet<_>>().len() > 1).map(|m| (m, true)));
        let mut out = Vec::new();
        for (members, possible) in sets {
            let cards: Vec<CardRow> = members.iter().map(|&i| rows[i].clone()).collect();
            if account.is_some_and(|a| !cards.iter().any(|c| c.account == a)) {
                continue;
            }
            // the same contact kept alike in several accounts (a merge leaves one in each) is
            // nothing left to merge
            let accounts: HashSet<i64> = cards.iter().map(|c| c.account).collect();
            if accounts.len() == cards.len() && cards.iter().all(|c| c.card == cards[0].card) {
                continue;
            }
            let mut reasons: Vec<String> = Vec::new();
            if possible {
                reasons.push(format!("only the same name, {}: no email address or phone number in common, so it may be someone else", cards[0].shown));
            } else {
                // the reasons that hold inside this group (a key two of its cards share)
                for (key, said) in &why {
                    let holders = cards
                        .iter()
                        .filter(|r| match key.split_once(':') {
                            Some(("e", v)) => r.card.emails.iter().any(|e| e.value.trim().to_lowercase() == v),
                            Some(("p", v)) => r.card.phones.iter().any(|p| phone_key(&p.value).as_deref() == Some(v)),
                            _ => false,
                        })
                        .count();
                    if holders > 1 {
                        reasons.push(said.clone());
                    }
                }
                reasons.sort();
            }
            let (merged, keep) = Self::merged_group(&cards, None);
            let differences = card_differences(&cards.iter().map(|c| &c.card).collect::<Vec<_>>());
            out.push(DuplicateGroup { cards, why: reasons, keep, merged, possible, differences });
        }
        out.sort_by(|a, b| {
            a.possible
                .cmp(&b.possible)
                .then(b.cards.len().cmp(&a.cards.len()))
                .then(a.merged.shown().to_lowercase().cmp(&b.merged.shown().to_lowercase()))
        });
        Ok(out)
    }

    /// The merged card of a group, led by `keep` (else the card most filled in), and its id.
    fn merged_group(cards: &[CardRow], keep: Option<i64>) -> (Card, i64) {
        let lead = keep
            .and_then(|k| cards.iter().find(|c| c.id == k))
            .or_else(|| cards.iter().max_by(|a, b| filled(&a.card).cmp(&filled(&b.card)).then(b.id.cmp(&a.id))))
            .expect("a group has cards");
        let others: Vec<&Card> = cards.iter().filter(|c| c.id != lead.id).map(|c| &c.card).collect();
        (merge_of(&lead.card, &others), lead.id)
    }

    /// What merging these contacts would make (`keep` leads), without changing anything.
    pub fn merge_preview(&self, ids: &[i64], keep: Option<i64>) -> Result<Card> {
        let cards = ids.iter().map(|&id| self.card(id)).collect::<Result<Vec<_>>>()?;
        if cards.is_empty() {
            return Err(Error::Other("no contacts to merge".into()));
        }
        Ok(Self::merged_group(&cards, keep).0)
    }

    /// Merges contacts into one, on their servers too (decided 2026-10-18: always on the
    /// servers): in each account one of them (`keep` where it's there, else the fullest) becomes
    /// the merged contact (`card` as the user edited it, else `merge_preview`'s), and the account's
    /// other copies are deleted. Every card changed or deleted is kept as it was first, so
    /// `undo_merge` can bring it all back. One save for all of it.
    pub fn merge_cards(&self, ids: &[i64], keep: Option<i64>, card: Option<&Card>) -> Result<CardsMerged> {
        let mut done = CardsMerged::default();
        let mut cards = Vec::new();
        for &id in ids {
            match self.card(id).and_then(|row| self.card_editable(id).map(|_| row)) {
                Ok(row) => cards.push(row),
                Err(e) => done.failed.push((id, e.to_string())),
            }
        }
        if cards.len() < 2 {
            return Err(Error::Other(match done.failed.first() {
                Some((_, why)) => format!("there's nothing to merge: {why}"),
                None => "choose two contacts or more to merge".into(),
            }));
        }
        let (computed, lead) = Self::merged_group(&cards, keep);
        let merged = card.cloned().unwrap_or(computed);
        done.merge = format!("{}-{}", now(), lead);
        let tx = self.conn.unchecked_transaction()?;
        let mut accounts: Vec<i64> = cards.iter().map(|c| c.account).collect();
        accounts.dedup();
        accounts.sort();
        accounts.dedup();
        for account in accounts {
            let here: Vec<&CardRow> = cards.iter().filter(|c| c.account == account).collect();
            let keeper = here
                .iter()
                .find(|c| c.id == lead)
                .or_else(|| here.iter().max_by(|a, b| filled(&a.card).cmp(&filled(&b.card)).then(b.id.cmp(&a.id))))
                .map(|c| c.id)
                .expect("an account here has cards");
            for c in &here {
                if c.id == keeper {
                    if c.card != merged {
                        self.update_card_as(c.id, &merged, &format!("merge {} edited", done.merge))?;
                    } else {
                        self.keep_card_history(c.id, &format!("merge {} kept", done.merge))?;
                    }
                    done.updated += 1;
                } else {
                    self.delete_card_as(c.id, &format!("merge {} deleted", done.merge))?;
                    done.deleted += 1;
                }
            }
        }
        tx.commit()?;
        Ok(done)
    }

    /// The merges of the last 30 days, newest first.
    pub fn recent_merges(&self) -> Result<Vec<MergeDone>> {
        // ("merge <mark> edited|kept|deleted"; a card kept names it, as it is now: merged)
        let mut stmt = self.conn.prepare("SELECT h.reason, h.at, COALESCE(c.card, h.card_json) FROM card_history h LEFT JOIN cards c ON c.id = h.card WHERE h.reason LIKE 'merge %' AND h.at > ?1 ORDER BY h.id")?;
        let rows = stmt.query_map(params![now() - 30 * 86_400], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
        let mut out: Vec<MergeDone> = Vec::new();
        for row in rows {
            let (reason, at, json) = row?;
            let mut words = reason.split(' ');
            let (Some("merge"), Some(merge), Some(what)) = (words.next(), words.next(), words.next()) else { continue };
            let i = match out.iter().position(|m| m.merge == merge) {
                Some(i) => i,
                None => {
                    out.push(MergeDone { merge: merge.to_string(), at, name: String::new(), cards: 0 });
                    out.len() - 1
                }
            };
            out[i].cards += 1;
            if what != "deleted" && out[i].name.is_empty() {
                out[i].name = serde_json::from_str::<Card>(&json).map(|c| c.shown()).unwrap_or_default();
            }
        }
        out.reverse();
        Ok(out)
    }

    /// Takes a merge back: each contact changed into the merged one is changed back, each copy
    /// deleted is made again (in its account and address book, on its server through the queue).
    /// Returns how many contacts were brought back.
    pub fn undo_merge(&self, merge: &str) -> Result<u64> {
        let rows: Vec<(i64, Option<i64>, i64, String, String, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, card, account, book, card_json, reason FROM card_history WHERE reason IN (?1, ?2, ?3) ORDER BY id")?;
            let r = stmt.query_map(params![format!("merge {merge} edited"), format!("merge {merge} deleted"), format!("merge {merge} kept")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            r.collect::<rusqlite::Result<_>>()?
        };
        if rows.is_empty() {
            return Err(Error::Other("that merge isn't in the history any more (or was already undone)".into()));
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut back = 0;
        for (history, card, account, book, json, reason) in &rows {
            let old: Card = serde_json::from_str(json)?;
            let exists = match card {
                Some(id) => self.conn.query_row("SELECT EXISTS (SELECT 1 FROM cards WHERE id = ?1)", params![id], |r| r.get::<_, bool>(0))?,
                None => false,
            };
            if reason.ends_with(" kept") {
                // (unchanged by the merge: nothing to put back)
            } else if exists && reason.ends_with(" edited") {
                self.update_card_as(card.expect("exists"), &old, "undoing a merge")?;
                back += 1;
            } else {
                self.create_card(*account, book, &old)?;
                back += 1;
            }
            self.conn.execute("UPDATE card_history SET reason = 'undone ' || reason WHERE id = ?1", params![history])?;
        }
        tx.commit()?;
        Ok(back)
    }

    /// Share: a draft with the contact attached as a vCard (`Name.vcf`, read by Outlook, Apple,
    /// Google and phones) and summed up in the text, from the contact's own account when the
    /// user can send from it. Returns the draft's id (the window opens it to address and send).
    pub fn share_card_draft(&self, id: i64) -> Result<i64> {
        use base64::Engine;
        let row = self.card(id)?;
        let c = &row.card;
        let name = if row.shown.trim().is_empty() { "Contact".to_string() } else { row.shown.clone() };
        let mut lines = vec![name.clone()];
        let what = [c.title.as_str(), c.company.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
        if !what.is_empty() {
            lines.push(what);
        }
        lines.extend(c.emails.iter().map(|e| e.value.clone()));
        let kind = |k: &str| match k {
            "mobile" => "Mobile",
            "work" => "Work",
            "home" => "Home",
            "fax" => "Fax",
            _ => "Phone",
        };
        lines.extend(c.phones.iter().map(|p| format!("{}: {}", kind(&p.kind), p.value)));
        let text = format!("{}\n\n(The contact card is attached.)", lines.join("\n"));
        let html = format!("<p>{}</p><p>(The contact card is attached.)</p>", lines.iter().map(|l| crate::compose::escape(l)).collect::<Vec<_>>().join("<br>"));
        let identities = self.identities()?;
        let from = identities
            .iter()
            .find(|i| i.can_send && i.address.eq_ignore_ascii_case(&row.account_address))
            .or_else(|| identities.iter().find(|i| i.can_send))
            .map(|i| (i.address.clone(), i.name.clone()))
            .unwrap_or_default();
        let file = format!("{}.vcf", crate::saving::safe_file_name(&name));
        let draft = crate::compose::Draft {
            from: from.0,
            from_name: from.1,
            subject: format!("Contact: {name}"),
            text,
            html,
            attachments: vec![crate::compose::NewAttachment { name: file, mime: "text/vcard".into(), data: base64::engine::general_purpose::STANDARD.encode(to_vcard(c)) }],
            ..Default::default()
        };
        self.save_draft(None, &serde_json::to_string(&draft)?)
    }

    fn keep_card_history(&self, id: i64, reason: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO card_history (card, account, book, server_id, original, card_json, reason, at) SELECT id, account, book, server_id, original, card, ?2, ?3 FROM cards WHERE id = ?1",
            params![id, reason, now()],
        )?;
        Ok(())
    }

    /// How many contacts each account's address books hold: (account, its address, book, count),
    /// for the sidebar.
    pub fn card_counts(&self) -> Result<Vec<(i64, String, String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.account, COALESCE(a.address, ''), c.book_name, COUNT(*) FROM cards c JOIN accounts a ON a.id = c.account
             GROUP BY c.account, c.book_name ORDER BY a.address, c.book_name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u64)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One card.
    pub fn card(&self, id: i64) -> Result<CardRow> {
        let found: Option<(i64, String, String, String)> = self
            .conn
            .query_row(
                "SELECT c.account, COALESCE(a.address, ''), c.book_name, c.card FROM cards c JOIN accounts a ON a.id = c.account WHERE c.id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (account, address, book, json) = found.ok_or_else(|| Error::Other(format!("no card {id}")))?;
        let card: Card = serde_json::from_str(&json)?;
        Ok(CardRow { id, account, account_address: address, book, shown: card.shown(), card })
    }

    /// The address book written to a file: vCard (`.vcf`) or CSV (`.csv`), by the file's name.
    pub fn export_cards(&self, path: &std::path::Path, account: Option<i64>) -> Result<u64> {
        let cards: Vec<Card> = self.cards("", account)?.into_iter().map(|r| r.card).collect();
        let csv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv"));
        let text = if csv { to_csv(&cards) } else { cards.iter().map(to_vcard).collect::<String>() };
        std::fs::write(path, text).map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
        Ok(cards.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann() -> Card {
        Card {
            given: "Ann".into(),
            family: "Smith, Jr".into(),
            company: "Acme; Inc".into(),
            emails: vec![Labeled { kind: "work".into(), value: "ann@acme.example".into() }],
            phones: vec![Labeled { kind: "mobile".into(), value: "+1 555 0100".into() }, Labeled { kind: "work".into(), value: "+1 555 0101".into() }],
            addresses: vec![Postal { kind: "work".into(), street: "1 Main St".into(), city: "Springfield".into(), region: "IL".into(), postal_code: "62701".into(), country: "USA".into() }],
            birthday: "1970-05-04".into(),
            notes: "Met at the fair\nlikes tea".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_card_survives_vcard_and_graph() {
        let card = ann();
        assert_eq!(card.shown(), "Ann Smith, Jr");
        let back = from_vcard(&to_vcard(&card));
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!((b.given.as_str(), b.family.as_str(), b.company.as_str(), b.notes.as_str()), ("Ann", "Smith, Jr", "Acme; Inc", "Met at the fair\nlikes tea"));
        assert_eq!(b.emails, card.emails);
        assert_eq!(b.phones, card.phones);
        assert_eq!(b.addresses, card.addresses);
        assert_eq!(b.birthday, "1970-05-04");
        let g = from_graph(&to_graph(&card));
        assert_eq!((g.given.as_str(), g.company.as_str()), ("Ann", "Acme; Inc"));
        assert_eq!(g.emails[0].value, "ann@acme.example");
        assert_eq!(g.phones.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(), vec!["mobile", "work"]);
        assert_eq!(g.addresses[0].city, "Springfield");
        assert_eq!(g.birthday, "1970-05-04");
    }

    #[test]
    fn vcards_from_other_programs_are_read() {
        // vCard 2.1 from an old phone, folded lines, a group prefix, and two cards in one file
        let text = "BEGIN:VCARD\r\nVERSION:2.1\r\nN:Doe;John\r\nTEL;CELL:555-1234\r\nitem1.EMAIL;TYPE=INTERNET:john@x.example\r\nNOTE:a very long note that\r\n  goes on\r\nEND:VCARD\r\nBEGIN:VCARD\nVERSION:4.0\nFN:Bea\nEMAIL:mailto:bea@y.example\nBDAY:19800102\nEND:VCARD\n";
        let cards = from_vcard(text);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].shown(), "John Doe");
        assert_eq!(cards[0].phones[0], Labeled { kind: "mobile".into(), value: "555-1234".into() });
        assert_eq!(cards[0].emails[0].value, "john@x.example");
        assert_eq!(cards[0].notes, "a very long note that goes on");
        assert_eq!((cards[1].shown().as_str(), cards[1].emails[0].value.as_str(), cards[1].birthday.as_str()), ("Bea", "bea@y.example", "1980-01-02"));
    }

    #[test]
    fn csv_quotes_what_needs_it() {
        let csv = to_csv(&[ann()]);
        let line = csv.lines().nth(1).unwrap();
        assert!(line.starts_with("Ann,,\"Smith, Jr\",\"Ann Smith, Jr\",,Acme; Inc,"), "{line}");
        assert!(csv.contains("\"Met at the fair\nlikes tea\""));
    }

    #[test]
    fn contacts_are_copied_or_moved_to_another_account_and_the_choice_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        // an IMAP account (no address book), a Gmail one, an empty Microsoft 365 one
        library.conn.execute("INSERT INTO accounts (id, kind, name, address, created) VALUES (3, 'imap', 'm', 'me@imap.example', 0)", []).unwrap();
        library.conn.execute("INSERT INTO accounts (id, kind, name, address, created) VALUES (5, 'gmail', 'g', 'me@gmail.example', 0)", []).unwrap();
        library.conn.execute("INSERT INTO accounts (id, kind, name, address, created) VALUES (7, 'graph', 'w', 'me@work.example', 0)", []).unwrap();
        let person = |name: &str, email: &str| Card { name: name.into(), emails: vec![Labeled { kind: "work".into(), value: email.into() }], ..Default::default() };
        let books = vec![(Some("people".to_string()), "My Contacts".to_string(), vec![
            json!({ "resourceName": "people/1", "etag": "1", "names": [{ "displayName": "Ann" }], "emailAddresses": [{ "value": "ann@x.example" }] }),
        ])];
        library.record_cards_from(5, &books, Source::Google).unwrap();
        // every account that takes contacts, empty ones too; the main (Microsoft) one first chosen
        let targets = library.contact_targets().unwrap();
        assert_eq!(targets.iter().map(|t| (t.account, t.service.as_str(), t.count)).collect::<Vec<_>>(), vec![(5, "Google", 1), (7, "Microsoft 365", 0)]);
        assert_eq!(targets[0].book, "people");
        assert_eq!(library.new_contact_account().unwrap(), Some(7));
        library.set_new_contact_account(5).unwrap();
        assert_eq!(library.new_contact_account().unwrap(), Some(5), "the last choice is kept");
        // Bob and Ann in Gmail; Ann is already in the work account
        let bob = library.create_card(5, "", &person("Bob", "bob@x.example")).unwrap();
        let ann = library.cards("ann", Some(5)).unwrap()[0].id;
        library.create_card(7, "", &person("Ann Smith", "ANN@x.example")).unwrap();
        library.conn.execute("DELETE FROM pending_ops", []).unwrap();
        let copied = library.copy_cards(&[ann, bob], 7, false, true).unwrap();
        assert_eq!((copied.copied, copied.moved, copied.skipped), (1, 0, 1), "Ann is there already");
        assert_eq!(library.cards("", Some(5)).unwrap().len(), 2, "a copy leaves them where they were");
        let creates: i64 = library.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = 'card-create'", [], |r| r.get(0)).unwrap();
        assert_eq!(creates, 1);
        // a move: made there, deleted here (kept in the history); Ann copied too when asked
        let moved = library.copy_cards(&[ann, bob], 7, true, false).unwrap();
        assert_eq!((moved.copied, moved.moved, moved.skipped), (0, 2, 0));
        assert!(library.cards("", Some(5)).unwrap().is_empty());
        assert_eq!(library.cards("", Some(7)).unwrap().len(), 4);
        let history: i64 = library.conn.query_row("SELECT COUNT(*) FROM card_history WHERE reason = 'deleted'", [], |r| r.get(0)).unwrap();
        assert_eq!(history, 2);
        // not to an account without an address book
        assert!(library.copy_cards(&[bob], 3, false, false).is_err());
    }

    #[test]
    fn duplicates_are_found_merged_on_every_account_and_the_merge_undone() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        library.conn.execute("INSERT INTO accounts (id, kind, name, address, created) VALUES (5, 'gmail', 'g', 'me@gmail.example', 0)", []).unwrap();
        library.conn.execute("INSERT INTO accounts (id, kind, name, address, created) VALUES (7, 'graph', 'w', 'me@work.example', 0)", []).unwrap();
        let labeled = |v: &str| Labeled { kind: "work".into(), value: v.into() };
        // Ann twice at work (once by address, once by phone), once in Gmail by name; Bob alone;
        // two Johns (one word: not the same person for that alone)
        let ann_work = library.create_card(7, "", &Card { name: "Ann Smith".into(), emails: vec![labeled("ann@x.example")], company: "X Corp".into(), ..Default::default() }).unwrap();
        let ann_phone = library.create_card(7, "", &Card { name: "A. Smith".into(), emails: vec![labeled("ANN@x.example")], phones: vec![labeled("+1 (813) 555-0100")], notes: "Met at the fair".into(), ..Default::default() }).unwrap();
        let ann_gmail = library.create_card(5, "", &Card { name: "ann smith".into(), phones: vec![labeled("813.555.0100")], emails: vec![labeled("ann@home.example")], ..Default::default() }).unwrap();
        library.create_card(5, "", &Card { name: "Bob Jones".into(), emails: vec![labeled("bob@x.example")], ..Default::default() }).unwrap();
        library.create_card(7, "", &Card { name: "Bob Jones".into(), emails: vec![labeled("bob@elsewhere.example")], ..Default::default() }).unwrap();
        library.create_card(5, "", &Card { name: "John".into(), ..Default::default() }).unwrap();
        library.create_card(7, "", &Card { name: "John".into(), ..Default::default() }).unwrap();
        library.conn.execute("DELETE FROM pending_ops", []).unwrap();
        library.conn.execute("UPDATE cards SET server_id = 'srv' || id", []).unwrap();

        let groups = library.contact_duplicates(None).unwrap();
        assert_eq!(groups.len(), 2, "{groups:?}");
        let g = &groups[0];
        assert!(!g.possible);
        // two Bob Joneses with nothing else in common: maybe, maybe not, listed after
        assert!(groups[1].possible && groups[1].cards.len() == 2 && groups[1].why[0].contains("only the same name"));
        assert_eq!(g.differences, vec!["name"], "the rest merges by itself");
        let mut ids: Vec<i64> = g.cards.iter().map(|c| c.id).collect();
        ids.sort();
        assert_eq!(ids, vec![ann_work, ann_phone, ann_gmail]);
        assert!(g.why.iter().any(|w| w.contains("email address")) && g.why.iter().any(|w| w.contains("phone number")), "{:?}", g.why);
        // nothing lost: every address once, the phone once, the notes and company kept
        let m = &g.merged;
        assert_eq!(m.emails.len(), 2);
        assert_eq!(m.phones.len(), 1);
        assert_eq!((m.company.as_str(), m.notes.as_str()), ("X Corp", "Met at the fair"));
        assert_eq!(library.contact_duplicates(Some(5)).unwrap().len(), 2);

        // merged with Ann Smith (work) leading: one card per account, the extra one deleted
        let done = library.merge_cards(&ids, Some(ann_work), None).unwrap();
        assert_eq!((done.updated, done.deleted), (2, 1));
        let work = library.cards("", Some(7)).unwrap();
        assert!(work.iter().any(|r| r.id == ann_work && r.card.name == "Ann Smith" && r.card.phones.len() == 1));
        assert!(!work.iter().any(|r| r.id == ann_phone));
        assert_eq!(library.card(ann_gmail).unwrap().card.name, "Ann Smith", "the Gmail copy is the merged contact too");
        let ops: Vec<String> = {
            let mut s = library.conn.prepare("SELECT kind FROM pending_ops ORDER BY kind").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(ops, vec!["card-delete", "card-update", "card-update"], "the servers get it");
        assert!(library.contact_duplicates(None).unwrap().iter().all(|g| g.possible), "Ann is done");
        let merges = library.recent_merges().unwrap();
        assert_eq!((merges.len(), merges[0].cards, merges[0].name.as_str()), (1, 3, "Ann Smith"));

        // undone: each as it was, the deleted one made again
        assert_eq!(library.undo_merge(&done.merge).unwrap(), 3);
        assert_eq!(library.card(ann_gmail).unwrap().card.name, "ann smith");
        assert_eq!(library.card(ann_work).unwrap().card.phones.len(), 0);
        assert!(library.cards("", Some(7)).unwrap().iter().any(|r| r.card.name == "A. Smith" && r.card.notes == "Met at the fair"));
        assert!(library.recent_merges().unwrap().is_empty());
        assert!(library.undo_merge(&done.merge).is_err(), "only once");
        assert!(library.merge_cards(&[ann_work], None, None).is_err(), "one contact isn't a merge");
    }

    #[test]
    fn fetched_address_books_are_recorded_changed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        library.conn.execute("INSERT INTO accounts (id, kind, name, address, created) VALUES (7, 'graph', 'me', 'me@x.example', 0)", []).unwrap();
        let contact = |id: &str, key: &str, name: &str| json!({ "id": id, "changeKey": key, "displayName": name, "emailAddresses": [{ "address": format!("{}@x.example", name.to_lowercase()) }] });
        let books = vec![(None, "Contacts".to_string(), vec![contact("a", "1", "Ann"), contact("b", "1", "Bob")])];
        assert_eq!(library.record_contacts(7, &books).unwrap(), CardsRecorded { added: 2, changed: 0, removed: 0 });
        assert_eq!(library.cards("", None).unwrap().iter().map(|r| r.shown.as_str()).collect::<Vec<_>>(), vec!["Ann", "Bob"]);
        // the same again: nothing; Ann changed and Bob gone
        assert_eq!(library.record_contacts(7, &books).unwrap(), CardsRecorded::default());
        let books = vec![(Some("f1".to_string()), "Friends".to_string(), vec![contact("a", "2", "Annie")])];
        assert_eq!(library.record_contacts(7, &books).unwrap(), CardsRecorded { added: 0, changed: 1, removed: 1 });
        let rows = library.cards("annie", None).unwrap();
        assert_eq!((rows.len(), rows[0].book.as_str(), rows[0].account_address.as_str()), (1, "Friends", "me@x.example"));
        assert_eq!(library.contacts_jobs("ME@x.example").unwrap().len(), 1);
        // an edit: at once here, one change queued, and the server's older copy doesn't undo it
        let annie = rows[0].id;
        let mut card = rows[0].card.clone();
        card.name = "Annie Smith".into();
        library.update_card(annie, &card).unwrap();
        library.update_card(annie, &card).unwrap();
        let queued: i64 = library.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = 'card-update'", [], |r| r.get(0)).unwrap();
        assert_eq!(queued, 1, "one change on its way, however many edits");
        assert_eq!(library.record_contacts(7, &books).unwrap(), CardsRecorded::default());
        assert_eq!(library.card(annie).unwrap().shown, "Annie Smith");
        assert!(library.card_origin(annie).unwrap().waiting.is_some());
        // a new one, then a deletion: kept in the history, not brought back by the server
        let bea = library.create_card(7, "", &Card { name: "Bea".into(), ..Default::default() }).unwrap();
        let created: i64 = library.conn.query_row("SELECT COUNT(*) FROM pending_ops WHERE kind = 'card-create'", [], |r| r.get(0)).unwrap();
        assert_eq!(created, 1);
        library.delete_card(annie).unwrap();
        assert_eq!(library.record_contacts(7, &books).unwrap(), CardsRecorded::default(), "the deleted one isn't added again");
        assert_eq!(library.cards("", None).unwrap().iter().map(|r| r.id).collect::<Vec<_>>(), vec![bea]);
        let history: i64 = library.conn.query_row("SELECT COUNT(*) FROM card_history WHERE reason = 'deleted'", [], |r| r.get(0)).unwrap();
        assert_eq!(history, 1);
        library.conn.execute("DELETE FROM pending_ops", []).unwrap();
        library.delete_card(bea).unwrap();
        library.create_card(7, "f1", &Card { name: "Annie".into(), ..Default::default() }).unwrap();
        let file = dir.path().join("book.vcf");
        assert_eq!(library.export_cards(&file, None).unwrap(), 1);
        assert!(std::fs::read_to_string(&file).unwrap().contains("FN:Annie"));
        // Share: a draft with the card attached as a vCard
        let annie = library.cards("annie", None).unwrap()[0].id;
        let draft = library.share_card_draft(annie).unwrap();
        let json: String = library.conn.query_row("SELECT payload FROM pending_ops WHERE id = ?1 AND kind = 'draft'", [draft], |r| r.get(0)).unwrap();
        let d: crate::compose::Draft = serde_json::from_str(&json).unwrap();
        assert_eq!((d.subject.as_str(), d.attachments[0].name.as_str(), d.attachments[0].mime.as_str()), ("Contact: Annie", "Annie.vcf", "text/vcard"));
        use base64::Engine;
        let vcf = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(&d.attachments[0].data).unwrap()).unwrap();
        assert!(vcf.starts_with("BEGIN:VCARD") && vcf.contains("FN:Annie"));
    }
}
