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
pub struct Labeled {
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Google's People API contacts (one book), not Microsoft's.
    pub google: bool,
}

/// What recording an account's address books did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CardsRecorded {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
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
    /// Changes go back to its server (Microsoft 365, Outlook.com, Google).
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
        self.record_books(account, books, false)
    }

    /// A Google account's contacts (People API), recorded the same way: one address book,
    /// "Google Contacts"; each person's `resourceName` its id, its `etag` its version.
    pub fn record_google_contacts(&self, account: i64, people: &[Value]) -> Result<CardsRecorded> {
        self.record_books(account, &[(None, "Google Contacts".to_string(), people.to_vec())], true)
    }

    fn record_books(&self, account: i64, books: &[(Option<String>, String, Vec<Value>)], google: bool) -> Result<CardsRecorded> {
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
        let mut seen = HashSet::new();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<()> {
            for (book, book_name, contacts) in books {
                for v in contacts {
                    let (id, version, card) = if google {
                        (v["resourceName"].as_str(), v["etag"].as_str(), crate::google::card_from_person(v))
                    } else {
                        (v["id"].as_str(), v["changeKey"].as_str(), from_graph(v))
                    };
                    let Some(id) = id else { continue };
                    seen.insert(id.to_string());
                    if waiting.contains(id) {
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
                if !seen.contains(server_id) && !waiting.contains(server_id) {
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
        Ok(CardOrigin {
            account,
            address,
            service: crate::db::kind_name(&kind).to_string(),
            book,
            on_server: server_id.is_some(),
            updated,
            waiting: waiting.as_ref().map(|(k, _)| if k == "card-create" { "new, waiting to be made on the server" } else { "changed here, waiting to go to the server" }.to_string()),
            last_error: waiting.and_then(|(_, e)| e),
            editable: kind.starts_with("graph") || kind == "gmail",
        })
    }

    /// A card whose changes can go back to its server (Microsoft's or Google's), else why not.
    fn card_editable(&self, id: i64) -> Result<()> {
        let kind: String = self.conn.query_row("SELECT a.kind FROM cards c JOIN accounts a ON a.id = c.account WHERE c.id = ?1", params![id], |r| r.get(0))?;
        if kind.starts_with("graph") || kind == "gmail" {
            Ok(())
        } else {
            Err(Error::Other("this contact comes from Google: change it in Google Contacts (changing Google contacts in Eudora comes later)".into()))
        }
    }

    /// Changes a card: at once here, and on its server through the queue (kept as it was in
    /// `card_history` first).
    pub fn update_card(&self, id: i64, card: &Card) -> Result<()> {
        self.card_editable(id)?;
        self.keep_card_history(id, "edited")?;
        self.conn.execute("UPDATE cards SET card = ?2, updated = ?3 WHERE id = ?1", params![id, serde_json::to_string(card)?, now()])?;
        let already: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pending_ops WHERE kind IN ('card-update', 'card-create') AND json_extract(payload, '$.card') = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if !already {
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
        self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'card-create', ?2)", params![now(), json!({ "card": id }).to_string()])?;
        Ok(id)
    }

    /// Deletes a card: here at once, on its server through the queue (Exchange keeps it in
    /// Deleted Items); kept as it was in `card_history`, so it can be brought back.
    pub fn delete_card(&self, id: i64) -> Result<()> {
        self.card_editable(id)?;
        self.keep_card_history(id, "deleted")?;
        let (account, server_id): (i64, Option<String>) = self.conn.query_row("SELECT account, server_id FROM cards WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.conn.execute("DELETE FROM pending_ops WHERE kind IN ('card-update', 'card-create') AND json_extract(payload, '$.card') = ?1", params![id])?;
        self.conn.execute("DELETE FROM cards WHERE id = ?1", params![id])?;
        if let Some(server_id) = server_id {
            self.conn.execute(
                "INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'card-delete', ?2)",
                params![now(), json!({ "account": account, "server_id": server_id }).to_string()],
            )?;
        }
        Ok(())
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
            addresses: vec![Postal { kind: "work".into(), street: "1 Main St".into(), city: "Rockaway".into(), region: "NJ".into(), postal_code: "07866".into(), country: "USA".into() }],
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
        assert_eq!(g.addresses[0].city, "Rockaway");
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
